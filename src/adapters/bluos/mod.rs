//! BluOS (Bluesound, BluOS-equipped NAD modules) device adapter (#774). **Alpha.**
//!
//! A *device* adapter: it controls the player a zone feeds, not a music provider. It publishes
//! [`BusEvent::DeviceObserved`] snapshots and executes [`DeviceOperation`]s handed to it by
//! [`crate::devices::service::DeviceService`], which has already resolved, clamped and quantised
//! every level. This adapter never computes a level.
//!
//! # Discovery is not capability detection
//!
//! mDNS `_musc._tcp.local.` only nominates candidates. A player is accepted after `/SyncStatus`
//! answers with a MAC, which is its identity (never its address, name or model).
//!
//! # Volume, fixed output, power
//!
//! Volume is a 0-100 percentage. A player in fixed-output mode reports a negative volume: it then
//! has **no** volume (`value = None`, `volume.supported = false`) and volume and mute writes are
//! refused. Power is not universally available on BluOS, so `PowerOn`/`PowerStandby` return an
//! error rather than pretend.
//!
//! # State
//!
//! HTTP polling of `/Status` ([`ControlChannel::Polling`]); BluOS long-polling (`etag`) is not
//! used. Before every write the unit re-reads `/SyncStatus` and refuses if the address now
//! answers with a different MAC.
//!
//! Reference: <https://github.com/LouisChrist/pyblu>.

pub mod protocol;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio_util::sync::CancellationToken;

use crate::bus::{BusEvent, SharedBus, VolumeScale};
use crate::devices::service::{DeviceDriver, DeviceOperation};
use crate::devices::{
    ControlChannel, Device, DeviceCapabilities, DeviceConnection, DeviceId, DevicePower,
    DeviceVolume, DEVICE_CONTROL_MATURITY,
};
use protocol::{CaptureInput, Status, SyncStatus, VolumeSupport};

pub use protocol::capabilities_for;

/// Family name (`ManualDevice::family`) and settings/coordinator name.
pub const ADAPTER_NAME: &str = "bluos";
/// The device kind (id prefix) this family serves.
pub const KIND: &str = "bluos";

const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_secs(3);
/// Inputs and identity change rarely; refresh every this many polls.
const CATALOG_REFRESH_TICKS: u32 = 20;
const MANUAL_PROBE_INTERVAL: Duration = Duration::from_secs(60);
const READBACK_SCHEDULE: &[Duration] = &[
    Duration::from_millis(50),
    Duration::from_millis(400),
    Duration::from_millis(1000),
];

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

fn device_id_for(sync: &SyncStatus) -> Option<DeviceId> {
    DeviceId::new(KIND, &sync.mac)
}

struct Probed {
    device_id: DeviceId,
    sync: SyncStatus,
}

/// A candidate is a BluOS player only if `/SyncStatus` answers with a MAC.
async fn probe(http: &Client, host: &str, port: u16) -> Result<Probed, String> {
    let sync = protocol::parse_sync_status(
        &get_text(http, host, port, protocol::sync_status_path()).await?,
    )?;
    let device_id =
        device_id_for(&sync).ok_or_else(|| format!("{} is not a usable MAC", sync.mac))?;
    Ok(Probed { device_id, sync })
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
}

#[derive(Clone)]
pub struct BluosAdapter {
    bus: SharedBus,
    http: Client,
    manual: Arc<RwLock<Vec<(String, u16)>>>,
    state: Arc<RwLock<AdapterState>>,
}

impl BluosAdapter {
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

