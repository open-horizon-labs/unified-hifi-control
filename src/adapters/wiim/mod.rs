//! WiiM (LinkPlay) device adapter (#774). **Alpha.**
//!
//! A *device* adapter: it controls the streamer's own volume, mute and input, never a zone.
//! It publishes [`BusEvent::DeviceObserved`] snapshots and executes the already-resolved,
//! already-clamped [`DeviceOperation`]s the [`crate::devices::service::DeviceService`] hands it.
//!
//! # Protocol
//!
//! The LinkPlay HTTP API (`/httpapi.asp?command=...`, see [`protocol`]). It is polled: there is no
//! push channel here, so the control channel is [`ControlChannel::Polling`]. Devices serve it over
//! HTTPS (port 443, self-signed certificate) and older firmware over HTTP (port 80); this adapter
//! tries HTTPS first. The certificate is not validated because the device cannot present a
//! verifiable one, the traffic never leaves the LAN, and it carries no secret.
//!
//! # No power
//!
//! LinkPlay has no power command. Power operations return an error; pause is never reported as
//! standby, and the device's `power` stays `unknown`.
//!
//! # Discovery
//!
//! mDNS `_linkplay._tcp.local.` and configured manual hosts. A host is published only after
//! `getStatusEx` answers with an identity, so "discovered" means "answers the control protocol".
//! Identity is the status `uuid` (else `MAC`), never the model, name or IP.

pub mod protocol;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio_util::sync::CancellationToken;

use crate::adapters::devices::{DeviceFamily, FamilyContext};
use crate::bus::{BusEvent, SharedBus, VolumeScale};
use crate::devices::links::ManualDevice;
use crate::devices::service::{DeviceDriver, DeviceOperation};
use crate::devices::{
    CapabilityEvidence, ControlChannel, Device, DeviceCapabilities, DeviceCapability,
    DeviceConnection, DeviceId, DevicePower, DeviceVolume, DEVICE_CONTROL_MATURITY,
};
use protocol::{PlayerStatus, StatusEx};

/// Family / settings name.
pub const ADAPTER_NAME: &str = "wiim";

const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_secs(3);
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(60);
/// After a write, read back on this schedule so the confirmation sees the applied state.
const READBACK_SCHEDULE: &[Duration] = &[
    Duration::from_millis(150),
    Duration::from_millis(500),
    Duration::from_millis(1200),
];

/// Capability claims. All implementation-derived: no WiiM hardware has been exercised.
/// Models whose read path (identity, level, mute, `max_volume`) was exercised on real units,
/// with captures in the named fixture directory (`tests/device_claims.rs` checks it exists).
pub const READ_VERIFIED_MODELS: &[(&str, &str)] = &[
    ("WiiM Pro with gc4a", "tests/fixtures/devices/wiim"),
    ("WiiM Pro No MFI", "tests/fixtures/devices/wiim"),
    ("WiiM Ultra", "tests/fixtures/devices/wiim"),
    ("WiiM Amp Pro", "tests/fixtures/devices/wiim"),
    ("WiiM Pro Plus", "tests/fixtures/devices/wiim"),
];

/// Models whose writes were exercised on a real unit through `DeviceService` (`tests/wiim_live.rs`):
/// volume set/no-op/restore, mute on/off, and input Network → Bluetooth → Network, each confirmed by
/// readback.
pub const WRITE_VERIFIED_MODELS: &[(&str, &str)] = &[("WiiM Ultra", "tests/fixtures/devices/wiim")];

