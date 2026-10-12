//! Eversolo device adapter (#774). **Alpha.**
//!
//! A *device* adapter: it controls the streamer/DAC a zone feeds, not a music provider. It never
//! publishes zones; it publishes [`BusEvent::DeviceObserved`] snapshots and executes
//! [`DeviceOperation`]s handed to it by [`crate::devices::service::DeviceService`].
//!
//! # Protocol
//!
//! Eversolo's control center is Zidoo's platform: plain HTTP GET with JSON replies on port 9529.
//! The vendor PDF documents only the remote-key call; the state and set endpoints are established
//! from community clients (every source and the endpoint map are listed in [`protocol`]). **No
//! Eversolo hardware has been available**: every capability claim is implementation-derived, and
//! the JSON fixtures are real captures from other people's units (provenance in
//! `tests/fixtures/devices/eversolo/SOURCES.md`).
//!
//! # Behaviour
//!
//! - **State**: `getState` (volume, mute, device info) and `getInputAndOutputList` (inputs,
//!   current input) are polled every few seconds and republished with a fresh revision on every
//!   readback, changed or not.
//! - **Identity**: the device-reported LAN `net_mac` (`getState.deviceInfo` / `getModel`), never an
//!   IP. A configured MAC that disagrees with what the host reports stops the unit from being
//!   controlled (the address now belongs to another unit).
//! - **Volume**: the device's native integer steps, `0..maxVolume` (200 on every captured unit),
//!   published as [`VolumeScale::Steps`]. The `display` string is dB on a DMP-A8 Gen 2 and a plain
//!   number on a PLAY, so no dB meaning is assumed. `hardware_max` is the device's `maxVolume`.
//! - **Mute**: explicit `setMuteVolume?isMute=0|1`, confirmed by `isMute`.
//! - **Input**: `setInputList?tag=&index=`, selected by the device's tag (`XMOS`, `BT`, `SPDIF`, ...).
//! - **Power off**: `setPowerOption?tag=poweroff` (falling back to the PDF's `Key.Poweroff`). While
//!   off the unit does not answer HTTP, so `standby` is reported only when the unit stopped
//!   answering *after* UHC's own accepted power-off; any other silence is `unknown`. A network
//!   failure right after a power-off would look the same: that is the documented caveat.
//! - **Power on**: Wake-on-LAN only (no HTTP path exists). Sending a magic packet is not
//!   confirmation; the unit is then polled quickly until it answers.
//! - **Discovery**: SSDP `MediaRenderer:1` whose descriptor manufacturer is `EVERSOLO` (the
//!   matcher the Home Assistant integration uses), then validated by a `getState` answer; plus
//!   configured devices.
//! - Only reads are sent on connect and reconnect.

pub mod protocol;

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::bus::{BusEvent, SharedBus, VolumeScale};
use crate::devices::links::ManualDevice;
use crate::devices::service::{DeviceDriver, DeviceOperation};
use crate::devices::{
    CapabilityEvidence, ControlChannel, Device, DeviceCapabilities, DeviceCapability,
    DeviceConnection, DeviceId, DeviceInput, DevicePower, DeviceVolume, DEVICE_CONTROL_MATURITY,
};

/// Settings/coordinator name (`ManualDevice::family`).
pub const ADAPTER_NAME: &str = "eversolo";

const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const OFFLINE_POLL_INTERVAL: Duration = Duration::from_secs(10);
/// Polling cadence while waiting for a woken unit to boot, and for how long.
const FAST_POLL_INTERVAL: Duration = Duration::from_secs(2);
const FAST_POLL_WINDOW: Duration = Duration::from_secs(120);
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(30);
/// Readbacks after a write: how many, and the gap before each.
const READBACK_ATTEMPTS: u32 = 4;
const READBACK_GAP: Duration = Duration::from_millis(250);
const POWER_OFF_READBACK_GAP: Duration = Duration::from_secs(1);
/// A power-off inference older than this is dropped when the unit answers again.
const OFF_LATCH_MAX: Duration = Duration::from_secs(60);

