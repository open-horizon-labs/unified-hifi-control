//! Trinnov Altitude device adapter (#774). **Alpha.**
//!
//! A *device* adapter for the Altitude processor a zone feeds. It never publishes zones.
//!
//! Spec: <https://github.com/binarylogic/py-trinnov-altitude> (read its `protocol.py` and
//! `client.py`); command strings and event grammar are in [`protocol`].
//!
//! # No discovery
//!
//! The catalog establishes a configured-host baseline only. Units come exclusively from
//! `FamilyContext.manual` (`host`, optional `port`, and the `mac` that is both the unit's identity
//! and its Wake-on-LAN target). A unit configured without a MAC is identified from the
//! processor's own `Welcome ... ID <n>` line.
//!
//! # Power is a lifecycle, not a command
//!
//! - **Off** is `power_off_SECURED_FHZMCH48FE`, completed only when the processor answers `OK`;
//!   the unit then reads `standby` and the session is closed on purpose.
//! - **On** is a Wake-on-LAN magic packet (UDP broadcast, port 9) to the configured MAC. A packet
//!   sent is *not* confirmation: the unit reads `on` only after the control session has
//!   reconnected and the processor has reported its state. Until then the device keeps its
//!   previous power reading and says a wake is pending, so the device service reports the command
//!   as unconfirmed rather than successful.
//! - A lost session is `unknown`, never `standby`: an unexplained disconnect is not evidence of a
//!   shutdown.
//!
//! # Volume
//!
//! Decibels, -120 to +20 per the client's documented range. The processor reports no maximum of
//! its own, so `hardware_max` is the protocol's full scale (+20 dB) and the user's ceiling lowers
//! it.
//!
//! # One owner per unit
//!
//! One task owns the unit's single TCP session. Connecting sends `id <client>` and queries only;
//! no state-changing line is ever sent on connect or reconnect.

pub mod protocol;

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
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
use protocol::Event;

/// Settings/coordinator name, and the family a `ManualDevice` carries.
pub const ADAPTER_NAME: &str = "trinnov";
pub const KIND: &str = "trinnov";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const ACK_TIMEOUT: Duration = Duration::from_secs(3);
const RECONNECT_MIN: Duration = Duration::from_secs(2);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// Reads state every interval (the client's reconcile + liveness probe in one).
const PROBE_INTERVAL: Duration = Duration::from_secs(15);
const SILENCE_LIMIT: Duration = Duration::from_secs(45);
/// After an acknowledged shutdown, do not reconnect for this long: the processor is still
/// closing, and a session opened now would be mistaken for a fresh boot.
const SHUTDOWN_HOLD: Duration = Duration::from_secs(20);
/// After a wake packet, retry the session at this pace for [`WAKE_WINDOW`].
const WAKE_RETRY: Duration = Duration::from_secs(3);
const WAKE_WINDOW: Duration = Duration::from_secs(180);
const BOOTSTRAP_INTERVAL: Duration = Duration::from_secs(30);
const COMMAND_SPACING: Duration = Duration::from_millis(50);

