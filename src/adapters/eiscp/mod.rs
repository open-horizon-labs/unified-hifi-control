//! Onkyo / Integra / eISCP-capable Pioneer receiver adapter (#774). **Alpha.**
//!
//! A *device* adapter: it controls the receiver a zone feeds, not a music provider. It never
//! publishes zones. It publishes [`BusEvent::DeviceObserved`] snapshots and executes
//! [`DeviceOperation`]s the [`crate::devices::service::DeviceService`] hands it, already
//! resolved, clamped and quantised. This adapter never computes a level.
//!
//! Implemented from Onkyo's published ISCP/eISCP documentation (see [`protocol`] for the spec
//! URLs), not from any third-party client. **No hardware has exercised it**: every capability
//! claim is `implementation-derived`.
//!
//! # One owner per unit
//!
//! eISCP allows one client connection per receiver (spec §1.2: "The number of the connections who
//! can connect with a client is one"). Each unit therefore has exactly one task owning its only
//! TCP session; surfaces never open sessions. The cost is that another eISCP controller (the
//! vendor's app, another home-automation system) cannot connect while this adapter holds the
//! slot, which is why the device adapter set is opt-in.
//!
//! # Discovery
//!
//! 1. UDP broadcast of `!xECNQSTN` to port 60128; each receiver answers
//!    `!1ECN<model>/<port>/<region>/<identifier>` (identifier = MAC, the hardware id);
//! 2. SSDP `MediaRenderer` responders whose description names ONKYO / Pioneer / Integra, then a
//!    unicast `ECNQSTN` to that host: SSDP alone does not prove usable eISCP control;
//! 3. manually configured hosts (`FamilyContext::manual`), probed the same way.
//!
//! A unit is published only after its eISCP session has actually answered a query.
//!
//! # Standby
//!
//! A receiver in standby accepts the TCP session only when "Network Standby" is enabled in its
//! setup menu. Otherwise it is unreachable and `power_on` fails with a message saying so; UHC
//! sends no Wake-on-LAN.

pub mod protocol;

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
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

use crate::adapters::devices::{DeviceFamily, FamilyContext};
use crate::bus::{BusEvent, SharedBus};
use crate::devices::links::ManualDevice;
use crate::devices::service::{DeviceDriver, DeviceOperation};
use crate::devices::{
    CapabilityEvidence, ControlChannel, Device, DeviceCapabilities, DeviceCapability,
    DeviceConnection, DeviceId, DevicePower, DeviceVolume, DEVICE_CONTROL_MATURITY,
};
use protocol::{Discovery, Event};

/// Family name used in logs and `ManualDevice::family`.
pub const ADAPTER_NAME: &str = "eiscp";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// The reply to a command covers a possible connect plus two spaced writes.
const REPLY_TIMEOUT: Duration = Duration::from_secs(6);
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
const PROBE_WAIT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_secs(30);
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(60);
const RECONNECT_MIN: Duration = Duration::from_secs(2);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// Spec §1.2: "The Interval Time Receiving the message needs to take more than 50msec."
const COMMAND_SPACING: Duration = Duration::from_millis(60);
/// After an accepted power-off, a session that closes within this window is standby, not a fault.
const STANDBY_GRACE: Duration = Duration::from_secs(8);
const SSDP_TARGETS: &[&str] = &[
    "urn:schemas-upnp-org:device:MediaRenderer:1",
    "urn:schemas-upnp-org:device:MediaRenderer:2",
    "urn:schemas-upnp-org:device:MediaRenderer:3",
];

const NETWORK_STANDBY_NOTE: &str = "a receiver in standby accepts network control only when \
                                    Network Standby is enabled in its setup menu";

/// Capability claims for one model. Pure, so the AGENTS.md device table can be generated.
///
/// The model does not change the claim today (the protocol path is the same for every eISCP
/// receiver); it is part of the signature so a model caveat can be added without a new API.
pub fn capabilities_for(_model: &str) -> DeviceCapabilities {
    let derived = |detail: &str| DeviceCapability {
        supported: true,
        evidence: CapabilityEvidence::ImplementationDerived,
        detail: format!(
            "{detail}; implemented from the Onkyo ISCP spec v1.15, not yet exercised on hardware"
        ),
    };
    DeviceCapabilities {
        observe: derived(
            "PWRQSTN/MVLQSTN/AMTQSTN/SLIQSTN over the eISCP TCP session on port 60128, with push \
             status from the receiver",
        ),
        volume: derived(
            "MVL in the receiver's native hexadecimal units; maximum is the spec's 100 (0x64), and \
             models that stop at 80 clamp a higher write themselves, which the readback reports",
        ),
        mute: derived("AMT01/AMT00 over eISCP"),
        power: derived(&format!(
            "PWR01/PWR00 over eISCP; {NETWORK_STANDBY_NOTE}, otherwise power-on is unreachable"
        )),
        input_select: derived(
            "SLI with the spec's input codes; the receiver offers no input catalog, so models \
             refuse the codes they lack",
        ),
    }
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
    /// SSDP `LOCATION`s already examined and found not to be eISCP receivers.
    rejected: HashSet<String>,
}

