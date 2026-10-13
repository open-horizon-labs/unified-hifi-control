//! Lyngdorf / Steinway Lyngdorf device adapter (#774). **Alpha.**
//!
//! A *device* adapter: it controls the amplifier or processor a zone feeds, not a music
//! provider. It never publishes zones; it publishes [`BusEvent::DeviceObserved`] snapshots and
//! executes [`DeviceOperation`]s handed to it by [`crate::devices::service::DeviceService`],
//! which has already resolved, clamped and quantised every level. This adapter never computes
//! a level.
//!
//! # Protocol
//!
//! `!COMMAND(param)` ASCII over a persistent TCP session on port 84, CR-terminated. Implemented
//! from the `lyngdorf` library (<https://github.com/fishloa/lyngdorf>, MIT) and the vendor
//! control manuals it transcribes. Two dialects (MP/P processors, TDAI amplifiers) share the
//! framing; see [`protocol`]. **No Lyngdorf hardware has been available**: every capability
//! claim is implementation-derived (the library's own measurements on an MP-60 and a P200 are
//! cited where they matter, but they are not ours).
//!
//! # Discovery
//!
//! SSDP `MediaRenderer:2` with manufacturer "Lyngdorf" / "Steinway Lyngdorf"; identity is the
//! description's `serialNumber` (UDN uuid when absent). A unit is published only after it
//! answers `!DEVICE?` on TCP 84 **with a supported model**: discovery is not capability
//! detection, and a guessed model would pick the wrong command set and volume range. A
//! configured host (`ManualDevice`) is probed with a unicast M-SEARCH for its description.
//!
//! # Safety decisions
//!
//! - **Volume maximum.** MP/P units report `!MAXVOL`, the unit's own ceiling (read-only over the
//!   protocol, and enforced by the unit); it becomes [`DeviceVolume::hardware_max`]. TDAI units
//!   report none, so the model's documented maximum (+12 dB; +24 dB for MP/P until `!MAXVOL`
//!   arrives) is used. The user's ceiling only lowers it.
//! - **Standby.** A real P200 silently discards `!VOL`, `!MUTEOFF` and most other writes in
//!   standby, while `!SRC(n)` powers the unit *on* as a side effect. So volume, mute and input
//!   writes are refused unless the unit reports it is on, and mute is not published in standby
//!   (the unit answers `!MUTEON` there regardless of what was set).
//! - **Pacing.** A real MP-60 drops everything past roughly 16 in-flight commands, so every write
//!   and query is followed by a 50 ms gap, and one session owns the connection.
//! - **No verbosity change.** The reference client sends `!VERB(1)` on connect to get pushes.
//!   That is a session setting, but it is a write, so this adapter sends queries only and polls.
//!   Remote-control changes therefore appear on the next poll, not instantly.

pub mod protocol;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use ssdp_client::{SearchTarget, URN};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
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
use protocol::{Description, Dialect, LyngdorfEvent, Model, SourceTracker};

/// Settings/coordinator name (`ManualDevice::family`).
pub const ADAPTER_NAME: &str = "lyngdorf";