    /// Hosts to probe directly, for players mDNS cannot reach.
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
        tracing::info!("BluOS device adapter started (alpha)");
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
        tracing::info!("BluOS device adapter stopped");
    }

    /// mDNS browse plus a periodic probe of the configured hosts, in one task.
    async fn discovery_loop(&self, cancel: CancellationToken) {
        let daemon = match mdns_sd::ServiceDaemon::new() {
            Ok(daemon) => Some(daemon),
            Err(error) => {
                tracing::warn!("BluOS mDNS unavailable ({error}); manual hosts only");
                None
            }
        };
        let browse = daemon.as_ref().and_then(|daemon| {
            daemon
                .browse(protocol::MDNS_SERVICE)
                .map_err(|error| tracing::warn!("BluOS mDNS browse failed: {error}"))
                .ok()
        });
        let mut manual_tick = tokio::time::interval(MANUAL_PROBE_INTERVAL);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = manual_tick.tick() => self.probe_manual(&cancel).await,
                event = async {
                    match &browse {
                        Some(receiver) => receiver.recv_async().await.ok(),
                        None => std::future::pending().await,
                    }
                } => {
                    match event {
                        Some(mdns_sd::ServiceEvent::ServiceResolved(service)) => {
                            let port = service.get_port();
                            for address in service.get_addresses_v4() {
                                self.consider(&address.to_string(), port, None, "mdns", &cancel).await;
                            }
                        }
                        Some(_) => {}
                        // The browse channel closed: stop listening, keep manual probing.
                        None => tokio::time::sleep(Duration::from_secs(5)).await,
                    }
                }
            }
        }
        if let Some(daemon) = daemon {
            if let Err(error) = daemon.shutdown() {
                tracing::debug!("BluOS mDNS shutdown: {error}");
            }
        }
    }

    async fn probe_manual(&self, cancel: &CancellationToken) {
        for (host, port) in self.manual.read().await.clone() {
            if cancel.is_cancelled() {
                return;
            }
            self.consider(&host, port, None, "manual", cancel).await;
        }
    }

    /// Probe an address and, if it is a BluOS player, start (or re-host) its unit.
    async fn consider(
        &self,
        host: &str,
        port: u16,
        name: Option<String>,
        discovery: &str,
        cancel: &CancellationToken,
    ) {
        // A known unit already at this address needs no second probe.
        {
            let state = self.state.read().await;
            if state
                .units
                .values()
                .any(|unit| unit.host == host && unit.port == port)
            {
                return;
            }
        }
        match probe(&self.http, host, port).await {
            Ok(probed) => {
                self.ensure_unit(probed, host, port, name, discovery, cancel)
                    .await
            }
            Err(error) => tracing::debug!(%host, "BluOS candidate rejected: {error}"),
        }
    }

    async fn ensure_unit(
        &self,
        probed: Probed,
        host: &str,
        port: u16,
        name: Option<String>,
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
                    name,
                    discovery,
                    adapter_cancel.child_token(),
                )
                .await
            }
        }
    }

    /// Attach a player at a known address without multicast: a configured host on another subnet,
    /// or a mock player on an ephemeral port in tests. The player is probed first, exactly as a
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
        name: Option<String>,
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
        let name = name
            .or_else(|| (!probed.sync.name.is_empty()).then(|| probed.sync.name.clone()))
            .unwrap_or_else(|| probed.sync.display_model());
        tracing::info!(
            device_id = %probed.device_id,
            host,
            model = %probed.sync.display_model(),
            %name,
            "Discovered BluOS device"
        );
        let unit = Unit {
            bus: self.bus.clone(),
            http: self.http.clone(),
            host: host.to_string(),
            port,
            device_id: probed.device_id,
            vendor: if probed.sync.brand.is_empty() {
                "Bluesound".to_string()
            } else {
                probed.sync.brand.clone()
            },
            model: probed.sync.display_model(),
            name,
            discovery: discovery.to_string(),
            inputs: Vec::new(),
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
impl crate::adapters::devices::DeviceFamily for BluosAdapter {
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
            .map(|device| (device.host, device.port.unwrap_or(protocol::DEFAULT_PORT)))
            .collect();
        if !hosts.is_empty() {
            self.set_manual_hosts(hosts).await;
        }
        self.start_internal().await?;
        // Probe configured hosts now rather than waiting out the first interval.
        let adapter = self.clone();
        let cancel = self.state.read().await.cancel.clone();
        tokio::spawn(async move { adapter.probe_manual(&cancel).await });
        Ok(())
    }

    async fn stop_family(&self) {
        self.stop_internal().await
    }
}