/// Per-model facts from the community sources. Caveats only, never behaviour.
fn model_note(model: &str) -> String {
    let m = model.to_ascii_uppercase();
    if m.starts_with("DMP-A6") {
        "DMP-A6: HDMI output and 6 inputs seen on firmware v1.5.46 (mase1981/uc-intg-eversolo \
         DISCOVERY_ANALYSIS.md); the UPnP descriptor in andrewdunndev/narjo-eversolo-upnp is a \
         DMP-A6"
            .to_string()
    } else if m.starts_with("DMP-A8") {
        "DMP-A8: the real JSON captures behind this adapter's fixtures (tmeynell/ha-eversolo, \
         DMP-A8 Gen 2, firmware v1.1.50/v1.1.80); display string is dB there"
            .to_string()
    } else if m.starts_with("DMP-A10") {
        "DMP-A10: pure DAC with 11 inputs and a knob LED that accepts commands but does nothing \
         (mase1981/uc-intg-eversolo DISCOVERY_ANALYSIS.md)"
            .to_string()
    } else if m.starts_with("PLAY") {
        "PLAY: only SDK test data (tomekceszke/eversolo-sdk, firmware v1.5.75); its display \
         string is a plain number, not dB"
            .to_string()
    } else {
        format!(
            "model {model:?} has no community capture this adapter was written from (T8 and \
             other lines are unverified; tmeynell/ha-eversolo refuses non-DMP-A models for the \
             same reason)"
        )
    }
}

