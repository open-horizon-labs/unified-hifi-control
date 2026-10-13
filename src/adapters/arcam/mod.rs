//! Arcam device adapter (#774). **Alpha.**
//!
//! A *device* adapter: it controls the amplifier or receiver a zone feeds, not a music
//! provider. It never publishes zones. It publishes [`BusEvent::DeviceObserved`] snapshots and
//! executes [`DeviceOperation`]s the [`crate::devices::service::DeviceService`] hands it,
//! already resolved, clamped and quantised. This adapter never computes a level.
//!
//! Command codes and tables come from the MIT-licensed `arcam_fmj` client (see [`protocol`]).
//! **No hardware has exercised it**: every capability claim is `implementation-derived`.
//!
//! # One owner per unit
//!
//! Each unit gets one task owning its single TCP session on port 50000, polled with a 5 second
//! `POWER` heartbeat (the client library's interval) and a full state refresh every 30 seconds.
//! Queries only on connect and reconnect.
//!
//! # Discovery
//!
//! SSDP `MediaRenderer:1` responders whose description names manufacturer `ARCAM`, plus
//! manually configured hosts. Arcam units expose no serial over the control protocol, so a
//! manual device needs a configured `mac` for its identity. A unit is published only after its
//! control session has answered a query.
//!
//! # Standby
//!
//! Older models switch network control off in standby: the TCP port refuses, and there is **no
//! IP wake**. `power_on` then fails with a message saying so. Newer models retain LAN control
//! when "HDMI & IP On" is configured, and then power-on works over the session.

pub mod protocol;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use ssdp_client::{SearchTarget, URN};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::adapters::devices::{DeviceFamily, FamilyContext};
use crate::bus::{BusEvent, SharedBus};
use crate::devices::links::ManualDevice;
use crate::devices::service::{DeviceDriver, DeviceOperation};
use crate::devices::{
    CapabilityEvidence, ControlChannel, Device, DeviceCapabilities, DeviceCapability,
    DeviceConnection, DeviceId, DevicePower, DeviceVolume, DEVICE_CONTROL_MATURITY,
};
use protocol::{Reply, Series};

/// Family name used in logs and `ManualDevice::family`.
pub const ADAPTER_NAME: &str = "arcam";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const REPLY_TIMEOUT: Duration = Duration::from_secs(6);
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
/// Every Nth heartbeat is a full refresh.
const FULL_REFRESH_TICKS: u32 = 6;
/// A session that has produced no bytes for this long (three missed heartbeats) is dead.
const SILENCE_LIMIT: Duration = Duration::from_secs(15);
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(60);
const RECONNECT_MIN: Duration = Duration::from_secs(2);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// Gap between back-to-back frames; the units drop frames sent without settling time.
const COMMAND_SPACING: Duration = Duration::from_millis(80);
/// After an accepted power-off, a session that closes within this window is standby.
const STANDBY_GRACE: Duration = Duration::from_secs(8);
const SSDP_TARGET: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";

const NO_IP_WAKE: &str = "IP wake is unsupported: older Arcam models turn network control off in \
                          standby, so switch the unit on at the unit itself (newer models keep \
                          LAN control when \"HDMI & IP On\" is enabled)";

