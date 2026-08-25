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

pub mod command;
pub mod discovery;
pub mod state;
pub mod topics;

use std::collections::HashMap;
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

pub use crate::api::credentials::MqttCredentialRecord;

/// Default namespace for state/command topics, distinct from HA's
/// `discovery_prefix` so operators can point discovery at a shared
/// `homeassistant` prefix while keeping UHC's own traffic namespaced.
pub const DEFAULT_BASE_TOPIC: &str = "unified-hifi";
/// Default Home Assistant MQTT discovery prefix.
pub const DEFAULT_DISCOVERY_PREFIX: &str = "homeassistant";
pub const DEFAULT_PORT: u16 = 1883;
pub const DEFAULT_TLS_PORT: u16 = 8883;

/// Lifecycle state guarded together so configure/enable/disable never race
/// each other into starting two publisher tasks.
#[derive(Default)]
struct Runtime {
    record: Option<MqttCredentialRecord>,
    enabled: bool,
    task: Option<(CancellationToken, JoinHandle<()>)>,
}

/// Optional MQTT publisher. Held on `AppState` as `Arc<MqttPublisher>`;
/// cheap to construct, inert until [`MqttPublisher::configure`] and
/// [`MqttPublisher::set_enabled`] have both been satisfied.
pub struct MqttPublisher {
    bus: SharedBus,
    aggregator: Arc<ZoneAggregator>,
    adapter_registry: Arc<AdapterRegistry>,
    base_url: std::sync::RwLock<String>,
    /// Legacy-provider command gateway (#529), set once `AppState` finishes composing it via
    /// [`MqttPublisher::set_reliable_commands`]. `AppState` cannot hand this to
    /// [`MqttPublisher::new`] directly: it is itself assembled after the `Arc<MqttPublisher>`
    /// this struct lives behind, exactly like `AppState::reliable_commands` starts `None` and is
    /// attached via `AppState::with_reliable_commands`.
    reliable_commands: std::sync::RwLock<Option<CommandGateway>>,
    runtime: Mutex<Runtime>,
}

/// Snapshot of publisher state for the settings API, deliberately excluding
/// the broker password.
#[derive(Debug, Clone, PartialEq)]
pub struct MqttStatus {
    pub configured: bool,
    pub enabled: bool,
    pub running: bool,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub tls: Option<bool>,
    pub base_topic: Option<String>,
    pub discovery_prefix: Option<String>,
    pub has_username: bool,
    pub has_password: bool,
}

