//! Hegel amplifier device adapter (#774). **Alpha.**
//!
//! A *device* adapter: it controls the amplifier a zone feeds, not a music provider. It never
//! publishes zones; it publishes [`BusEvent::DeviceObserved`] snapshots and executes
//! [`DeviceOperation`]s handed to it by [`crate::devices::service::DeviceService`], which has
//! already resolved, clamped and quantised every level. This adapter never computes a level.
//!
//! # Protocol
//!
//! ASCII over a persistent TCP session on port 50001, CR-terminated (`-p.1`, `-v.50`, `-m.0`,
//! `-i.3`; queries end `.?`). The amplifier pushes state changes on the same session. Implemented
//! from `hegel-ip-client` (<https://github.com/boazca/hegel-ip-client>, MIT) and Home Assistant's
//! `hegel` integration (<https://github.com/home-assistant/core/tree/dev/homeassistant/components/hegel>).
//! **No Hegel hardware has been available**: every capability claim is implementation-derived.
//!
//! # Discovery
//!
//! SSDP `MediaRenderer:1` with manufacturer "Hegel"; identity is the description's
//! `serialNumber` (UDN uuid when absent). A unit is published only after it answers `-p.?` on
//! TCP 50001: discovery is not capability detection. A configured host (`ManualDevice`) is
//! probed with a unicast M-SEARCH for its description, or identified by its configured `mac`.
//!
//! # Boundaries
//!
//! - Covered models are Röst, H95, H120, H190, H190V, H390 and H590. The protocol is not
//!   extrapolated to newer Hegel platforms: another model is published (the volume/power/mute
//!   path is the same ASCII grammar) with a caveat, and input selection is refused because the
//!   input numbering is model specific.
//! - The amplifier reports no maximum level, but the protocol has a fixed full scale (0-100), so
//!   [`DeviceVolume::hardware_max`] is 100 and the user's ceiling lowers it.
//! - hegel-ip-client sends a `-r.3` heartbeat every two minutes. That command sets the
//!   amplifier's reset timer (a device setting), so this adapter does not send it: it polls with
//!   queries only, which keeps the session active without changing anything.

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
use protocol::{Description, HegelEvent};

/// Settings/coordinator name (`ManualDevice::family`).
pub const ADAPTER_NAME: &str = "hegel";

const SSDP_TARGET: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
/// State queries while the session is up. The amplifier pushes changes; this catches anything
/// missed and keeps the session from idling out.
const POLL_INTERVAL: Duration = Duration::from_secs(20);
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(60);
const RECONNECT_MIN: Duration = Duration::from_secs(2);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// Gap between a command and its confirming query.
const COMMAND_SPACING: Duration = Duration::from_millis(80);