/// Capability claims for one model. Pure, so the AGENTS.md device table can be generated.
pub fn capabilities_for(model: &str) -> DeviceCapabilities {
    let series = Series::for_model(model);
    let derived = |supported: bool, detail: String| {
        DeviceCapability {
        supported,
        evidence: CapabilityEvidence::ImplementationDerived,
        detail: format!("{detail}; implemented from arcam_fmj and Arcam's control documents, not yet exercised on hardware"),
    }
    };
    let unrecognised = "the model is not in the series tables, so its encoding is not known";
    DeviceCapabilities {
        observe: derived(
            true,
            "POWER/VOLUME/MUTE/CURRENT_SOURCE queries over the TCP control session on port 50000, \
             with a 5 second heartbeat"
                .to_string(),
        ),
        volume: match series {
            Some(Series::Pa) => derived(
                false,
                "power amplifiers (PA series) have no volume command".to_string(),
            ),
            _ => derived(
                true,
                "VOLUME (0x0D) in the unit's native 0-99 steps; 99 is the protocol's full scale"
                    .to_string(),
            ),
        },
        mute: match series {
            Some(series) if series_direct(series) => {
                derived(true, "MUTE (0x0E) direct write".to_string())
            }
            Some(_) => derived(
                true,
                "simulated RC5 mute key via SIMULATE_RC5_IR_COMMAND (0x08)".to_string(),
            ),
            None => derived(false, unrecognised.to_string()),
        },
        power: match series {
            Some(series) if series_direct(series) => derived(
                true,
                format!("POWER (0x00) direct write while the unit answers; {NO_IP_WAKE}"),
            ),
            Some(_) => derived(
                true,
                format!(
                    "simulated RC5 power keys via SIMULATE_RC5_IR_COMMAND (0x08) while the unit \
                     answers; {NO_IP_WAKE}"
                ),
            ),
            None => derived(false, unrecognised.to_string()),
        },
        input_select: match series {
            Some(Series::Pa) => derived(
                false,
                "power amplifiers have no source selector".to_string(),
            ),
            Some(Series::Sa) => derived(
                true,
                "CURRENT_SOURCE (0x1D) direct write, SA-series source bytes".to_string(),
            ),
            Some(_) => derived(
                true,
                "simulated RC5 source keys via SIMULATE_RC5_IR_COMMAND (0x08); only sources with \
                 an RC5 key are selectable"
                    .to_string(),
            ),
            None => derived(false, unrecognised.to_string()),
        },
    }
}

fn series_direct(series: Series) -> bool {
    matches!(series, Series::Sa | Series::Pa | Series::St)
}

