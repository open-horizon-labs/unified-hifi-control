//! Optional MQTT publisher: exposes every UHC zone to Home Assistant via
//! MQTT discovery (#508).
//!
//! Fully off by default. Once configured with a broker and enabled, this
//! module is a plain bus consumer: it reads [`crate::bus::SharedBus`] and
//! [`crate::aggregator::ZoneAggregator`] snapshots to publish HA discovery
//! configs and retained per-zone state, and routes inbound HA command
//! topics through [`crate::adapters::AdapterRegistry`] for registry-backed
//! providers and the reliable command gateway
//! (`crate::bus::runtime::CommandGateway`, #529) for legacy ones - the same
//! sanctioned surfaces every other UHC feature outside `src/adapters/`,
//! `src/bus/`, `src/aggregator.rs`, `src/coordinator.rs` and `src/main.rs`
//! is restricted to (`tests/adapter_boundary_lint.rs`). See [`command`]'s
//! module doc for exactly which zone prefix routes where.
//!
//! Home Assistant has no native MQTT `media_player` platform (see
//! [`discovery`]'s module doc), so one UHC zone is represented as a small
//! HA device composed of a state `sensor`, an `image`, a volume `number`,
//! a mute `switch`, and up to four transport `button`s.
//!
//! Control devices ("knobs") are published the same way (#523), but every
//! entity they need - `sensor`, `binary_sensor`, `select`, `number` - is
//! natively supported by HA MQTT discovery, so [`knob_discovery`] needs no
//! workaround composition. Unlike zones, the knob store has no bus events
//! of its own (battery/last-seen updates land via HTTP polls from the
//! device, not the zone bus), so knob state is re-published on a fixed
//! timer (see `run`'s `knob_tick`) rather than driven by [`BusEvent`]s -
//! zone-add/remove events still trigger an immediate re-announce so the
//! zone-reassignment `select`'s options stay current without waiting for
//! the next tick.

pub mod command;
pub mod discovery;
pub mod knob_command;
pub mod knob_discovery;
pub mod knob_state;
pub mod state;
pub mod topics;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use rumqttc::{AsyncClient, Event, LastWill, MqttOptions, Packet, QoS, Transport};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::aggregator::ZoneAggregator;
use crate::api::AdapterRegistry;
use crate::bus::runtime::CommandGateway;
use crate::bus::{BusEvent, SharedBus, Zone};
use crate::knobs::store::Knob;
use crate::knobs::KnobStore;

pub use crate::api::credentials::{MqttConfigSource, MqttCredentialRecord};

/// How often knob state/discovery is re-published, since the knob store has
/// no bus events of its own to react to (see module doc).
const KNOB_POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Default namespace for state/command topics, distinct from HA's
/// `discovery_prefix` so operators can point discovery at a shared
/// `homeassistant` prefix while keeping UHC's own traffic namespaced.
pub const DEFAULT_BASE_TOPIC: &str = "unified-hifi";
/// Default Home Assistant MQTT discovery prefix.
pub const DEFAULT_DISCOVERY_PREFIX: &str = "homeassistant";
pub const DEFAULT_PORT: u16 = 1883;
pub const DEFAULT_TLS_PORT: u16 = 8883;

/// Whether the publisher is actually talking to a broker (#607).
///
/// Deliberately *not* the same question as [`MqttStatus::running`], which
/// only says the background task exists. `rumqttc` retries a broker it
/// cannot reach forever, so a task can be alive and healthy while nothing
/// has ever been published - which is what made a typo'd broker host look
/// identical to a working one in Settings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MqttConnectionState {
    /// No publisher task is running at all (switched off, or configured but
    /// not enabled).
    #[default]
    Disconnected,
    /// A task is running and trying to reach the broker. Because `rumqttc`
    /// retries indefinitely, this is also where a *failed* attempt lands -
    /// [`MqttStatus::last_error`] carries why the last one failed.
    Connecting,
    /// The broker accepted the connection (`ConnAck`) and it has not failed
    /// since. Only this state means entities are really being published.
    Connected,
}