/// Capability claims for one model. Pure, so documentation can be generated from it.
///
/// Every claim is implementation-derived: `hegel-ip-client` and Home Assistant's integration are
/// the evidence, not a device. A model outside the covered list keeps volume, mute and power
/// (the same grammar) with a caveat, and loses input selection (the numbering is per model).
pub fn capabilities_for(model: &str) -> DeviceCapabilities {
    let covered = protocol::covered_model(model);
    let caveat = if covered.is_some() {
        String::new()
    } else {
        format!(
            "; model {model:?} is not one of the covered Hegel models (Röst, H95, H120, H190, \
             H190V, H390, H590): the protocol is not verified for newer Hegel platforms"
        )
    };
    let derived = |supported: bool, detail: String| DeviceCapability {
        supported,
        evidence: CapabilityEvidence::ImplementationDerived,
        detail,
    };
    DeviceCapabilities {
        observe: derived(
            true,
            format!(
                "power, level, mute and input read with -p.? -v.? -m.? -i.? over TCP 50001 \
                 (hegel-ip-client 0.1.4); not exercised on hardware{caveat}"
            ),
        ),
        volume: derived(
            true,
            format!(
                "-v.N, 0-100 percent (the protocol's full scale is the maximum; a user ceiling \
                 lowers it){caveat}"
            ),
        ),
        mute: derived(true, format!("-m.1 / -m.0{caveat}")),
        power: derived(
            true,
            format!("-p.1 / -p.0 over the control session{caveat}"),
        ),
        input_select: derived(
            covered.is_some(),
            if covered.is_some() {
                "-i.N with the model's numbered input list (Home Assistant MODEL_INPUTS)"
                    .to_string()
            } else {
                format!(
                    "refused: input numbers are model specific and this model has no known \
                     list{caveat}"
                )
            },
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
    /// SSDP `LOCATION`s already examined and found not to be a Hegel (`None`) or found to be
    /// the unit with this id. An unreachable Hegel is *not* recorded, so it is retried.
    examined: HashMap<String, Option<String>>,
}

#[derive(Clone)]
pub struct HegelAdapter {
    bus: SharedBus,
    http: Client,
    manual: Arc<RwLock<Vec<ManualDevice>>>,
    state: Arc<RwLock<AdapterState>>,
}

enum Identified {
    NotHegel,
    Unreachable,
    Hegel(DeviceId, Description),
}

impl HegelAdapter {
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
        tracing::info!("Hegel device adapter started (alpha)");
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
        tracing::info!("Hegel device adapter stopped");
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
                Err(error) => tracing::debug!("Hegel SSDP search failed: {error}"),
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
                Identified::NotHegel => {
                    self.state.write().await.examined.insert(location, None);
                }
                Identified::Unreachable => {}
                Identified::Hegel(device_id, description) => {
                    self.state
                        .write()
                        .await
                        .examined
                        .insert(location, Some(device_id.to_string()));
                    self.ensure_unit(&device_id, &host, port, description, discovery, cancel)
                        .await;
                }
            }
        }
        self.discover_configured_by_mac(cancel).await;
    }

    /// Configured units whose multicast/unicast SSDP answer never arrived but whose identity the
    /// user supplied (`mac`). Probed over the control protocol before they are published.
    async fn discover_configured_by_mac(&self, cancel: &CancellationToken) {
        for manual in self.manual.read().await.clone() {
            if cancel.is_cancelled() {
                return;
            }
            let Some(device_id) = manual
                .mac
                .as_deref()
                .and_then(|mac| DeviceId::new("hegel", mac))
            else {
                continue;
            };
            {
                let state = self.state.read().await;
                if state.units.contains_key(device_id.as_str())
                    || state.units.values().any(|unit| unit.host == manual.host)
                {
                    continue;
                }
            }
            let port = manual.port.unwrap_or(protocol::DEFAULT_PORT);
            if !probe(&manual.host, port).await {
                continue;
            }
            let description = Description {
                manufacturer: "Hegel".to_string(),
                model: String::new(),
                friendly_name: format!("Hegel {}", manual.host),
                serial: None,
                udn: None,
            };
            self.ensure_unit(
                &device_id,
                &manual.host,
                port,
                description,
                "manual",
                cancel,
            )
            .await;
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
        let Some(description) = protocol::parse_description(&body) else {
            return Identified::NotHegel;
        };
        let Some(kind) = description.kind() else {
            return Identified::NotHegel;
        };
        let Some(device_id) = description
            .hardware_id()
            .and_then(|hardware_id| DeviceId::new(kind, &hardware_id))
        else {
            return Identified::NotHegel;
        };
        // Discovery is not capability detection: the unit must answer the control protocol.
        if !probe(host, port).await {
            return Identified::Unreachable;
        }
        Identified::Hegel(device_id, description)
    }

    async fn ensure_unit(
        &self,
        device_id: &DeviceId,
        host: &str,
        port: u16,
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
            port,
            description,
            discovery,
            adapter_cancel.child_token(),
        )
        .await;
    }

    /// Attach a unit at a known address without discovery: a mock amplifier on an ephemeral port
    /// in tests, or a configured host. The unit runs until the adapter stops.
    pub async fn attach_unit(
        &self,
        device_id: &DeviceId,
        host: &str,
        port: u16,
        description: Description,
    ) {
        let cancel = {
            let mut state = self.state.write().await;
            state.running = true;
            state.cancel.child_token()
        };
        self.spawn_unit(device_id, host, port, description, "manual", cancel)
            .await;
    }

    async fn spawn_unit(
        &self,
        device_id: &DeviceId,
        host: &str,
        port: u16,
        description: Description,
        discovery: &str,
        cancel: CancellationToken,
    ) {
        let mut state = self.state.write().await;
        // One session per amplifier: neither a repeated id nor a second id at the same address.
        if state.units.contains_key(device_id.as_str())
            || state
                .units
                .values()
                .any(|unit| unit.host == host && unit.port == port)
        {
            return;
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
            model = %description.model,
            name = %description.friendly_name,
            "Discovered Hegel device"
        );
        let inputs = protocol::inputs_for(&description.model)
            .into_iter()
            .map(|(id, name)| DeviceInput {
                id,
                name,
                available: true,
            })
            .collect();
        let unit = Unit {
            bus: self.bus.clone(),
            host: host.to_string(),
            port,
            device_id: device_id.clone(),
            vendor: if description.manufacturer.is_empty() {
                "Hegel".to_string()
            } else {
                description.manufacturer
            },
            model: description.model,
            name: description.friendly_name,
            discovery: discovery.to_string(),
            inputs,
            state: UnitState::default(),
        };
        tokio::spawn(unit.run(receiver, cancel));
    }

    pub async fn device_count(&self) -> usize {
        self.state.read().await.units.len()
    }
}

