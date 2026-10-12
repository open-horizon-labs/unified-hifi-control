//! Eversolo device adapter (#774). **Alpha.**
//!
//! A *device* adapter: it controls the streamer/DAC a zone feeds, not a music provider. It never
//! publishes zones; it publishes [`BusEvent::DeviceObserved`] snapshots and executes
//! [`DeviceOperation`]s handed to it by [`crate::devices::service::DeviceService`].
//!
//! # Protocol
//!
//! Implemented from the vendor document `Eversolo_DMP-A6_TCP_en_v1.0.pdf`
//! (<https://music.eversolo.com/dmp/instruction/Eversolo_DMP-A6_TCP_en_v1.0.pdf>). All three pages
//! were read. It defines one thing: an HTTP GET on port 9529,
//! `/ControlCenter/RemoteControl/sendkey?key=KEY`, with a table of key codes. See
//! [`protocol`] for the table and the URL-path ambiguity. **No Eversolo hardware has been
//! available**: every capability claim is implementation-derived.
//!
//! # What the document leaves out, and what this adapter does about it
//!
//! The document defines no state query, no response body, no discovery and no identity. So:
//!
//! - **Discovery**: none exists in the document. Only configured devices (`ManualDevice`) are
//!   used, and each needs a `mac`, because the protocol cannot report an identity and the brief
//!   forbids deriving one from an IP. A unit is published only once its control port accepts a
//!   TCP connection (reachability; the document has no query to validate against, and every key
//!   changes something, so no key is ever sent speculatively).
//! - **Volume**: the document has only relative `Key.VolumeUp`/`Key.VolumeDown`, no scale, range,
//!   step or level readback. A level cannot be bounded, so volume is **unsupported** and the
//!   scale is reported without a value or maximum. `hardware_max` is `None` because the maximum
//!   genuinely cannot be known from the document.
//! - **Mute**: `Key.Mute` is a toggle with no readable state; `SetMute(bool)` cannot be honoured.
//! - **Power**: only `Key.Poweroff` exists; there is no power-on key and no power state. Power is
//!   always reported `unknown`, never rounded to `standby`.
//! - **Input**: the five `Key.DAC.*` selections are supported; the current input is not readable.
//!
//! Because nothing can be read back, the "readback after every write" publishes a fresh revision
//! with only what is known (reachability); the service will therefore report a write as
//! *unconfirmed* rather than the adapter inventing a state it never read.
//!
//! The only traffic on connect or reconnect is a TCP handshake; no key is sent.

pub mod protocol;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio_util::sync::CancellationToken;

use crate::bus::{BusEvent, SharedBus, VolumeScale};
use crate::devices::links::ManualDevice;
use crate::devices::service::{DeviceDriver, DeviceOperation};
use crate::devices::{
    CapabilityEvidence, ControlChannel, Device, DeviceCapabilities, DeviceCapability,
    DeviceConnection, DeviceId, DevicePower, DeviceVolume, DEVICE_CONTROL_MATURITY,
};

/// Settings/coordinator name (`ManualDevice::family`).
pub const ADAPTER_NAME: &str = "eversolo";

/// The model the document is written for (its file name).
pub const DOCUMENTED_MODEL: &str = "DMP-A6";

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// Reachability polling per unit; also the discovery cadence for configured hosts.
const POLL_INTERVAL: Duration = Duration::from_secs(20);
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(30);

const NO_READBACK: &str = "the Eversolo document defines no state query";