impl MqttPublisher {
    pub fn new(
        bus: SharedBus,
        aggregator: Arc<ZoneAggregator>,
        adapter_registry: Arc<AdapterRegistry>,
    ) -> Self {
        Self {
            bus,
            aggregator,
            adapter_registry,
            base_url: std::sync::RwLock::new(String::new()),
            reliable_commands: std::sync::RwLock::new(None),
            runtime: Mutex::new(Runtime::default()),
        }
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
        if let Some((shutdown, handle)) = previous_task {
            stop_task(shutdown, handle).await;
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
        if let Some((shutdown, handle)) = previous_task {
            stop_task(shutdown, handle).await;
        }

        let Some(record) = record else {
            tracing::info!(
                "MQTT publisher enabled but not yet configured; waiting for broker settings"
            );
            return;
        };

        let shutdown = CancellationToken::new();
        let task = tokio::spawn(run(
            record,
            self.bus.clone(),
            self.aggregator.clone(),
            self.adapter_registry.clone(),
            self.reliable_commands_snapshot(),
            self.base_url_snapshot(),
            shutdown.clone(),
        ));

        let mut runtime = self.runtime.lock().await;
        // Another caller may have already disabled/reconfigured while this
        // task was spawning; only keep it if still enabled.
        if runtime.enabled {
            runtime.task = Some((shutdown, task));
        } else {
            drop(runtime);
            stop_task(shutdown, task).await;
        }
    }

    pub async fn is_running(&self) -> bool {
        self.runtime.lock().await.task.is_some()
    }

    pub async fn is_configured(&self) -> bool {
        self.runtime.lock().await.record.is_some()
    }

    pub async fn status(&self) -> MqttStatus {
        let runtime = self.runtime.lock().await;
        MqttStatus {
            configured: runtime.record.is_some(),
            enabled: runtime.enabled,
            running: runtime.task.is_some(),
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
        }
    }

    /// Stop the publisher for shutdown, publishing "offline" for a clean
    /// availability transition rather than relying solely on the LWT.
    pub async fn shutdown(&self) {
        let previous_task = {
            let mut runtime = self.runtime.lock().await;
            runtime.task.take()
        };
        if let Some((shutdown, handle)) = previous_task {
            stop_task(shutdown, handle).await;
        }
    }
}

async fn stop_task(shutdown: CancellationToken, handle: JoinHandle<()>) {
    shutdown.cancel();
    if tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .is_err()
    {
        tracing::warn!("MQTT publisher task did not stop within 5s of cancellation");
    }
}

fn client_id() -> String {
    format!(
        "uhc-{}",
        gethostname::gethostname().to_string_lossy().replace(
            |c: char| !c.is_ascii_alphanumeric(),
            "-"
        )
    )
}

fn mqtt_options(record: &MqttCredentialRecord, availability_topic: &str) -> MqttOptions {
    let mut options = MqttOptions::new(client_id(), record.host.clone(), record.port);
    options.set_keep_alive(Duration::from_secs(30));
    if let Some(username) = record.username.as_ref().filter(|u| !u.is_empty()) {
        options.set_credentials(username.clone(), record.password.clone().unwrap_or_default());
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
        if let Err(error) = client.publish(topic, QoS::AtLeastOnce, true, Vec::new()).await {
            tracing::warn!("MQTT discovery retraction failed: {error}");
        }
    }
    let topic = topics::state_topic(&record.base_topic, zone_id);
    if let Err(error) = client.publish(topic, QoS::AtLeastOnce, true, Vec::new()).await {
        tracing::warn!("MQTT state retraction failed: {error}");
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
        .publish(availability_topic, QoS::AtLeastOnce, true, b"online".to_vec())
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

async fn handle_bus_event(
    client: &AsyncClient,
    record: &MqttCredentialRecord,
    aggregator: &ZoneAggregator,
    base_url: &str,
    availability_topic: &str,
    zone_slugs: &mut HashMap<String, String>,
    event: BusEvent,
) {
    match event {
        BusEvent::ZoneDiscovered { zone } => {
            zone_slugs.insert(topics::zone_slug(&zone.zone_id), zone.zone_id.clone());
            publish_zone(client, record, &zone, base_url, availability_topic).await;
        }
        BusEvent::ZoneRemoved { zone_id } => {
            zone_slugs.remove(&topics::zone_slug(zone_id.as_str()));
            retract_zone(client, record, zone_id.as_str()).await;
        }
        BusEvent::ZonesFlushed { zone_ids, .. } => {
            for zone_id in zone_ids {
                zone_slugs.remove(&topics::zone_slug(&zone_id));
                retract_zone(client, record, &zone_id).await;
            }
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

async fn handle_incoming_publish(
    record: &MqttCredentialRecord,
    adapter_registry: &Arc<AdapterRegistry>,
    aggregator: &ZoneAggregator,
    reliable_commands: Option<&CommandGateway>,
    zone_slugs: &HashMap<String, String>,
    publish: &rumqttc::Publish,
) {
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
    match command::dispatch(adapter_registry, aggregator, reliable_commands, zone_id, parsed).await
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
async fn run(
    record: MqttCredentialRecord,
    bus: SharedBus,
    aggregator: Arc<ZoneAggregator>,
    adapter_registry: Arc<AdapterRegistry>,
    reliable_commands: Option<CommandGateway>,
    base_url: String,
    shutdown: CancellationToken,
) {
    let availability_topic = topics::availability_topic(&record.base_topic);
    let options = mqtt_options(&record, &availability_topic);
    let (client, mut eventloop) = AsyncClient::new(options, 64);
    let mut bus_rx = bus.subscribe();
    let mut zone_slugs: HashMap<String, String> = HashMap::new();

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
                        announce_all_zones(
                            &client,
                            &record,
                            &aggregator,
                            &base_url,
                            &availability_topic,
                            &mut zone_slugs,
                        )
                        .await;
                    }
                    Ok(Event::Incoming(Packet::Publish(publish))) => {
                        handle_incoming_publish(
                            &record,
                            &adapter_registry,
                            &aggregator,
                            reliable_commands.as_ref(),
                            &zone_slugs,
                            &publish,
                        )
                        .await;
                    }
                    Ok(_) => {}
                    Err(error) => {
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
                            &base_url,
                            &availability_topic,
                            &mut zone_slugs,
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
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}