/// Capability claims for one model and one set of facts read from the unit. Pure.
///
/// `volume_adjustable` is the device's own flag (`isVolumeEnable`, not `isLock`, with a level
/// and a maximum); `can_wake` is `getModel.ableRemoteBoot` with a known MAC. Both are `false`
/// until read. Every claim is implementation-derived from community clients, not from a device.
pub fn capabilities_for(
    model: &str,
    volume_adjustable: bool,
    can_wake: bool,
) -> DeviceCapabilities {
    let note = model_note(model);
    let derived = |supported: bool, detail: String| DeviceCapability {
        supported,
        evidence: CapabilityEvidence::ImplementationDerived,
        detail: format!("{detail}; {note}"),
    };
    DeviceCapabilities {
        observe: derived(
            true,
            "polls GET /ZidooMusicControl/v2/getState and getInputAndOutputList on port 9529 \
             every few seconds"
                .to_string(),
        ),
        volume: derived(
            volume_adjustable,
            if volume_adjustable {
                "absolute GET setDevicesVolume?volume=N in native steps 0..maxVolume (200 on \
                 every captured unit), read back from getState volumeData.currenttVolume"
                    .to_string()
            } else {
                "not available until the unit reports an adjustable level: getState \
                 volumeData needs isVolumeEnable true, isLock false and a maxVolume"
                    .to_string()
            },
        ),
        mute: derived(
            volume_adjustable,
            "explicit GET setMuteVolume?isMute=1|0, read back from volumeData.isMute; offered \
             only while volume is adjustable (a fixed-volume unit's mute is unverified)"
                .to_string(),
        ),
        power: derived(
            true,
            format!(
                "off: GET setPowerOption?tag=poweroff (fallback Key.Poweroff); standby is \
                 inferred only when the unit stops answering after UHC's own accepted power-off. \
                 On: Wake-on-LAN (UDP 9 and 9517) with the unit's MAC, currently {}; a magic \
                 packet is not confirmation",
                if can_wake {
                    "available (ableRemoteBoot true)"
                } else {
                    "not available (ableRemoteBoot false, or no MAC known yet)"
                }
            ),
        ),
        input_select: derived(
            true,
            "GET setInputList?tag=<tag>&index=<n> using the tags and indexes from \
             getInputAndOutputList; the current input is its inputIndex. Hidden inputs are not \
             listed by the device"
                .to_string(),
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

/// Why a read failed: `unreachable` is a transport failure (nothing answered), otherwise the
/// unit answered with something unusable.
#[derive(Debug)]
struct ReadError {
    unreachable: bool,
    detail: String,
}

async fn get(http: &Client, host: &str, port: u16, path: &str) -> Result<(u16, String), ReadError> {
    let url = protocol::endpoint_url(host, port, path).ok_or_else(|| ReadError {
        unreachable: false,
        detail: format!("unusable host {host:?}"),
    })?;
    let response = http.get(&url).send().await.map_err(|e| ReadError {
        unreachable: true,
        detail: format!("could not reach {host}:{port}: {e}"),
    })?;
    let status = response.status().as_u16();
    let body = response.text().await.map_err(|e| ReadError {
        unreachable: true,
        detail: format!("reading the reply failed: {e}"),
    })?;
    Ok((status, body))
}

async fn get_ok(http: &Client, host: &str, port: u16, path: &str) -> Result<String, ReadError> {
    let (status, body) = get(http, host, port, path).await?;
    if protocol::status_accepted(status) {
        Ok(body)
    } else {
        Err(ReadError {
            // A 5xx means the control service is not serving (a unit shutting down or booting).
            unreachable: status >= 500,
            detail: format!("HTTP {status} from {path}"),
        })
    }
}

async fn fetch_state(
    http: &Client,
    host: &str,
    port: u16,
) -> Result<protocol::StateReading, ReadError> {
    let body = get_ok(http, host, port, protocol::STATE_PATH).await?;
    protocol::parse_state(&body).map_err(|detail| ReadError {
        unreachable: false,
        detail,
    })
}

async fn fetch_model(http: &Client, host: &str, port: u16) -> Option<protocol::ModelInfo> {
    for path in protocol::MODEL_PATHS {
        if let Ok(body) = get_ok(http, host, port, path).await {
            if let Ok(model) = protocol::parse_model(&body) {
                return Some(model);
            }
        }
    }
    None
}

/// A unit that answered: state, model (when `getModel` answered) and the identity they report.
struct Probed {
    device_id: DeviceId,
}

/// Discovery is not capability detection: the host must answer `getState` and report a MAC.
async fn probe(http: &Client, host: &str, port: u16) -> Result<Probed, String> {
    let reading = fetch_state(http, host, port).await.map_err(|e| e.detail)?;
    let mac = match reading.mac {
        Some(mac) => Some(mac),
        None => fetch_model(http, host, port).await.and_then(|m| m.net_mac),
    };
    let hardware_id = mac
        .as_deref()
        .and_then(protocol::hardware_id_from_mac)
        .ok_or("the unit reports no usable MAC")?;
    let device_id = DeviceId::new(ADAPTER_NAME, &hardware_id).ok_or("unusable hardware id")?;
    Ok(Probed { device_id })
}

/// Send the magic packet to every target. Ok when at least one send succeeded.
async fn send_magic_packet(mac: &str, targets: &[SocketAddr]) -> Result<(), String> {
    let packet = protocol::magic_packet(mac).ok_or_else(|| format!("{mac:?} is not a MAC"))?;
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| format!("could not open a UDP socket: {e}"))?;
    socket
        .set_broadcast(true)
        .map_err(|e| format!("could not enable broadcast: {e}"))?;
    let mut sent = 0;
    let mut last_error = String::from("no Wake-on-LAN target");
    for target in targets {
        match socket.send_to(&packet, target).await {
            Ok(_) => sent += 1,
            Err(e) => last_error = format!("send to {target} failed: {e}"),
        }
    }
    if sent > 0 {
        Ok(())
    } else {
        Err(last_error)
    }
}

/// Options for attaching a unit directly (tests, or a known address).
#[derive(Debug, Clone, Default)]
pub struct UnitOptions {
    /// Where Wake-on-LAN packets go. `None` derives the host's /24 and global broadcast.
    pub wol_targets: Option<Vec<SocketAddr>>,
}

enum UnitCommand {
    Execute {
        operation: DeviceOperation,
        reply: oneshot::Sender<Result<(), String>>,
    },
}

struct UnitHandle {
    host: String,
    port: u16,
    commands: mpsc::Sender<UnitCommand>,
}

#[derive(Default)]
struct AdapterState {
    running: bool,
    cancel: CancellationToken,
    units: HashMap<String, UnitHandle>,
    /// SSDP locations already judged not to be an Eversolo.
    rejected_locations: HashSet<String>,
}

#[derive(Clone)]
pub struct EversoloAdapter {
    bus: SharedBus,
    http: Client,
    manual: Arc<RwLock<Vec<ManualDevice>>>,
    state: Arc<RwLock<AdapterState>>,
}

impl EversoloAdapter {
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

    /// Configured hosts, used alongside SSDP discovery.
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
        tracing::info!("Eversolo device adapter started (alpha)");
        Ok(())
    }

    async fn stop_internal(&self) {
        let units = {
            let mut state = self.state.write().await;
            state.running = false;
            state.cancel.cancel();
            state.rejected_locations.clear();
            std::mem::take(&mut state.units)
        };
        for device_id in units.keys() {
            if let Some(device_id) = DeviceId::parse(device_id) {
                self.bus.publish(BusEvent::DeviceRemoved { device_id });
            }
        }
        tracing::info!("Eversolo device adapter stopped");
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
        if let Ok(urn) = "urn:schemas-upnp-org:device:MediaRenderer:1".parse::<ssdp_client::URN>() {
            let target = ssdp_client::SearchTarget::URN(urn);
            match ssdp_client::search(&target, Duration::from_secs(2), 2, None).await {
                Ok(responses) => {
                    futures::pin_mut!(responses);
                    while let Some(response) = responses.next().await {
                        if let Ok(response) = response {
                            locations.push(response.location().to_string());
                        }
                    }
                }
                Err(error) => tracing::debug!("Eversolo SSDP search failed: {error}"),
            }
        }
        locations.sort();
        locations.dedup();
        locations
    }

    async fn is_attached(&self, device_id: Option<&DeviceId>, host: &str, port: u16) -> bool {
        let state = self.state.read().await;
        device_id.is_some_and(|id| state.units.contains_key(id.as_str()))
            || state
                .units
                .values()
                .any(|unit| unit.host == host && unit.port == port)
    }

    async fn discover_once(&self, cancel: &CancellationToken) {
        for location in self.ssdp_locations().await {
            if cancel.is_cancelled() {
                return;
            }
            self.examine_location(&location, cancel).await;
        }
        for manual in self.manual.read().await.clone() {
            if cancel.is_cancelled() {
                return;
            }
            let port = manual.port.unwrap_or(protocol::DEFAULT_PORT);
            let configured = manual
                .mac
                .as_deref()
                .and_then(protocol::hardware_id_from_mac)
                .and_then(|id| DeviceId::new(ADAPTER_NAME, &id));
            if self
                .is_attached(configured.as_ref(), &manual.host, port)
                .await
            {
                continue;
            }
            // A configured MAC is enough to attach a unit that is currently off (the unit then
            // verifies it against what the host reports). Without one the host must answer.
            let device_id = match configured {
                Some(id) => id,
                None => match probe(&self.http, &manual.host, port).await {
                    Ok(probed) => probed.device_id,
                    Err(error) => {
                        tracing::debug!(host = %manual.host, "manual Eversolo probe failed: {error}");
                        continue;
                    }
                },
            };
            self.spawn_unit(
                &device_id,
                &manual.host,
                port,
                "manual",
                UnitOptions::default(),
                cancel.child_token(),
            )
            .await;
        }
    }

    /// Fetch and judge one SSDP `LOCATION`. Non-Eversolo answers are remembered.
    async fn examine_location(&self, location: &str, cancel: &CancellationToken) {
        if self
            .state
            .read()
            .await
            .rejected_locations
            .contains(location)
        {
            return;
        }
        let Some(host) = reqwest::Url::parse(location)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
        else {
            return;
        };
        if self.is_attached(None, &host, protocol::DEFAULT_PORT).await {
            return;
        }
        let body = match self.http.get(location).send().await {
            Ok(response) => response.text().await.ok(),
            Err(_) => None,
        };
        let Some(body) = body else { return };
        if !protocol::parse_description(&body).is_some_and(|d| d.is_eversolo()) {
            self.state
                .write()
                .await
                .rejected_locations
                .insert(location.to_string());
            return;
        }
        match probe(&self.http, &host, protocol::DEFAULT_PORT).await {
            Ok(probed) => {
                self.spawn_unit(
                    &probed.device_id,
                    &host,
                    protocol::DEFAULT_PORT,
                    "ssdp",
                    UnitOptions::default(),
                    cancel.child_token(),
                )
                .await;
            }
            // Retried next cycle: the unit may be waking.
            Err(error) => tracing::debug!(%host, "Eversolo SSDP candidate did not answer: {error}"),
        }
    }

    /// Attach a unit at a known address without discovery: a mock on an ephemeral port in tests.
    /// The unit runs until the adapter stops.
    pub async fn attach_unit(&self, device_id: &DeviceId, host: &str, port: u16) {
        self.attach_unit_with(device_id, host, port, UnitOptions::default())
            .await;
    }

    /// [`Self::attach_unit`] with explicit options (Wake-on-LAN targets).
    pub async fn attach_unit_with(
        &self,
        device_id: &DeviceId,
        host: &str,
        port: u16,
        options: UnitOptions,
    ) {
        let cancel = {
            let mut state = self.state.write().await;
            state.running = true;
            state.cancel.child_token()
        };
        self.spawn_unit(device_id, host, port, "manual", options, cancel)
            .await;
    }

    async fn spawn_unit(
        &self,
        device_id: &DeviceId,
        host: &str,
        port: u16,
        discovery: &'static str,
        options: UnitOptions,
        cancel: CancellationToken,
    ) {
        let mut state = self.state.write().await;
        // One unit per device: neither a repeated id nor a second id at the same address.
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
            },
        );
        drop(state);
        tracing::info!(%device_id, host, "Attached Eversolo device");
        let expected_hardware_id = device_id
            .as_str()
            .split_once(':')
            .map(|(_, id)| id.to_string());
        let unit = Unit {
            bus: self.bus.clone(),
            http: self.http.clone(),
            host: host.to_string(),
            port,
            device_id: device_id.clone(),
            discovery,
            wol_targets: options
                .wol_targets
                .unwrap_or_else(|| protocol::wol_targets(host)),
            expected_hardware_id,
            model: None,
            reading: None,
            inputs: protocol::InputList::default(),
            last_range: None,
            reachable: false,
            identity_ok: true,
            off: OffLatch::default(),
            fast_until: None,
        };
        tokio::spawn(unit.run(receiver, cancel));
    }

    pub async fn device_count(&self) -> usize {
        self.state.read().await.units.len()
    }
}