impl MqttConnectionState {
    /// Wire form for `/api/mqtt/status`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disconnected => "disconnected",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
        }
    }
}

/// Live connection state shared between one publisher task and
/// [`MqttPublisher::status`] (#607).
///
/// A `std::sync::RwLock` rather than the runtime's async `Mutex`: the task
/// records transitions inline in its `select!` arms, and awaiting the
/// runtime lock there would tie the event loop to whatever
/// `configure`/`set_enabled` is doing (they hold it across `stop_task`,
/// which waits up to 5s for this very task to exit).
#[derive(Debug)]
struct ConnectionMonitor {
    inner: std::sync::RwLock<ConnectionSnapshot>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ConnectionSnapshot {
    state: MqttConnectionState,
    last_error: Option<String>,
}

impl ConnectionMonitor {
    /// A monitor for a task that is about to start dialling the broker.
    fn connecting() -> Self {
        Self {
            inner: std::sync::RwLock::new(ConnectionSnapshot {
                state: MqttConnectionState::Connecting,
                last_error: None,
            }),
        }
    }

    fn snapshot(&self) -> ConnectionSnapshot {
        self.inner
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// The broker accepted us. Clears any previous error: it described an
    /// attempt that has since been superseded by a working one.
    fn connected(&self) {
        if let Ok(mut guard) = self.inner.write() {
            guard.state = MqttConnectionState::Connected;
            guard.last_error = None;
        }
    }

    /// The connection failed, or dropped after having succeeded. Never
    /// latches `Connected`: `rumqttc` will dial again, so the honest state
    /// is "connecting", annotated with why the last attempt ended.
    fn interrupted(&self, error: impl Into<String>) {
        if let Ok(mut guard) = self.inner.write() {
            guard.state = MqttConnectionState::Connecting;
            guard.last_error = Some(error.into());
        }
    }
}

/// One live publisher task, with the connection state it reports into.
struct RunningTask {
    shutdown: CancellationToken,
    handle: JoinHandle<()>,
    connection: Arc<ConnectionMonitor>,
}

/// Lifecycle state guarded together so configure/enable/disable never race
/// each other into starting two publisher tasks.
#[derive(Default)]
struct Runtime {
    record: Option<MqttCredentialRecord>,
    enabled: bool,
    task: Option<RunningTask>,
}

/// Optional MQTT publisher. Held on `AppState` as `Arc<MqttPublisher>`;
/// cheap to construct, inert until [`MqttPublisher::configure`] and
/// [`MqttPublisher::set_enabled`] have both been satisfied.
pub struct MqttPublisher {
    bus: SharedBus,
    aggregator: Arc<ZoneAggregator>,
    adapter_registry: Arc<AdapterRegistry>,
    knobs: KnobStore,
    base_url: std::sync::RwLock<String>,
    runtime: Mutex<Runtime>,
    /// Legacy-provider command gateway (#529), set once `AppState` finishes composing it via
    /// [`MqttPublisher::set_reliable_commands`]. `AppState` cannot hand this to
    /// [`MqttPublisher::new`] directly: it is itself assembled after the `Arc<MqttPublisher>`
    /// this struct lives behind, exactly like `AppState::reliable_commands` starts `None` and is
    /// attached via `AppState::with_reliable_commands`.
    reliable_commands: std::sync::RwLock<Option<CommandGateway>>,
}

/// Snapshot of publisher state for the settings API, deliberately excluding
/// the broker password.
#[derive(Debug, Clone, PartialEq)]
pub struct MqttStatus {
    pub configured: bool,
    pub enabled: bool,
    /// Whether the publisher's background task exists. Kept as-is (#607):
    /// it answers "did we start", not "are we connected" - see
    /// [`MqttStatus::connection`] for the latter.
    pub running: bool,
    /// Real broker connectivity (#607), independent of `running`.
    pub connection: MqttConnectionState,
    /// Why the last connection attempt failed, verbatim from `rumqttc`.
    /// Present while retrying; cleared on a successful `ConnAck`. "bad
    /// credentials" and "unknown host" need different fixes, so the text
    /// has to survive as far as the user.
    pub last_error: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub tls: Option<bool>,
    pub base_topic: Option<String>,
    pub discovery_prefix: Option<String>,
    pub has_username: bool,
    pub has_password: bool,
    /// Who supplied the active broker settings (#605). `None` while
    /// unconfigured. Lets Settings show an add-on-managed broker as managed
    /// instead of inviting the user to re-type details they never entered.
    pub source: Option<MqttConfigSource>,
}

impl MqttPublisher {
    pub fn new(
        bus: SharedBus,
        aggregator: Arc<ZoneAggregator>,
        adapter_registry: Arc<AdapterRegistry>,
        knobs: KnobStore,
    ) -> Self {
        Self {
            bus,
            aggregator,
            adapter_registry,
            knobs,
            base_url: std::sync::RwLock::new(String::new()),
            runtime: Mutex::new(Runtime::default()),
            reliable_commands: std::sync::RwLock::new(None),
        }
    }

