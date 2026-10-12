//! Denon / Marantz AVR device adapter (#781, #784). **Alpha.**
//!
//! A *device* adapter: it controls the amplifier a zone feeds, not a music provider. It never
//! publishes zones. It publishes [`BusEvent::DeviceObserved`] snapshots, and executes
//! [`DeviceOperation`]s handed to it by [`crate::devices::service::DeviceService`] — which has
//! already resolved, clamped and quantised every level. This adapter never computes a level.
//!
//! # One owner per unit
//!
//! Each unit gets exactly one task, which owns the unit's only Telnet session (TCP 23 allows a
//! single client on these receivers) and its HTTP polling. Surfaces never open sessions.
//!
//! # What the hardware showed (`tests/fixtures/devices/marantz/sr6008/README.md`)
//!
//! - Identity is the description's `serialNumber` (the MAC); two identical SR6008s differ only there.
//! - Telnet carries push feedback and is the only source of the unit's maximum (`MVMAX`); without
//!   it, volume writes are refused upstream.
//! - In standby the SR6008 refuses Telnet but answers HTTP, so power is read over HTTP while in
//!   standby and power-on is sent over HTTP.
//!
//! # Coexistence
//!
//! Holding the Telnet slot means another Telnet client (an older control app, another home
//! automation system) cannot connect while this adapter runs. That is the cost of push feedback,
//! and the reason the adapter is opt-in and off by default.

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
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::bus::{BusEvent, SharedBus};
use crate::devices::service::{DeviceDriver, DeviceOperation};
use crate::devices::{
    CapabilityEvidence, ControlChannel, Device, DeviceCapabilities, DeviceCapability,
    DeviceConnection, DeviceId, DeviceInput, DevicePower, DeviceVolume, DEVICE_CONTROL_MATURITY,
};
use protocol::{Description, TelnetEvent};

/// Settings/coordinator name.
pub const ADAPTER_NAME: &str = "denonavr";

/// Models whose **read** path has been exercised on real hardware, and the fixture directory
/// holding the captures. `tests/device_claims.rs` fails if a listed directory is missing, so a
/// `hardware-verified` claim cannot exist without its evidence.
pub const READ_VERIFIED_MODELS: &[(&str, &str)] =
    &[("SR6008", "tests/fixtures/devices/marantz/sr6008")];

/// Models whose **write** path (volume, mute, power, input) has been exercised on real hardware.
/// Empty until a write capture lands (#788): every write claim is implementation-derived.
pub const WRITE_VERIFIED_MODELS: &[(&str, &str)] = &[];

/// Capability claims for one model. Pure, so the AGENTS.md device table is generated from it.
///
/// `volume_range_known`: the push session is up and has reported `MVMAX`. Without it, volume
/// writes are refused, and the claim says so rather than advertising a path that will refuse.
pub fn capabilities_for(model: &str, volume_range_known: bool) -> DeviceCapabilities {
    let read_verified = READ_VERIFIED_MODELS.iter().any(|(m, _)| *m == model);
    let write_verified = WRITE_VERIFIED_MODELS.iter().any(|(m, _)| *m == model);
    let write_evidence = if write_verified {
        CapabilityEvidence::HardwareVerified
    } else {
        CapabilityEvidence::ImplementationDerived
    };
    let pending = if write_verified {
        ""
    } else {
        "; pending on-device verification (#788)"
    };
    let write = |supported: bool, detail: String| DeviceCapability {
        supported,
        evidence: write_evidence,
        detail,
    };
    DeviceCapabilities {
        observe: DeviceCapability {
            supported: true,
            evidence: if read_verified {
                CapabilityEvidence::HardwareVerified
            } else {
                CapabilityEvidence::ImplementationDerived
            },
            detail: if read_verified {
                "power, level, maximum, mute, input and input catalog reads captured on this \
                 model (tests/fixtures/devices/marantz/sr6008)"
                    .to_string()
            } else {
                "read path implemented from the Denon/Marantz protocol; not exercised on this model"
                    .to_string()
            },
        },
        volume: write(
            volume_range_known,
            if volume_range_known {
                format!("MV over Telnet, clamped to the unit's MVMAX and the user ceiling{pending}")
            } else {
                "refused until the unit reports its maximum (MVMAX), which it sends only over \
                 Telnet"
                    .to_string()
            },
        ),
        mute: write(
            true,
            format!("MU over Telnet, or formiPhoneAppMute over HTTP{pending}"),
        ),
        power: write(
            true,
            format!(
                "ZMON/ZMOFF over Telnet, or formiPhoneAppPower over HTTP (the SR6008 refuses \
                 Telnet in standby){pending}"
            ),
        ),
        input_select: write(
            true,
            format!("SI over Telnet, or formiPhoneAppDirect over HTTP{pending}"),
        ),
    }
}

