//! Sonos device adapter (#774). **Alpha.**
//!
//! A *device* adapter: it controls one player's own volume, mute and (where the model has them)
//! line-in / TV input. It never publishes zones and never touches playback, grouping or queues.
//! It executes the already-resolved, already-clamped [`DeviceOperation`]s the
//! [`crate::devices::service::DeviceService`] hands it, and reads state back after every write.
//!
//! # Protocol
//!
//! UPnP SOAP on port 1400 (`RenderingControl` for volume/mute, `AVTransport` for input), polled
//! every few seconds: no event subscription is held, so the control channel is
//! [`ControlChannel::Polling`]. Volume is **per player** (`RenderingControl`, channel `Master`),
//! not the group's: a grouped player's own level is what a linked zone controls here.
//!
//! # Boundaries
//!
//! - No power: Sonos has no generic on/off. Power operations return an error.
//! - Input selection sets the line-in or TV stream URI and then sends `Play` (a stream URI leaves
//!   the transport stopped). On a grouped, non-coordinator player the player answers with a UPnP
//!   fault, which is returned to the caller unchanged.
//! - A fixed-output unit (`GetOutputFixed` = 1) reports `value = None` and volume unsupported.
//!
//! # Discovery
//!
//! SSDP `urn:schemas-upnp-org:device:ZonePlayer:1` and configured manual hosts. A host is
//! published only after its `device_description.xml` parses as Sonos *and* `GetVolume` or
//! `GetMute` answers, so "discovered" means "answers the control protocol". Identity is the MAC in
//! the `RINCON_<MAC>01400` UDN, never the room name, model or IP.

pub mod protocol;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use ssdp_client::{SearchTarget, URN};
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio_util::sync::CancellationToken;

use crate::adapters::devices::{DeviceFamily, FamilyContext};
use crate::bus::{BusEvent, SharedBus, VolumeScale};
use crate::devices::links::ManualDevice;
use crate::devices::service::{DeviceDriver, DeviceOperation};
use crate::devices::{
    CapabilityEvidence, ControlChannel, Device, DeviceCapabilities, DeviceCapability,
    DeviceConnection, DeviceId, DeviceInput, DevicePower, DeviceVolume, DEVICE_CONTROL_MATURITY,
};
use protocol::{Description, SoapCall};

/// Family / settings name.
pub const ADAPTER_NAME: &str = "sonos";

const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
const WRITE_TIMEOUT: Duration = Duration::from_secs(4);
const POLL_INTERVAL: Duration = Duration::from_secs(4);
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(60);
const READBACK_SCHEDULE: &[Duration] = &[
    Duration::from_millis(150),
    Duration::from_millis(500),
    Duration::from_millis(1200),
];