    /// Attach the reliable command gateway (#529) so inbound HA commands for legacy
    /// (non-registry) zones - Roon, LMS, HQPlayer, OpenHome, UPnP - can route through it. Safe to
    /// call before or after the publisher is running; a task already in flight picks up the
    /// gateway on its next restart, and `AppState::with_reliable_commands` calls this immediately
    /// after the gateway exists, before the publisher can be enabled from settings.
    pub fn set_reliable_commands(&self, gateway: CommandGateway) {
        if let Ok(mut guard) = self.reliable_commands.write() {
            *guard = Some(gateway);
        }
    }

    fn reliable_commands_snapshot(&self) -> Option<CommandGateway> {
        self.reliable_commands
            .read()
            .ok()
            .and_then(|guard| guard.clone())
    }

    /// Set the absolute base URL (`http://host:port`) UHC's own HTTP server
    /// is reachable at, used to build `entity_picture` art URLs. Safe to
    /// call before or after the publisher is running.
    pub fn set_base_url(&self, base_url: String) {
        if let Ok(mut guard) = self.base_url.write() {
            *guard = base_url;
        }
    }

    fn base_url_snapshot(&self) -> String {
        self.base_url
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }

    /// Persist and adopt new broker connection settings. Restarts the
    /// publisher task immediately if it is currently enabled.
    pub async fn configure(&self, record: MqttCredentialRecord) {
        let enabled = {
            let mut runtime = self.runtime.lock().await;
            runtime.record = Some(record.clone());
            runtime.enabled
        };
        if enabled {
            self.restart(Some(record)).await;
        }
    }

    /// Turn the publisher on or off. A no-op flip when already in that
    /// state still restarts on enable, matching the settings-toggle
    /// semantics `api_settings_post_handler` uses for every other adapter.
    pub async fn set_enabled(&self, enabled: bool) {
        let (record, previous_task) = {
            let mut runtime = self.runtime.lock().await;
            runtime.enabled = enabled;
            let previous_task = if !enabled { runtime.task.take() } else { None };
            (runtime.record.clone(), previous_task)
        };
        if let Some(task) = previous_task {
            stop_task(task).await;
        }
        if enabled {
            self.restart(record).await;
        }
    }

    /// Stop any running task and, if `record` is present, start a fresh one.
    async fn restart(&self, record: Option<MqttCredentialRecord>) {
        let previous_task = {
            let mut runtime = self.runtime.lock().await;
            runtime.task.take()
        };
        if let Some(task) = previous_task {
            stop_task(task).await;
        }

        let Some(record) = record else {
            tracing::info!(
                "MQTT publisher enabled but not yet configured; waiting for broker settings"
            );
            return;
        };

        let shutdown = CancellationToken::new();
        let connection = Arc::new(ConnectionMonitor::connecting());
        let handle = tokio::spawn(run(
            record,
            self.bus.clone(),
            self.aggregator.clone(),
            self.adapter_registry.clone(),
            self.knobs.clone(),
            self.reliable_commands_snapshot(),
            self.base_url_snapshot(),
            connection.clone(),
            shutdown.clone(),
        ));
        let task = RunningTask {
            shutdown,
            handle,
            connection,
        };

        let mut runtime = self.runtime.lock().await;
        // Another caller may have already disabled/reconfigured while this
        // task was spawning; only keep it if still enabled.
        if runtime.enabled {
            runtime.task = Some(task);
        } else {
            drop(runtime);
            stop_task(task).await;
        }
    }