/// Process-wide revision counter seeded from the clock, so revisions stay monotonic across an
/// adapter restart.
fn next_revision() -> u64 {
    static REVISION: AtomicU64 = AtomicU64::new(0);
    let seed = now_ms().saturating_mul(1000);
    // Seeds once; a lost race means another caller already seeded, which is the same outcome.
    REVISION
        .compare_exchange(0, seed, Ordering::SeqCst, Ordering::SeqCst)
        .ok();
    REVISION.fetch_add(1, Ordering::SeqCst) + 1
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

enum UnitCommand {
    Execute {
        operation: DeviceOperation,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Rehost(String),
}

struct UnitHandle {
    host: String,
    commands: mpsc::Sender<UnitCommand>,
}

#[derive(Default)]
struct AdapterState {
    running: bool,
    cancel: CancellationToken,
    units: HashMap<String, UnitHandle>,
    /// SSDP `LOCATION`s already examined and found not to be Arcam units.
    rejected: HashSet<String>,
}

#[derive(Clone)]
pub struct ArcamAdapter {
    bus: SharedBus,
    http: Client,
    manual: Arc<RwLock<Vec<ManualDevice>>>,
    state: Arc<RwLock<AdapterState>>,
}

impl ArcamAdapter {
    pub fn new(bus: SharedBus) -> Self {
        Self {
            bus,
            http: Client::builder()
                .timeout(HTTP_TIMEOUT)
                .build()
                .unwrap_or_default(),
            manual: Arc::new(RwLock::new(Vec::new())),
            state: Arc::new(RwLock::new(AdapterState::default())),
        }
    }

    pub async fn set_manual_devices(&self, devices: Vec<ManualDevice>) {
        *self.manual.write().await = devices;
    }

    async fn start_internal(&self) -> anyhow::Result<()> {
        let cancel = {
            let mut state = self.state.write().await;
            if state.running {
                return Ok(());
            }
            state.running = true;
            state.cancel = CancellationToken::new();
            state.cancel.clone()
        };
        let adapter = self.clone();
        tokio::spawn(async move { adapter.discovery_loop(cancel).await });
        tracing::info!("Arcam device adapter started (alpha)");
        Ok(())
    }

    async fn stop_internal(&self) {
        let units = {
            let mut state = self.state.write().await;
            state.running = false;
            state.cancel.cancel();
            state.rejected.clear();
            std::mem::take(&mut state.units)
        };
        // Each unit task observes the cancelled token and publishes its own `DeviceRemoved`.
        drop(units);
        tracing::info!("Arcam device adapter stopped");
    }

    async fn discovery_loop(&self, cancel: CancellationToken) {
        let mut ticker = tokio::time::interval(DISCOVERY_INTERVAL);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = ticker.tick() => self.discover_once(&cancel).await,
            }
        }
    }

    async fn discover_once(&self, cancel: &CancellationToken) {
        // SSDP.
        for location in self.ssdp_locations().await {
            if cancel.is_cancelled() {
                return;
            }
            if self.state.read().await.rejected.contains(&location) {
                continue;
            }
            let Some(host) = reqwest::Url::parse(&location)
                .ok()
                .and_then(|url| url.host_str().map(str::to_string))
            else {
                continue;
            };
            let description = match self.http.get(&location).send().await {
                Ok(response) => response.text().await.ok(),
                Err(_) => None,
            }
            .and_then(|body| protocol::parse_description(&body));
            let Some(description) = description.filter(|d| d.is_arcam()) else {
                self.state.write().await.rejected.insert(location);
                continue;
            };
            let Some(device_id) = description
                .hardware_id()
                .and_then(|id| DeviceId::new("arcam", &id))
            else {
                tracing::debug!(%host, "Arcam description carries no usable hardware id");
                continue;
            };
            let name = if description.friendly_name.is_empty() {
                description.model.clone()
            } else {
                description.friendly_name.clone()
            };
            self.ensure_unit(
                &device_id,
                &host,
                protocol::DEFAULT_PORT,
                &description.model,
                &name,
                "ssdp",
                cancel,
            )
            .await;
        }

        // Manual hosts: the MAC is the identity (the protocol reports none).
        for manual in self.manual.read().await.clone() {
            if cancel.is_cancelled() {
                return;
            }
            let Some(device_id) = manual
                .mac
                .as_deref()
                .and_then(|mac| DeviceId::new("arcam", mac))
            else {
                tracing::warn!(host = %manual.host, "Arcam manual device needs a mac: the control protocol reports no serial");
                continue;
            };
            self.ensure_unit(
                &device_id,
                &manual.host,
                manual.port.unwrap_or(protocol::DEFAULT_PORT),
                "",
                &manual.host,
                "manual",
                cancel,
            )
            .await;
        }
    }

    async fn ssdp_locations(&self) -> Vec<String> {
        let Ok(urn) = SSDP_TARGET.parse::<URN>() else {
            return Vec::new();
        };
        let mut locations = Vec::new();
        match ssdp_client::search(&SearchTarget::URN(urn), Duration::from_secs(2), 2, None).await {
            Ok(responses) => {
                futures::pin_mut!(responses);
                while let Some(response) = responses.next().await {
                    if let Ok(response) = response {
                        locations.push(response.location().to_string());
                    }
                }
            }
            Err(error) => tracing::debug!("Arcam SSDP search failed: {error}"),
        }
        locations.sort();
        locations.dedup();
        locations
    }

    #[allow(clippy::too_many_arguments)]
    async fn ensure_unit(
        &self,
        device_id: &DeviceId,
        host: &str,
        port: u16,
        model: &str,
        name: &str,
        discovery: &str,
        adapter_cancel: &CancellationToken,
    ) {
        let existing = {
            let state = self.state.read().await;
            if !state.running {
                return;
            }
            state
                .units
                .get(device_id.as_str())
                .map(|unit| (unit.host.clone(), unit.commands.clone()))
        };
        match existing {
            Some((current, commands)) if current != host => {
                // Follow an IP change.
                if commands
                    .try_send(UnitCommand::Rehost(host.to_string()))
                    .is_ok()
                {
                    if let Some(unit) = self.state.write().await.units.get_mut(device_id.as_str()) {
                        unit.host = host.to_string();
                    }
                }
            }
            Some(_) => {}
            None => {
                self.spawn_unit(
                    device_id,
                    host,
                    port,
                    model,
                    name,
                    discovery,
                    adapter_cancel.child_token(),
                )
                .await;
            }
        }
    }

    /// Attach a unit at a known address without discovery: a configured host, or a mock unit on
    /// an ephemeral port in tests. `model` may be empty; the unit then asks for it (AMX).
    pub async fn attach_unit(
        &self,
        device_id: &DeviceId,
        host: &str,
        port: u16,
        model: &str,
        name: &str,
    ) {
        let cancel = {
            let mut state = self.state.write().await;
            state.running = true;
            if state.cancel.is_cancelled() {
                // Restarting after a stop: the old token would cancel the new unit at birth.
                state.cancel = CancellationToken::new();
            }
            state.cancel.child_token()
        };
        self.spawn_unit(device_id, host, port, model, name, "manual", cancel)
            .await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn spawn_unit(
        &self,
        device_id: &DeviceId,
        host: &str,
        port: u16,
        model: &str,
        name: &str,
        discovery: &str,
        cancel: CancellationToken,
    ) {
        let mut state = self.state.write().await;
        if state.units.contains_key(device_id.as_str()) {
            return;
        }
        let (commands, receiver) = mpsc::channel(8);
        state.units.insert(
            device_id.to_string(),
            UnitHandle {
                host: host.to_string(),
                commands,
            },
        );
        drop(state);
        tracing::info!(%device_id, host, model, name, "Discovered Arcam device");
        let unit = Unit {
            bus: self.bus.clone(),
            host: host.to_string(),
            port,
            device_id: device_id.clone(),
            model: model.to_string(),
            revision_text: None,
            name: name.to_string(),
            discovery: discovery.to_string(),
            state: UnitState::default(),
            validated: false,
            standby_deadline: None,
            last_rx: Instant::now(),
        };
        tokio::spawn(unit.run(receiver, cancel));
    }

    pub async fn device_count(&self) -> usize {
        self.state.read().await.units.len()
    }
}

#[async_trait]
impl DeviceFamily for ArcamAdapter {
    fn family(&self) -> &'static str {
        ADAPTER_NAME
    }

    async fn start_family(&self, context: FamilyContext) -> anyhow::Result<()> {
        if !context.manual.is_empty() {
            self.set_manual_devices(context.manual).await;
        }
        self.start_internal().await
    }

    async fn stop_family(&self) {
        self.stop_internal().await
    }
}