const SSDP_TARGET: &str = "urn:schemas-upnp-org:device:MediaRenderer:2";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
/// Verbosity is left at its default (replies only), so this poll is the only way a change made
/// with the unit's own remote reaches UHC.
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(60);
const RECONNECT_MIN: Duration = Duration::from_secs(2);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// Capability claims for one model. Pure, so documentation can be generated from it.
///
/// Every claim is implementation-derived: the `lyngdorf` library and the vendor manuals are the
/// evidence, not a device. An unrecognised model claims nothing, because no command set is known
/// for it (and the adapter never publishes one).
pub fn capabilities_for(model: &str) -> DeviceCapabilities {
    let derived = |supported: bool, detail: String| DeviceCapability {
        supported,
        evidence: CapabilityEvidence::ImplementationDerived,
        detail,
    };
    let Some(known) = protocol::lookup_model(model) else {
        let detail = format!(
            "model {model:?} is not a supported Lyngdorf model (MP-40/50/60, P100/200/300, \
             TDAI-1120/2210/3400); TDAI-2170 is excluded because its manual documents a serial \
             interface only"
        );
        return DeviceCapabilities {
            observe: derived(false, detail.clone()),
            volume: derived(false, detail.clone()),
            mute: derived(false, detail.clone()),
            power: derived(false, detail.clone()),
            input_select: derived(false, detail),
        };
    };
    let pending = "; implementation-derived from the lyngdorf library and vendor manual, not \
                   exercised on hardware";
    let (power, ceiling) = match known.dialect {
        Dialect::Mp => (
            "!POWERONMAIN / !POWEROFFMAIN (main zone only)",
            "the unit's own !MAXVOL (read-only over the protocol) bounds the range, lowered by the user ceiling",
        ),
        Dialect::Tdai => (
            "!ON / !OFF",
            "the unit reports no maximum, so the model's documented maximum applies, lowered by the user ceiling",
        ),
    };
    DeviceCapabilities {
        observe: derived(
            true,
            format!(
                "power, level, mute, source and source list read over TCP 84; replies only (no \
                 verbosity change), so remote-control changes arrive on a 5 s poll{pending}"
            ),
        ),
        volume: derived(
            true,
            format!(
                "!VOL(n) in tenths of a dB, -99.9 to {:+.1} dB; {ceiling}; refused while the unit \
                 is not on, because standby discards volume writes silently{pending}",
                known.max_db
            ),
        ),
        mute: derived(
            true,
            format!(
                "!MUTEON / !MUTEOFF; refused while the unit is not on, and not published in \
                 standby (the unit answers muted there regardless){pending}"
            ),
        ),
        power: derived(true, format!("{power}{pending}")),
        input_select: derived(
            true,
            format!(
                "!SRC(n) with the numbers from the unit's own source list; refused while the unit \
                 is not on, because !SRC(n) powers a unit on from standby{pending}"
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
    Execute {
        operation: DeviceOperation,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Rehost(String),
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
    /// SSDP `LOCATION`s already examined and found not to be a supported Lyngdorf (`None`) or
    /// found to be the unit with this id. An unreachable Lyngdorf is *not* recorded, so it is
    /// retried.
    examined: HashMap<String, Option<String>>,
}

#[derive(Clone)]
pub struct LyngdorfAdapter {
    bus: SharedBus,
    http: Client,
    manual: Arc<RwLock<Vec<ManualDevice>>>,
    state: Arc<RwLock<AdapterState>>,
}

enum Identified {
    NotSupported,
    Unreachable,
    Lyngdorf(DeviceId, Description),
}

impl LyngdorfAdapter {
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

    /// Hosts to probe directly, for units multicast cannot reach.
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
        tracing::info!("Lyngdorf device adapter started (alpha)");
        Ok(())
    }

    async fn stop_internal(&self) {
        let units = {
            let mut state = self.state.write().await;
            state.running = false;
            state.cancel.cancel();
            state.examined.clear();
            std::mem::take(&mut state.units)
        };
        for (device_id, unit) in units {
            unit.cancel.cancel();
            if let Some(device_id) = DeviceId::parse(&device_id) {
                self.bus.publish(BusEvent::DeviceRemoved { device_id });
            }
        }
        tracing::info!("Lyngdorf device adapter stopped");
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

    /// `(location, port)` pairs worth examining: SSDP answers, plus each configured host.
    async fn candidate_locations(&self) -> Vec<(String, Option<u16>)> {
        let mut locations: Vec<(String, Option<u16>)> = Vec::new();
        if let Ok(urn) = SSDP_TARGET.parse::<URN>() {
            match ssdp_client::search(&SearchTarget::URN(urn), Duration::from_secs(2), 2, None)
                .await
            {
                Ok(responses) => {
                    futures::pin_mut!(responses);
                    while let Some(response) = responses.next().await {
                        if let Ok(response) = response {
                            locations.push((response.location().to_string(), None));
                        }
                    }
                }
                Err(error) => tracing::debug!("Lyngdorf SSDP search failed: {error}"),
            }
        }
        for manual in self.manual.read().await.clone() {
            if let Some(location) = unicast_location(&manual.host, SSDP_TARGET).await {
                locations.push((location, manual.port));
            }
        }
        locations.sort();
        locations.dedup();
        locations
    }

    async fn discover_once(&self, cancel: &CancellationToken) {
        for (location, manual_port) in self.candidate_locations().await {
            if cancel.is_cancelled() {
                return;
            }
            let Some(host) = reqwest::Url::parse(&location)
                .ok()
                .and_then(|url| url.host_str().map(str::to_string))
            else {
                continue;
            };
            let manual = self
                .manual
                .read()
                .await
                .iter()
                .find(|device| device.host == host)
                .cloned();
            let port = manual
                .as_ref()
                .and_then(|device| device.port)
                .or(manual_port)
                .unwrap_or(protocol::DEFAULT_PORT);
            let discovery = if manual.is_some() { "manual" } else { "ssdp" };
            let known = self.state.read().await.examined.get(&location).cloned();
            match known {
                Some(None) => continue,
                Some(Some(device_id)) => {
                    if let Some(device_id) = DeviceId::parse(&device_id) {
                        self.follow_address(&device_id, &host).await;
                    }
                    continue;
                }
                None => {}
            }
            match self.identify(&location, &host, port).await {
                Identified::NotSupported => {
                    self.state.write().await.examined.insert(location, None);
                }
                Identified::Unreachable => {}
                Identified::Lyngdorf(device_id, description) => {
                    self.state
                        .write()
                        .await
                        .examined
                        .insert(location, Some(device_id.to_string()));
                    if let Err(error) = self
                        .ensure_unit(&device_id, &host, port, description, discovery, cancel)
                        .await
                    {
                        tracing::warn!(%device_id, "Lyngdorf device not attached: {error}");
                    }
                }
            }
        }
    }

    /// Known unit: follow an IP change without re-reading the description.
    async fn follow_address(&self, device_id: &DeviceId, host: &str) {
        let rehost = {
            let state = self.state.read().await;
            state
                .units
                .get(device_id.as_str())
                .filter(|unit| unit.host != host)
                .map(|unit| unit.commands.clone())
        };
        if let Some(commands) = rehost {
            if let Err(error) = commands.try_send(UnitCommand::Rehost(host.to_string())) {
                tracing::debug!(%device_id, "could not re-host unit: {error}");
            } else if let Some(unit) = self.state.write().await.units.get_mut(device_id.as_str()) {
                unit.host = host.to_string();
            }
        }
    }

    async fn identify(&self, location: &str, host: &str, port: u16) -> Identified {
        let body = match self.http.get(location).send().await {
            Ok(response) => match response.text().await {
                Ok(body) => body,
                Err(_) => return Identified::Unreachable,
            },
            Err(_) => return Identified::Unreachable,
        };
        let Some(mut description) = protocol::parse_description(&body) else {
            return Identified::NotSupported;
        };
        let Some(kind) = description.kind() else {
            return Identified::NotSupported;
        };
        let Some(device_id) = description
            .hardware_id()
            .and_then(|hardware_id| DeviceId::new(kind, &hardware_id))
        else {
            return Identified::NotSupported;
        };
        // Discovery is not capability detection: the unit must answer the control protocol, and
        // the model it names (not the description's) selects the command set.
        let Some(reported) = probe(host, port).await else {
            return Identified::Unreachable;
        };
        let Some(model) = protocol::lookup_model(&reported) else {
            tracing::info!(%device_id, reported, "Lyngdorf model is not supported; not publishing it");
            return Identified::NotSupported;
        };
        description.model = model.name.to_ascii_uppercase();
        Identified::Lyngdorf(device_id, description)
    }

    async fn ensure_unit(
        &self,
        device_id: &DeviceId,
        host: &str,
        port: u16,
        description: Description,
        discovery: &str,
        adapter_cancel: &CancellationToken,
    ) -> Result<(), String> {
        if !self.state.read().await.running {
            return Ok(());
        }
        self.spawn_unit(
            device_id,
            host,
            port,
            description,
            discovery,
            adapter_cancel.child_token(),
        )
        .await
    }

    /// Attach a unit at a known address without discovery: a mock unit on an ephemeral port in
    /// tests, or a configured host. The unit runs until the adapter stops. Fails when the
    /// description's model is not a supported Lyngdorf model (no command set is known for it).
    pub async fn attach_unit(
        &self,
        device_id: &DeviceId,
        host: &str,
        port: u16,
        description: Description,
    ) -> Result<(), String> {
        let cancel = {
            let mut state = self.state.write().await;
            state.running = true;
            state.cancel.child_token()
        };
        self.spawn_unit(device_id, host, port, description, "manual", cancel)
            .await
    }

    async fn spawn_unit(
        &self,
        device_id: &DeviceId,
        host: &str,
        port: u16,
        description: Description,
        discovery: &str,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        let model = protocol::lookup_model(&description.model).ok_or_else(|| {
            format!(
                "{:?} is not a supported Lyngdorf model, so no command set is known for it",
                description.model
            )
        })?;
        let mut state = self.state.write().await;
        // One session per unit: neither a repeated id nor a second id at the same address.
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
        tracing::info!(
            %device_id,
            host,
            model = model.name,
            name = %description.friendly_name,
            "Discovered Lyngdorf device"
        );
        let unit = Unit {
            bus: self.bus.clone(),
            host: host.to_string(),
            port,
            device_id: device_id.clone(),
            vendor: if description.manufacturer.is_empty() {
                "Lyngdorf".to_string()
            } else {
                description.manufacturer
            },
            model,
            name: description.friendly_name,
            discovery: discovery.to_string(),
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
impl crate::adapters::devices::DeviceFamily for LyngdorfAdapter {
    fn family(&self) -> &'static str {
        ADAPTER_NAME
    }

    async fn start_family(
        &self,
        context: crate::adapters::devices::FamilyContext,
    ) -> anyhow::Result<()> {
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
impl DeviceDriver for LyngdorfAdapter {
    fn kinds(&self) -> &'static [&'static str] {
        &["lyngdorf"]
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
        tokio::time::timeout(WRITE_TIMEOUT * 4, response)
            .await
            .map_err(|_| "the unit did not accept the command in time".to_string())?
            .map_err(|_| "the unit's session stopped before sending".to_string())?
    }
}

/// Everything the unit has reported, before it becomes a [`Device`].
#[derive(Default)]
struct UnitState {
    power: Option<bool>,
    volume_db: Option<f32>,
    max_db: Option<f32>,
    muted: Option<bool>,
    sources: SourceTracker,
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

    fn apply(&mut self, event: LyngdorfEvent) {
        let now = std::time::Instant::now();
        match event {
            LyngdorfEvent::Device(_) => {}
            LyngdorfEvent::Power(on) => self.power = Some(on),
            LyngdorfEvent::Volume(db) => self.volume_db = Some(db),
            LyngdorfEvent::MaxVolume(db) => self.max_db = Some(db),
            LyngdorfEvent::Mute(muted) => self.muted = Some(muted),
            LyngdorfEvent::SourceCount(count) => self.sources.count(count, now),
            LyngdorfEvent::SourceNamed { index, name } => self.sources.named(index, name, now),
            LyngdorfEvent::SourceIndex(index) => self.sources.index(index),
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

struct Unit {
    bus: SharedBus,
    host: String,
    port: u16,
    device_id: DeviceId,
    vendor: String,
    model: &'static Model,
    name: String,
    discovery: String,
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

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                command = commands.recv() => {
                    let Some(command) = command else { break };
                    match command {
                        UnitCommand::Rehost(host) => {
                            tracing::info!(device_id = %self.device_id, %host, "Device moved to a new address");
                            self.host = host;
                            stream = None;
                            buffer.clear();
                            next_connect = Instant::now();
                        }
                        UnitCommand::Execute { operation, reply } => {
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
                            for line in protocol::drain_lines(&mut buffer) {
                                if let Some(event) = protocol::parse_line(&line) {
                                    self.state.apply(event);
                                    // Every readback is a new revision, changed or not: command
                                    // confirmation waits for one.
                                    self.publish();
                                }
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
                            // Queries only: a reconnect must never change what the unit is doing.
                            let list = [protocol::source_list_query(self.model.dialect)];
                            let sent = send_queries(&mut stream, &list).await;
                            let sent = match sent {
                                Ok(()) => send_queries(&mut stream, protocol::state_queries(self.model.dialect)).await,
                                Err(error) => Err(error),
                            };
                            if let Err(error) = sent {
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
                        if let Err(error) = send_queries(&mut stream, protocol::state_queries(self.model.dialect)).await {
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
                "control port refused: the unit is in deep sleep (network standby off) or another \
                 client holds its control session"
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
            return Err("the unit's control session is not connected".to_string());
        }
        // In standby the unit discards volume and mute writes without a word, and `!SRC(n)`
        // powers it on. Refuse rather than time out, or switch it on by surprise.
        let needs_power = !matches!(
            operation,
            DeviceOperation::PowerOn | DeviceOperation::PowerStandby
        );
        if needs_power && self.state.power() != DevicePower::On {
            return Err(match self.state.power() {
                DevicePower::Standby => format!(
                    "{operation:?} is not supported while the unit is in standby: it discards \
                     volume and mute writes, and a source change would switch it on; power it on first"
                ),
                _ => format!(
                    "{operation:?} is refused until the unit has reported that it is on"
                ),
            });
        }
        let (command, query) = protocol::command(self.model, operation)
            .ok_or_else(|| format!("{operation:?} cannot be encoded for this device"))?;
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
        let power = self.state.power();
        let inputs = self
            .state
            .sources
            .catalog
            .iter()
            .map(|(index, name)| DeviceInput {
                id: index.to_string(),
                name: name.clone(),
                available: true,
            })
            .collect();
        let device = Device {
            device_id: self.device_id.clone(),
            vendor: self.vendor.clone(),
            model: self.model.name.to_ascii_uppercase(),
            name: self.name.clone(),
            host: self.host.clone(),
            discovery: self.discovery.clone(),
            maturity: DEVICE_CONTROL_MATURITY.to_string(),
            connection: DeviceConnection {
                // Replies only (verbosity is not raised): a polled session, not a push one.
                control_channel: if self.state.connected {
                    ControlChannel::Polling
                } else {
                    ControlChannel::None
                },
                detail: self.state.detail.clone(),
            },
            power,
            input: self.state.sources.current.map(|index| index.to_string()),
            inputs,
            volume: DeviceVolume {
                scale: crate::bus::VolumeScale::Decibel,
                value: self.state.volume_db,
                min: protocol::MIN_DB,
                // `!MAXVOL` (the unit's own ceiling) on MP/P, bounded by the model's documented
                // maximum; TDAI reports none, so the documented maximum stands. The user's
                // ceiling only lowers it.
                hardware_max: Some(
                    self.state
                        .max_db
                        .map_or(self.model.max_db, |max| max.min(self.model.max_db)),
                ),
                ceiling: None,
                step: 0.1,
                // In standby the unit answers "muted" whatever was set: not an observation.
                is_muted: if power == DevicePower::Standby {
                    None
                } else {
                    self.state.muted
                },
                display: None,
            },
            capabilities: capabilities_for(self.model.name),
            revision: next_revision(),
            last_updated: now_ms(),
        };
        self.bus.publish(BusEvent::DeviceObserved {
            device: Box::new(device),
        });
    }
}

/// Connect, ask `!DEVICE?`, and return the model the unit names: the unit speaks the control
/// protocol (`discovery.py::discover_model`: the unit does not volunteer its identity).
async fn probe(host: &str, port: u16) -> Option<String> {
    let attempt = async {
        let mut stream = TcpStream::connect((host, port)).await.ok()?;
        stream.write_all(b"!DEVICE?\r").await.ok()?;
        let mut buffer = String::new();
        let mut chunk = [0u8; 256];
        loop {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buffer.push_str(&String::from_utf8_lossy(&chunk[..n]));
            for line in protocol::drain_lines(&mut buffer) {
                if let Some(LyngdorfEvent::Device(model)) = protocol::parse_line(&line) {
                    return Some(model);
                }
            }
        }
    };
    tokio::time::timeout(PROBE_TIMEOUT, attempt).await.ok()?
}

/// Unicast SSDP M-SEARCH to a known host, for the `LOCATION` of its description
/// (`discovery.py::discover_ssdp_location`). Multicast cannot reach a configured host on
/// another subnet.
async fn unicast_location(host: &str, target: &str) -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").await.ok()?;
    let request = format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {target}\r\n\r\n"
    );
    socket
        .send_to(request.as_bytes(), (host, 1900))
        .await
        .ok()?;
    let mut buffer = [0u8; 2048];
    let (n, _) = tokio::time::timeout(Duration::from_secs(3), socket.recv_from(&mut buffer))
        .await
        .ok()?
        .ok()?;
    String::from_utf8_lossy(&buffer[..n])
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case("location")
                .then(|| value.trim().to_string())
        })
}

/// Read from the session if there is one; pends forever otherwise. `read` is cancel-safe, so a
/// partial line survives in the caller's buffer when another `select!` branch wins.
async fn read_chunk(stream: &mut Option<TcpStream>, chunk: &mut [u8]) -> std::io::Result<usize> {
    match stream {
        Some(stream) => stream.read(chunk).await,
        None => std::future::pending().await,
    }
}

/// Write one `!`-prefixed line, then wait out the pacing gap: a real MP-60 silently drops
/// commands past about 16 in flight.
async fn send_line(stream: &mut Option<TcpStream>, line: &str) -> Result<(), String> {
    let socket = stream
        .as_mut()
        .ok_or_else(|| "no control session".to_string())?;
    let bytes = format!("{line}\r");
    tokio::time::timeout(WRITE_TIMEOUT, socket.write_all(bytes.as_bytes()))
        .await
        .map_err(|_| "control write timed out".to_string())?
        .map_err(|error| format!("control write failed: {error}"))?;
    tokio::time::sleep(protocol::COMMAND_PACING).await;
    Ok(())
}

async fn send_queries(stream: &mut Option<TcpStream>, queries: &[&str]) -> Result<(), String> {
    for query in queries {
        send_line(stream, query).await?;
    }
    Ok(())
}