#[async_trait]
impl crate::adapters::devices::DeviceFamily for EversoloAdapter {
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
impl DeviceDriver for EversoloAdapter {
    fn kinds(&self) -> &'static [&'static str] {
        &["eversolo"]
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
            COMMAND_TIMEOUT,
            commands.send(UnitCommand::Execute { operation, reply }),
        )
        .await
        .map_err(|_| "the unit is busy".to_string())?
        .map_err(|_| "the unit has stopped".to_string())?;
        tokio::time::timeout(COMMAND_TIMEOUT * 4, response)
            .await
            .map_err(|_| "the unit did not accept the command in time".to_string())?
            .map_err(|_| "the unit stopped before sending".to_string())?
    }
}

/// Tracks UHC's own accepted power-off so silence afterwards can be called `standby`.
#[derive(Debug, Default)]
struct OffLatch {
    commanded_at: Option<Instant>,
    /// The unit has stopped answering since the command.
    seen_down: bool,
}

struct Unit {
    bus: SharedBus,
    http: Client,
    host: String,
    port: u16,
    device_id: DeviceId,
    discovery: &'static str,
    wol_targets: Vec<SocketAddr>,
    /// Hardware id the unit was attached under; the host must report the same MAC.
    expected_hardware_id: Option<String>,
    model: Option<protocol::ModelInfo>,
    /// Last good `getState`; `None` while unreachable.
    reading: Option<protocol::StateReading>,
    inputs: protocol::InputList,
    /// Last volume bounds seen (min, max), kept while unreachable so the range survives a reboot.
    last_range: Option<(i64, i64)>,
    reachable: bool,
    identity_ok: bool,
    off: OffLatch,
    fast_until: Option<Instant>,
}