#[derive(Clone)]
pub struct EiscpAdapter {
    bus: SharedBus,
    http: Client,
    manual: Arc<RwLock<Vec<ManualDevice>>>,
    state: Arc<RwLock<AdapterState>>,
}

/// A receiver found by any discovery path, before a session exists.
struct Found {
    host: String,
    /// TCP control port.
    port: u16,
    discovery: Discovery,
    name: String,
    via: &'static str,
}

impl EiscpAdapter {
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
        tracing::info!("eISCP (Onkyo/Integra/Pioneer) device adapter started (alpha)");
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
        tracing::info!("eISCP device adapter stopped");
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
        let mut found: Vec<Found> = Vec::new();

        // 1. UDP broadcast.
        let broadcast = SocketAddr::from(([255, 255, 255, 255], protocol::DEFAULT_PORT));
        for (address, discovery) in udp_probe(broadcast, true, PROBE_WAIT).await {
            found.push(Found {
                host: address.to_string(),
                port: discovery.port,
                name: discovery.model.clone(),
                discovery,
                via: "eiscp-udp",
            });
        }

        // 2. SSDP, then a unicast probe: SSDP alone does not prove eISCP control.
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
            let Some(description) = description.filter(|d| d.is_eiscp_vendor()) else {
                self.state.write().await.rejected.insert(location);
                continue;
            };
            let Ok(ip) = host.parse::<IpAddr>() else {
                continue;
            };
            let target = SocketAddr::new(ip, protocol::DEFAULT_PORT);
            if let Some((_, discovery)) = udp_probe(target, false, PROBE_WAIT)
                .await
                .into_iter()
                .next()
            {
                found.push(Found {
                    host,
                    port: discovery.port,
                    name: if description.friendly_name.is_empty() {
                        discovery.model.clone()
                    } else {
                        description.friendly_name
                    },
                    discovery,
                    via: "ssdp",
                });
            }
        }

        // 3. Manual hosts.
        for manual in self.manual.read().await.clone() {
            if cancel.is_cancelled() {
                return;
            }
            if let Some(entry) = self.probe_manual(&manual).await {
                found.push(entry);
            }
        }

        for entry in found {
            if cancel.is_cancelled() {
                return;
            }
            self.ensure_unit(entry, cancel).await;
        }
    }

    async fn ssdp_locations(&self) -> Vec<String> {
        let mut locations = Vec::new();
        for target in SSDP_TARGETS {
            let Ok(urn) = target.parse::<URN>() else {
                continue;
            };
            match ssdp_client::search(&SearchTarget::URN(urn), Duration::from_secs(2), 2, None)
                .await
            {
                Ok(responses) => {
                    futures::pin_mut!(responses);
                    while let Some(response) = responses.next().await {
                        if let Ok(response) = response {
                            locations.push(response.location().to_string());
                        }
                    }
                }
                Err(error) => tracing::debug!("eISCP SSDP search failed: {error}"),
            }
        }
        locations.sort();
        locations.dedup();
        locations
    }

    async fn probe_manual(&self, manual: &ManualDevice) -> Option<Found> {
        let port = manual.port.unwrap_or(protocol::DEFAULT_PORT);
        let target = tokio::net::lookup_host((manual.host.as_str(), port))
            .await
            .ok()?
            .find(SocketAddr::is_ipv4)?;
        if let Some((_, discovery)) = udp_probe(target, false, PROBE_WAIT)
            .await
            .into_iter()
            .next()
        {
            return Some(Found {
                host: manual.host.clone(),
                port: manual.port.unwrap_or(discovery.port),
                name: discovery.model.clone(),
                discovery,
                via: "manual",
            });
        }
        // No ECN answer: a configured MAC still names the unit, and the TCP session decides
        // whether it is really there.
        let mac = manual.mac.as_deref()?;
        let identifier: String = mac.chars().filter(char::is_ascii_alphanumeric).collect();
        if identifier.len() != 12 {
            tracing::warn!(host = %manual.host, "eISCP manual device has no ECN answer and an unusable mac");
            return None;
        }
        Some(Found {
            host: manual.host.clone(),
            port,
            name: manual.host.clone(),
            discovery: Discovery {
                model: "eISCP receiver".to_string(),
                port,
                region: "XX".to_string(),
                identifier: identifier.to_ascii_uppercase(),
            },
            via: "manual",
        })
    }

    async fn ensure_unit(&self, found: Found, adapter_cancel: &CancellationToken) {
        let Some(device_id) = DeviceId::new("onkyo", &found.discovery.identifier) else {
            return;
        };
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
            Some((host, commands)) if host != found.host => {
                // Follow an IP change.
                if commands
                    .try_send(UnitCommand::Rehost(found.host.clone()))
                    .is_ok()
                {
                    if let Some(unit) = self.state.write().await.units.get_mut(device_id.as_str()) {
                        unit.host = found.host;
                    }
                }
            }
            Some(_) => {}
            None => {
                self.spawn_unit(
                    &device_id,
                    &found.host,
                    found.port,
                    found.discovery.vendor(),
                    &found.discovery.model,
                    &found.name,
                    if found.via == "manual" {
                        "manual"
                    } else {
                        found.via
                    },
                    adapter_cancel.child_token(),
                )
                .await;
            }
        }
    }

    /// Attach a unit at a known address without discovery: a configured host, or a mock
    /// receiver on an ephemeral port in tests. The unit runs until the adapter stops.
    pub async fn attach_unit(
        &self,
        device_id: &DeviceId,
        host: &str,
        port: u16,
        vendor: &str,
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
        self.spawn_unit(device_id, host, port, vendor, model, name, "manual", cancel)
            .await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn spawn_unit(
        &self,
        device_id: &DeviceId,
        host: &str,
        port: u16,
        vendor: &str,
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
        tracing::info!(%device_id, host, model, name, "Discovered eISCP receiver");
        let unit = Unit {
            bus: self.bus.clone(),
            host: host.to_string(),
            port,
            device_id: device_id.clone(),
            vendor: vendor.to_string(),
            model: model.to_string(),
            name: name.to_string(),
            discovery: discovery.to_string(),
            state: UnitState::default(),
            validated: false,
            standby_deadline: None,
        };
        tokio::spawn(unit.run(receiver, cancel));
    }

    pub async fn device_count(&self) -> usize {
        self.state.read().await.units.len()
    }
}