/// Capability claims. `wake_configured`: the unit has a MAC, so power-on has a target.
pub fn capabilities_for(model: &str, wake_configured: bool) -> DeviceCapabilities {
    let pending = "; implementation-derived from py-trinnov-altitude, not yet exercised on a unit";
    let write = |supported: bool, detail: String| DeviceCapability {
        supported,
        evidence: CapabilityEvidence::ImplementationDerived,
        detail,
    };
    DeviceCapabilities {
        observe: write(
            true,
            format!("TCP 44100 text session, push VOLUME/MUTE/CURRENT_PROFILE/PROFILE ({model}){pending}"),
        ),
        volume: write(
            true,
            format!(
                "volume <dB> (-120..+20, the protocol's full scale), lowered by the user ceiling{pending}"
            ),
        ),
        mute: write(true, format!("mute 1|0{pending}")),
        power: write(
            true,
            if wake_configured {
                format!(
                    "off: power_off_SECURED_FHZMCH48FE, complete on OK. on: Wake-on-LAN magic packet \
                     to the configured MAC (UDP broadcast, port 9); a sent packet is not \
                     confirmation, the unit reads on only when its session reconnects{pending}"
                )
            } else {
                format!(
                    "off: power_off_SECURED_FHZMCH48FE, complete on OK. on: refused, no MAC address \
                     configured for Wake-on-LAN{pending}"
                )
            },
        ),
        input_select: write(
            true,
            format!("profile <n> (source profiles; presets are a separate identity){pending}"),
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

/// Where a unit lives and how to wake it.
#[derive(Debug, Clone)]
struct UnitConfig {
    host: String,
    port: u16,
    mac: Option<[u8; 6]>,
    wol_target: SocketAddr,
    discovery: &'static str,
    shutdown_hold: Duration,
}

fn default_wol_target() -> SocketAddr {
    SocketAddr::from(([255, 255, 255, 255], protocol::WOL_PORT))
}

enum UnitCommand {
    Execute(crate::devices::service::DeviceWork),
}

struct UnitHandle {
    commands: mpsc::Sender<UnitCommand>,
}

#[derive(Default)]
struct AdapterState {
    running: bool,
    cancel: CancellationToken,
    units: HashMap<String, UnitHandle>,
}

#[derive(Clone)]
pub struct TrinnovAdapter {
    bus: SharedBus,
    state: Arc<RwLock<AdapterState>>,
    shutdown_hold: Duration,
}

impl TrinnovAdapter {
    pub fn new(bus: SharedBus) -> Self {
        Self {
            bus,
            state: Arc::new(RwLock::new(AdapterState::default())),
            shutdown_hold: SHUTDOWN_HOLD,
        }
    }

    /// How long after an acknowledged shutdown the unit neither reconnects nor accepts a wake.
    /// Tests shorten it; production keeps [`SHUTDOWN_HOLD`].
    pub fn with_shutdown_hold(mut self, hold: Duration) -> Self {
        self.shutdown_hold = hold;
        self
    }

    pub async fn device_count(&self) -> usize {
        self.state.read().await.units.len()
    }

    async fn start_internal(&self, manual: Vec<ManualDevice>) -> anyhow::Result<()> {
        let cancel = {
            let mut state = self.state.write().await;
            if state.running {
                return Ok(());
            }
            state.running = true;
            state.cancel = CancellationToken::new();
            state.cancel.clone()
        };
        for device in manual {
            let adapter = self.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move { adapter.supervise(device, cancel).await });
        }
        tracing::info!("Trinnov device adapter started (alpha)");
        Ok(())
    }

    async fn stop_internal(&self) {
        let units = {
            let mut state = self.state.write().await;
            state.running = false;
            state.cancel.cancel();
            std::mem::take(&mut state.units)
        };
        // Each unit task sees the cancelled token and publishes its own removal.
        drop(units);
        tracing::info!("Trinnov device adapter stopped");
    }

    /// Keep trying to bring one configured unit up. A unit with a MAC is published at once
    /// (offline, so it can be woken); one without waits until it has answered with its id.
    async fn supervise(&self, device: ManualDevice, cancel: CancellationToken) {
        let mut ticker = tokio::time::interval(BOOTSTRAP_INTERVAL);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = ticker.tick() => {
                    match self.attach_with(&device, default_wol_target(), "manual", &cancel).await {
                        Ok(_) => break,
                        Err(error) => tracing::debug!(host = %device.host, "Trinnov not attached yet: {error}"),
                    }
                }
            }
        }
    }

    /// Attach a configured unit without waiting for the supervisor tick: used by tests, with the
    /// Wake-on-LAN packet aimed at a local listener instead of the broadcast address.
    pub async fn attach_unit(
        &self,
        device: &ManualDevice,
        wol_target: SocketAddr,
    ) -> Result<DeviceId, String> {
        let cancel = {
            let mut state = self.state.write().await;
            state.running = true;
            state.cancel.child_token()
        };
        self.attach_with(device, wol_target, "manual", &cancel)
            .await
    }

    async fn attach_with(
        &self,
        device: &ManualDevice,
        wol_target: SocketAddr,
        discovery: &'static str,
        cancel: &CancellationToken,
    ) -> Result<DeviceId, String> {
        let port = device.port.unwrap_or(protocol::DEFAULT_PORT);
        let mac = match device.mac.as_deref() {
            Some(text) => Some(
                protocol::parse_mac(text)
                    .ok_or_else(|| format!("{text:?} is not a MAC address"))?,
            ),
            None => None,
        };
        let device_id = match device.mac.as_deref() {
            Some(text) => DeviceId::new(KIND, text)
                .ok_or_else(|| format!("{text:?} cannot identify a device"))?,
            None => {
                let id = identify(&device.host, port).await?;
                DeviceId::new(KIND, &id).ok_or_else(|| "the unit's id is not usable".to_string())?
            }
        };
        let mut state = self.state.write().await;
        if state.units.contains_key(device_id.as_str()) {
            return Ok(device_id);
        }
        let (commands, receiver) = mpsc::channel(8);
        state
            .units
            .insert(device_id.to_string(), UnitHandle { commands });
        drop(state);
        tracing::info!(%device_id, host = %device.host, "Configured Trinnov device");
        let unit = Unit {
            bus: self.bus.clone(),
            config: UnitConfig {
                host: device.host.clone(),
                port,
                mac,
                wol_target,
                discovery,
                shutdown_hold: self.shutdown_hold,
            },
            device_id: device_id.clone(),
            state: UnitState::default(),
        };
        tokio::spawn(unit.run(receiver, cancel.child_token()));
        Ok(device_id)
    }
}

