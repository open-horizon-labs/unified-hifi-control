//! Yamaha MusicCast (Yamaha Extended Control) device adapter (#774). **Alpha.**
//!
//! A *device* adapter: it controls the amplifier a zone feeds, not a music provider. It publishes
//! [`BusEvent::DeviceObserved`] snapshots and executes [`DeviceOperation`]s handed to it by
//! [`crate::devices::service::DeviceService`], which has already resolved, clamped and quantised
//! every level in the device's native scale. This adapter never computes a level.
//!
//! # Discovery is not capability detection
//!
//! SSDP with manufacturer "Yamaha Corporation" and a `yamaha:X_yxcControlURL` is only a candidate
//! filter. A device is accepted only after `system/getDeviceInfo` (identity) and
//! `system/getFeatures` (what the main zone can do, and its volume range) both answer.
//!
//! # State
//!
//! HTTP polling of `main/getStatus`; UDP event subscription (aiomusiccast's push path) is not
//! used, so [`ControlChannel::Polling`] is reported. Before every write the unit re-reads
//! `getDeviceInfo` and refuses if the address now answers as a different device, so a DHCP
//! reassignment can never route a level to the wrong amplifier.
//!
//! # Volume
//!
//! Levels travel in the model's native units (commonly 0-161 or 0-60). The scale is
//! `Percentage` only for an exact 0-100 range, otherwise `Unknown`: a faithful native scale, not a
//! guess at decibels. Models that report `actual_volume` in dB expose it as
//! `DeviceVolume::display` (display only; nothing is ever sent in it).
//!
//! References: <https://github.com/vigonotion/aiomusiccast>,
//! <https://github.com/foxthefox/yamaha-yxc-nodejs>.

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

use crate::bus::{BusEvent, SharedBus};
use crate::devices::service::{DeviceDriver, DeviceOperation};
use crate::devices::{
    ControlChannel, Device, DeviceCapabilities, DeviceConnection, DeviceId, DeviceInput,
    DevicePower, DeviceVolume, DEVICE_CONTROL_MATURITY,
};
use protocol::{DeviceInfo, Features, MainStatus};

pub use protocol::capabilities_for;

/// Family name (`ManualDevice::family`) and settings/coordinator name.
pub const ADAPTER_NAME: &str = "musiccast";
/// The device kind (id prefix) this family serves.
pub const KIND: &str = "yamaha";
/// Default YXC port.
pub const HTTP_PORT: u16 = 80;

const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// Features and input names change rarely; refresh every this many polls.
const CATALOG_REFRESH_TICKS: u32 = 20;
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(60);
/// After a write, read back on this schedule so confirmation sees the applied state (a power-on
/// takes a moment to reach `on`).
const READBACK_SCHEDULE: &[Duration] = &[
    Duration::from_millis(50),
    Duration::from_millis(400),
    Duration::from_millis(1000),
    Duration::from_millis(2000),
];

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

