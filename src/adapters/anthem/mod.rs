//! Anthem MRX / AVM device adapter (#774). **Alpha.**
//!
//! A *device* adapter: it controls the receiver a zone feeds, not a music provider. It never
//! publishes zones; it publishes [`BusEvent::DeviceObserved`] snapshots and executes
//! [`DeviceOperation`]s handed to it by [`crate::devices::service::DeviceService`], which has
//! already resolved, clamped and quantised every level. This adapter never computes a level.
//!
//! # Protocol
//!
//! Semicolon-framed ASCII over a persistent TCP session on port 14999 (`Z1POW1;`, `Z1VOL-35;`,
//! `Z1PVOL53;`, `Z1INP3;`, `IDM?;`). Implemented from `anthemav`
//! (<https://github.com/nugget/python-anthemav>, MIT: `protocol.py`, `connection.py`). The x20
//! and x40 generations differ in volume (whole dB versus percent), input-name queries and MAC
//! queries; see [`protocol`]. **No Anthem hardware has been available**: every capability claim
//! is implementation-derived.
//!
//! # No discovery
//!
//! The selected Home Assistant integration verifies no discovery mechanism, so this family uses
//! **manual devices only**: a `ManualDevice` with `family = "anthem"`, a host and optionally a
//! port and a `mac`. A configured host is published only after it answers `IDM?;` with a model
//! this adapter can classify, and a usable hardware address (the unit's own MAC, or the
//! configured one when the unit reports none): discovery is not capability detection, and a
//! guessed identity would let a link follow the wrong receiver.
//!
//! # Safety decisions
//!
//! - **Zone 1 only.** The catalog scopes this adapter to Zone 1 of the MRX / AVM family. MDX /
//!   MDA distribution amplifiers are refused.
//! - **No feedback switch on connect.** The reference client sends `ECH1` (x20) or `GCTXS1` (x40)
//!   on connect to turn the unit's status transmission on. Those are writes, so this adapter does
//!   not send them: it sends queries and polls. If the unit reports that transmission is off, the
//!   connection detail says so and the setting must be changed in the unit's own menu.
//! - **Standby.** A powered-off zone answers writes with `!Z` and ignores them, so volume, mute
//!   and input writes are refused unless the unit reports it is on. Waking a unit from standby
//!   needs standby IP control enabled on the unit.
//! - **One session.** A persistent client can compete with Anthem's own app or ARC software on
//!   limited-connection models; that is the cost of readback, and the reason the adapter is
//!   opt-in.

pub mod protocol;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::bus::{BusEvent, SharedBus};
use crate::devices::links::ManualDevice;
use crate::devices::service::{DeviceDriver, DeviceOperation};
use crate::devices::{
    CapabilityEvidence, ControlChannel, Device, DeviceCapabilities, DeviceCapability,
    DeviceConnection, DeviceId, DeviceInput, DevicePower, DeviceVolume, DEVICE_CONTROL_MATURITY,
};
use protocol::{AnthemEvent, InputCatalog, MacKind, Series};

/// Settings/coordinator name (`ManualDevice::family`).
pub const ADAPTER_NAME: &str = "anthem";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Verified-by-query poll: transmission may be off, and the unit does not always report a
/// change that did not come from its own front panel.
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Input names change rarely; re-read them every this many polls (and whenever the catalog is
/// empty, which is how a unit that was just powered on, whose names lag, fills in).
const CATALOG_REFRESH_TICKS: u32 = 12;
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(60);
const RECONNECT_MIN: Duration = Duration::from_secs(2);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// Gap between writes; the reference client waits 20 ms between queries.
const COMMAND_SPACING: Duration = Duration::from_millis(30);

/// What the unit reported about itself: enough to classify and identify it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnthemIdentity {
    /// `IDM` reply, e.g. `MRX 740`.
    pub model: String,
    /// The unit's hardware address (see [`protocol::preferred_mac`]).
    pub mac: String,
}