/// Open a session just long enough to read the processor's own id from its welcome line.
async fn identify(host: &str, port: u16) -> Result<String, String> {
    let mut stream = connect(host, port).await?;
    send_line(
        &mut Some(&mut stream),
        &protocol::id_command(protocol::CLIENT_ID),
    )
    .await?;
    let mut buffer = String::new();
    let mut chunk = [0u8; 1024];
    let deadline = Instant::now() + ACK_TIMEOUT;
    loop {
        let read = tokio::time::timeout_at(deadline, stream.read(&mut chunk))
            .await
            .map_err(|_| "no welcome line".to_string())?
            .map_err(|error| format!("read failed: {error}"))?;
        if read == 0 {
            return Err("closed before the welcome line".to_string());
        }
        buffer.push_str(&String::from_utf8_lossy(&chunk[..read]));
        for line in protocol::drain_lines(&mut buffer) {
            if let Some(Event::Welcome { id, .. }) = protocol::parse_line(&line) {
                return Ok(id);
            }
        }
    }
}

async fn connect(host: &str, port: u16) -> Result<TcpStream, String> {
    match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host, port))).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(error)) => Err(format!("connect failed: {error}")),
        Err(_) => Err("connect timed out".to_string()),
    }
}

#[async_trait]
impl crate::adapters::devices::DeviceFamily for TrinnovAdapter {
    fn family(&self) -> &'static str {
        ADAPTER_NAME
    }

    async fn start_family(
        &self,
        context: crate::adapters::devices::FamilyContext,
    ) -> anyhow::Result<()> {
        self.start_internal(context.manual).await
    }

    async fn stop_family(&self) {
        self.stop_internal().await
    }
}

#[async_trait]
impl DeviceDriver for TrinnovAdapter {
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
        tokio::time::timeout(ACK_TIMEOUT + WRITE_TIMEOUT * 2, response)
            .await
            .map_err(|_| "the unit did not accept the command in time".to_string())?
            .map_err(|_| "the unit's session stopped before sending".to_string())?
    }
}

/// Everything the processor has reported, before it becomes a [`Device`].
#[derive(Default)]
struct UnitState {
    connected: bool,
    welcome_seen: bool,
    version: Option<String>,
    volume_db: Option<f32>,
    muted: Option<bool>,
    current_profile: Option<i32>,
    profiles: BTreeMap<i32, String>,
    /// The processor acknowledged `power_off`; cleared when a later session is greeted.
    shutdown_acked: bool,
    /// Until when a wake is refused after an acknowledged shutdown.
    shutdown_hold_until: Option<Instant>,
    /// A wake packet was sent and no session has answered yet.
    wake_pending: bool,
    detail: Option<String>,
}

impl UnitState {
    /// On only while a session is up and the processor has reported its state; standby only
    /// after an acknowledged shutdown; otherwise unknown.
    fn power(&self) -> DevicePower {
        if self.shutdown_acked {
            DevicePower::Standby
        } else if self.connected && self.welcome_seen && self.volume_db.is_some() {
            DevicePower::On
        } else {
            DevicePower::Unknown
        }
    }

    fn apply(&mut self, event: Event) {
        match event {
            Event::Welcome { version, .. } => {
                // The processor greeting us is the boot signal; a bare TCP accept is not. Only
                // now does an earlier acknowledged shutdown stop being the latest word.
                self.shutdown_acked = false;
                self.welcome_seen = true;
                self.version = Some(version);
            }
            Event::Volume(db) => self.volume_db = Some(db),
            Event::Mute(muted) => self.muted = Some(muted),
            Event::CurrentProfile(index) => self.current_profile = Some(index),
            Event::Profile { index, name } => {
                self.profiles.insert(index, name);
            }
            Event::ProfilesClear => self.profiles.clear(),
            Event::Ok | Event::Error(_) | Event::Bye => {}
        }
        if self.connected && self.welcome_seen && self.volume_db.is_some() {
            self.wake_pending = false;
        }
    }