impl Unit {
    async fn run(mut self, mut commands: mpsc::Receiver<UnitCommand>, cancel: CancellationToken) {
        // Queries only: nothing is written on connect.
        self.refresh().await;
        self.publish();
        loop {
            let wait = self.next_poll_delay();
            tokio::select! {
                _ = cancel.cancelled() => break,
                command = commands.recv() => {
                    let Some(UnitCommand::Execute { operation, reply }) = command else { break };
                    let result = self.execute(&operation).await;
                    if reply.send(result).is_err() {
                        tracing::debug!(device_id = %self.device_id, "command caller went away before the reply");
                    }
                }
                _ = tokio::time::sleep(wait) => {
                    self.refresh().await;
                    self.publish();
                }
            }
        }
        self.bus.publish(BusEvent::DeviceRemoved {
            device_id: self.device_id.clone(),
        });
    }

    fn next_poll_delay(&mut self) -> Duration {
        if let Some(until) = self.fast_until {
            if Instant::now() < until && !(self.reachable && self.identity_ok) {
                return FAST_POLL_INTERVAL;
            }
            self.fast_until = None;
        }
        if self.reachable {
            POLL_INTERVAL
        } else {
            OFFLINE_POLL_INTERVAL
        }
    }

    /// One readback: `getState`, then the input list, then the model once. Updates reachability,
    /// identity and the power inference. Never writes.
    async fn refresh(&mut self) {
        match fetch_state(&self.http, &self.host, self.port).await {
            Err(error) => {
                if self.reachable {
                    tracing::info!(device_id = %self.device_id, detail = %error.detail, "Eversolo stopped answering");
                }
                self.reachable = false;
                self.reading = None;
                if error.unreachable && self.off.commanded_at.is_some() {
                    self.off.seen_down = true;
                }
            }
            Ok(reading) => {
                if !self.reachable {
                    tracing::info!(device_id = %self.device_id, "Eversolo answering");
                }
                self.reachable = true;
                if self.model.is_none() {
                    self.model = fetch_model(&self.http, &self.host, self.port).await;
                }
                self.check_identity(&reading);
                if let Some(max) = reading.volume.max {
                    self.last_range = Some((reading.volume.min, max));
                }
                // The unit is back after a power-off, or the inference is stale.
                if let Some(at) = self.off.commanded_at {
                    if self.off.seen_down || at.elapsed() > OFF_LATCH_MAX {
                        self.off = OffLatch::default();
                    }
                }
                if self.identity_ok {
                    match get_ok(
                        &self.http,
                        &self.host,
                        self.port,
                        protocol::INPUT_OUTPUT_PATH,
                    )
                    .await
                    .map_err(|e| e.detail)
                    .and_then(|body| protocol::parse_input_list(&body))
                    {
                        Ok(list) => self.inputs = list,
                        Err(detail) => {
                            tracing::debug!(device_id = %self.device_id, %detail, "input list unavailable; keeping the last one");
                        }
                    }
                }
                self.reading = Some(reading);
            }
        }
    }