async fn get_text(http: &Client, host: &str, port: u16, path: &str) -> Result<String, String> {
    let url = format!("http://{host}:{port}{path}");
    let response = http
        .get(&url)
        .send()
        .await
        .map_err(|error| format!("{path}: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("{path}: HTTP {}", response.status()));
    }
    response
        .text()
        .await
        .map_err(|error| format!("{path}: {error}"))
}

/// What a successful probe learned about a candidate.
struct Probed {
    device_id: DeviceId,
    info: DeviceInfo,
    features: Features,
    inputs: Vec<DeviceInput>,
    network_name: Option<String>,
}

/// Probe a candidate: it is a MusicCast device only if both documents parse.
async fn probe(http: &Client, host: &str, port: u16) -> Result<Probed, String> {
    let info = protocol::parse_device_info(
        &get_text(http, host, port, &protocol::device_info_path()).await?,
    )?;
    let features =
        protocol::parse_features(&get_text(http, host, port, &protocol::features_path()).await?)?;
    let hardware_id = info
        .hardware_id()
        .ok_or_else(|| "no hardware identity".to_string())?;
    let device_id = DeviceId::new(KIND, &hardware_id)
        .ok_or_else(|| format!("{hardware_id} is not a usable hardware id"))?;
    // Names and the friendly name are cosmetic: their absence never rejects a device.
    let names = match get_text(http, host, port, &protocol::name_text_path()).await {
        Ok(body) => protocol::parse_input_names(&body).unwrap_or_default(),
        Err(_) => HashMap::new(),
    };
    let network_name = match get_text(http, host, port, &protocol::network_status_path()).await {
        Ok(body) => protocol::parse_network_name(&body),
        Err(_) => None,
    };
    let inputs = protocol::build_inputs(&features, &names);
    Ok(Probed {
        device_id,
        info,
        features,
        inputs,
        network_name,
    })
}

enum UnitCommand {
    Execute(crate::devices::service::DeviceWork),
    Rehost { host: String, port: u16 },
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
    /// SSDP `LOCATION`s already examined, with the device they named (`None`: not MusicCast).
    examined: HashMap<String, Option<String>>,
}

#[derive(Clone)]
pub struct MusicCastAdapter {
    bus: SharedBus,
    http: Client,
    manual: Arc<RwLock<Vec<(String, u16)>>>,
    state: Arc<RwLock<AdapterState>>,
}

impl MusicCastAdapter {
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
    pub async fn set_manual_hosts(&self, hosts: Vec<(String, u16)>) {
        *self.manual.write().await = hosts;
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
        tracing::info!("MusicCast device adapter started (alpha)");
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
        tracing::info!("MusicCast device adapter stopped");
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

    async fn ssdp_locations(&self) -> Vec<String> {
        let mut locations = Vec::new();
        let mut targets = vec![SearchTarget::RootDevice];
        if let Ok(urn) = "urn:schemas-upnp-org:device:MediaRenderer:1".parse::<URN>() {
            targets.push(SearchTarget::URN(urn));
        }
        for target in targets {
            match ssdp_client::search(&target, Duration::from_secs(2), 2, None).await {
                Ok(responses) => {
                    futures::pin_mut!(responses);
                    while let Some(response) = responses.next().await {
                        if let Ok(response) = response {
                            locations.push(response.location().to_string());
                        }
                    }
                }
                Err(error) => tracing::debug!("MusicCast SSDP search failed: {error}"),
            }
        }
        locations.sort();
        locations.dedup();
        locations
    }

    async fn discover_once(&self, cancel: &CancellationToken) {
        for location in self.ssdp_locations().await {
            if cancel.is_cancelled() {
                return;
            }
            if self.state.read().await.examined.contains_key(&location) {
                continue;
            }
            self.examine_location(&location, cancel).await;
        }
        for (host, port) in self.manual.read().await.clone() {
            if cancel.is_cancelled() {
                return;
            }
            let key = format!("manual:{host}:{port}");
            if matches!(self.state.read().await.examined.get(&key), Some(Some(_))) {
                continue;
            }
            match probe(&self.http, &host, port).await {
                Ok(probed) => {
                    self.state
                        .write()
                        .await
                        .examined
                        .insert(key, Some(probed.device_id.to_string()));
                    self.ensure_unit(probed, &host, port, None, "manual", cancel)
                        .await;
                }
                Err(error) => tracing::debug!(%host, "manual MusicCast probe failed: {error}"),
            }
        }
    }

    /// Fetch and judge one SSDP `LOCATION`. Not-Yamaha answers are remembered; a Yamaha device
    /// whose probe failed is retried next cycle (it may be waking).
    async fn examine_location(&self, location: &str, cancel: &CancellationToken) {
        let Some(host) = reqwest::Url::parse(location)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
        else {
            return;
        };
        let body = match self.http.get(location).send().await {
            Ok(response) => response.text().await.ok(),
            Err(_) => None,
        };
        let Some(body) = body else { return };
        let description = protocol::parse_description(&body).filter(|d| d.is_candidate());
        let Some(description) = description else {
            self.state
                .write()
                .await
                .examined
                .insert(location.to_string(), None);
            return;
        };
        let port = description.control_port();
        match probe(&self.http, &host, port).await {
            Ok(probed) => {
                self.state
                    .write()
                    .await
                    .examined
                    .insert(location.to_string(), Some(probed.device_id.to_string()));
                let name = (!description.friendly_name.is_empty())
                    .then(|| description.friendly_name.clone());
                self.ensure_unit(probed, &host, port, name, "ssdp", cancel)
                    .await;
            }
            Err(error) => {
                tracing::debug!(%host, "Yamaha candidate is not answering YXC: {error}")
            }
        }
    }

    async fn ensure_unit(
        &self,
        probed: Probed,
        host: &str,
        port: u16,
        friendly_name: Option<String>,
        discovery: &str,
        adapter_cancel: &CancellationToken,
    ) {
        let existing = {
            let state = self.state.read().await;
            if !state.running {
                return;
            }
            state.units.get(probed.device_id.as_str()).map(|unit| {
                (
                    unit.host != host || unit.port != port,
                    unit.commands.clone(),
                )
            })
        };
        match existing {
            Some((false, _)) => {}
            Some((true, commands)) => {
                // Known unit at a new address: follow it.
                let rehost = UnitCommand::Rehost {
                    host: host.to_string(),
                    port,
                };
                if let Err(error) = commands.try_send(rehost) {
                    tracing::debug!(device_id = %probed.device_id, "could not re-host unit: {error}");
                } else if let Some(unit) = self
                    .state
                    .write()
                    .await
                    .units
                    .get_mut(probed.device_id.as_str())
                {
                    unit.host = host.to_string();
                    unit.port = port;
                }
            }
            None => {
                self.spawn_unit(
                    probed,
                    host,
                    port,
                    friendly_name,
                    discovery,
                    adapter_cancel.child_token(),
                )
                .await
            }
        }
    }

    /// Attach a unit at a known address without multicast: a configured host on another subnet,
    /// or a mock device on an ephemeral port in tests. The device is probed first, exactly as a
    /// discovered one is. The unit runs until the adapter stops.
    pub async fn attach_unit(&self, host: &str, port: u16) -> Result<DeviceId, String> {
        let probed = probe(&self.http, host, port).await?;
        let device_id = probed.device_id.clone();
        let cancel = {
            let mut state = self.state.write().await;
            state.running = true;
            state.cancel.child_token()
        };
        self.spawn_unit(probed, host, port, None, "manual", cancel)
            .await;
        Ok(device_id)
    }

    async fn spawn_unit(
        &self,
        probed: Probed,
        host: &str,
        port: u16,
        friendly_name: Option<String>,
        discovery: &str,
        cancel: CancellationToken,
    ) {
        let mut state = self.state.write().await;
        if state.units.contains_key(probed.device_id.as_str()) {
            return;
        }
        let (commands, receiver) = mpsc::channel(8);
        state.units.insert(
            probed.device_id.to_string(),
            UnitHandle {
                host: host.to_string(),
                port,
                commands,
                cancel: cancel.clone(),
            },
        );
        drop(state);
        let name = friendly_name
            .or(probed.network_name.clone())
            .unwrap_or_else(|| probed.info.model_name.clone());
        tracing::info!(
            device_id = %probed.device_id,
            host,
            model = %probed.info.model_name,
            %name,
            "Discovered MusicCast device"
        );
        let unit = Unit {
            bus: self.bus.clone(),
            http: self.http.clone(),
            host: host.to_string(),
            port,
            device_id: probed.device_id,
            model: probed.info.model_name.clone(),
            name,
            discovery: discovery.to_string(),
            features: probed.features,
            inputs: probed.inputs,
            state: UnitState::default(),
            cancel: cancel.clone(),
        };
        tokio::spawn(unit.run(receiver, cancel));
    }

    pub async fn device_count(&self) -> usize {
        self.state.read().await.units.len()
    }
}

#[async_trait]
impl crate::adapters::devices::DeviceFamily for MusicCastAdapter {
    fn family(&self) -> &'static str {
        ADAPTER_NAME
    }

    async fn start_family(
        &self,
        context: crate::adapters::devices::FamilyContext,
    ) -> anyhow::Result<()> {
        let hosts: Vec<(String, u16)> = context
            .manual
            .into_iter()
            .map(|device| (device.host, device.port.unwrap_or(HTTP_PORT)))
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
impl DeviceDriver for MusicCastAdapter {
    fn kinds(&self) -> &'static [&'static str] {
        &[KIND]
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
        tokio::time::timeout(WRITE_TIMEOUT * 3, response)
            .await
            .map_err(|_| "the unit did not accept the command in time".to_string())?
            .map_err(|_| "the unit's session stopped before sending".to_string())?
    }
}

/// Everything the unit has reported, before it becomes a [`Device`].
#[derive(Default)]
struct UnitState {
    reachable: bool,
    detail: Option<String>,
    status: MainStatus,
}

struct Unit {
    bus: SharedBus,
    http: Client,
    host: String,
    port: u16,
    device_id: DeviceId,
    model: String,
    name: String,
    discovery: String,
    features: Features,
    inputs: Vec<DeviceInput>,
    state: UnitState,
    cancel: CancellationToken,
}

impl Unit {
    async fn run(mut self, mut commands: mpsc::Receiver<UnitCommand>, cancel: CancellationToken) {
        let mut poll = tokio::time::interval(POLL_INTERVAL);
        let mut ticks: u32 = 0;
        self.refresh().await;
        self.publish();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                command = commands.recv() => {
                    let Some(command) = command else { break };
                    match command {
                        UnitCommand::Rehost { host, port } => {
                            tracing::info!(device_id = %self.device_id, %host, "Device moved to a new address");
                            self.host = host;
                            self.port = port;
                            self.refresh().await;
                            self.publish();
                        }
                        UnitCommand::Execute(work) => {
                            let Some((operation, reply)) = work.begin_dispatch() else { continue; };
                            let result = self.execute(&operation).await;
                            let sent = result.is_ok();
                            if reply.send(result).is_err() {
                                tracing::debug!(device_id = %self.device_id, "command caller went away before the reply");
                            }
                            // Every write is followed by a fresh read, even a rejected one: the
                            // device may have changed (or refused) differently than we expect.
                            self.read_back(&operation, sent).await;
                        }
                    }
                }
                _ = poll.tick() => {
                    ticks = ticks.wrapping_add(1);
                    let was_reachable = self.state.reachable;
                    if ticks.is_multiple_of(CATALOG_REFRESH_TICKS) || !was_reachable {
                        self.refresh_catalog().await;
                    }
                    self.refresh().await;
                    self.publish();
                }
            }
        }
        self.bus.publish(BusEvent::DeviceRemoved {
            device_id: self.device_id.clone(),
        });
    }

    /// Read the status after a write until it reflects the operation (or the schedule ends).
    async fn read_back(&mut self, operation: &DeviceOperation, sent: bool) {
        let schedule: &[Duration] = if sent {
            READBACK_SCHEDULE
        } else {
            &READBACK_SCHEDULE[..1]
        };
        for delay in schedule {
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = tokio::time::sleep(*delay) => {}
            }
            self.refresh().await;
            self.publish();
            if !sent || self.satisfies(operation) {
                break;
            }
        }
    }

    fn satisfies(&self, operation: &DeviceOperation) -> bool {
        let status = &self.state.status;
        match operation {
            DeviceOperation::SetVolume(level) => status
                .volume
                .is_some_and(|observed| (observed as f32 - level).abs() < 0.5),
            DeviceOperation::SetMute(muted) => status.mute == Some(*muted),
            DeviceOperation::PowerOn => status.power == Some(DevicePower::On),
            DeviceOperation::PowerStandby => status.power == Some(DevicePower::Standby),
            DeviceOperation::SelectInput(input) => status.input.as_deref() == Some(input.as_str()),
        }
    }

    fn volume_range(&self) -> Option<protocol::VolumeRange> {
        self.features.volume.filter(|_| self.features.has("volume"))
    }

    /// The device's current hardware volume limit: `max_volume` from the status, never above the
    /// range maximum.
    fn volume_limit(&self) -> Option<i64> {
        let range = self.volume_range()?;
        Some(
            self.state
                .status
                .max_volume
                .map_or(range.max, |limit| limit.min(range.max)),
        )
    }

    async fn execute(&mut self, operation: &DeviceOperation) -> Result<(), String> {
        let unsupported =
            |what: &str| format!("{what} is not supported by this device ({})", self.model);
        match operation {
            DeviceOperation::SetVolume(_) if self.volume_range().is_none() => {
                return Err(unsupported("volume control"))
            }
            DeviceOperation::SetMute(_) if !self.features.has("mute") => {
                return Err(unsupported("mute"))
            }
            DeviceOperation::PowerOn | DeviceOperation::PowerStandby
                if !self.features.has("power") =>
            {
                return Err(unsupported("power control"))
            }
            DeviceOperation::SelectInput(_) if self.inputs.is_empty() => {
                return Err(unsupported("input selection"))
            }
            _ => {}
        }
        // The address may have been reassigned to another device since the last poll.
        self.verify_identity().await?;
        let path = protocol::encode_operation(
            operation,
            self.volume_range().as_ref(),
            self.volume_limit(),
            &self.inputs,
        )?;
        let body = get_text(&self.http, &self.host, self.port, &path).await?;
        protocol::check_response(&body)
    }

    async fn verify_identity(&mut self) -> Result<(), String> {
        let body = get_text(
            &self.http,
            &self.host,
            self.port,
            &protocol::device_info_path(),
        )
        .await?;
        let info = protocol::parse_device_info(&body)?;
        let id = info
            .hardware_id()
            .and_then(|hardware_id| DeviceId::new(KIND, &hardware_id));
        if id.as_ref() == Some(&self.device_id) {
            Ok(())
        } else {
            self.state.reachable = false;
            self.state.detail = Some(format!(
                "{} now answers as a different device; commands are refused",
                self.host
            ));
            self.publish();
            Err(format!(
                "{} no longer answers as {}; refusing to send",
                self.host, self.device_id
            ))
        }
    }

    /// Re-read features and input names. Failure keeps the last known catalog.
    async fn refresh_catalog(&mut self) {
        let Ok(body) = get_text(
            &self.http,
            &self.host,
            self.port,
            &protocol::features_path(),
        )
        .await
        else {
            return;
        };
        let Ok(features) = protocol::parse_features(&body) else {
            return;
        };
        let names = match get_text(
            &self.http,
            &self.host,
            self.port,
            &protocol::name_text_path(),
        )
        .await
        {
            Ok(body) => protocol::parse_input_names(&body).unwrap_or_default(),
            Err(_) => HashMap::new(),
        };
        self.inputs = protocol::build_inputs(&features, &names);
        self.features = features;
    }

    /// Read `main/getStatus`. Unreachable or refused: power becomes `Unknown`, never `standby`.
    async fn refresh(&mut self) {
        let result =
            match get_text(&self.http, &self.host, self.port, &protocol::status_path()).await {
                Ok(body) => protocol::parse_status(&body),
                Err(error) => Err(error),
            };
        match result {
            Ok(status) => {
                self.state.status = status;
                self.state.reachable = true;
                self.state.detail = None;
            }
            Err(error) => {
                self.state.status = MainStatus::default();
                self.state.reachable = false;
                self.state.detail = Some(format!(
                    "not answering ({error}); a unit in standby without network standby does not answer"
                ));
            }
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        capabilities_for(&self.model, &self.features)
    }

    fn publish(&self) {
        let status = &self.state.status;
        let range = self.volume_range();
        let volume = DeviceVolume {
            scale: range.map(|r| r.scale()).unwrap_or_default(),
            value: range.and(status.volume).map(|v| v as f32),
            min: range.map_or(0.0, |r| r.min as f32),
            hardware_max: self.volume_limit().map(|max| max as f32),
            ceiling: None,
            step: range.map_or(1.0, |r| r.step as f32),
            is_muted: if self.features.has("mute") {
                status.mute
            } else {
                None
            },
            display: status.actual_volume.as_ref().map(|actual| actual.display()),
        };
        let device = Device {
            device_id: self.device_id.clone(),
            vendor: "Yamaha".to_string(),
            model: self.model.clone(),
            name: self.name.clone(),
            host: self.host.clone(),
            discovery: self.discovery.clone(),
            maturity: DEVICE_CONTROL_MATURITY.to_string(),
            connection: DeviceConnection {
                control_channel: if self.state.reachable {
                    ControlChannel::Polling
                } else {
                    ControlChannel::None
                },
                detail: self.state.detail.clone(),
            },
            power: status.power.unwrap_or(DevicePower::Unknown),
            input: status.input.clone(),
            inputs: self.inputs.clone(),
            volume,
            capabilities: self.capabilities(),
            revision: next_revision(),
            last_updated: now_ms(),
        };
        self.bus.publish(BusEvent::DeviceObserved {
            device: Box::new(device),
        });
    }
}