/// Capability claims for one model. Pure, so documentation can be generated from it.
///
/// Every claim is implementation-derived from `anthemav`, not from a device. A model that cannot
/// be classified (an MDX / MDA, or no model at all) claims nothing.
pub fn capabilities_for(model: &str) -> DeviceCapabilities {
    let derived = |supported: bool, detail: String| DeviceCapability {
        supported,
        evidence: CapabilityEvidence::ImplementationDerived,
        detail,
    };
    let Some(series) = protocol::series_for_model(model) else {
        let detail = format!(
            "model {model:?} is not a supported Anthem MRX / AVM receiver (MDX / MDA \
             distribution amplifiers and unreported models are out of scope)"
        );
        return DeviceCapabilities {
            observe: derived(false, detail.clone()),
            volume: derived(false, detail.clone()),
            mute: derived(false, detail.clone()),
            power: derived(false, detail.clone()),
            input_select: derived(false, detail),
        };
    };
    let (generation, volume) = match series {
        Series::X20 => (
            "x20 generation (MRX 520/720/1120, AVM 60)",
            "Z1VOL in whole dB, -90 to 0",
        ),
        Series::X40 => (
            "x40 generation (MRX 540/740/1140, AVM 70/90)",
            "Z1PVOL in percent, 0 to 100",
        ),
    };
    let pending = "; implementation-derived from anthemav, not exercised on hardware; Zone 1 only";
    DeviceCapabilities {
        observe: derived(
            true,
            format!(
                "Zone 1 power, level, mute, input and input names read over TCP 14999 on the \
                 {generation}; queries only, so the unit's status transmission must already be on{pending}"
            ),
        ),
        volume: derived(
            true,
            format!(
                "{volume}; the protocol's full scale is the maximum, lowered by the user ceiling; \
                 refused while the zone is off{pending}"
            ),
        ),
        mute: derived(
            true,
            format!("Z1MUT1 / Z1MUT0; refused while the zone is off{pending}"),
        ),
        power: derived(
            true,
            format!(
                "Z1POW1 / Z1POW0 (Zone 1, not the whole unit); waking from standby needs \
                 standby IP control enabled on the unit{pending}"
            ),
        ),
        input_select: derived(
            true,
            format!(
                "Z1INP<n> with the numbers from the unit's own input list; refused while the \
                 zone is off{pending}"
            ),
        ),
    }
}

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
    Execute(crate::devices::service::DeviceWork),
}

struct UnitHandle {
    host: String,
    port: u16,
    commands: mpsc::Sender<UnitCommand>,
    cancel: CancellationToken,
}

#[derive(Default)]
struct AdapterState {
    running: bool,
    cancel: CancellationToken,
    units: HashMap<String, UnitHandle>,
}

#[derive(Clone)]
pub struct AnthemAdapter {
    bus: SharedBus,
    manual: Arc<RwLock<Vec<ManualDevice>>>,
    state: Arc<RwLock<AdapterState>>,
}

impl AnthemAdapter {
    pub fn new(bus: SharedBus) -> Self {
        Self {
            bus,
            manual: Arc::new(RwLock::new(Vec::new())),
            state: Arc::new(RwLock::new(AdapterState::default())),
        }
    }