    /// The host must report the MAC the unit was attached under. A different one means the
    /// address now belongs to another unit, so nothing is observed or written through it.
    fn check_identity(&mut self, reading: &protocol::StateReading) {
        let reported = reading
            .mac
            .as_deref()
            .or_else(|| self.model.as_ref().and_then(|m| m.net_mac.as_deref()))
            .and_then(protocol::hardware_id_from_mac);
        let ok = match (&reported, &self.expected_hardware_id) {
            (Some(reported), Some(expected)) => reported.eq_ignore_ascii_case(expected),
            _ => true,
        };
        if self.identity_ok && !ok {
            tracing::warn!(
                device_id = %self.device_id, host = %self.host,
                "the host reports a different MAC than this unit was attached under; ignoring it"
            );
        }
        self.identity_ok = ok;
    }

    fn live(&self) -> Option<&protocol::StateReading> {
        if self.reachable && self.identity_ok {
            self.reading.as_ref()
        } else {
            None
        }
    }

    fn power(&self) -> DevicePower {
        if self.reachable {
            return if self.identity_ok {
                DevicePower::On
            } else {
                DevicePower::Unknown
            };
        }
        if self.off.commanded_at.is_some() && self.off.seen_down {
            DevicePower::Standby
        } else {
            DevicePower::Unknown
        }
    }

    fn wake_mac(&self) -> Option<String> {
        self.expected_hardware_id.clone().or_else(|| {
            self.model
                .as_ref()
                .and_then(|m| m.net_mac.as_deref())
                .and_then(protocol::hardware_id_from_mac)
        })
    }

    fn can_wake(&self) -> bool {
        self.model.as_ref().is_some_and(|m| m.able_remote_boot) && self.wake_mac().is_some()
    }

    async fn execute(&mut self, operation: &DeviceOperation) -> Result<(), String> {
        match operation {
            DeviceOperation::PowerOn => self.power_on().await,
            DeviceOperation::PowerStandby => self.power_off().await,
            _ => {
                if self.live().is_none() {
                    return Err(format!(
                        "{} is not answering (or answers as a different unit), so nothing was sent",
                        self.device_id
                    ));
                }
                match operation {
                    DeviceOperation::SetVolume(level) => self.set_volume(*level).await?,
                    DeviceOperation::SetMute(muted) => self.set_mute(*muted).await?,
                    DeviceOperation::SelectInput(tag) => self.select_input(tag).await?,
                    DeviceOperation::PowerOn | DeviceOperation::PowerStandby => {}
                }
                self.readback(operation).await;
                Ok(())
            }
        }
    }