/// Production ports. Tests attach units on ephemeral ports through [`DenonAvrAdapter::attach_unit`].
pub const TELNET_PORT: u16 = 23;
pub const HTTP_PORT: u16 = 80;
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// With a push session up, HTTP is only needed for the input catalog.
const CATALOG_REFRESH_TICKS: u32 = 12;
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(60);
const RECONNECT_MIN: Duration = Duration::from_secs(2);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// Gap between a command and its confirming query; the receivers drop back-to-back lines.
const COMMAND_SPACING: Duration = Duration::from_millis(80);
/// After an HTTP write, read back on this schedule so the confirmation sees the applied state.
const HTTP_READBACK_SCHEDULE: &[Duration] = &[
    Duration::from_millis(300),
    Duration::from_millis(700),
    Duration::from_millis(1200),
];
const DESCRIPTION_PORTS: &[u16] = &[8080, 80];
const SSDP_TARGETS: &[&str] = &[
    "urn:schemas-upnp-org:device:MediaRenderer:1",
    "urn:schemas-denon-com:device:AiosDevice:1",
];

/// Process-wide revision counter seeded from the clock, so revisions stay monotonic across an
/// adapter restart and a command never mistakes a restarted unit's first readback for an old one.
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
    cancel: CancellationToken,
}

#[derive(Default)]
struct AdapterState {
    running: bool,
    cancel: CancellationToken,
    units: HashMap<String, UnitHandle>,
    /// SSDP `LOCATION`s already examined, with the device they named (or `None`).
    examined: HashMap<String, Option<String>>,
}

#[derive(Clone)]
pub struct DenonAvrAdapter {
    bus: SharedBus,
    http: Client,
    manual_hosts: Arc<RwLock<Vec<String>>>,
    state: Arc<RwLock<AdapterState>>,
}

impl DenonAvrAdapter {
    pub fn new(bus: SharedBus) -> Self {
        Self {
            bus,
            http: Client::builder()
                .timeout(HTTP_TIMEOUT)
                .build()
                .unwrap_or_default(),
            manual_hosts: Arc::new(RwLock::new(Vec::new())),
            state: Arc::new(RwLock::new(AdapterState::default())),
        }
    }