    /// The configured receivers. Anthem has no discovery: these are the only units it will see.
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
        tracing::info!("Anthem device adapter started (alpha, manual devices only)");
        Ok(())
    }

    async fn stop_internal(&self) {
        let units = {
            let mut state = self.state.write().await;
            state.running = false;
            state.cancel.cancel();
            std::mem::take(&mut state.units)
        };
        for (device_id, unit) in units {
            unit.cancel.cancel();
            if let Some(device_id) = DeviceId::parse(&device_id) {
                self.bus.publish(BusEvent::DeviceRemoved { device_id });
            }
        }
        tracing::info!("Anthem device adapter stopped");
    }

    async fn discovery_loop(&self, cancel: CancellationToken) {
        let mut ticker = tokio::time::interval(DISCOVERY_INTERVAL);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = ticker.tick() => self.connect_configured(&cancel).await,
            }
        }
    }

    /// Probe every configured receiver that is not attached yet.
    async fn connect_configured(&self, cancel: &CancellationToken) {
        for manual in self.manual.read().await.clone() {
            if cancel.is_cancelled() {
                return;
            }
            let port = manual.port.unwrap_or(protocol::DEFAULT_PORT);
            let attached = self
                .state
                .read()
                .await
                .units
                .values()
                .any(|unit| unit.host == manual.host && unit.port == port);
            if attached {
                continue;
            }
            let Some(identity) = identify(&manual.host, port, manual.mac.as_deref()).await else {
                tracing::debug!(host = %manual.host, "Anthem receiver did not identify itself");
                continue;
            };
            let Some(device_id) = DeviceId::new("anthem", &identity.mac) else {
                continue;
            };
            if !self.state.read().await.running {
                return;
            }
            let unit_cancel = cancel.child_token();
            if let Err(error) = self
                .spawn_unit(&device_id, &manual.host, port, identity, unit_cancel)
                .await
            {
                tracing::warn!(%device_id, host = %manual.host, "Anthem device not attached: {error}");
            }
        }
    }

    /// Attach a unit at a known address and identity: a mock receiver on an ephemeral port in
    /// tests. The unit runs until the adapter stops. Fails when the model cannot be classified.
    pub async fn attach_unit(
        &self,
        device_id: &DeviceId,
        host: &str,
        port: u16,
        identity: AnthemIdentity,
    ) -> Result<(), String> {
        let cancel = {
            let mut state = self.state.write().await;
            state.running = true;
            state.cancel.child_token()
        };
        self.spawn_unit(device_id, host, port, identity, cancel)
            .await
    }

    async fn spawn_unit(
        &self,
        device_id: &DeviceId,
        host: &str,
        port: u16,
        identity: AnthemIdentity,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        let series = protocol::series_for_model(&identity.model).ok_or_else(|| {
            format!(
                "{:?} is not a supported Anthem MRX / AVM receiver, so no command set is known \
                 for it",
                identity.model
            )
        })?;
        let mut state = self.state.write().await;
        // One session per receiver: neither a repeated id nor a second id at the same address.
        if state.units.contains_key(device_id.as_str())
            || state
                .units
                .values()
                .any(|unit| unit.host == host && unit.port == port)
        {
            return Ok(());
        }
        let (commands, receiver) = mpsc::channel(8);
        state.units.insert(
            device_id.to_string(),
            UnitHandle {
                host: host.to_string(),
                port,
                commands,
                cancel: cancel.clone(),
            },
        );
        drop(state);
        tracing::info!(%device_id, host, model = %identity.model, "Attached Anthem device");
        let unit = Unit {
            bus: self.bus.clone(),
            host: host.to_string(),
            port,
            device_id: device_id.clone(),
            name: format!("Anthem {}", identity.model),
            model: identity.model,
            series,
            state: UnitState::default(),
        };
        tokio::spawn(unit.run(receiver, cancel));
        Ok(())
    }

    pub async fn device_count(&self) -> usize {
        self.state.read().await.units.len()
    }
}

#[async_trait]
impl crate::adapters::devices::DeviceFamily for AnthemAdapter {
    fn family(&self) -> &'static str {
        ADAPTER_NAME
    }

    async fn start_family(
        &self,
        context: crate::adapters::devices::FamilyContext,
    ) -> anyhow::Result<()> {
        self.set_manual_devices(context.manual).await;
        self.start_internal().await
    }

    async fn stop_family(&self) {
        self.stop_internal().await
    }
}

#[async_trait]
impl DeviceDriver for AnthemAdapter {
    fn kinds(&self) -> &'static [&'static str] {
        &["anthem"]
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
            commands.send(UnitCommand::Execute(
                crate::devices::service::DeviceWork::new(operation, reply),
            )),
        )
        .await
        .map_err(|_| "the unit's session is busy".to_string())?
        .map_err(|_| "the unit's session has stopped".to_string())?;
        tokio::time::timeout(WRITE_TIMEOUT * 2, response)
            .await
            .map_err(|_| "the unit did not accept the command in time".to_string())?
            .map_err(|_| "the unit's session stopped before sending".to_string())?
    }
}

/// Everything the unit has reported, before it becomes a [`Device`].
#[derive(Default)]
struct UnitState {
    power: Option<bool>,
    volume_db: Option<i32>,
    volume_percent: Option<u8>,
    muted: Option<bool>,
    input: Option<u16>,
    inputs: InputCatalog,
    tx_on: Option<bool>,
    connected: bool,
    detail: Option<String>,
}

impl UnitState {
    fn power(&self) -> DevicePower {
        match self.power {
            Some(true) => DevicePower::On,
            Some(false) => DevicePower::Standby,
            None => DevicePower::Unknown,
        }
    }