    async fn write(&self, path: &str) -> Result<(), String> {
        let (status, _) = get(&self.http, &self.host, self.port, path)
            .await
            .map_err(|e| e.detail)?;
        // ADR-0004 (tmeynell/ha-eversolo): HTTP status only; the JSON `status` lies both ways.
        if protocol::status_accepted(status) {
            Ok(())
        } else {
            Err(format!("the device refused {path}: HTTP {status}"))
        }
    }

    async fn set_volume(&mut self, level: f32) -> Result<(), String> {
        let volume = self
            .live()
            .map(|r| r.volume.clone())
            .ok_or("volume has not been read")?;
        if !volume.adjustable() {
            return Err(
                "volume is not supported by this device right now: it reports it locked or \
                 disabled (isLock / isVolumeEnable), or gave no level or maximum"
                    .to_string(),
            );
        }
        if !level.is_finite() {
            return Err("volume level is not a number".to_string());
        }
        let target = level.round() as i64;
        let max = volume.max.ok_or("the device reported no maximum volume")?;
        if target < volume.min || target > max {
            return Err(format!(
                "volume {target} is outside the device range {}..={max}",
                volume.min
            ));
        }
        self.write(&protocol::set_volume_path(target)).await
    }

    async fn set_mute(&mut self, muted: bool) -> Result<(), String> {
        let adjustable = self.live().is_some_and(|r| r.volume.adjustable());
        if !adjustable {
            return Err(
                "mute is not supported by this device right now: volume is reported locked or \
                 disabled"
                    .to_string(),
            );
        }
        self.write(&protocol::set_mute_path(muted)).await
    }

    async fn select_input(&mut self, tag: &str) -> Result<(), String> {
        let entry = self
            .inputs
            .find(tag)
            .ok_or_else(|| format!("{tag:?} is not an input this Eversolo lists"))?;
        let path = protocol::set_input_path(&entry.tag, entry.index)
            .ok_or_else(|| format!("{tag:?} is not a valid input tag"))?;
        self.write(&path).await
    }