pub fn capabilities_for(model: &str) -> DeviceCapabilities {
    let read_verified = READ_VERIFIED_MODELS.iter().any(|(m, _)| *m == model);
    let write_verified = WRITE_VERIFIED_MODELS.iter().any(|(m, _)| *m == model);
    let model_note = if model.is_empty() {
        String::new()
    } else {
        format!(" ({model})")
    };
    let evidence = |verified: bool| {
        if verified {
            CapabilityEvidence::HardwareVerified
        } else {
            CapabilityEvidence::ImplementationDerived
        }
    };
    let claim = |supported: bool, detail: String| DeviceCapability {
        supported,
        evidence: CapabilityEvidence::ImplementationDerived,
        detail,
    };
    DeviceCapabilities {
        observe: DeviceCapability {
            evidence: evidence(read_verified),
            ..claim(
                true,
                format!(
                    "LinkPlay HTTP API getStatusEx/getPlayerStatus polled every 3 s{model_note}; {}",
                    if read_verified {
                        "captured on this model (tests/fixtures/devices/wiim)"
                    } else {
                        "not exercised on this model"
                    }
                ),
            )
        },
        volume: DeviceCapability {
            evidence: evidence(write_verified),
            ..claim(
            true,
            "setPlayerCmd:vol:N, N in 0-100 (percentage). The device's max_volume setting (else 100) is the maximum; a user ceiling lowers it; fixed-output mode is not detectable over this API"
                .to_string(),
            )
        },
        mute: DeviceCapability {
            evidence: evidence(write_verified),
            ..claim(true, "setPlayerCmd:mute:1|0".to_string())
        },
        power: claim(
            false,
            "WiiM/LinkPlay has no power command; pause is transport, not power, and is never \
             reported as standby"
                .to_string(),
        ),
        input_select: DeviceCapability {
            evidence: evidence(write_verified),
            ..claim(
            true,
            "setPlayerCmd:switchmode:<input>. The input catalog is per model (vendor SDK); a \
             physical input with nothing connected may not report as selected, so it stays \
             unconfirmed rather than succeeding"
                .to_string(),
            )
        },
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
    Rehost(Vec<String>),
}

struct UnitHandle {
    /// Base URLs the unit is reached at, to detect an address change.
    bases: Vec<String>,
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
pub struct WiimAdapter {
    bus: SharedBus,
    http: Client,
    manual: Arc<RwLock<Vec<ManualDevice>>>,
    state: Arc<RwLock<AdapterState>>,
}

/// Candidate base URLs for a host: HTTPS first (current firmware), then plain HTTP.
fn candidate_bases(host: &str, port: Option<u16>) -> Vec<String> {
    match port {
        Some(port) => vec![
            format!("http://{host}:{port}"),
            format!("https://{host}:{port}"),
        ],
        None => vec![format!("https://{host}"), format!("http://{host}")],
    }
}

impl WiimAdapter {
    pub fn new(bus: SharedBus) -> Self {
        Self {
            bus,
            // Self-signed device certificate; see the module docs.
            http: Client::builder()
                .timeout(HTTP_TIMEOUT)
                .danger_accept_invalid_certs(true)
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
        tracing::info!("WiiM device adapter started (alpha)");
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
        tracing::info!("WiiM device adapter stopped");
    }

    async fn discovery_loop(&self, cancel: CancellationToken) {
        let daemon = match mdns_sd::ServiceDaemon::new() {
            Ok(daemon) => Some(daemon),
            Err(error) => {
                tracing::warn!("WiiM mDNS unavailable, using manual hosts only: {error}");
                None
            }
        };
        let browse = daemon.as_ref().and_then(|daemon| {
            daemon
                .browse(protocol::MDNS_SERVICE)
                .map_err(|error| tracing::warn!("WiiM mDNS browse failed: {error}"))
                .ok()
        });
        let mut ticker = tokio::time::interval(DISCOVERY_INTERVAL);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = ticker.tick() => self.probe_manual(&cancel).await,
                event = next_mdns_event(&browse) => match event {
                    Some(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                        for address in info.get_addresses_v4() {
                            if cancel.is_cancelled() {
                                break;
                            }
                            self.probe(&address.to_string(), None, "mdns", &cancel).await;
                        }
                    }
                    Some(_) => {}
                    // The daemon's channel closed: stop browsing, keep the manual ticker.
                    None => tokio::time::sleep(Duration::from_secs(5)).await,
                },
            }
        }
        if let Some(daemon) = daemon {
            if let Err(error) = daemon.shutdown() {
                tracing::debug!("WiiM mDNS shutdown: {error}");
            }
        }
    }

    async fn probe_manual(&self, cancel: &CancellationToken) {
        for device in self.manual.read().await.clone() {
            if cancel.is_cancelled() {
                return;
            }
            self.probe(&device.host, device.port, "manual", cancel)
                .await;
        }
    }

    /// Ask a host for `getStatusEx`; publish a unit only when it identifies itself.
    async fn probe(
        &self,
        host: &str,
        port: Option<u16>,
        discovery: &str,
        cancel: &CancellationToken,
    ) {
        let bases = candidate_bases(host, port);
        let Some((status, base)) = self.identify(&bases).await else {
            return;
        };
        let Some(device_id) = status
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
                .filter(|unit| unit.bases != bases)
                .map(|unit| unit.commands.clone())
        };
        if let Some(commands) = rehost {
            if commands
                .try_send(UnitCommand::Rehost(bases.clone()))
                .is_ok()
            {
                if let Some(unit) = self.state.write().await.units.get_mut(device_id.as_str()) {
                    unit.bases = bases;
                }
            }
            return;
        }
        self.spawn_unit(
            &device_id,
            bases,
            base,
            status,
            discovery,
            cancel.child_token(),
        )
        .await;
    }

    async fn identify(&self, bases: &[String]) -> Option<(StatusEx, String)> {
        for base in bases {
            let url = format!("{base}{}", protocol::STATUS_EX_PATH);
            let Ok(response) = self.http.get(&url).send().await else {
                continue;
            };
            if !response.status().is_success() {
                continue;
            }
            let Ok(body) = response.text().await else {
                continue;
            };
            if let Some(status) = protocol::parse_status_ex(&body) {
                return Some((status, base.clone()));
            }
        }
        None
    }

    /// Attach a unit at a known base URL without discovery: a configured host, or a mock device
    /// on an ephemeral port in tests (`base` like `http://127.0.0.1:41234`). The unit runs until
    /// the adapter stops.
    pub async fn attach_unit(&self, base: &str, status: StatusEx) -> Option<DeviceId> {
        let device_id = DeviceId::new(ADAPTER_NAME, &status.hardware_id()?)?;
        let cancel = {
            let mut state = self.state.write().await;
            state.running = true;
            state.cancel.child_token()
        };
        self.spawn_unit(
            &device_id,
            vec![base.to_string()],
            base.to_string(),
            status,
            "manual",
            cancel,
        )
        .await;
        Some(device_id)
    }

    async fn spawn_unit(
        &self,
        device_id: &DeviceId,
        bases: Vec<String>,
        active: String,
        status: StatusEx,
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
                bases: bases.clone(),
                commands,
                cancel: cancel.clone(),
            },
        );
        drop(state);
        tracing::info!(%device_id, base = %active, model = %status.model(), "Discovered WiiM device");
        let unit = Unit {
            bus: self.bus.clone(),
            http: self.http.clone(),
            bases,
            active,
            device_id: device_id.clone(),
            model: status.model(),
            max_volume: status.max_volume,
            name: status.name,
            discovery: discovery.to_string(),
            player: None,
            reachable: false,
            detail: None,
        };
        tokio::spawn(unit.run(receiver, cancel));
    }

    pub async fn device_count(&self) -> usize {
        self.state.read().await.units.len()
    }
}