    pub async fn is_running(&self) -> bool {
        self.runtime.lock().await.task.is_some()
    }

    pub async fn is_configured(&self) -> bool {
        self.runtime.lock().await.record.is_some()
    }

    /// Real broker connectivity, as opposed to [`Self::is_running`] (#607).
    pub async fn connection_state(&self) -> MqttConnectionState {
        self.status().await.connection
    }

    pub async fn status(&self) -> MqttStatus {
        let runtime = self.runtime.lock().await;
        // No task means no connection, whatever the last one reported: the
        // monitor is owned by the task and dies with it.
        let connection = runtime
            .task
            .as_ref()
            .map(|task| task.connection.snapshot())
            .unwrap_or_default();
        MqttStatus {
            configured: runtime.record.is_some(),
            enabled: runtime.enabled,
            running: runtime.task.is_some(),
            connection: connection.state,
            last_error: connection.last_error,
            host: runtime.record.as_ref().map(|r| r.host.clone()),
            port: runtime.record.as_ref().map(|r| r.port),
            tls: runtime.record.as_ref().map(|r| r.tls),
            base_topic: runtime.record.as_ref().map(|r| r.base_topic.clone()),
            discovery_prefix: runtime.record.as_ref().map(|r| r.discovery_prefix.clone()),
            has_username: runtime
                .record
                .as_ref()
                .is_some_and(|r| r.username.as_ref().is_some_and(|u| !u.is_empty())),
            has_password: runtime
                .record
                .as_ref()
                .is_some_and(|r| r.password.as_ref().is_some_and(|p| !p.is_empty())),
            source: runtime.record.as_ref().map(|r| r.source),
        }
    }