    /// Nothing reported over a closed session is still true.
    fn session_lost(&mut self, detail: &str) {
        self.connected = false;
        self.welcome_seen = false;
        self.volume_db = None;
        self.muted = None;
        self.current_profile = None;
        self.detail = Some(detail.to_string());
    }
}

struct Unit {
    bus: SharedBus,
    config: UnitConfig,
    device_id: DeviceId,
    state: UnitState,
}

async fn read_chunk(stream: &mut Option<TcpStream>, chunk: &mut [u8]) -> std::io::Result<usize> {
    match stream {
        Some(stream) => stream.read(chunk).await,
        None => std::future::pending().await,
    }
}

/// Write one `\n`-terminated line. Generic over the stream holder so `identify` can use it on a
/// borrowed socket.
async fn send_line<S: StreamSlot>(slot: &mut S, line: &str) -> Result<(), String> {
    let stream = slot.stream().ok_or_else(|| "no session".to_string())?;
    let bytes = format!("{line}\n");
    tokio::time::timeout(WRITE_TIMEOUT, stream.write_all(bytes.as_bytes()))
        .await
        .map_err(|_| "write timed out".to_string())?
        .map_err(|error| format!("write failed: {error}"))
}

trait StreamSlot {
    fn stream(&mut self) -> Option<&mut TcpStream>;
}

impl StreamSlot for Option<TcpStream> {
    fn stream(&mut self) -> Option<&mut TcpStream> {
        self.as_mut()
    }
}

impl StreamSlot for Option<&mut TcpStream> {
    fn stream(&mut self) -> Option<&mut TcpStream> {
        self.as_deref_mut()
    }
}