#[async_trait]
impl DeviceDriver for ArcamAdapter {
    fn kinds(&self) -> &'static [&'static str] {
        &["arcam"]
    }

    async fn execute(
        &self,
        device_id: &DeviceId,
        operation: DeviceOperation,
    ) -> Result<(), String> {
        let commands = self
            .state
            .read()
            .await
            .units
            .get(device_id.as_str())
            .map(|unit| unit.commands.clone())
            .ok_or_else(|| format!("{device_id} is not connected to this adapter"))?;
        let (reply, response) = oneshot::channel();
        tokio::time::timeout(
            WRITE_TIMEOUT,
            commands.send(UnitCommand::Execute { operation, reply }),
        )
        .await
        .map_err(|_| "the unit's session is busy".to_string())?
        .map_err(|_| "the unit's session has stopped".to_string())?;
        tokio::time::timeout(REPLY_TIMEOUT, response)
            .await
            .map_err(|_| "the unit did not accept the command in time".to_string())?
            .map_err(|_| "the unit's session stopped before sending".to_string())?
    }
}

#[derive(Default)]
struct UnitState {
    power: Option<bool>,
    volume: Option<u8>,
    muted: Option<bool>,
    input: Option<String>,
    connected: bool,
    inferred_standby: bool,
    detail: Option<String>,
}