    /// `setPowerOption?tag=poweroff`; the PDF's `Key.Poweroff` when that path is refused.
    async fn power_off(&mut self) -> Result<(), String> {
        if self.live().is_none() {
            return Err(format!(
                "{} is not answering: a unit that is already off cannot be powered off",
                self.device_id
            ));
        }
        if self.model.as_ref().is_some_and(|m| !m.able_remote_shutdown) {
            return Err("power off is not supported by this device: getModel reports ableRemoteShutdown false".to_string());
        }
        let path =
            protocol::set_power_path(protocol::POWER_TAG_OFF).ok_or("invalid power option tag")?;
        if let Err(first) = self.write(&path).await {
            tracing::debug!(device_id = %self.device_id, error = %first, "setPowerOption refused; trying Key.Poweroff");
            self.send_key(protocol::KEY_POWEROFF)
                .await
                .map_err(|second| format!("{first}; fallback key also failed: {second}"))?;
        }
        self.off = OffLatch {
            commanded_at: Some(Instant::now()),
            seen_down: false,
        };
        let operation = DeviceOperation::PowerStandby;
        for attempt in 0..READBACK_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(POWER_OFF_READBACK_GAP).await;
            }
            self.refresh().await;
            self.publish();
            if self.satisfied(&operation) {
                break;
            }
        }
        Ok(())
    }

    async fn send_key(&self, key: &str) -> Result<(), String> {
        let mut last = String::from("no request made");
        for path in protocol::SENDKEY_PATHS {
            let request = protocol::sendkey_path(path, key)
                .ok_or_else(|| format!("{key:?} is not a valid Eversolo key"))?;
            let (status, _) = get(&self.http, &self.host, self.port, &request)
                .await
                .map_err(|e| e.detail)?;
            if protocol::status_accepted(status) {
                return Ok(());
            }
            last = format!("HTTP {status} from {path}");
            if status != 404 {
                break;
            }
        }
        Err(format!("the device refused {key}: {last}"))
    }

    /// Wake-on-LAN. The unit is polled quickly afterwards; the packet itself proves nothing.
    async fn power_on(&mut self) -> Result<(), String> {
        if self.live().is_some() {
            // Already answering: nothing to wake; the readback below reports On.
            self.refresh().await;
            self.publish();
            return Ok(());
        }
        let mac = self.wake_mac().ok_or(
            "power on is not supported without a MAC: configure one, or let the unit be seen once",
        )?;
        if self.model.as_ref().is_some_and(|m| !m.able_remote_boot) {
            return Err(
                "power on is not supported by this device: getModel reports ableRemoteBoot false"
                    .to_string(),
            );
        }
        send_magic_packet(&mac, &self.wol_targets).await?;
        self.off = OffLatch::default();
        self.fast_until = Some(Instant::now() + FAST_POLL_WINDOW);
        // A unit that was off cannot have booted yet; this readback is honest about that.
        self.refresh().await;
        self.publish();
        Ok(())
    }

    fn satisfied(&self, operation: &DeviceOperation) -> bool {
        match operation {
            DeviceOperation::SetVolume(level) => self
                .live()
                .and_then(|r| r.volume.current)
                .is_some_and(|c| (c as f32 - level.round()).abs() < 0.5),
            DeviceOperation::SetMute(muted) => {
                self.live().and_then(|r| r.volume.muted) == Some(*muted)
            }
            DeviceOperation::SelectInput(tag) => {
                self.live().is_some() && self.inputs.current_tag() == Some(tag.as_str())
            }
            DeviceOperation::PowerOn => self.live().is_some(),
            DeviceOperation::PowerStandby => self.power() == DevicePower::Standby,
        }
    }

    /// Read back after a write, publishing a new revision each time, until the readback shows
    /// the target or the attempts run out (the device may apply a write a moment late).
    async fn readback(&mut self, operation: &DeviceOperation) {
        for _ in 0..READBACK_ATTEMPTS {
            tokio::time::sleep(READBACK_GAP).await;
            self.refresh().await;
            self.publish();
            if self.satisfied(operation) {
                break;
            }
        }
    }

    fn publish(&self) {
        let model = self
            .model
            .as_ref()
            .map(|m| m.model.clone())
            .or_else(|| self.reading.as_ref().and_then(|r| r.model.clone()))
            .unwrap_or_else(|| "Eversolo".to_string());
        let live = self.live();
        let adjustable = live.is_some_and(|r| r.volume.adjustable());
        let (min, hardware_max) = match self.last_range {
            Some((min, max)) => (min as f32, Some(max as f32)),
            None => (0.0, None),
        };
        let input = if live.is_some() {
            self.inputs.current_tag().map(str::to_string)
        } else {
            None
        };
        let device = Device {
            device_id: self.device_id.clone(),
            vendor: "Eversolo".to_string(),
            name: self
                .model
                .as_ref()
                .and_then(|m| m.name.clone())
                .unwrap_or_else(|| model.clone()),
            model: model.clone(),
            host: self.host.clone(),
            discovery: self.discovery.to_string(),
            maturity: DEVICE_CONTROL_MATURITY.to_string(),
            connection: DeviceConnection {
                control_channel: if live.is_some() {
                    ControlChannel::Polling
                } else {
                    ControlChannel::None
                },
                detail: Some(if !self.identity_ok {
                    "the host reports a different MAC than this unit was attached under".to_string()
                } else if live.is_some() {
                    "HTTP polling of getState and getInputAndOutputList".to_string()
                } else {
                    "not answering on the control port".to_string()
                }),
            },
            power: self.power(),
            input,
            inputs: self
                .inputs
                .inputs
                .iter()
                .map(|i| DeviceInput {
                    id: i.tag.clone(),
                    name: i.name.clone(),
                    available: true,
                })
                .collect(),
            volume: DeviceVolume {
                scale: VolumeScale::Steps,
                value: live
                    .filter(|_| adjustable)
                    .and_then(|r| r.volume.current)
                    .map(|c| c as f32),
                min,
                hardware_max,
                ceiling: None,
                step: 1.0,
                is_muted: live.and_then(|r| r.volume.muted),
                display: None,
            },
            capabilities: capabilities_for(&model, adjustable, self.can_wake()),
            revision: next_revision(),
            last_updated: now_ms(),
        };
        self.bus.publish(BusEvent::DeviceObserved {
            device: Box::new(device),
        });
    }
}