    /// Hosts to probe directly, for units multicast cannot reach.
    pub async fn set_manual_hosts(&self, hosts: Vec<String>) {
        *self.manual_hosts.write().await = hosts;
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
        tracing::info!("Denon/Marantz device adapter started (alpha)");
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
        tracing::info!("Denon/Marantz device adapter stopped");
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
                Err(error) => tracing::debug!("Denon/Marantz SSDP search failed: {error}"),
            }
        }
        for host in self.manual_hosts.read().await.clone() {
            for port in DESCRIPTION_PORTS {
                locations.push(format!("http://{host}:{port}/description.xml"));
            }
        }
        locations.sort();
        locations.dedup();

        for location in locations {
            if cancel.is_cancelled() {
                return;
            }
            let Some(host) = reqwest::Url::parse(&location)
                .ok()
                .and_then(|url| url.host_str().map(str::to_string))
            else {
                continue;
            };
            let known = self.state.read().await.examined.get(&location).cloned();
            let device_id = match known {
                Some(Some(device_id)) => DeviceId::parse(&device_id),
                Some(None) => continue,
                None => {
                    let identified = self.identify(&location).await;
                    let id = identified.as_ref().map(|(id, _)| id.to_string());
                    self.state
                        .write()
                        .await
                        .examined
                        .insert(location.clone(), id);
                    match identified {
                        Some((device_id, description)) => {
                            let discovery = if self.manual_hosts.read().await.contains(&host) {
                                "manual"
                            } else {
                                "ssdp"
                            };
                            self.ensure_unit(&device_id, &host, description, discovery, cancel)
                                .await;
                            continue;
                        }
                        None => continue,
                    }
                }
            };
            // Known unit: follow an IP change without re-reading the description.
            if let Some(device_id) = device_id {
                let rehost = {
                    let state = self.state.read().await;
                    state
                        .units
                        .get(device_id.as_str())
                        .filter(|unit| unit.host != host)
                        .map(|unit| unit.commands.clone())
                };
                if let Some(commands) = rehost {
                    if let Err(error) = commands.try_send(UnitCommand::Rehost(host.clone())) {
                        tracing::debug!(%device_id, "could not re-host unit: {error}");
                    } else if let Some(unit) =
                        self.state.write().await.units.get_mut(device_id.as_str())
                    {
                        unit.host = host;
                    }
                }
            }
        }
    }

    async fn identify(&self, location: &str) -> Option<(DeviceId, Description)> {
        let body = self
            .http
            .get(location)
            .send()
            .await
            .ok()?
            .text()
            .await
            .ok()?;
        let description = protocol::parse_description(&body)?;
        let kind = description.kind()?;
        let device_id = DeviceId::new(kind, &description.hardware_id()?)?;
        Some((device_id, description))
    }

    async fn ensure_unit(
        &self,
        device_id: &DeviceId,
        host: &str,
        description: Description,
        discovery: &str,
        adapter_cancel: &CancellationToken,
    ) {
        if !self.state.read().await.running {
            return;
        }
        self.spawn_unit(
            device_id,
            host,
            (TELNET_PORT, HTTP_PORT),
            description,
            discovery,
            adapter_cancel.child_token(),
        )
        .await;
    }

    /// Attach a unit at a known address without multicast discovery: a configured host on
    /// another subnet, or a mock receiver on ephemeral ports in tests. The unit runs until the
    /// adapter stops.
    pub async fn attach_unit(
        &self,
        device_id: &DeviceId,
        host: &str,
        ports: (u16, u16),
        description: Description,
    ) {
        let cancel = {
            let mut state = self.state.write().await;
            state.running = true;
            // After `stop_family` the adapter token is cancelled; a unit attached later needs a
            // live one, or it would stop the moment it starts.
            if state.cancel.is_cancelled() {
                state.cancel = CancellationToken::new();
            }
            state.cancel.child_token()
        };
        self.spawn_unit(device_id, host, ports, description, "manual", cancel)
            .await;
    }

    async fn spawn_unit(
        &self,
        device_id: &DeviceId,
        host: &str,
        (telnet_port, http_port): (u16, u16),
        description: Description,
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
                cancel: cancel.clone(),
            },
        );
        drop(state);
        tracing::info!(
            %device_id,
            host,
            model = %description.model,
            name = %description.friendly_name,
            "Discovered Denon/Marantz device"
        );
        let unit = Unit {
            bus: self.bus.clone(),
            http: self.http.clone(),
            host: host.to_string(),
            telnet_port,
            http_port,
            device_id: device_id.clone(),
            vendor: description.manufacturer,
            model: description.model,
            name: description.friendly_name,
            discovery: discovery.to_string(),
            state: UnitState::default(),
        };
        tokio::spawn(unit.run(receiver, cancel));
    }

    pub async fn device_count(&self) -> usize {
        self.state.read().await.units.len()
    }
}

#[async_trait]
impl crate::adapters::devices::DeviceFamily for DenonAvrAdapter {
    fn family(&self) -> &'static str {
        ADAPTER_NAME
    }

    async fn start_family(
        &self,
        context: crate::adapters::devices::FamilyContext,
    ) -> anyhow::Result<()> {
        let hosts: Vec<String> = context
            .manual
            .into_iter()
            .map(|device| device.host)
            .collect();
        if !hosts.is_empty() {
            self.set_manual_hosts(hosts).await;
        }
        self.start_internal().await
    }

    async fn stop_family(&self) {
        self.stop_internal().await
    }
}