impl UnitState {
    /// The unit is on but has not yet reported what it could not answer in standby.
    fn needs_refresh(&self) -> bool {
        self.power == Some(true)
            && (self.volume.is_none() || self.muted.is_none() || self.input.is_none())
    }
}

struct Unit {
    bus: SharedBus,
    host: String,
    port: u16,
    device_id: DeviceId,
    model: String,
    /// Firmware revision from the AMX answer, when asked.
    revision_text: Option<String>,
    name: String,
    discovery: String,
    state: UnitState,
    /// True once the session has answered a query; nothing is published before.
    validated: bool,
    standby_deadline: Option<Instant>,
    last_rx: Instant,
}

impl Unit {
    fn series(&self) -> Option<Series> {
        Series::for_model(&self.model)
    }

    async fn run(mut self, mut commands: mpsc::Receiver<UnitCommand>, cancel: CancellationToken) {
        let mut session: Option<TcpStream> = None;
        let mut buffer: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 1024];
        let mut backoff = RECONNECT_MIN;
        let mut next_connect = Instant::now();
        let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
        heartbeat.tick().await;
        let mut ticks: u32 = 0;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                command = commands.recv() => {
                    let Some(command) = command else { break };
                    match command {
                        UnitCommand::Rehost(host) => {
                            tracing::info!(device_id = %self.device_id, %host, "Device moved to a new address");
                            self.host = host;
                            session = None;
                            buffer.clear();
                            next_connect = Instant::now();
                        }
                        UnitCommand::Execute { operation, reply } => {
                            let result = self.execute(&mut session, &mut buffer, &operation).await;
                            if reply.send(result).is_err() {
                                tracing::debug!(device_id = %self.device_id, "command caller went away before the reply");
                            }
                        }
                    }
                }
                read = read_chunk(&mut session, &mut chunk), if session.is_some() => {
                    match read {
                        Ok(0) | Err(_) => {
                            session = None;
                            buffer.clear();
                            self.on_session_lost();
                            next_connect = Instant::now() + backoff;
                            self.publish();
                        }
                        Ok(n) => {
                            self.last_rx = Instant::now();
                            buffer.extend_from_slice(&chunk[..n]);
                            if self.drain(&mut buffer) {
                                self.publish();
                            }
                        }
                    }
                }
                _ = tokio::time::sleep_until(next_connect), if session.is_none() => {
                    match self.open_session().await {
                        Ok(stream) => {
                            session = Some(stream);
                            buffer.clear();
                            backoff = RECONNECT_MIN;
                        }
                        Err(detail) => {
                            self.on_unreachable(detail);
                            backoff = (backoff * 2).min(RECONNECT_MAX);
                            next_connect = Instant::now() + backoff;
                            self.publish();
                        }
                    }
                }
                _ = heartbeat.tick(), if session.is_some() => {
                    ticks = ticks.wrapping_add(1);
                    let silent = self.last_rx.elapsed() > SILENCE_LIMIT;
                    let sent = if silent {
                        Err("no data from the unit for three heartbeats".to_string())
                    } else if ticks.is_multiple_of(FULL_REFRESH_TICKS) || self.state.needs_refresh() {
                        send_frames(&mut session, &protocol::state_queries(self.series())).await
                    } else {
                        send_frames(&mut session, &[protocol::query(protocol::cc::POWER)]).await
                    };
                    if let Err(error) = sent {
                        tracing::debug!(device_id = %self.device_id, "heartbeat failed: {error}");
                        session = None;
                        buffer.clear();
                        self.on_session_lost();
                        next_connect = Instant::now() + backoff;
                        self.publish();
                    }
                }
            }
        }
        if self.validated {
            self.bus.publish(BusEvent::DeviceRemoved {
                device_id: self.device_id.clone(),
            });
        }
    }

    /// Apply every complete response in `buffer`. True when state changed.
    fn drain(&mut self, buffer: &mut Vec<u8>) -> bool {
        // The AMX answer carries the model; take it before the framing parser discards it.
        if let Some(end) = buffer.iter().position(|b| *b == 0x0D) {
            if let Some(info) = protocol::parse_amx_response(&buffer[..=end]) {
                if self.model.is_empty() {
                    if let Some(model) = info.model {
                        self.model = model;
                    }
                }
                self.revision_text = info.revision;
            }
        }
        let mut changed = false;
        for response in protocol::drain_responses(buffer) {
            let Some(reply) = protocol::parse_reply(&response, self.series()) else {
                continue;
            };
            match reply {
                Reply::Power(on) => {
                    self.state.power = Some(on);
                    self.state.inferred_standby = false;
                }
                Reply::Mute(muted) => self.state.muted = Some(muted),
                Reply::Volume(level) => self.state.volume = Some(level),
                Reply::Source(name) => self.state.input = Some(name),
                Reply::Rejected { command, answer } => {
                    // Queries made while the unit is in standby answer 0x85; that is expected.
                    if !(answer == protocol::ac::COMMAND_INVALID_AT_THIS_TIME
                        && self.state.power != Some(true))
                    {
                        self.state.detail = Some(format!(
                            "unit rejected command 0x{command:02X}: {}",
                            protocol::answer_text(answer)
                        ));
                        changed = true;
                    }
                    continue;
                }
            }
            self.validated = true;
            self.state.detail = None;
            changed = true;
        }
        changed
    }

    async fn open_session(&mut self) -> Result<TcpStream, String> {
        let stream = match tokio::time::timeout(
            CONNECT_TIMEOUT,
            TcpStream::connect((self.host.as_str(), self.port)),
        )
        .await
        {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                return Err(
                    "connection refused: the unit is in standby with network control off, or \
                     another client holds its control session"
                        .to_string(),
                )
            }
            Ok(Err(error)) => return Err(format!("connection failed: {error}")),
            Err(_) => return Err("connection timed out".to_string()),
        };
        tracing::info!(device_id = %self.device_id, host = %self.host, "Arcam session open");
        self.state.connected = true;
        self.state.detail = None;
        self.standby_deadline = None;
        self.last_rx = Instant::now();
        let mut session = Some(stream);
        // Queries only: nothing on connect changes the unit's state.
        let mut frames: Vec<Vec<u8>> = Vec::new();
        if self.model.is_empty() {
            frames.push(protocol::AMX_REQUEST.to_vec());
        }
        frames.extend(protocol::state_queries(self.series()));
        if let Err(error) = send_frames(&mut session, &frames).await {
            return Err(format!("state queries failed: {error}"));
        }
        session.ok_or_else(|| "session lost".to_string())
    }

    fn on_session_lost(&mut self) {
        self.state.connected = false;
        if self
            .standby_deadline
            .is_some_and(|deadline| Instant::now() < deadline)
        {
            self.state.power = Some(false);
            self.state.inferred_standby = true;
            self.state.detail = Some(
                "session closed right after power-off was accepted: network control is \
                 unavailable in standby (power inferred)"
                    .to_string(),
            );
        } else {
            if !self.state.inferred_standby {
                self.state.power = None;
            }
            self.state.detail = Some("control session closed; reconnecting".to_string());
        }
    }

    fn on_unreachable(&mut self, detail: String) {
        self.state.connected = false;
        if !self.state.inferred_standby {
            self.state.power = None;
        }
        self.state.detail = Some(detail);
    }

    async fn execute(
        &mut self,
        session: &mut Option<TcpStream>,
        buffer: &mut Vec<u8>,
        operation: &DeviceOperation,
    ) -> Result<(), String> {
        let plan = protocol::plan_operation(self.series(), operation)?;
        if session.is_none() {
            match self.open_session().await {
                Ok(stream) => {
                    *session = Some(stream);
                    buffer.clear();
                }
                Err(detail) => {
                    self.on_unreachable(detail.clone());
                    self.publish();
                    let wake = if matches!(operation, DeviceOperation::PowerOn) {
                        format!(" Power-on cannot reach a unit in standby: {NO_IP_WAKE}.")
                    } else {
                        String::new()
                    };
                    return Err(format!(
                        "Arcam {} at {}:{} is not reachable ({detail}).{wake}",
                        self.model, self.host, self.port
                    ));
                }
            }
        }
        let mut frames = plan.frames.iter();
        if let Some(first) = frames.next() {
            if let Err(error) = send_frames(session, std::slice::from_ref(first)).await {
                *session = None;
                self.on_session_lost();
                self.publish();
                return Err(error);
            }
        }
        if matches!(operation, DeviceOperation::PowerStandby) {
            self.standby_deadline = Some(Instant::now() + STANDBY_GRACE);
        }
        let queries: Vec<Vec<u8>> = plan
            .readback
            .iter()
            .map(|cc| protocol::query(*cc))
            .collect();
        if let Err(error) = send_frames(session, &queries).await {
            *session = None;
            self.on_session_lost();
            self.publish();
            return Err(error);
        }
        Ok(())
    }

    fn publish(&self) {
        if !self.validated {
            return;
        }
        let series = self.series();
        let has_volume = series.is_none_or(Series::has_volume);
        let device = Device {
            device_id: self.device_id.clone(),
            vendor: "Arcam".to_string(),
            model: if self.model.is_empty() {
                "Arcam".to_string()
            } else {
                self.model.clone()
            },
            name: self.name.clone(),
            host: self.host.clone(),
            discovery: self.discovery.clone(),
            maturity: DEVICE_CONTROL_MATURITY.to_string(),
            connection: DeviceConnection {
                control_channel: if self.state.connected {
                    ControlChannel::Push
                } else {
                    ControlChannel::None
                },
                detail: self.state.detail.clone(),
            },
            power: self
                .state
                .power
                .map(protocol::power_of)
                .unwrap_or(DevicePower::Unknown),
            input: self.state.input.clone(),
            inputs: series.map(Series::inputs).unwrap_or_default(),
            volume: DeviceVolume {
                scale: crate::bus::VolumeScale::Percentage,
                value: if has_volume {
                    self.state.volume.map(f32::from)
                } else {
                    None
                },
                min: 0.0,
                hardware_max: has_volume.then_some(f32::from(protocol::MAX_VOLUME)),
                ceiling: None,
                step: 1.0,
                is_muted: self.state.muted,
                display: None,
            },
            capabilities: capabilities_for(&self.model),
            revision: next_revision(),
            last_updated: now_ms(),
        };
        self.bus.publish(BusEvent::DeviceObserved {
            device: Box::new(device),
        });
    }
}

/// Read from the session if there is one; pends forever otherwise. `read` is cancel-safe.
async fn read_chunk(session: &mut Option<TcpStream>, chunk: &mut [u8]) -> std::io::Result<usize> {
    match session {
        Some(stream) => stream.read(chunk).await,
        None => std::future::pending().await,
    }
}

/// Write each frame with a settling gap after it.
async fn send_frames(session: &mut Option<TcpStream>, frames: &[Vec<u8>]) -> Result<(), String> {
    for frame in frames {
        let stream = session
            .as_mut()
            .ok_or_else(|| "no control session".to_string())?;
        tokio::time::timeout(WRITE_TIMEOUT, stream.write_all(frame))
            .await
            .map_err(|_| "write timed out".to_string())?
            .map_err(|error| format!("write failed: {error}"))?;
        tokio::time::sleep(COMMAND_SPACING).await;
    }
    Ok(())
}