#[async_trait]
impl DeviceDriver for BluosAdapter {
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

#[derive(Default)]
struct UnitState {
    reachable: bool,
    detail: Option<String>,
    status: Option<Status>,
    support: Option<VolumeSupport>,
}

struct Unit {
    bus: SharedBus,
    http: Client,
    host: String,
    port: u16,
    device_id: DeviceId,
    vendor: String,
    model: String,
    name: String,
    discovery: String,
    inputs: Vec<CaptureInput>,
    state: UnitState,
    cancel: CancellationToken,
}

impl Unit {
    async fn run(mut self, mut commands: mpsc::Receiver<UnitCommand>, cancel: CancellationToken) {
        let mut poll = tokio::time::interval(POLL_INTERVAL);
        let mut ticks: u32 = 0;
        self.refresh_catalog().await;
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
                            self.read_back(&operation, sent).await;
                        }
                    }
                }
                _ = poll.tick() => {
                    ticks = ticks.wrapping_add(1);
                    if ticks.is_multiple_of(CATALOG_REFRESH_TICKS) || !self.state.reachable {
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
        let Some(status) = &self.state.status else {
            return false;
        };
        match operation {
            DeviceOperation::SetVolume(level) => status
                .volume
                .is_some_and(|observed| (observed as f32 - level).abs() < 0.5),
            DeviceOperation::SetMute(muted) => status.muted == Some(*muted),
            DeviceOperation::SelectInput(id) => {
                self.current_input().as_deref() == Some(id.as_str())
            }
            // Power is never executed, so it is never awaited.
            DeviceOperation::PowerOn | DeviceOperation::PowerStandby => false,
        }
    }

    fn support(&self) -> VolumeSupport {
        self.state.support.unwrap_or(VolumeSupport::Unknown)
    }

    /// The current input's id, when the player is on one of its Capture inputs.
    fn current_input(&self) -> Option<String> {
        let current = self.state.status.as_ref()?.input_id.as_deref()?;
        self.inputs
            .iter()
            .find(|input| input.id == current)
            .map(|input| input.id.clone())
    }

    async fn execute(&mut self, operation: &DeviceOperation) -> Result<(), String> {
        // Refuse what can never work before touching the network (power, fixed volume).
        let path = protocol::encode_operation(operation, self.support(), &self.inputs)?;
        // The address may have been reassigned to another player since the last poll.
        self.verify_identity().await?;
        let body = get_text(&self.http, &self.host, self.port, &path).await?;
        protocol::check_command_response(&body)
    }

    async fn verify_identity(&mut self) -> Result<(), String> {
        let body = get_text(
            &self.http,
            &self.host,
            self.port,
            protocol::sync_status_path(),
        )
        .await?;
        let sync = protocol::parse_sync_status(&body)?;
        if device_id_for(&sync).as_ref() == Some(&self.device_id) {
            Ok(())
        } else {
            self.state.reachable = false;
            self.state.detail = Some(format!(
                "{} now answers as a different player; commands are refused",
                self.host
            ));
            self.publish();
            Err(format!(
                "{} no longer answers as {}; refusing to send",
                self.host, self.device_id
            ))
        }
    }

    /// Re-read the Capture input list. Failure keeps the last known list.
    async fn refresh_catalog(&mut self) {
        if let Ok(body) = get_text(&self.http, &self.host, self.port, protocol::inputs_path()).await
        {
            if let Ok(inputs) = protocol::parse_inputs(&body) {
                self.inputs = inputs;
            }
        }
    }

    async fn refresh(&mut self) {
        let result =
            match get_text(&self.http, &self.host, self.port, protocol::status_path()).await {
                Ok(body) => protocol::parse_status(&body),
                Err(error) => Err(error),
            };
        match result {
            Ok(status) => {
                self.state.support = Some(status.volume_support);
                self.state.status = Some(status);
                self.state.reachable = true;
                self.state.detail = None;
            }
            Err(error) => {
                self.state.status = None;
                self.state.reachable = false;
                self.state.detail = Some(format!("not answering ({error})"));
            }
        }
    }

    fn capabilities(&self) -> DeviceCapabilities {
        capabilities_for(&self.model, self.support(), !self.inputs.is_empty())
    }

    fn publish(&self) {
        let status = self.state.status.as_ref();
        let variable = self.support() == VolumeSupport::Variable;
        let volume = DeviceVolume {
            scale: VolumeScale::Percentage,
            value: status
                .filter(|_| variable)
                .and_then(|s| s.volume)
                .map(|v| v as f32),
            min: protocol::MIN_LEVEL as f32,
            hardware_max: variable.then_some(protocol::MAX_LEVEL as f32),
            ceiling: None,
            step: 1.0,
            is_muted: status.filter(|_| variable).and_then(|s| s.muted),
            display: None,
        };
        let device = Device {
            device_id: self.device_id.clone(),
            vendor: self.vendor.clone(),
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
            // BluOS has no universal power state: never rounded to `standby`.
            power: DevicePower::Unknown,
            input: self.current_input(),
            inputs: protocol::catalog(&self.inputs),
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