#[async_trait]
impl crate::adapters::devices::DeviceFamily for HegelAdapter {
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
impl DeviceDriver for HegelAdapter {
    fn kinds(&self) -> &'static [&'static str] {
        &["hegel"]
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
    power: Option<bool>,
    volume: Option<f32>,
    muted: Option<bool>,
    input: Option<u8>,
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

    fn apply(&mut self, event: HegelEvent) {
        match event {
            HegelEvent::Power(on) => self.power = Some(on),
            HegelEvent::Volume(level) => self.volume = Some(level),
            HegelEvent::Mute(muted) => self.muted = Some(muted),
            HegelEvent::Input(number) => self.input = Some(number),
            HegelEvent::Error(code) => {
                tracing::debug!("Hegel refused a command (-e.{code})");
            }
        }
    }

    /// An unreachable unit's last readings are not facts: clear them so nothing downstream
    /// shows (or computes a relative step from) a stale level.
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
    model: String,
    name: String,
    discovery: String,
    inputs: Vec<DeviceInput>,
    state: UnitState,
}

impl Unit {
    async fn run(mut self, mut commands: mpsc::Receiver<UnitCommand>, cancel: CancellationToken) {
        let mut stream: Option<TcpStream> = None;
        let mut buffer = String::new();
        let mut chunk = [0u8; 512];
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
                            // Queries only: a reconnect must never change what the amp is doing.
                            if let Err(error) = send_queries(&mut stream, protocol::STATE_QUERIES).await {
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
                        if let Err(error) = send_queries(&mut stream, protocol::STATE_QUERIES).await {
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
                "control port refused: the amplifier is in a standby that closes the network, or \
                 another client holds its control session"
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
            return Err("the amplifier's control session is not connected".to_string());
        }
        if matches!(operation, DeviceOperation::SelectInput(_)) && self.inputs.is_empty() {
            return Err(format!(
                "input selection is not supported by this device: input numbers are model \
                 specific and {:?} is not a covered Hegel model",
                self.model
            ));
        }
        let (command, query) = protocol::command(operation)
            .ok_or_else(|| format!("{operation:?} cannot be encoded for this device"))?;
        if let Err(error) = send_line(stream, &command).await {
            *stream = None;
            return Err(error);
        }
        tokio::time::sleep(COMMAND_SPACING).await;
        if let Err(error) = send_line(stream, query).await {
            *stream = None;
            return Err(error);
        }
        Ok(())
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
                control_channel: if self.state.connected {
                    ControlChannel::Push
                } else {
                    ControlChannel::None
                },
                detail: self.state.detail.clone(),
            },
            power: self.state.power(),
            input: self.state.input.map(|number| number.to_string()),
            inputs: self.inputs.clone(),
            volume: DeviceVolume {
                scale: crate::bus::VolumeScale::Percentage,
                value: self.state.volume,
                min: protocol::MIN_VOLUME,
                // The protocol's full scale (`-v.100`) is the device maximum; the user's ceiling
                // only ever lowers it.
                hardware_max: Some(protocol::MAX_VOLUME),
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

/// Connect, ask `-p.?`, and wait for a power answer: the amplifier speaks the control protocol.
async fn probe(host: &str, port: u16) -> bool {
    let attempt = async {
        let mut stream = TcpStream::connect((host, port)).await.ok()?;
        stream.write_all(b"-p.?\r").await.ok()?;
        let mut buffer = String::new();
        let mut chunk = [0u8; 256];
        loop {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buffer.push_str(&String::from_utf8_lossy(&chunk[..n]));
            for line in protocol::drain_lines(&mut buffer) {
                if matches!(protocol::parse_line(&line), Some(HegelEvent::Power(_))) {
                    return Some(());
                }
            }
        }
    };
    matches!(
        tokio::time::timeout(PROBE_TIMEOUT, attempt).await,
        Ok(Some(()))
    )
}

/// Unicast SSDP M-SEARCH to a known host, for the `LOCATION` of its description. Multicast
/// cannot reach a configured host on another subnet.
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

async fn send_line(stream: &mut Option<TcpStream>, line: &str) -> Result<(), String> {
    let stream = stream
        .as_mut()
        .ok_or_else(|| "no control session".to_string())?;
    let bytes = format!("{line}\r");
    tokio::time::timeout(WRITE_TIMEOUT, stream.write_all(bytes.as_bytes()))
        .await
        .map_err(|_| "control write timed out".to_string())?
        .map_err(|error| format!("control write failed: {error}"))
}

async fn send_queries(stream: &mut Option<TcpStream>, queries: &[&str]) -> Result<(), String> {
    for query in queries {
        send_line(stream, query).await?;
        tokio::time::sleep(COMMAND_SPACING).await;
    }
    Ok(())
}