impl Unit {
    async fn run(mut self, mut commands: mpsc::Receiver<UnitCommand>, cancel: CancellationToken) {
        let mut stream: Option<TcpStream> = None;
        let mut buffer = String::new();
        let mut chunk = [0u8; 2048];
        let mut backoff = RECONNECT_MIN;
        let mut next_connect = Instant::now();
        let mut wake_until: Option<Instant> = None;
        let mut probe = tokio::time::interval(PROBE_INTERVAL);
        let mut last_rx = Instant::now();

        // Offline until a session says otherwise.
        self.state.detail = Some("not connected yet".to_string());
        self.publish();

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                command = commands.recv() => {
                    let Some(UnitCommand::Execute(work)) = command else { break };
                    let Some((operation, reply)) = work.begin_dispatch() else { continue; };
                    let outcome = self
                        .execute(&mut stream, &mut buffer, &operation, &mut next_connect, &mut backoff, &mut wake_until)
                        .await;
                    if stream.is_none() && self.state.connected {
                        self.state.session_lost("session closed during a command");
                        next_connect = Instant::now() + backoff;
                    }
                    self.publish();
                    if reply.send(outcome).is_err() {
                        tracing::debug!(device_id = %self.device_id, "command caller went away before the reply");
                    }
                }
                read = read_chunk(&mut stream, &mut chunk), if stream.is_some() => {
                    match read {
                        Ok(0) | Err(_) => {
                            tracing::info!(device_id = %self.device_id, "Trinnov session closed; reconnecting");
                            stream = None;
                            buffer.clear();
                            self.state.session_lost("session closed; reconnecting");
                            next_connect = Instant::now() + backoff;
                            self.publish();
                        }
                        Ok(n) => {
                            last_rx = Instant::now();
                            buffer.push_str(&String::from_utf8_lossy(&chunk[..n]));
                            let mut changed = false;
                            for line in protocol::drain_lines(&mut buffer) {
                                if let Some(event) = protocol::parse_line(&line) {
                                    if let Event::Error(error) = &event {
                                        tracing::warn!(device_id = %self.device_id, "Trinnov error: {error}");
                                    }
                                    self.state.apply(event);
                                    changed = true;
                                }
                            }
                            if changed {
                                self.publish();
                            }
                        }
                    }
                }
                _ = tokio::time::sleep_until(next_connect), if stream.is_none() => {
                    match connect(&self.config.host, self.config.port).await {
                        Ok(socket) => {
                            stream = Some(socket);
                            buffer.clear();
                            backoff = RECONNECT_MIN;
                            last_rx = Instant::now();
                            wake_until = None;
                            self.state.connected = true;
                            self.state.detail = None;
                            if let Err(error) = self.open_session(&mut stream).await {
                                tracing::debug!(device_id = %self.device_id, "session setup failed: {error}");
                                stream = None;
                                self.state.session_lost("session setup failed");
                                next_connect = Instant::now() + backoff;
                            }
                            self.publish();
                        }
                        Err(detail) => {
                            let waking = wake_until.is_some_and(|until| Instant::now() < until);
                            if waking {
                                backoff = WAKE_RETRY;
                            } else {
                                self.state.wake_pending = false;
                                backoff = (backoff * 2).min(RECONNECT_MAX);
                            }
                            self.state.detail = Some(if self.state.wake_pending {
                                format!("wake requested; waiting for the processor to answer ({detail})")
                            } else {
                                detail
                            });
                            next_connect = Instant::now() + backoff;
                            self.publish();
                        }
                    }
                }
                _ = probe.tick(), if stream.is_some() => {
                    if last_rx.elapsed() > SILENCE_LIMIT {
                        tracing::info!(device_id = %self.device_id, "Trinnov session silent; reconnecting");
                        stream = None;
                        buffer.clear();
                        self.state.session_lost("no traffic; reconnecting");
                        next_connect = Instant::now();
                        self.publish();
                    } else if let Err(error) = send_line(&mut stream, protocol::STATE_QUERY).await {
                        tracing::debug!(device_id = %self.device_id, "state query failed: {error}");
                    }
                }
            }
        }
        self.bus.publish(BusEvent::DeviceRemoved {
            device_id: self.device_id.clone(),
        });
    }

    /// `id` handshake and queries: opening a session never changes the processor.
    async fn open_session(&mut self, stream: &mut Option<TcpStream>) -> Result<(), String> {
        send_line(stream, &protocol::id_command(protocol::CLIENT_ID)).await?;
        tokio::time::sleep(COMMAND_SPACING).await;
        send_line(stream, protocol::STATE_QUERY).await?;
        tokio::time::sleep(COMMAND_SPACING).await;
        send_line(stream, protocol::PROFILE_QUERY).await
    }

    async fn execute(
        &mut self,
        stream: &mut Option<TcpStream>,
        buffer: &mut String,
        operation: &DeviceOperation,
        next_connect: &mut Instant,
        backoff: &mut Duration,
        wake_until: &mut Option<Instant>,
    ) -> Result<(), String> {
        match operation {
            DeviceOperation::PowerOn => {
                if self
                    .state
                    .shutdown_hold_until
                    .is_some_and(|until| Instant::now() < until)
                {
                    return Err(
                        "the processor is still shutting down; try again shortly".to_string()
                    );
                }
                if stream.is_some() && self.state.power() == DevicePower::On {
                    // Already up: nothing to send, the readback below confirms it.
                    return Ok(());
                }
                let mac = self.config.mac.ok_or_else(|| {
                    "power on is not supported without a MAC address for Wake-on-LAN".to_string()
                })?;
                send_magic_packet(mac, self.config.wol_target).await?;
                // Sent is not confirmed: keep the reading as it was and poll for the session.
                self.state.wake_pending = true;
                self.state.detail =
                    Some("wake requested; waiting for the processor to answer".to_string());
                *wake_until = Some(Instant::now() + WAKE_WINDOW);
                *backoff = WAKE_RETRY;
                *next_connect = Instant::now() + WAKE_RETRY;
                Ok(())
            }
            DeviceOperation::PowerStandby => {
                require_session(stream)?;
                send_line(stream, protocol::POWER_OFF_COMMAND).await?;
                self.await_ack(stream, buffer).await?;
                // Acknowledged. Close our side on purpose and hold off reconnecting.
                *stream = None;
                buffer.clear();
                self.state.session_lost("shutdown acknowledged");
                self.state.shutdown_acked = true;
                *next_connect = Instant::now() + self.config.shutdown_hold;
                self.state.shutdown_hold_until = Some(*next_connect);
                *backoff = RECONNECT_MIN;
                Ok(())
            }
            DeviceOperation::SetVolume(level) => {
                require_session(stream)?;
                let command = protocol::volume_command(*level)
                    .ok_or_else(|| format!("{level} dB is outside the Altitude's range"))?;
                send_line(stream, &command).await?;
                tokio::time::sleep(COMMAND_SPACING).await;
                send_line(stream, protocol::VOLUME_QUERY).await
            }
            DeviceOperation::SetMute(muted) => {
                require_session(stream)?;
                send_line(stream, &protocol::mute_command(*muted)).await?;
                tokio::time::sleep(COMMAND_SPACING).await;
                send_line(stream, protocol::STATE_QUERY).await
            }
            DeviceOperation::SelectInput(id) => {
                require_session(stream)?;
                let index: u32 = id
                    .parse()
                    .map_err(|_| format!("{id:?} is not a Trinnov profile index"))?;
                send_line(stream, &protocol::profile_command(index)).await?;
                tokio::time::sleep(COMMAND_SPACING).await;
                send_line(stream, protocol::PROFILE_QUERY).await
            }
        }
    }

    /// Wait for the processor's `OK` (or `ERROR:`) after an acknowledged command, applying every
    /// other event that arrives meanwhile.
    async fn await_ack(
        &mut self,
        stream: &mut Option<TcpStream>,
        buffer: &mut String,
    ) -> Result<(), String> {
        let deadline = Instant::now() + ACK_TIMEOUT;
        let mut chunk = [0u8; 1024];
        loop {
            let read = {
                let Some(socket) = stream.as_mut() else {
                    return Err("session closed before the acknowledgement".to_string());
                };
                tokio::time::timeout_at(deadline, socket.read(&mut chunk)).await
            };
            let read = match read {
                Err(_) => return Err("the processor did not acknowledge the command".to_string()),
                Ok(Err(_)) | Ok(Ok(0)) => {
                    *stream = None;
                    return Err("session closed before the acknowledgement".to_string());
                }
                Ok(Ok(n)) => n,
            };
            buffer.push_str(&String::from_utf8_lossy(&chunk[..read]));
            for line in protocol::drain_lines(buffer) {
                match protocol::parse_line(&line) {
                    Some(Event::Ok) => return Ok(()),
                    Some(Event::Error(error)) => {
                        return Err(format!("the processor refused the command: {error}"))
                    }
                    Some(event) => self.state.apply(event),
                    None => {}
                }
            }
        }
    }

    fn publish(&self) {
        let state = &self.state;
        let inputs = state
            .profiles
            .iter()
            .map(|(index, name)| DeviceInput {
                id: index.to_string(),
                name: name.clone(),
                available: true,
            })
            .collect();
        let device = Device {
            device_id: self.device_id.clone(),
            vendor: "Trinnov".to_string(),
            model: "Altitude".to_string(),
            name: "Trinnov Altitude".to_string(),
            host: self.config.host.clone(),
            discovery: self.config.discovery.to_string(),
            maturity: DEVICE_CONTROL_MATURITY.to_string(),
            connection: DeviceConnection {
                control_channel: if state.connected {
                    ControlChannel::Push
                } else {
                    ControlChannel::None
                },
                detail: state.detail.clone(),
            },
            power: state.power(),
            input: state.current_profile.map(|index| index.to_string()),
            inputs,
            volume: DeviceVolume {
                scale: crate::bus::VolumeScale::Decibel,
                value: state.volume_db,
                min: protocol::VOLUME_MIN_DB,
                // The protocol's documented full scale (+20 dB); a user ceiling only lowers it.
                hardware_max: Some(protocol::VOLUME_SPEC_MAX_DB),
                ceiling: None,
                step: 0.5,
                is_muted: state.muted,
                display: None,
            },
            capabilities: capabilities_for("Altitude", self.config.mac.is_some()),
            revision: next_revision(),
            last_updated: now_ms(),
        };
        self.bus.publish(BusEvent::DeviceObserved {
            device: Box::new(device),
        });
    }
}

fn require_session(stream: &Option<TcpStream>) -> Result<(), String> {
    if stream.is_some() {
        Ok(())
    } else {
        Err("the processor is not connected (off, or unreachable); power it on first".to_string())
    }
}

/// Send a Wake-on-LAN magic packet. Success means the datagram left this host, nothing more.
async fn send_magic_packet(mac: [u8; 6], target: SocketAddr) -> Result<(), String> {
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|error| format!("cannot open a UDP socket: {error}"))?;
    socket
        .set_broadcast(true)
        .map_err(|error| format!("cannot enable broadcast: {error}"))?;
    socket
        .send_to(&protocol::magic_packet(mac), target)
        .await
        .map_err(|error| format!("Wake-on-LAN send failed: {error}"))?;
    Ok(())
}