/// Next mDNS event, or pends forever without a browse.
async fn next_mdns_event(
    browse: &Option<mdns_sd::Receiver<mdns_sd::ServiceEvent>>,
) -> Option<mdns_sd::ServiceEvent> {
    match browse {
        Some(receiver) => receiver.recv_async().await.ok(),
        None => std::future::pending().await,
    }
}

#[async_trait]
impl DeviceFamily for WiimAdapter {
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
impl DeviceDriver for WiimAdapter {
    fn kinds(&self) -> &'static [&'static str] {
        &["wiim"]
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

struct Unit {
    bus: SharedBus,
    http: Client,
    bases: Vec<String>,
    /// The base URL that last answered.
    active: String,
    device_id: DeviceId,
    model: String,
    /// The device's own maximum-volume setting, when it reports one.
    max_volume: Option<f32>,
    name: String,
    discovery: String,
    player: Option<PlayerStatus>,
    reachable: bool,
    detail: Option<String>,
}

impl Unit {
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
                        UnitCommand::Rehost(bases) => {
                            tracing::info!(device_id = %self.device_id, "WiiM device moved to a new address");
                            if let Some(first) = bases.first() {
                                self.active = first.clone();
                            }
                            self.bases = bases;
                        }
                        UnitCommand::Execute { operation, reply } => {
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

    /// GET a path, trying the base that last worked first. Remembers the base that answers.
    async fn get(&mut self, path: &str) -> Result<String, String> {
        let mut order = vec![self.active.clone()];
        order.extend(self.bases.iter().filter(|b| **b != self.active).cloned());
        let mut last = "no address configured".to_string();
        for base in order {
            match self.http.get(format!("{base}{path}")).send().await {
                Ok(response) if response.status().is_success() => match response.text().await {
                    Ok(body) => {
                        self.active = base;
                        return Ok(body);
                    }
                    Err(error) => last = format!("reading reply: {error}"),
                },
                Ok(response) => last = format!("HTTP {}", response.status()),
                Err(error) => last = error.to_string(),
            }
        }
        Err(last)
    }

    async fn execute(&mut self, operation: &DeviceOperation) -> Result<(), String> {
        let path = protocol::command_path(operation)?;
        let body = self
            .get(&path)
            .await
            .map_err(|error| format!("command failed: {error}"))?;
        if protocol::command_accepted(&body) {
            Ok(())
        } else {
            Err(format!("the device refused the command: {}", body.trim()))
        }
    }

    /// Read `getPlayerStatus`; keeps the previous values (and marks the unit unreachable) on a
    /// failed or unparseable reply.
    async fn refresh(&mut self) {
        match self.get(protocol::PLAYER_STATUS_PATH).await {
            Ok(body) => match protocol::parse_player_status(&body) {
                Some(player) => {
                    self.reachable = true;
                    self.detail = None;
                    let new_unknown = player.unknown_mode.filter(|mode| {
                        self.player.as_ref().and_then(|p| p.unknown_mode) != Some(*mode)
                    });
                    if let Some(mode) = new_unknown {
                        crate::devices::anomalies::report(
                            &self.bus,
                            crate::devices::anomalies::DeviceAnomaly::new(
                                self.device_id.clone(),
                                crate::devices::anomalies::AnomalyKind::UnknownValue,
                                format!(
                                    "getPlayerStatus mode {mode} maps to no documented input \
                                     ({})",
                                    self.model
                                ),
                            )
                            .with_excerpt(body.clone()),
                        );
                    }
                    self.player = Some(player);
                }
                None => {
                    self.reachable = true;
                    self.detail = Some("getPlayerStatus returned an unreadable reply".to_string());
                }
            },
            Err(error) => {
                self.reachable = false;
                self.detail = Some(format!("unreachable: {error}"));
            }
        }
    }

    fn publish(&self) {
        let player = self.player.clone().unwrap_or_default();
        let host = self
            .active
            .split("://")
            .nth(1)
            .unwrap_or(&self.active)
            .split('/')
            .next()
            .unwrap_or_default()
            .to_string();
        let device = Device {
            device_id: self.device_id.clone(),
            vendor: "WiiM".to_string(),
            model: self.model.clone(),
            name: self.name.clone(),
            host,
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
            // No power model on LinkPlay: never inferred from playback state.
            power: DevicePower::Unknown,
            input: player.input,
            inputs: protocol::input_catalog_for(&self.model),
            volume: DeviceVolume {
                scale: VolumeScale::Percentage,
                value: player.volume,
                min: protocol::MIN_VOLUME,
                // The device's own `max_volume` setting, else the protocol's full scale (100).
                // A user ceiling lowers it further.
                hardware_max: Some(self.max_volume.unwrap_or(100.0)),
                ceiling: None,
                step: 1.0,
                is_muted: player.muted,
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