/// Send one `ECNQSTN` and collect the answers for `wait`. Broadcast or unicast.
async fn udp_probe(
    target: SocketAddr,
    broadcast: bool,
    wait: Duration,
) -> Vec<(IpAddr, Discovery)> {
    let Ok(socket) = UdpSocket::bind("0.0.0.0:0").await else {
        return Vec::new();
    };
    if broadcast && socket.set_broadcast(true).is_err() {
        return Vec::new();
    }
    if let Err(error) = socket.send_to(&protocol::discovery_request(), target).await {
        tracing::debug!(%target, "eISCP discovery send failed: {error}");
        return Vec::new();
    }
    let mut found: Vec<(IpAddr, Discovery)> = Vec::new();
    let deadline = Instant::now() + wait;
    let mut buffer = [0u8; 2048];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, socket.recv_from(&mut buffer)).await {
            Ok(Ok((length, from))) => {
                if let Some(discovery) = protocol::parse_discovery_response(&buffer[..length]) {
                    if !found
                        .iter()
                        .any(|(_, d)| d.identifier == discovery.identifier)
                    {
                        found.push((from.ip(), discovery));
                    }
                    if !broadcast {
                        break;
                    }
                }
            }
            _ => break,
        }
    }
    found
}

#[async_trait]
impl DeviceFamily for EiscpAdapter {
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
impl DeviceDriver for EiscpAdapter {
    fn kinds(&self) -> &'static [&'static str] {
        &["onkyo"]
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
    /// Power is `standby` by inference (a session closed right after an accepted power-off).
    inferred_standby: bool,
    detail: Option<String>,
}

impl UnitState {
    fn power(&self) -> DevicePower {
        self.power
            .map(protocol::power_of)
            .unwrap_or(DevicePower::Unknown)
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::Power(on) => {
                self.power = Some(on);
                self.inferred_standby = false;
            }
            Event::Mute(muted) => self.muted = Some(muted),
            Event::Volume(level) => self.volume = Some(level),
            Event::Input(code) => self.input = Some(code),
        }
    }
}

struct Unit {
    bus: SharedBus,
    host: String,
    port: u16,
    device_id: DeviceId,
    vendor: String,
    model: String,
    name: String,
    discovery: String,
    state: UnitState,
    /// True once the session has answered a query; nothing is published before.
    validated: bool,
    standby_deadline: Option<Instant>,
}