#[async_trait]
impl DeviceDriver for DenonAvrAdapter {
    fn kinds(&self) -> &'static [&'static str] {
        &["denon", "marantz"]
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
        tokio::time::timeout(WRITE_TIMEOUT * 2, response)
            .await
            .map_err(|_| "the unit did not accept the command in time".to_string())?
            .map_err(|_| "the unit's session stopped before sending".to_string())?
    }
}

/// Everything the unit has reported, before it becomes a [`Device`].
#[derive(Default)]
struct UnitState {
    unit_power: Option<bool>,
    zone_power: Option<bool>,
    volume_db: Option<f32>,
    max_db: Option<f32>,
    muted: Option<bool>,
    input: Option<String>,
    inputs: Vec<DeviceInput>,
    display: Option<String>,
    channel: ControlChannel,
    detail: Option<String>,
}

impl UnitState {
    fn power(&self) -> DevicePower {
        if self.unit_power == Some(false) {
            return DevicePower::Standby;
        }
        match self.zone_power {
            Some(true) => DevicePower::On,
            Some(false) => DevicePower::Standby,
            None if self.unit_power == Some(true) => DevicePower::On,
            None => DevicePower::Unknown,
        }
    }

    fn apply_telnet(&mut self, event: TelnetEvent) {
        match event {
            TelnetEvent::UnitPower(on) => self.unit_power = Some(on),
            TelnetEvent::MainZonePower(on) => self.zone_power = Some(on),
            TelnetEvent::VolumeDb(db) => self.volume_db = Some(db),
            TelnetEvent::VolumeMaxDb(db) => self.max_db = Some(db),
            TelnetEvent::Mute(muted) => self.muted = Some(muted),
            TelnetEvent::Input(code) => self.input = Some(code),
        }
    }

    fn apply_status(&mut self, status: protocol::MainZoneStatus) {
        if let Some(power) = status.power {
            let on = power == DevicePower::On;
            self.zone_power = Some(on);
            if on {
                self.unit_power = Some(true);
            }
        }
        if status.volume_db.is_some() {
            self.volume_db = status.volume_db;
        }
        if status.muted.is_some() {
            self.muted = status.muted;
        }
        if status.input.is_some() {
            self.input = status.input;
        }
        if status.display.is_some() {
            self.display = status.display;
        }
        if !status.inputs.is_empty() {
            // In standby the unit blanks its user renames; keep the names it reported while on.
            let standby = status.power == Some(DevicePower::Standby);
            let previous = std::mem::take(&mut self.inputs);
            self.inputs = status
                .inputs
                .into_iter()
                .map(|mut input| {
                    if standby {
                        if let Some(known) = previous.iter().find(|p| p.id == input.id) {
                            input.name = known.name.clone();
                        }
                    }
                    input
                })
                .collect();
        }
    }
}

struct Unit {
    bus: SharedBus,
    http: Client,
    host: String,
    telnet_port: u16,
    http_port: u16,
    device_id: DeviceId,
    vendor: String,
    model: String,
    name: String,
    discovery: String,
    state: UnitState,
}