    /// Apply an event; returns what the session should ask next, if anything.
    fn apply(&mut self, event: AnthemEvent) -> FollowUp {
        match event {
            AnthemEvent::Model(_) | AnthemEvent::Mac { .. } => FollowUp::None,
            AnthemEvent::Power(on) => {
                let came_on = on && self.power != Some(true);
                self.power = Some(on);
                // A unit's input names lag a power-on: ask again.
                if came_on {
                    FollowUp::InputCount
                } else {
                    FollowUp::None
                }
            }
            AnthemEvent::VolumeDb(db) => {
                self.volume_db = Some(db);
                FollowUp::None
            }
            AnthemEvent::VolumePercent(percent) => {
                self.volume_percent = Some(percent);
                FollowUp::None
            }
            AnthemEvent::Mute(muted) => {
                self.muted = Some(muted);
                FollowUp::None
            }
            AnthemEvent::Input(number) => {
                self.input = Some(number);
                FollowUp::None
            }
            AnthemEvent::InputCount(count) => FollowUp::InputNames(count),
            AnthemEvent::InputName { number, name } => {
                self.inputs.insert(number, name);
                FollowUp::None
            }
            AnthemEvent::TxStatus(on) => {
                self.tx_on = Some(on);
                FollowUp::None
            }
            AnthemEvent::Rejected(kind) => {
                tracing::debug!("Anthem refused a command: {kind:?}");
                FollowUp::None
            }
        }
    }

    /// An unreachable unit's last readings are not facts: clear them so nothing downstream shows
    /// (or computes a relative step from) a stale level.
    fn lost(&mut self, detail: &str) {
        *self = Self {
            detail: Some(detail.to_string()),
            ..Self::default()
        };
    }
}

enum FollowUp {
    None,
    InputCount,
    InputNames(usize),
}

struct Unit {
    bus: SharedBus,
    host: String,
    port: u16,
    device_id: DeviceId,
    model: String,
    name: String,
    series: Series,
    state: UnitState,
}