/// Capability claims for one model. All implementation-derived: no Sonos hardware was exercised.
///
/// `fixed_output`: the unit reported a fixed-level output (`GetOutputFixed`), so volume is not
/// settable.
pub fn capabilities_for(model: &str, fixed_output: bool) -> DeviceCapabilities {
    let claim = |supported: bool, detail: String| DeviceCapability {
        supported,
        evidence: CapabilityEvidence::ImplementationDerived,
        detail,
    };
    let inputs = protocol::inputs_for_model(model);
    DeviceCapabilities {
        observe: claim(
            true,
            "UPnP RenderingControl GetVolume/GetMute polled every 4 s; not exercised on hardware"
                .to_string(),
        ),
        volume: claim(
            !fixed_output,
            if fixed_output {
                "the unit reports a fixed output (GetOutputFixed); volume cannot be set".to_string()
            } else {
                "RenderingControl SetVolume, Channel Master, 0-100 (percentage), per player not \
                 per group. 100 is the maximum; a user ceiling lowers it"
                    .to_string()
            },
        ),
        mute: claim(true, "RenderingControl SetMute, Channel Master".to_string()),
        power: claim(
            false,
            "Sonos has no generic power on/off; players sleep and wake by themselves".to_string(),
        ),
        input_select: claim(
            !inputs.is_empty(),
            if inputs.is_empty() {
                format!(
                    "no line-in or TV input is known for model {model:?}; the catalog is derived \
                     from the model name and is empty for unlisted models"
                )
            } else {
                "AVTransport SetAVTransportURI with x-rincon-stream (line-in) or \
                 x-sonos-htastream (TV), then Play; catalog derived from the model name"
                    .to_string()
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
    Execute(crate::devices::service::DeviceWork),
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
}

#[derive(Clone)]
pub struct SonosAdapter {
    bus: SharedBus,
    http: Client,
    manual: Arc<RwLock<Vec<ManualDevice>>>,
    state: Arc<RwLock<AdapterState>>,
}

impl SonosAdapter {
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
        tracing::info!("Sonos device adapter started (alpha)");
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
        tracing::info!("Sonos device adapter stopped");
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
        let mut hosts: Vec<(String, u16, &'static str)> = Vec::new();
        if let Ok(urn) = SSDP_URN.parse::<URN>() {
            match ssdp_client::search(&SearchTarget::URN(urn), Duration::from_secs(2), 2, None)
                .await
            {
                Ok(responses) => {
                    futures::pin_mut!(responses);
                    while let Some(response) = responses.next().await {
                        if let Ok(response) = response {
                            if let Some(url) = reqwest::Url::parse(response.location())
                                .ok()
                                .filter(|url| url.host_str().is_some())
                            {
                                hosts.push((
                                    url.host_str().unwrap_or_default().to_string(),
                                    url.port().unwrap_or(protocol::PORT),
                                    "ssdp",
                                ));
                            }
                        }
                    }
                }
                Err(error) => tracing::debug!("Sonos SSDP search failed: {error}"),
            }
        }
        for device in self.manual.read().await.iter() {
            hosts.push((
                device.host.clone(),
                device.port.unwrap_or(protocol::PORT),
                "manual",
            ));
        }
        hosts.sort();
        hosts.dedup();
        for (host, port, discovery) in hosts {
            if cancel.is_cancelled() {
                return;
            }
            self.probe(&host, port, discovery, cancel).await;
        }
    }

    /// Read the description and prove the control protocol answers before publishing a unit.
    async fn probe(&self, host: &str, port: u16, discovery: &str, cancel: &CancellationToken) {
        let url = format!("http://{host}:{port}{}", protocol::DESCRIPTION_PATH);
        let Ok(response) = self.http.get(&url).send().await else {
            return;
        };
        let Ok(body) = response.text().await else {
            return;
        };
        let Some(description) = protocol::parse_description(&body).filter(Description::is_sonos)
        else {
            return;
        };
        let Some(device_id) = description
            .hardware_id()
            .and_then(|hardware| DeviceId::new(ADAPTER_NAME, &hardware))
        else {
            return;
        };
        let rehost = {
            let state = self.state.read().await;
            if !state.running {
                return;
            }
            state
                .units
                .get(device_id.as_str())
                .filter(|unit| unit.host != format!("{host}:{port}"))
                .map(|unit| unit.commands.clone())
        };
        if let Some(commands) = rehost {
            if commands
                .try_send(UnitCommand::Rehost(format!("{host}:{port}")))
                .is_ok()
            {
                if let Some(unit) = self.state.write().await.units.get_mut(device_id.as_str()) {
                    unit.host = format!("{host}:{port}");
                }
            }
            return;
        }
        // Discovery is not capability detection: the control service must answer.
        let mut probe = Unit::new(self, &device_id, host, port, description, discovery);
        if !probe.answers().await {
            tracing::debug!(%device_id, "Sonos player did not answer RenderingControl; skipped");
            return;
        }
        self.spawn(probe, cancel.child_token()).await;
    }

    /// Attach a unit at a known address without discovery: a configured host, or a mock player on
    /// an ephemeral port in tests. The unit runs until the adapter stops.
    pub async fn attach_unit(
        &self,
        host: &str,
        port: u16,
        description: Description,
    ) -> Option<DeviceId> {
        let device_id = DeviceId::new(ADAPTER_NAME, &description.hardware_id()?)?;
        let cancel = {
            let mut state = self.state.write().await;
            state.running = true;
            state.cancel.child_token()
        };
        let unit = Unit::new(self, &device_id, host, port, description, "manual");
        self.spawn(unit, cancel).await;
        Some(device_id)
    }

    async fn spawn(&self, unit: Unit, cancel: CancellationToken) {
        let mut state = self.state.write().await;
        if state.units.contains_key(unit.device_id.as_str()) {
            return;
        }
        let (commands, receiver) = mpsc::channel(8);
        state.units.insert(
            unit.device_id.to_string(),
            UnitHandle {
                host: format!("{}:{}", unit.host, unit.port),
                commands,
                cancel: cancel.clone(),
            },
        );
        drop(state);
        tracing::info!(device_id = %unit.device_id, host = %unit.host, model = %unit.model, "Discovered Sonos device");
        tokio::spawn(unit.run(receiver, cancel));
    }

    pub async fn device_count(&self) -> usize {
        self.state.read().await.units.len()
    }
}

const SSDP_URN: &str = protocol::SSDP_TARGET;

#[async_trait]
impl DeviceFamily for SonosAdapter {
    fn family(&self) -> &'static str {
        ADAPTER_NAME
    }

    async fn start_family(&self, context: FamilyContext) -> anyhow::Result<()> {
        *self.manual.write().await = context.manual;
        self.start_internal().await
    }

    async fn stop_family(&self) {
        self.stop_internal().await
    }
}

#[async_trait]
impl DeviceDriver for SonosAdapter {
    fn kinds(&self) -> &'static [&'static str] {
        &["sonos"]
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

/// How a SOAP call failed.
enum SoapError {
    /// No HTTP answer: the player is unreachable.
    Unreachable(String),
    /// The player answered with a fault or an error status.
    Refused(String),
}

impl SoapError {
    fn message(&self) -> &str {
        match self {
            Self::Unreachable(m) | Self::Refused(m) => m,
        }
    }
}

struct Unit {
    bus: SharedBus,
    http: Client,
    host: String,
    port: u16,
    device_id: DeviceId,
    /// `RINCON_<MAC>01400`, used in input stream URIs.
    uid: String,
    model: String,
    name: String,
    discovery: String,
    inputs: Vec<DeviceInput>,
    volume: Option<f32>,
    muted: Option<bool>,
    fixed: bool,
    input: Option<String>,
    reachable: bool,
    detail: Option<String>,
}

impl Unit {
    fn new(
        adapter: &SonosAdapter,
        device_id: &DeviceId,
        host: &str,
        port: u16,
        description: Description,
        discovery: &str,
    ) -> Self {
        let uid = description.rincon().unwrap_or_default();
        let model = description.model_label();
        Self {
            bus: adapter.bus.clone(),
            http: adapter.http.clone(),
            host: host.to_string(),
            port,
            device_id: device_id.clone(),
            uid,
            inputs: protocol::inputs_for_model(&model),
            name: description.name(),
            model,
            discovery: discovery.to_string(),
            volume: None,
            muted: None,
            fixed: false,
            input: None,
            reachable: false,
            detail: None,
        }
    }

    /// Whether the player answers the control protocol at all.
    async fn answers(&mut self) -> bool {
        self.refresh().await;
        self.reachable
    }

    async fn run(mut self, mut commands: mpsc::Receiver<UnitCommand>, cancel: CancellationToken) {
        let mut poll = tokio::time::interval(POLL_INTERVAL);
        // Queries only on start: nothing is written until a caller asks.
        self.refresh().await;
        self.publish();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                command = commands.recv() => {
                    let Some(command) = command else { break };
                    match command {
                        UnitCommand::Rehost(address) => {
                            tracing::info!(device_id = %self.device_id, %address, "Sonos device moved to a new address");
                            if let Some((host, port)) = address.rsplit_once(':') {
                                if let Ok(port) = port.parse() {
                                    self.host = host.to_string();
                                    self.port = port;
                                }
                            }
                        }
                        UnitCommand::Execute(work) => {
                            let Some((operation, reply)) = work.begin_dispatch() else { continue; };
                            let result = self.execute(&operation).await;
                            let sent = result.is_ok();
                            if reply.send(result).is_err() {
                                tracing::debug!(device_id = %self.device_id, "command caller went away before the reply");
                            }
                            if sent {
                                for delay in READBACK_SCHEDULE {
                                    tokio::time::sleep(*delay).await;
                                    self.refresh().await;
                                    self.publish();
                                }
                            }
                        }
                    }
                }
                _ = poll.tick() => {
                    self.refresh().await;
                    self.publish();
                }
            }
        }
        self.bus.publish(BusEvent::DeviceRemoved {
            device_id: self.device_id.clone(),
        });
    }

    async fn soap(&self, call: &SoapCall) -> Result<String, SoapError> {
        let url = format!(
            "http://{}:{}{}",
            self.host,
            self.port,
            call.service.control_path()
        );
        let response = self
            .http
            .post(&url)
            .header("SOAPACTION", call.soap_action())
            .header("Content-Type", "text/xml; charset=\"utf-8\"")
            .body(call.envelope())
            .send()
            .await
            .map_err(|error| SoapError::Unreachable(error.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| SoapError::Unreachable(error.to_string()))?;
        if status.is_success() {
            Ok(body)
        } else {
            let code = protocol::parse_fault(&body)
                .map(|code| format!(" (UPnP error {code})"))
                .unwrap_or_default();
            Err(SoapError::Refused(format!(
                "{} refused with {status}{code}",
                call.action
            )))
        }
    }

    async fn execute(&mut self, operation: &DeviceOperation) -> Result<(), String> {
        let calls = protocol::calls_for(operation, &self.uid, &self.inputs, self.fixed)?;
        for call in calls {
            self.soap(&call)
                .await
                .map_err(|error| format!("command failed: {}", error.message()))?;
        }
        Ok(())
    }

    /// Read volume, mute, fixed-output and (when the model has inputs) the current input. Keeps
    /// previous values on a failed read; marks the unit unreachable only when nothing answered.
    async fn refresh(&mut self) {
        let mut answered = false;
        let mut last_error = None;
        match self.soap(&protocol::get_volume()).await {
            Ok(body) => {
                answered = true;
                self.volume = protocol::parse_volume(&body);
            }
            Err(SoapError::Refused(message)) => {
                // Players with no audio output (Boost, Bridge) answer but refuse volume.
                answered = true;
                self.volume = None;
                last_error = Some(message);
            }
            Err(SoapError::Unreachable(message)) => last_error = Some(message),
        }
        match self.soap(&protocol::get_mute()).await {
            Ok(body) => {
                answered = true;
                self.muted = protocol::parse_mute(&body);
            }
            Err(SoapError::Refused(_)) => answered = true,
            Err(SoapError::Unreachable(message)) => last_error = Some(message),
        }
        if answered {
            // A player without the action simply is not fixed.
            self.fixed = match self.soap(&protocol::get_output_fixed()).await {
                Ok(body) => protocol::parse_fixed(&body).unwrap_or(false),
                Err(_) => false,
            };
            if !self.inputs.is_empty() {
                self.input = match self.soap(&protocol::get_position_info()).await {
                    Ok(body) => protocol::parse_track_uri(&body)
                        .and_then(|uri| protocol::input_for_uri(&uri))
                        .map(str::to_string),
                    Err(_) => None,
                };
            }
        }
        self.reachable = answered;
        self.detail = if answered {
            None
        } else {
            last_error.map(|e| format!("unreachable: {e}"))
        };
    }

    fn publish(&self) {
        let device = Device {
            device_id: self.device_id.clone(),
            vendor: "Sonos".to_string(),
            model: self.model.clone(),
            name: self.name.clone(),
            host: self.host.clone(),
            discovery: self.discovery.clone(),
            maturity: DEVICE_CONTROL_MATURITY.to_string(),
            connection: DeviceConnection {
                control_channel: if self.reachable {
                    ControlChannel::Polling
                } else {
                    ControlChannel::None
                },
                detail: self.detail.clone(),
            },
            // Sonos has no power model; never inferred from transport state.
            power: DevicePower::Unknown,
            input: self.input.clone(),
            inputs: self.inputs.clone(),
            volume: DeviceVolume {
                scale: VolumeScale::Percentage,
                value: if self.fixed { None } else { self.volume },
                min: protocol::MIN_VOLUME,
                // 0–100 is the protocol's full scale: the device maximum. A user ceiling lowers it.
                hardware_max: Some(100.0),
                ceiling: None,
                step: 1.0,
                is_muted: self.muted,
                display: None,
            },
            capabilities: capabilities_for(&self.model, self.fixed),
            revision: next_revision(),
            last_updated: now_ms(),
        };
        self.bus.publish(BusEvent::DeviceObserved {
            device: Box::new(device),
        });
    }
}