impl Unit {
    async fn run(mut self, mut commands: mpsc::Receiver<UnitCommand>, cancel: CancellationToken) {
        let mut telnet: Option<TcpStream> = None;
        let mut buffer = String::new();
        let mut chunk = [0u8; 1024];
        let mut backoff = RECONNECT_MIN;
        let mut next_connect = Instant::now();
        let mut poll = tokio::time::interval(POLL_INTERVAL);
        let mut ticks: u32 = 0;

        // The first snapshot already says how the unit is reachable: HTTP answering means at
        // least polling, until the push session (attempted immediately below) upgrades it.
        if self.refresh_over_http().await {
            self.state.channel = ControlChannel::Polling;
        }
        self.publish();

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                command = commands.recv() => {
                    let Some(command) = command else { break };
                    match command {
                        UnitCommand::Rehost(host) => {
                            tracing::info!(device_id = %self.device_id, %host, "Device moved to a new address");
                            self.host = host;
                            telnet = None;
                            next_connect = Instant::now();
                        }
                        UnitCommand::Execute { operation, reply } => {
                            let used_http = telnet.is_none();
                            let result = self.execute(&mut telnet, &operation).await;
                            let sent = result.is_ok();
                            if reply.send(result).is_err() {
                                tracing::debug!(device_id = %self.device_id, "command caller went away before the reply");
                            }
                            if sent && used_http {
                                // No push session: read back on a schedule so the waiting command
                                // sees the applied state, and look for the session a power-on opens.
                                for delay in HTTP_READBACK_SCHEDULE {
                                    tokio::time::sleep(*delay).await;
                                    self.refresh_over_http().await;
                                    self.publish();
                                }
                                next_connect = Instant::now();
                            }
                        }
                    }
                }
                read = read_chunk(&mut telnet, &mut chunk), if telnet.is_some() => {
                    match read {
                        Ok(0) | Err(_) => {
                            tracing::info!(device_id = %self.device_id, "Telnet session closed; polling over HTTP");
                            telnet = None;
                            buffer.clear();
                            self.state.channel = ControlChannel::Polling;
                            self.state.detail = Some("push session closed; reconnecting".to_string());
                            next_connect = Instant::now() + backoff;
                            self.publish();
                        }
                        Ok(n) => {
                            buffer.push_str(&String::from_utf8_lossy(&chunk[..n]));
                            let mut changed = false;
                            for line in protocol::drain_lines(&mut buffer) {
                                if let Some(event) = protocol::parse_line(&line) {
                                    self.state.apply_telnet(event);
                                    changed = true;
                                } else if protocol::is_unparsed_modeled_line(&line) {
                                    crate::devices::anomalies::report(
                                        &self.bus,
                                        crate::devices::anomalies::DeviceAnomaly::new(
                                            self.device_id.clone(),
                                            crate::devices::anomalies::AnomalyKind::UnparsedLine,
                                            format!("{}: Telnet line not understood", self.model),
                                        )
                                        .with_excerpt(line.clone()),
                                    );
                                }
                            }
                            if changed {
                                self.publish();
                            }
                        }
                    }
                }
                _ = tokio::time::sleep_until(next_connect), if telnet.is_none() => {
                    match self.connect().await {
                        Ok(stream) => {
                            tracing::info!(device_id = %self.device_id, host = %self.host, "Telnet push session open");
                            telnet = Some(stream);
                            backoff = RECONNECT_MIN;
                            self.state.channel = ControlChannel::Push;
                            self.state.detail = None;
                            if let Err(error) = send_queries(&mut telnet, protocol::STATE_QUERIES).await {
                                tracing::debug!(device_id = %self.device_id, "state queries failed: {error}");
                                telnet = None;
                            }
                            // The catalog's renames are only complete while the unit is on.
                            self.refresh_over_http().await;
                            self.publish();
                        }
                        Err(detail) => {
                            let reachable = self.refresh_over_http().await;
                            self.state.channel = if reachable { ControlChannel::Polling } else { ControlChannel::None };
                            self.state.detail = Some(detail);
                            backoff = (backoff * 2).min(RECONNECT_MAX);
                            next_connect = Instant::now() + backoff;
                            self.publish();
                        }
                    }
                }
                _ = poll.tick() => {
                    ticks = ticks.wrapping_add(1);
                    if telnet.is_none() || ticks.is_multiple_of(CATALOG_REFRESH_TICKS) {
                        let reachable = self.refresh_over_http().await;
                        if telnet.is_none() {
                            self.state.channel = if reachable { ControlChannel::Polling } else { ControlChannel::None };
                            if self.state.power() == DevicePower::On {
                                // The unit came on (from its own remote): try the push session now.
                                next_connect = next_connect.min(Instant::now());
                            }
                        }
                        self.publish();
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
            TcpStream::connect((self.host.as_str(), self.telnet_port)),
        )
        .await
        {
            Ok(Ok(stream)) => Ok(stream),
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::ConnectionRefused => Err(
                "Telnet refused: the unit is in standby or another client holds its only control \
                 session"
                    .to_string(),
            ),
            Ok(Err(error)) => Err(format!("Telnet unavailable: {error}")),
            Err(_) => Err("Telnet connect timed out".to_string()),
        }
    }

    async fn execute(
        &mut self,
        telnet: &mut Option<TcpStream>,
        operation: &DeviceOperation,
    ) -> Result<(), String> {
        if telnet.is_some() {
            let (command, query) = protocol::telnet_command(operation)
                .ok_or_else(|| format!("{operation:?} cannot be encoded for this device"))?;
            let sent = send_line(telnet, &command).await;
            if let Err(error) = sent {
                *telnet = None;
                return Err(error);
            }
            tokio::time::sleep(COMMAND_SPACING).await;
            if let Err(error) = send_line(telnet, query).await {
                *telnet = None;
                return Err(error);
            }
            return Ok(());
        }
        let path = protocol::http_command_path(operation)
            .ok_or_else(|| format!("{operation:?} cannot be encoded for this device"))?;
        let url = format!("http://{}:{}{}", self.host, self.http_port, path);
        let response = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|error| format!("HTTP command failed: {error}"))?;
        if !response.status().is_success() {
            return Err(format!("HTTP command returned {}", response.status()));
        }
        Ok(())
    }