impl Unit {
    async fn run(mut self, mut commands: mpsc::Receiver<UnitCommand>, cancel: CancellationToken) {
        let mut session: Option<TcpStream> = None;
        let mut buffer: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 2048];
        let mut backoff = RECONNECT_MIN;
        let mut next_connect = Instant::now();
        let mut poll = tokio::time::interval(POLL_INTERVAL);
        // The first tick fires at once; the connect branch already queries.
        poll.tick().await;

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
                            buffer.extend_from_slice(&chunk[..n]);
                            match self.drain(&mut buffer) {
                                Ok(changed) => {
                                    if changed {
                                        self.publish();
                                    }
                                }
                                Err(error) => {
                                    tracing::warn!(device_id = %self.device_id, "dropping eISCP session: {error}");
                                    session = None;
                                    buffer.clear();
                                    self.on_session_lost();
                                    next_connect = Instant::now() + backoff;
                                    self.publish();
                                }
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
                _ = poll.tick(), if session.is_some() => {
                    // Queries only; a failed write also reveals a dead socket.
                    if let Err(error) = send_queries(&mut session).await {
                        tracing::debug!(device_id = %self.device_id, "poll failed: {error}");
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

    /// Parse every complete frame in `buffer`. `Ok(true)` when state changed.
    fn drain(&mut self, buffer: &mut Vec<u8>) -> Result<bool, protocol::FrameError> {
        let mut changed = false;
        while let Some(data) = protocol::next_frame(buffer)? {
            let Some(('1', message)) = protocol::split_message(&data) else {
                continue;
            };
            if let Some(event) = protocol::parse_message(&message) {
                self.state.apply(event);
                self.validated = true;
                changed = true;
            }
        }
        Ok(changed)
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
                return Err(format!(
                    "connection refused: the receiver is off with network control disabled, or \
                     another eISCP client holds its only session ({NETWORK_STANDBY_NOTE})"
                ))
            }
            Ok(Err(error)) => return Err(format!("connection failed: {error}")),
            Err(_) => return Err("connection timed out".to_string()),
        };
        tracing::info!(device_id = %self.device_id, host = %self.host, "eISCP session open");
        self.state.connected = true;
        self.state.detail = None;
        self.standby_deadline = None;
        let mut session = Some(stream);
        // Queries only: nothing on connect changes the receiver's state.
        if let Err(error) = send_queries(&mut session).await {
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
            self.state.detail = Some("eISCP session closed; reconnecting".to_string());
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
        let (command, query) = protocol::encode_operation(operation)?;
        if session.is_none() {
            match self.open_session().await {
                Ok(stream) => {
                    *session = Some(stream);
                    buffer.clear();
                }
                Err(detail) => {
                    self.on_unreachable(detail.clone());
                    self.publish();
                    let power_hint = if matches!(operation, DeviceOperation::PowerOn) {
                        " Power-on needs the receiver reachable in standby; UHC sends no \
                         Wake-on-LAN."
                    } else {
                        ""
                    };
                    return Err(format!(
                        "{} at {}:{} is not reachable ({detail}).{power_hint}",
                        self.model, self.host, self.port
                    ));
                }
            }
        }
        if let Err(error) = send_message(session, &command).await {
            *session = None;
            self.on_session_lost();
            self.publish();
            return Err(error);
        }
        if matches!(operation, DeviceOperation::PowerStandby) {
            self.standby_deadline = Some(Instant::now() + STANDBY_GRACE);
        }
        tokio::time::sleep(COMMAND_SPACING).await;
        let readback = if matches!(operation, DeviceOperation::PowerOn) {
            // Powering on: re-read everything the receiver could not answer in standby.
            send_queries(session).await
        } else {
            send_message(session, query).await
        };
        if let Err(error) = readback {
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
        let device = Device {
            device_id: self.device_id.clone(),
            vendor: self.vendor.clone(),
            model: self.model.clone(),
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
            power: self.state.power(),
            input: self.state.input.clone(),
            inputs: protocol::input_catalog(self.state.input.as_deref()),
            volume: DeviceVolume {
                scale: crate::bus::VolumeScale::Percentage,
                value: self.state.volume.map(f32::from),
                min: 0.0,
                // The spec's full scale (MVL 00-64 hex). Some models stop at 80 and clamp a higher
                // write themselves; the confirming readback then reports the level they applied.
                hardware_max: Some(f32::from(protocol::MAX_PROTOCOL_VOLUME)),
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

async fn send_message(session: &mut Option<TcpStream>, message: &str) -> Result<(), String> {
    let stream = session
        .as_mut()
        .ok_or_else(|| "no eISCP session".to_string())?;
    let frame = protocol::encode_frame('1', message);
    tokio::time::timeout(WRITE_TIMEOUT, stream.write_all(&frame))
        .await
        .map_err(|_| "eISCP write timed out".to_string())?
        .map_err(|error| format!("eISCP write failed: {error}"))
}

async fn send_queries(session: &mut Option<TcpStream>) -> Result<(), String> {
    for query in protocol::STATE_QUERIES {
        send_message(session, query).await?;
        tokio::time::sleep(COMMAND_SPACING).await;
    }
    Ok(())
}