    /// Stop the publisher for shutdown, publishing "offline" for a clean
    /// availability transition rather than relying solely on the LWT.
    pub async fn shutdown(&self) {
        let previous_task = {
            let mut runtime = self.runtime.lock().await;
            runtime.task.take()
        };
        if let Some(task) = previous_task {
            stop_task(task).await;
        }
    }
}

async fn stop_task(task: RunningTask) {
    task.shutdown.cancel();
    if tokio::time::timeout(Duration::from_secs(5), task.handle)
        .await
        .is_err()
    {
        tracing::warn!("MQTT publisher task did not stop within 5s of cancellation");
    }
}

fn client_id() -> String {
    format!(
        "uhc-{}",
        gethostname::gethostname()
            .to_string_lossy()
            .replace(|c: char| !c.is_ascii_alphanumeric(), "-")
    )
}

fn mqtt_options(record: &MqttCredentialRecord, availability_topic: &str) -> MqttOptions {
    let mut options = MqttOptions::new(client_id(), record.host.clone(), record.port);
    options.set_keep_alive(Duration::from_secs(30));
    if let Some(username) = record.username.as_ref().filter(|u| !u.is_empty()) {
        options.set_credentials(
            username.clone(),
            record.password.clone().unwrap_or_default(),
        );
    }
    if record.tls {
        options.set_transport(Transport::tls_with_default_config());
    }
    options.set_last_will(LastWill::new(
        availability_topic,
        b"offline".to_vec(),
        QoS::AtLeastOnce,
        true,
    ));
    options
}

/// Publish HA discovery configs and the retained state topic for one zone.
async fn publish_zone(
    client: &AsyncClient,
    record: &MqttCredentialRecord,
    zone: &Zone,
    base_url: &str,
    availability_topic: &str,
) {
    let settings = discovery::DiscoverySettings {
        base_topic: &record.base_topic,
        discovery_prefix: &record.discovery_prefix,
        availability_topic,
    };
    for (topic, payload) in discovery::discovery_entries(zone, &settings) {
        match serde_json::to_vec(&payload) {
            Ok(json) => {
                if let Err(error) = client.publish(topic, QoS::AtLeastOnce, true, json).await {
                    tracing::warn!("MQTT discovery publish failed: {error}");
                }
            }
            Err(error) => tracing::warn!("failed to serialize MQTT discovery payload: {error}"),
        }
    }

    let payload = state::build_state_payload(zone, base_url);
    match serde_json::to_vec(&payload) {
        Ok(json) => {
            let topic = topics::state_topic(&record.base_topic, &zone.zone_id);
            if let Err(error) = client.publish(topic, QoS::AtLeastOnce, true, json).await {
                tracing::warn!("MQTT state publish failed: {error}");
            }
        }
        Err(error) => tracing::warn!("failed to serialize MQTT state payload: {error}"),
    }
}

/// Clear every retained discovery/state topic a removed zone could have had.
async fn retract_zone(client: &AsyncClient, record: &MqttCredentialRecord, zone_id: &str) {
    for topic in discovery::discovery_topics_for_removal(&record.discovery_prefix, zone_id) {
        if let Err(error) = client
            .publish(topic, QoS::AtLeastOnce, true, Vec::new())
            .await
        {
            tracing::warn!("MQTT discovery retraction failed: {error}");
        }
    }
    let topic = topics::state_topic(&record.base_topic, zone_id);
    if let Err(error) = client
        .publish(topic, QoS::AtLeastOnce, true, Vec::new())
        .await
    {
        tracing::warn!("MQTT state retraction failed: {error}");
    }
}

/// Publish HA discovery configs and the retained state topic for one knob.
async fn publish_knob(
    client: &AsyncClient,
    record: &MqttCredentialRecord,
    knob_id: &str,
    knob: &Knob,
    zone_ids: &[String],
    availability_topic: &str,
) {
    let settings = knob_discovery::KnobDiscoverySettings {
        base_topic: &record.base_topic,
        discovery_prefix: &record.discovery_prefix,
        availability_topic,
    };
    for (topic, payload) in knob_discovery::discovery_entries(knob_id, knob, zone_ids, &settings) {
        match serde_json::to_vec(&payload) {
            Ok(json) => {
                if let Err(error) = client.publish(topic, QoS::AtLeastOnce, true, json).await {
                    tracing::warn!("MQTT knob discovery publish failed: {error}");
                }
            }
            Err(error) => {
                tracing::warn!("failed to serialize MQTT knob discovery payload: {error}")
            }
        }
    }

    let payload = knob_state::build_state_payload(knob, chrono::Utc::now());
    match serde_json::to_vec(&payload) {
        Ok(json) => {
            let topic = topics::knob_state_topic(&record.base_topic, knob_id);
            if let Err(error) = client.publish(topic, QoS::AtLeastOnce, true, json).await {
                tracing::warn!("MQTT knob state publish failed: {error}");
            }
        }
        Err(error) => tracing::warn!("failed to serialize MQTT knob state payload: {error}"),
    }
}

/// Clear every retained discovery/state topic a removed knob could have had.
async fn retract_knob(client: &AsyncClient, record: &MqttCredentialRecord, knob_id: &str) {
    for topic in knob_discovery::discovery_topics_for_removal(&record.discovery_prefix, knob_id) {
        if let Err(error) = client
            .publish(topic, QoS::AtLeastOnce, true, Vec::new())
            .await
        {
            tracing::warn!("MQTT knob discovery retraction failed: {error}");
        }
    }
    let topic = topics::knob_state_topic(&record.base_topic, knob_id);
    if let Err(error) = client
        .publish(topic, QoS::AtLeastOnce, true, Vec::new())
        .await
    {
        tracing::warn!("MQTT knob state retraction failed: {error}");
    }
}

/// Republish discovery + state for every knob currently known to the store,
/// retracting any knob that has disappeared since the last call. Refreshes
/// `knob_slugs` (used to route inbound commands) and returns the current
/// zone id list so the caller can decide whether it changed.
async fn announce_all_knobs(
    client: &AsyncClient,
    record: &MqttCredentialRecord,
    knobs: &KnobStore,
    aggregator: &ZoneAggregator,
    availability_topic: &str,
    known_knob_ids: &mut HashSet<String>,
    knob_slugs: &mut HashMap<String, String>,
) {
    let zone_ids: Vec<String> = aggregator
        .get_zones()
        .await
        .into_iter()
        .map(|zone| zone.zone_id)
        .collect();

    let current = knobs.list_full().await;
    let current_ids: HashSet<String> = current.iter().map(|(id, _)| id.clone()).collect();

    for removed_id in known_knob_ids
        .difference(&current_ids)
        .cloned()
        .collect::<Vec<_>>()
    {
        knob_slugs.remove(&topics::zone_slug(&removed_id));
        retract_knob(client, record, &removed_id).await;
    }

    for (knob_id, knob) in &current {
        knob_slugs.insert(topics::zone_slug(knob_id), knob_id.clone());
        publish_knob(client, record, knob_id, knob, &zone_ids, availability_topic).await;
    }

    *known_knob_ids = current_ids;

    let command_filter = format!("{}/knob/+/+/set", record.base_topic);
    if let Err(error) = client.subscribe(command_filter, QoS::AtLeastOnce).await {
        tracing::warn!("MQTT knob command subscription failed: {error}");
    }
}

/// Republish discovery + state for every zone currently known to the
/// aggregator, refreshing the slug -> zone id map used to route inbound
/// commands. Called on every fresh broker connection, since a new session
/// has no retained knowledge of what this publisher already announced.
async fn announce_all_zones(
    client: &AsyncClient,
    record: &MqttCredentialRecord,
    aggregator: &ZoneAggregator,
    base_url: &str,
    availability_topic: &str,
    zone_slugs: &mut HashMap<String, String>,
) {
    if let Err(error) = client
        .publish(
            availability_topic,
            QoS::AtLeastOnce,
            true,
            b"online".to_vec(),
        )
        .await
    {
        tracing::warn!("MQTT availability publish failed: {error}");
    }

    let zones = aggregator.get_zones().await;
    zone_slugs.clear();
    for zone in &zones {
        zone_slugs.insert(topics::zone_slug(&zone.zone_id), zone.zone_id.clone());
        publish_zone(client, record, zone, base_url, availability_topic).await;
    }

    let command_filter = format!("{}/media_player/+/+/set", record.base_topic);
    if let Err(error) = client.subscribe(command_filter, QoS::AtLeastOnce).await {
        tracing::warn!("MQTT command subscription failed: {error}");
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_bus_event(
    client: &AsyncClient,
    record: &MqttCredentialRecord,
    aggregator: &ZoneAggregator,
    knobs: &KnobStore,
    base_url: &str,
    availability_topic: &str,
    zone_slugs: &mut HashMap<String, String>,
    known_knob_ids: &mut HashSet<String>,
    knob_slugs: &mut HashMap<String, String>,
    event: BusEvent,
) {
    match event {
        BusEvent::ZoneDiscovered { zone } => {
            zone_slugs.insert(topics::zone_slug(&zone.zone_id), zone.zone_id.clone());
            publish_zone(client, record, &zone, base_url, availability_topic).await;
            // A newly discovered zone changes every knob's zone-select
            // options, so re-announce them too.
            announce_all_knobs(
                client,
                record,
                knobs,
                aggregator,
                availability_topic,
                known_knob_ids,
                knob_slugs,
            )
            .await;
        }
        BusEvent::ZoneRemoved { zone_id } => {
            zone_slugs.remove(&topics::zone_slug(zone_id.as_str()));
            retract_zone(client, record, zone_id.as_str()).await;
            announce_all_knobs(
                client,
                record,
                knobs,
                aggregator,
                availability_topic,
                known_knob_ids,
                knob_slugs,
            )
            .await;
        }
        BusEvent::ZonesFlushed { zone_ids, .. } => {
            for zone_id in zone_ids {
                zone_slugs.remove(&topics::zone_slug(&zone_id));
                retract_zone(client, record, &zone_id).await;
            }
            announce_all_knobs(
                client,
                record,
                knobs,
                aggregator,
                availability_topic,
                known_knob_ids,
                knob_slugs,
            )
            .await;
        }
        BusEvent::ZoneUpdated { zone_id, .. }
        | BusEvent::NowPlayingChanged { zone_id, .. }
        | BusEvent::PlaybackModesChanged { zone_id, .. }
        | BusEvent::SeekPositionChanged { zone_id, .. } => {
            if let Some(zone) = aggregator.get_zone(zone_id.as_str()).await {
                zone_slugs.insert(topics::zone_slug(&zone.zone_id), zone.zone_id.clone());
                publish_zone(client, record, &zone, base_url, availability_topic).await;
            }
        }
        BusEvent::VolumeChanged { .. } => {
            // Keyed by output_id rather than zone_id - refresh every known
            // zone rather than guess which one owns this output.
            let zone_ids: Vec<String> = zone_slugs.values().cloned().collect();
            for zone_id in zone_ids {
                if let Some(zone) = aggregator.get_zone(&zone_id).await {
                    publish_zone(client, record, &zone, base_url, availability_topic).await;
                }
            }
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_incoming_publish(
    record: &MqttCredentialRecord,
    adapter_registry: &Arc<AdapterRegistry>,
    aggregator: &ZoneAggregator,
    reliable_commands: Option<&CommandGateway>,
    knobs: &KnobStore,
    zone_slugs: &HashMap<String, String>,
    knob_slugs: &HashMap<String, String>,
    publish: &rumqttc::Publish,
) {
    if let Some((slug, action)) =
        knob_command::parse_command_topic(&record.base_topic, &publish.topic)
    {
        let Some(knob_id) = knob_slugs.get(slug) else {
            tracing::debug!(slug, "MQTT command for unknown knob slug; ignoring");
            return;
        };
        let payload = String::from_utf8_lossy(&publish.payload);
        let Some(parsed) = knob_command::parse_action(action, &payload) else {
            tracing::debug!(topic = %publish.topic, "MQTT knob command topic had an unrecognized action or payload");
            return;
        };
        match knob_command::dispatch(knobs, knob_id, parsed).await {
            knob_command::DispatchOutcome::Applied => {}
            knob_command::DispatchOutcome::KnobNotFound => {
                tracing::warn!(knob_id, "MQTT knob command refused: knob not found");
            }
        }
        return;
    }

    let Some((slug, action)) = command::parse_command_topic(&record.base_topic, &publish.topic)
    else {
        return;
    };
    let Some(zone_id) = zone_slugs.get(slug) else {
        tracing::debug!(slug, "MQTT command for unknown zone slug; ignoring");
        return;
    };
    let payload = String::from_utf8_lossy(&publish.payload);
    let Some(parsed) = command::parse_action(action, &payload) else {
        tracing::debug!(topic = %publish.topic, "MQTT command topic had an unrecognized action or payload");
        return;
    };
    match command::dispatch(
        adapter_registry,
        aggregator,
        reliable_commands,
        zone_id,
        parsed,
    )
    .await
    {
        command::DispatchOutcome::Sent => {}
        command::DispatchOutcome::Refused(error) => {
            tracing::warn!(zone_id, error, "MQTT command refused");
        }
        command::DispatchOutcome::Unsupported(reason) => {
            tracing::debug!(zone_id, reason, "MQTT command ignored");
        }
    }
}

/// The publisher's connect/reconnect loop. `rumqttc`'s `EventLoop` retries
/// the underlying TCP/TLS connection on its own; this loop only needs to
/// re-announce state on every fresh `ConnAck`, since a new MQTT session (or
/// a broker restart) has no memory of what was previously retained here.
///
/// It also reports what the event loop is actually doing into `connection`
/// (#607), because "this task is alive" and "the broker answered" are not
/// the same fact - the task stays alive indefinitely retrying a host that
/// will never resolve.
#[allow(clippy::too_many_arguments)]
async fn run(
    record: MqttCredentialRecord,
    bus: SharedBus,
    aggregator: Arc<ZoneAggregator>,
    adapter_registry: Arc<AdapterRegistry>,
    knobs: KnobStore,
    reliable_commands: Option<CommandGateway>,
    base_url: String,
    connection: Arc<ConnectionMonitor>,
    shutdown: CancellationToken,
) {
    let availability_topic = topics::availability_topic(&record.base_topic);
    let options = mqtt_options(&record, &availability_topic);
    let (client, mut eventloop) = AsyncClient::new(options, 64);
    let mut bus_rx = bus.subscribe();
    let mut zone_slugs: HashMap<String, String> = HashMap::new();
    let mut known_knob_ids: HashSet<String> = HashSet::new();
    let mut knob_slugs: HashMap<String, String> = HashMap::new();
    // The knob store has no bus events of its own (see module doc) - poll
    // it on a fixed timer instead so battery/last-seen changes still reach
    // Home Assistant. `interval_at` (rather than `interval`) skips firing
    // immediately: the `ConnAck` handler below already announces knobs on
    // every fresh connection, and firing a tick before the client has even
    // connected would queue a knob command subscribe ahead of the
    // publisher's own CONNECT - observed to trip a spurious reconnect (the
    // broker treats the second CONNECT with the same client id as replacing
    // a live session and fires that session's LWT) that briefly retains
    // "offline" before the publisher's real "online" lands.
    let mut knob_tick = tokio::time::interval_at(
        tokio::time::Instant::now() + KNOB_POLL_INTERVAL,
        KNOB_POLL_INTERVAL,
    );
    knob_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                let _ = client
                    .publish(&availability_topic, QoS::AtLeastOnce, true, b"offline".to_vec())
                    .await;
                let _ = client.disconnect().await;
                break;
            }
            event = eventloop.poll() => {
                match event {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        // The one moment we know entities are really
                        // reaching a broker (#607).
                        connection.connected();
                        announce_all_zones(
                            &client,
                            &record,
                            &aggregator,
                            &base_url,
                            &availability_topic,
                            &mut zone_slugs,
                        )
                        .await;
                        announce_all_knobs(
                            &client,
                            &record,
                            &knobs,
                            &aggregator,
                            &availability_topic,
                            &mut known_knob_ids,
                            &mut knob_slugs,
                        )
                        .await;
                    }
                    Ok(Event::Incoming(Packet::Publish(publish))) => {
                        handle_incoming_publish(
                            &record,
                            &adapter_registry,
                            &aggregator,
                            reliable_commands.as_ref(),
                            &knobs,
                            &zone_slugs,
                            &knob_slugs,
                            &publish,
                        )
                        .await;
                    }
                    // The broker hung up on us. `rumqttc` dials again, so
                    // this is "connecting", never a latched "connected".
                    Ok(Event::Incoming(Packet::Disconnect)) => {
                        connection.interrupted("the broker closed the connection");
                    }
                    Ok(_) => {}
                    Err(error) => {
                        // Recorded before the log line, so /api/mqtt/status
                        // and the log can never disagree about why.
                        connection.interrupted(error.to_string());
                        tracing::warn!("MQTT publisher connection error: {error}");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
            bus_event = bus_rx.recv() => {
                match bus_event {
                    Ok(event) => {
                        handle_bus_event(
                            &client,
                            &record,
                            &aggregator,
                            &knobs,
                            &base_url,
                            &availability_topic,
                            &mut zone_slugs,
                            &mut known_knob_ids,
                            &mut knob_slugs,
                            event,
                        )
                        .await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(
                            skipped,
                            "MQTT publisher lagged behind the event bus; resyncing from the aggregator"
                        );
                        announce_all_zones(
                            &client,
                            &record,
                            &aggregator,
                            &base_url,
                            &availability_topic,
                            &mut zone_slugs,
                        )
                        .await;
                        announce_all_knobs(
                            &client,
                            &record,
                            &knobs,
                            &aggregator,
                            &availability_topic,
                            &mut known_knob_ids,
                            &mut knob_slugs,
                        )
                        .await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = knob_tick.tick() => {
                announce_all_knobs(
                    &client,
                    &record,
                    &knobs,
                    &aggregator,
                    &availability_topic,
                    &mut known_knob_ids,
                    &mut knob_slugs,
                )
                .await;
            }
        }
    }
}