    /// Read the main-zone status document. Returns whether the unit answered.
    async fn refresh_over_http(&mut self) -> bool {
        let url = format!(
            "http://{}:{}{}",
            self.host,
            self.http_port,
            protocol::MAIN_ZONE_STATUS_PATH
        );
        let body = match self.http.get(&url).send().await {
            Ok(response) if response.status().is_success() => response.bytes().await.ok(),
            _ => None,
        };
        match body {
            Some(bytes) => {
                let xml = String::from_utf8_lossy(&bytes);
                self.state
                    .apply_status(protocol::parse_main_zone_status(&xml));
                true
            }
            None => false,
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        capabilities_for(
            &self.model,
            self.state.channel == ControlChannel::Push && self.state.max_db.is_some(),
        )
    }

    fn publish(&self) {
        let device = Device {
            device_id: self.device_id.clone(),
            vendor: self.vendor.clone(),
            model: self.model.clone(),
            name: self.name.clone(),
            host: self.host.clone(),
            discovery: self.discovery.clone(),
            maturity: DEVICE_CONTROL_MATURITY.to_string(),
            connection: DeviceConnection {
                control_channel: self.state.channel,
                detail: self.state.detail.clone(),
            },
            power: self.state.power(),
            input: self.state.input.clone(),
            inputs: self.state.inputs.clone(),
            volume: DeviceVolume {
                scale: crate::bus::VolumeScale::Decibel,
                value: self.state.volume_db,
                min: protocol::MIN_DB,
                hardware_max: self.state.max_db,
                ceiling: None,
                step: 0.5,
                is_muted: self.state.muted,
                display: self.state.display.clone(),
            },
            capabilities: self.capabilities(),
            revision: next_revision(),
            last_updated: now_ms(),
        };
        self.bus.publish(BusEvent::DeviceObserved {
            device: Box::new(device),
        });
    }
}

/// Read from the session if there is one; pends forever otherwise. `read` is cancel-safe, so a
/// partial line survives in the caller's buffer when another `select!` branch wins.
async fn read_chunk(telnet: &mut Option<TcpStream>, chunk: &mut [u8]) -> std::io::Result<usize> {
    match telnet {
        Some(stream) => stream.read(chunk).await,
        None => std::future::pending().await,
    }
}

async fn send_line(telnet: &mut Option<TcpStream>, line: &str) -> Result<(), String> {
    let stream = telnet
        .as_mut()
        .ok_or_else(|| "no Telnet session".to_string())?;
    let bytes = format!("{line}\r");
    tokio::time::timeout(WRITE_TIMEOUT, stream.write_all(bytes.as_bytes()))
        .await
        .map_err(|_| "Telnet write timed out".to_string())?
        .map_err(|error| format!("Telnet write failed: {error}"))
}

async fn send_queries(telnet: &mut Option<TcpStream>, queries: &[&str]) -> Result<(), String> {
    for query in queries {
        send_line(telnet, query).await?;
        tokio::time::sleep(COMMAND_SPACING).await;
    }
    Ok(())
}