impl Unit {
    async fn run(mut self, mut commands: mpsc::Receiver<UnitCommand>, cancel: CancellationToken) {
        let mut stream: Option<TcpStream> = None;
        let mut buffer = String::new();
        let mut chunk = [0u8; 1024];
        let mut backoff = RECONNECT_MIN;
        let mut next_connect = Instant::now();
        let mut poll = tokio::time::interval(POLL_INTERVAL);
        let mut ticks: u32 = 0;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                command = commands.recv() => {
                    let Some(UnitCommand::Execute(work)) = command else { break };
                    let Some((operation, reply)) = work.begin_dispatch() else { continue; };
                    let result = self.execute(&mut stream, &operation).await;
                    if reply.send(result).is_err() {
                        tracing::debug!(device_id = %self.device_id, "command caller went away before the reply");
                    }
                    if stream.is_none() && self.state.connected {
                        self.state.lost("control session lost; reconnecting");
                        next_connect = Instant::now() + backoff;
                        self.publish();
                    }
                }
                read = read_chunk(&mut stream, &mut chunk), if stream.is_some() => {
                    match read {
                        Ok(0) | Err(_) => {
                            tracing::info!(device_id = %self.device_id, "Control session closed; reconnecting");
                            stream = None;
                            buffer.clear();
                            self.state.lost("control session closed; reconnecting");
                            next_connect = Instant::now() + backoff;
                            self.publish();
                        }
                        Ok(n) => {
                            buffer.push_str(&String::from_utf8_lossy(&chunk[..n]));
                            let mut follow_ups = Vec::new();
                            for message in protocol::drain_messages(&mut buffer) {
                                if let Some(event) = protocol::parse_message(&message) {
                                    follow_ups.push(self.state.apply(event));
                                    // Every readback is a new revision, changed or not: command
                                    // confirmation waits for one.
                                    self.publish();
                                }
                            }
                            let mut failed = false;
                            for follow_up in follow_ups {
                                let queries = match follow_up {
                                    FollowUp::None => Vec::new(),
                                    FollowUp::InputCount => vec![protocol::INPUT_COUNT_QUERY.to_string()],
                                    FollowUp::InputNames(count) => protocol::input_name_queries(self.series, count),
                                };
                                if send_queries(&mut stream, &queries).await.is_err() {
                                    failed = true;
                                    break;
                                }
                            }
                            if failed {
                                stream = None;
                                buffer.clear();
                                self.state.lost("control session lost; reconnecting");
                                next_connect = Instant::now() + backoff;
                                self.publish();
                            }
                        }
                    }
                }
                _ = tokio::time::sleep_until(next_connect), if stream.is_none() => {
                    match self.connect().await {
                        Ok(opened) => {
                            tracing::info!(device_id = %self.device_id, host = %self.host, "Control session open");
                            stream = Some(opened);
                            backoff = RECONNECT_MIN;
                            self.state.connected = true;
                            self.state.detail = None;
                            // Queries only: a reconnect must never change what the receiver is doing.
                            let mut queries = vec![
                                protocol::INPUT_COUNT_QUERY.to_string(),
                                protocol::tx_status_query(self.series).to_string(),
                            ];
                            queries.extend(protocol::state_queries(self.series).iter().map(|q| q.to_string()));
                            if let Err(error) = send_queries(&mut stream, &queries).await {
                                tracing::debug!(device_id = %self.device_id, "state queries failed: {error}");
                                stream = None;
                                self.state.lost("control session lost; reconnecting");
                                next_connect = Instant::now() + backoff;
                            }
                            self.publish();
                        }
                        Err(detail) => {
                            self.state.lost(&detail);
                            backoff = (backoff * 2).min(RECONNECT_MAX);
                            next_connect = Instant::now() + backoff;
                            self.publish();
                        }
                    }
                }
                _ = poll.tick() => {
                    if stream.is_some() {
                        ticks = ticks.wrapping_add(1);
                        let mut queries: Vec<String> = protocol::state_queries(self.series)
                            .iter()
                            .map(|q| q.to_string())
                            .collect();
                        if self.state.inputs.is_empty() || ticks.is_multiple_of(CATALOG_REFRESH_TICKS) {
                            queries.push(protocol::INPUT_COUNT_QUERY.to_string());
                        }
                        if let Err(error) = send_queries(&mut stream, &queries).await {
                            tracing::debug!(device_id = %self.device_id, "poll failed: {error}");
                            stream = None;
                            buffer.clear();
                            self.state.lost("control session lost; reconnecting");
                            next_connect = Instant::now() + backoff;
                            self.publish();
                        }
                    }
                }
            }
        }
        self.bus.publish(BusEvent::DeviceRemoved {
            device_id: self.device_id.clone(),
        });
    }

    async fn connect(&self) -> Result<TcpStream, String> {
        match tokio::time::timeout(
            CONNECT_TIMEOUT,
            TcpStream::connect((self.host.as_str(), self.port)),
        )
        .await
        {
            Ok(Ok(stream)) => Ok(stream),
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::ConnectionRefused => Err(
                "control port refused: the receiver is in a standby that closes the network \
                 (enable standby IP control on the unit), or another client holds its session"
                    .to_string(),
            ),
            Ok(Err(error)) => Err(format!("control port unavailable: {error}")),
            Err(_) => Err("control port connect timed out".to_string()),
        }
    }

    async fn execute(
        &mut self,
        stream: &mut Option<TcpStream>,
        operation: &DeviceOperation,
    ) -> Result<(), String> {
        if stream.is_none() {
            return Err("the receiver's control session is not connected".to_string());
        }
        // A powered-off zone answers writes with `!Z` and ignores them. Refuse rather than time out.
        let needs_power = !matches!(
            operation,
            DeviceOperation::PowerOn | DeviceOperation::PowerStandby
        );
        if needs_power && self.state.power() != DevicePower::On {
            return Err(match self.state.power() {
                DevicePower::Standby => format!(
                    "{operation:?} is not supported while Zone 1 is off: the receiver ignores it; \
                     power it on first"
                ),
                _ => format!(
                    "{operation:?} is refused until the receiver has reported that Zone 1 is on"
                ),
            });
        }
        let (command, query) = protocol::command(self.series, operation).ok_or_else(|| {
            format!(
                "{operation:?} cannot be encoded for this receiver ({})",
                match self.series {
                    Series::X20 => "x20 volume is whole dB from -90 to 0",
                    Series::X40 => "x40 volume is whole percent from 0 to 100",
                }
            )
        })?;
        if let Err(error) = send_line(stream, &command).await {
            *stream = None;
            return Err(error);
        }
        if let Err(error) = send_line(stream, query).await {
            *stream = None;
            return Err(error);
        }
        Ok(())
    }

    fn publish(&self) {
        let (scale, min, value, step) = match self.series {
            Series::X20 => (
                crate::bus::VolumeScale::Decibel,
                protocol::MIN_DB,
                self.state.volume_db.map(|db| db as f32),
                1.0,
            ),
            Series::X40 => (
                crate::bus::VolumeScale::Percentage,
                protocol::MIN_PERCENT,
                self.state.volume_percent.map(f32::from),
                1.0,
            ),
        };
        let detail = self.state.detail.clone().or_else(|| {
            (self.state.tx_on == Some(false)).then(|| {
                "the receiver's status transmission (TX status) is off, so it may not answer \
                 queries or report changes; turn it on in the receiver's menu"
                    .to_string()
            })
        });
        let device = Device {
            device_id: self.device_id.clone(),
            vendor: "Anthem".to_string(),
            model: self.model.clone(),
            name: self.name.clone(),
            host: self.host.clone(),
            discovery: "manual".to_string(),
            maturity: DEVICE_CONTROL_MATURITY.to_string(),
            connection: DeviceConnection {
                // Queries only (status transmission is never switched on): a polled session.
                control_channel: if self.state.connected {
                    ControlChannel::Polling
                } else {
                    ControlChannel::None
                },
                detail,
            },
            power: self.state.power(),
            input: self.state.input.map(|number| number.to_string()),
            inputs: self
                .state
                .inputs
                .iter()
                .map(|(number, name)| DeviceInput {
                    id: number.to_string(),
                    name: name.clone(),
                    available: true,
                })
                .collect(),
            volume: DeviceVolume {
                scale,
                value,
                min,
                // The protocol's full scale is the device maximum; the user's ceiling lowers it.
                hardware_max: Some(match self.series {
                    Series::X20 => protocol::MAX_DB,
                    Series::X40 => protocol::MAX_PERCENT,
                }),
                ceiling: None,
                step,
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

/// Connect, ask `IDM?;`, then the model's identity queries, and return the model and a usable
/// hardware address (the configured one when the unit reports none). `None` when the unit does
/// not speak the protocol, cannot be classified, or has no identity at all.
async fn identify(host: &str, port: u16, configured_mac: Option<&str>) -> Option<AnthemIdentity> {
    let mut stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port)))
        .await
        .ok()?
        .ok()?;
    stream
        .write_all(protocol::MODEL_QUERY.as_bytes())
        .await
        .ok()?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut buffer = String::new();
    let mut chunk = [0u8; 512];
    let mut model: Option<String> = None;
    let mut macs: Vec<(MacKind, String)> = Vec::new();
    let mut asked = false;
    loop {
        let n = match tokio::time::timeout_at(deadline, stream.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => n,
        };
        buffer.push_str(&String::from_utf8_lossy(&chunk[..n]));
        for message in protocol::drain_messages(&mut buffer) {
            match protocol::parse_message(&message) {
                Some(AnthemEvent::Model(reported)) if model.is_none() => model = Some(reported),
                Some(AnthemEvent::Mac { kind, value }) => macs.push((kind, value)),
                _ => {}
            }
        }
        if let Some(reported) = &model {
            let series = protocol::series_for_model(reported)?;
            if !asked {
                asked = true;
                for query in protocol::identity_queries(series) {
                    stream.write_all(query.as_bytes()).await.ok()?;
                    tokio::time::sleep(COMMAND_SPACING).await;
                }
            }
            if protocol::preferred_mac(&macs).is_some() {
                break;
            }
        }
    }
    let model = model?;
    protocol::series_for_model(&model)?;
    let mac =
        protocol::preferred_mac(&macs).or_else(|| configured_mac.and_then(protocol::valid_mac))?;
    Some(AnthemIdentity { model, mac })
}

/// Read from the session if there is one; pends forever otherwise. `read` is cancel-safe, so a
/// partial message survives in the caller's buffer when another `select!` branch wins.
async fn read_chunk(stream: &mut Option<TcpStream>, chunk: &mut [u8]) -> std::io::Result<usize> {
    match stream {
        Some(stream) => stream.read(chunk).await,
        None => std::future::pending().await,
    }
}

/// Write one already-framed (`;`-terminated) message, then wait out the spacing gap.
async fn send_line(stream: &mut Option<TcpStream>, line: &str) -> Result<(), String> {
    let socket = stream
        .as_mut()
        .ok_or_else(|| "no control session".to_string())?;
    tokio::time::timeout(WRITE_TIMEOUT, socket.write_all(line.as_bytes()))
        .await
        .map_err(|_| "control write timed out".to_string())?
        .map_err(|error| format!("control write failed: {error}"))?;
    tokio::time::sleep(COMMAND_SPACING).await;
    Ok(())
}

async fn send_queries<S: AsRef<str>>(
    stream: &mut Option<TcpStream>,
    queries: &[S],
) -> Result<(), String> {
    for query in queries {
        send_line(stream, query.as_ref()).await?;
    }
    Ok(())
}