/// Capability claims for one model. Pure, so documentation can be generated from it.
///
/// Every claim is implementation-derived from the vendor PDF, not from a device.
pub fn capabilities_for(model: &str) -> DeviceCapabilities {
    let caveat = if model.eq_ignore_ascii_case(DOCUMENTED_MODEL) {
        String::new()
    } else {
        format!(
            "; the document is written for the {DOCUMENTED_MODEL}, model {model:?} is not verified"
        )
    };
    let derived = |supported: bool, detail: String| DeviceCapability {
        supported,
        evidence: CapabilityEvidence::ImplementationDerived,
        detail,
    };
    DeviceCapabilities {
        observe: derived(
            false,
            format!(
                "reachability only (TCP connect to port 9529): {NO_READBACK}, so power, level, \
                 mute and input are never read{caveat}"
            ),
        ),
        volume: derived(
            false,
            format!(
                "the document has only relative Key.VolumeUp/Key.VolumeDown: no absolute level, \
                 scale, range or step, and no readback, so a level cannot be bounded{caveat}"
            ),
        ),
        mute: derived(
            false,
            format!(
                "Key.Mute is a toggle and mute state cannot be read ({NO_READBACK}), so an \
                 absolute set is not expressible{caveat}"
            ),
        ),
        power: derived(
            true,
            format!(
                "Key.Poweroff only (standby); the document has no power-on key and no power \
                 state{caveat}"
            ),
        ),
        input_select: derived(
            true,
            format!(
                "Key.DAC.XMOS/BT/USB/SPDIF/COA over HTTP port 9529; the current input is not \
                 readable{caveat}"
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

/// TCP reachability of the control port. Sends nothing.
async fn probe(host: &str, port: u16) -> bool {
    matches!(
        tokio::time::timeout(PROBE_TIMEOUT, TcpStream::connect((host, port))).await,
        Ok(Ok(_))
    )
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

    /// Configured hosts. The document defines no discovery, so these are the only source.
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
        for manual in self.manual.read().await.iter() {
            if manual
                .mac
                .as_deref()
                .and_then(protocol::hardware_id_from_mac)
                .is_none()
            {
                tracing::warn!(
                    host = %manual.host,
                    "Eversolo device needs a `mac`: the protocol reports no identity, so it is not used"
                );
            }
        }
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

    async fn discover_once(&self, cancel: &CancellationToken) {
        for manual in self.manual.read().await.clone() {
            if cancel.is_cancelled() {
                return;
            }
            let Some(device_id) = manual
                .mac
                .as_deref()
                .and_then(protocol::hardware_id_from_mac)
                .and_then(|id| DeviceId::new("eversolo", &id))
            else {
                continue;
            };
            let port = manual.port.unwrap_or(protocol::DEFAULT_PORT);
            {
                let state = self.state.read().await;
                if state.units.contains_key(device_id.as_str())
                    || state
                        .units
                        .values()
                        .any(|unit| unit.host == manual.host && unit.port == port)
                {
                    continue;
                }
            }
            // Discovery is not capability detection; the document offers no query, so the best
            // available evidence is that the control port answers.
            if !probe(&manual.host, port).await {
                continue;
            }
            self.spawn_unit(&device_id, &manual.host, port, cancel.child_token())
                .await;
        }
    }

    /// Attach a unit at a known address without discovery: a mock on an ephemeral port in tests.
    /// The unit runs until the adapter stops.
    pub async fn attach_unit(&self, device_id: &DeviceId, host: &str, port: u16) {
        let cancel = {
            let mut state = self.state.write().await;
            state.running = true;
            state.cancel.child_token()
        };
        self.spawn_unit(device_id, host, port, cancel).await;
    }

    async fn spawn_unit(
        &self,
        device_id: &DeviceId,
        host: &str,
        port: u16,
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
        let unit = Unit {
            bus: self.bus.clone(),
            http: self.http.clone(),
            host: host.to_string(),
            port,
            device_id: device_id.clone(),
            reachable: false,
            path_index: 0,
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
        tokio::time::timeout(COMMAND_TIMEOUT * 2, response)
            .await
            .map_err(|_| "the unit did not accept the command in time".to_string())?
            .map_err(|_| "the unit stopped before sending".to_string())?
    }
}

struct Unit {
    bus: SharedBus,
    http: Client,
    host: String,
    port: u16,
    device_id: DeviceId,
    reachable: bool,
    /// Index into [`protocol::SENDKEY_PATHS`] that last worked.
    path_index: usize,
}

impl Unit {
    async fn run(mut self, mut commands: mpsc::Receiver<UnitCommand>, cancel: CancellationToken) {
        // Reachability only: a TCP handshake, no key.
        self.reachable = probe(&self.host, self.port).await;
        self.publish();
        let mut poll = tokio::time::interval(POLL_INTERVAL);
        poll.tick().await; // the first tick is immediate and the probe above covered it
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                command = commands.recv() => {
                    let Some(UnitCommand::Execute { operation, reply }) = command else { break };
                    let result = self.execute(&operation).await;
                    // Readback after every write. The document has no query, so the readback is
                    // a reachability probe: a fresh revision, never an invented state.
                    self.reachable = probe(&self.host, self.port).await;
                    self.publish();
                    if reply.send(result).is_err() {
                        tracing::debug!(device_id = %self.device_id, "command caller went away before the reply");
                    }
                }
                _ = poll.tick() => {
                    let reachable = probe(&self.host, self.port).await;
                    if reachable != self.reachable {
                        tracing::info!(device_id = %self.device_id, reachable, "Eversolo reachability changed");
                    }
                    self.reachable = reachable;
                    self.publish();
                }
            }
        }
        self.bus.publish(BusEvent::DeviceRemoved {
            device_id: self.device_id.clone(),
        });
    }

    async fn execute(&mut self, operation: &DeviceOperation) -> Result<(), String> {
        match operation {
            DeviceOperation::SetVolume(_) => Err(format!(
                "volume is not supported by this device: the Eversolo document has only relative \
                 volume keys and no readable level ({NO_READBACK})"
            )),
            DeviceOperation::SetMute(_) => Err(format!(
                "mute is not supported by this device: Key.Mute is a toggle and its state cannot \
                 be read ({NO_READBACK})"
            )),
            DeviceOperation::PowerOn => Err(
                "power on is not supported by this device: the Eversolo document defines only \
                 Key.Poweroff"
                    .to_string(),
            ),
            // "Commands code table": Key.Poweroff, "Power off".
            DeviceOperation::PowerStandby => self.send_key(protocol::KEY_POWEROFF).await,
            DeviceOperation::SelectInput(code) => {
                let key = protocol::input_key(code).ok_or_else(|| {
                    format!("{code:?} is not an Eversolo input this adapter knows")
                })?;
                self.send_key(key).await
            }
        }
    }

    /// GET the key. A 404 on one documented path tries the other ("Base url" vs "For example").
    async fn send_key(&mut self, key: &str) -> Result<(), String> {
        let paths = protocol::SENDKEY_PATHS;
        let mut last = String::from("no request made");
        for offset in 0..paths.len() {
            let index = (self.path_index + offset) % paths.len();
            let url = protocol::sendkey_url(&self.host, self.port, paths[index], key)
                .ok_or_else(|| format!("{key:?} is not a valid Eversolo key"))?;
            match self.http.get(&url).send().await {
                Ok(response) => {
                    let status = response.status().as_u16();
                    if protocol::status_accepted(status) {
                        self.path_index = index;
                        return Ok(());
                    }
                    last = format!("HTTP {status} from {}", paths[index]);
                    if status != 404 {
                        break;
                    }
                }
                Err(error) => return Err(format!("could not reach the device: {error}")),
            }
        }
        Err(format!("the device refused {key}: {last}"))
    }

    fn publish(&self) {
        let caps = capabilities_for(DOCUMENTED_MODEL);
        let device = Device {
            device_id: self.device_id.clone(),
            vendor: "Eversolo".to_string(),
            model: DOCUMENTED_MODEL.to_string(),
            name: format!("Eversolo {DOCUMENTED_MODEL}"),
            host: self.host.clone(),
            discovery: "manual".to_string(),
            maturity: DEVICE_CONTROL_MATURITY.to_string(),
            connection: DeviceConnection {
                control_channel: if self.reachable {
                    ControlChannel::Polling
                } else {
                    ControlChannel::None
                },
                detail: Some(if self.reachable {
                    format!("HTTP key control; {NO_READBACK}, so state is not observable")
                } else {
                    "control port not reachable".to_string()
                }),
            },
            power: DevicePower::Unknown,
            input: None,
            inputs: protocol::inputs(),
            volume: DeviceVolume {
                // The document defines no scale at all; never claim dB or percent.
                scale: VolumeScale::Unknown,
                value: None,
                min: 0.0,
                // The document defines no volume range, so the maximum cannot be known.
                hardware_max: None,
                ceiling: None,
                step: 1.0,
                is_muted: None,
                display: None,
            },
            capabilities: caps,
            revision: next_revision(),
            last_updated: now_ms(),
        };
        self.bus.publish(BusEvent::DeviceObserved {
            device: Box::new(device),
        });
    }
}
