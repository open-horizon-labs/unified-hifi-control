//! Cambridge Audio StreamMagic device adapter (#774). **Alpha.**
//!
//! A *device* adapter for the streamer / all-in-one a zone feeds (Evo, CXN, Edge NQ, CXR with
//! network control). It never publishes zones: it publishes [`BusEvent::DeviceObserved`] and
//! executes the already-resolved [`DeviceOperation`]s the device service hands it.
//!
//! Spec: <https://github.com/noahhusby/aiostreammagic>. Transport is a JSON WebSocket at
//! `ws://<host>/smoip`; see [`protocol`] for the frames. Catalog record:
//! `docs/uhc-native-lan-client-catalog.md` §5.
//!
//! # Scope
//!
//! - **Volume only on variable-output units.** The unit's volume is `Percentage` (0-100) and is
//!   exposed only while `pre_amp_mode` is on. On a fixed-output unit, or with the pre-amp off,
//!   volume is *unsupported* (`value = None`); this adapter never turns the pre-amp on.
//! - The Control Bus volume path to a separate amplifier is **out of scope**: a fixed-output
//!   streamer does not become an amplifier-volume controller because it plays audio.
//! - Standby is `NETWORK` standby. In ECO standby the unit drops its network and UHC cannot wake
//!   it; Network Standby is a setting on the unit.
//!
//! # One owner per unit
//!
//! Each unit gets one task which owns its only WebSocket. Connecting and reconnecting send
//! only queries and subscriptions (`update: 100`), never a state-changing frame.

pub mod protocol;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

use crate::bus::{BusEvent, SharedBus};
use crate::devices::service::{DeviceDriver, DeviceOperation};
use crate::devices::{
    CapabilityEvidence, ControlChannel, Device, DeviceCapabilities, DeviceCapability,
    DeviceConnection, DeviceId, DeviceInput, DevicePower, DeviceVolume, DEVICE_CONTROL_MATURITY,
};
use protocol::{Endpoint, Incoming, Info, ZoneState};

/// Settings/coordinator name, and the family name a `ManualDevice` carries.
pub const ADAPTER_NAME: &str = "streammagic";

/// The device kind (id prefix) for Cambridge Audio.
pub const KIND: &str = "cambridge";

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// aiostreammagic pings every 30 s; a unit silent through two intervals is gone.
const SILENCE_LIMIT: Duration = Duration::from_secs(75);
const RECONNECT_MIN: Duration = Duration::from_secs(2);
const RECONNECT_MAX: Duration = Duration::from_secs(30);
const MANUAL_INTERVAL: Duration = Duration::from_secs(60);

/// Capability claims for one model. Pure, so the AGENTS.md device table can be generated from it.
///
/// `variable_output`: the unit reports `pre_amp_mode` on, so its volume is the unit's own level
/// rather than a fixed line-level output.
pub fn capabilities_for(model: &str, variable_output: bool) -> DeviceCapabilities {
    let pending = "; implementation-derived from aiostreammagic, not yet exercised on a unit";
    let write = |supported: bool, detail: String| DeviceCapability {
        supported,
        evidence: CapabilityEvidence::ImplementationDerived,
        detail,
    };
    DeviceCapabilities {
        observe: write(
            true,
            format!(
                "WebSocket /smoip subscriptions to /zone/state, /system/power, /system/info and \
                 /system/sources ({model}){pending}"
            ),
        ),
        volume: write(
            variable_output,
            if variable_output {
                format!(
                    "/zone/state volume_percent (Percentage 0-100), bounded by the unit's \
                     volume_limit_percent and the user ceiling{pending}"
                )
            } else {
                "unsupported: volume is exposed only on variable-output units (pre_amp_mode on); \
                 a fixed-output model or a Control Bus amplifier is not controlled from here"
                    .to_string()
            },
        ),
        mute: write(true, format!("/zone/state mute{pending}")),
        power: write(
            true,
            format!(
                "/system/power ON, and NETWORK standby (ECO standby disables the network and \
                 cannot be woken from here){pending}"
            ),
        ),
        input_select: write(true, format!("/zone/state source{pending}")),
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
    Rehost(Endpoint),
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
    /// Endpoints being probed right now, so a burst of mDNS events opens one socket.
    probing: HashSet<String>,
}

#[derive(Clone)]
pub struct StreamMagicAdapter {
    bus: SharedBus,
    manual: Arc<RwLock<Vec<Endpoint>>>,
    state: Arc<RwLock<AdapterState>>,
}

impl StreamMagicAdapter {
    pub fn new(bus: SharedBus) -> Self {
        Self {
            bus,
            manual: Arc::new(RwLock::new(Vec::new())),
            state: Arc::new(RwLock::new(AdapterState::default())),
        }
    }

    /// Hosts to probe directly, for units mDNS cannot reach.
    pub async fn set_manual_endpoints(&self, endpoints: Vec<Endpoint>) {
        *self.manual.write().await = endpoints;
    }

    pub async fn device_count(&self) -> usize {
        self.state.read().await.units.len()
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
        let mdns_cancel = cancel.clone();
        tokio::spawn(async move { adapter.mdns_loop(mdns_cancel).await });
        let adapter = self.clone();
        tokio::spawn(async move { adapter.manual_loop(cancel).await });
        tracing::info!("StreamMagic device adapter started (alpha)");
        Ok(())
    }

    async fn stop_internal(&self) {
        let units = {
            let mut state = self.state.write().await;
            state.running = false;
            state.cancel.cancel();
            state.probing.clear();
            std::mem::take(&mut state.units)
        };
        // Each unit task sees the cancelled parent token and publishes its own removal; the
        // handles are dropped here.
        drop(units);
        tracing::info!("StreamMagic device adapter stopped");
    }

    async fn mdns_loop(&self, cancel: CancellationToken) {
        let daemon = match mdns_sd::ServiceDaemon::new() {
            Ok(daemon) => daemon,
            Err(error) => {
                tracing::warn!("StreamMagic mDNS unavailable: {error}");
                return;
            }
        };
        for service_type in protocol::MDNS_SERVICE_TYPES {
            let receiver = match daemon.browse(service_type) {
                Ok(receiver) => receiver,
                Err(error) => {
                    tracing::warn!(service_type, "StreamMagic mDNS browse failed: {error}");
                    continue;
                }
            };
            let adapter = self.clone();
            let cancel = cancel.clone();
            let service_type = *service_type;
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        event = receiver.recv_async() => match event {
                            Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                                let addresses: Vec<_> = info
                                    .get_addresses()
                                    .iter()
                                    .map(|a| a.to_ip_addr())
                                    .collect();
                                if let Some(endpoint) = protocol::endpoint_from_service(
                                    service_type,
                                    &addresses,
                                    info.get_port(),
                                ) {
                                    adapter.discover(endpoint, "mdns", &cancel).await;
                                }
                            }
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                }
            });
        }
        cancel.cancelled().await;
        if let Err(error) = daemon.shutdown() {
            tracing::debug!("StreamMagic mDNS shutdown: {error}");
        }
    }

    async fn manual_loop(&self, cancel: CancellationToken) {
        let mut ticker = tokio::time::interval(MANUAL_INTERVAL);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = ticker.tick() => {
                    for endpoint in self.manual.read().await.clone() {
                        self.discover(endpoint, "manual", &cancel).await;
                    }
                }
            }
        }
    }

    /// Probe an endpoint and, if it answers the StreamMagic protocol, start (or re-host) its
    /// unit. Discovery is not capability detection: nothing is published for a host that does
    /// not answer `/system/info` with an identity.
    async fn discover(&self, endpoint: Endpoint, discovery: &str, cancel: &CancellationToken) {
        let key = format!("{}:{}", endpoint.host, endpoint.port);
        {
            let mut state = self.state.write().await;
            if !state.running
                || state.units.values().any(|unit| unit.host == endpoint.host)
                || !state.probing.insert(key.clone())
            {
                return;
            }
        }
        let probed = probe(&endpoint).await;
        self.state.write().await.probing.remove(&key);
        match probed {
            Ok((ws, info)) => {
                self.ensure_unit(endpoint, ws, info, discovery, cancel)
                    .await;
            }
            Err(error) => tracing::debug!(host = %endpoint.host, "not a StreamMagic unit: {error}"),
        }
    }

    /// Attach a unit at a known endpoint without mDNS: a configured host, or a mock unit on an
    /// ephemeral port in tests. Returns the identity it reported.
    pub async fn attach_endpoint(&self, host: &str, port: u16) -> Result<DeviceId, String> {
        let cancel = {
            let mut state = self.state.write().await;
            state.running = true;
            state.cancel.child_token()
        };
        let endpoint = Endpoint {
            host: host.to_string(),
            port,
        };
        let (ws, info) = probe(&endpoint).await?;
        let device_id = identity(&info)?;
        self.ensure_unit(endpoint, ws, info, "manual", &cancel)
            .await;
        Ok(device_id)
    }

    async fn ensure_unit(
        &self,
        endpoint: Endpoint,
        ws: Ws,
        info: Info,
        discovery: &str,
        cancel: &CancellationToken,
    ) {
        let Ok(device_id) = identity(&info) else {
            return;
        };
        let mut state = self.state.write().await;
        if let Some(existing) = state.units.get_mut(device_id.as_str()) {
            // Known unit seen at a new address: follow it instead of opening a second session.
            if existing.host != endpoint.host {
                let host = endpoint.host.clone();
                if existing
                    .commands
                    .try_send(UnitCommand::Rehost(endpoint))
                    .is_ok()
                {
                    existing.host = host;
                }
            }
            return;
        }
        let (commands, receiver) = mpsc::channel(8);
        state.units.insert(
            device_id.to_string(),
            UnitHandle {
                host: endpoint.host.clone(),
                commands,
            },
        );
        drop(state);
        tracing::info!(%device_id, host = %endpoint.host, model = %info.model, "Discovered StreamMagic device");
        let unit = Unit {
            bus: self.bus.clone(),
            endpoint,
            device_id,
            model: info.model,
            name: info.name,
            discovery: discovery.to_string(),
            state: UnitState::default(),
            owed: HashMap::new(),
        };
        tokio::spawn(unit.run(ws, receiver, cancel.child_token()));
    }
}

fn identity(info: &Info) -> Result<DeviceId, String> {
    let hardware_id = info
        .hardware_id()
        .ok_or_else(|| "the unit reported no unit_id or udn".to_string())?;
    DeviceId::new(KIND, &hardware_id).ok_or_else(|| "the unit's id is not usable".to_string())
}

async fn connect(endpoint: &Endpoint) -> Result<Ws, String> {
    let url = format!(
        "ws://{}:{}{}",
        endpoint.host,
        endpoint.port,
        protocol::SMOIP_PATH
    );
    let mut request = url
        .into_client_request()
        .map_err(|error| format!("bad address: {error}"))?;
    // aiostreammagic sends the unit's own origin.
    if let Ok(origin) = format!("ws://{}", endpoint.host).parse() {
        request.headers_mut().insert("Origin", origin);
    }
    match tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request)).await {
        Ok(Ok((ws, _response))) => Ok(ws),
        Ok(Err(error)) => Err(format!("WebSocket connect failed: {error}")),
        Err(_) => Err("WebSocket connect timed out".to_string()),
    }
}

/// Connect and ask for `/system/info`. The socket is returned open so the unit task reuses it.
async fn probe(endpoint: &Endpoint) -> Result<(Ws, Info), String> {
    let mut ws = connect(endpoint).await?;
    ws.send(Message::Text(protocol::query(protocol::PATH_INFO).into()))
        .await
        .map_err(|error| format!("probe write failed: {error}"))?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        let next = tokio::time::timeout_at(deadline, ws.next())
            .await
            .map_err(|_| "no /system/info answer".to_string())?;
        let Some(Ok(Message::Text(text))) = next else {
            if matches!(next, None | Some(Err(_))) {
                return Err("closed before /system/info".to_string());
            }
            continue;
        };
        let Some(frame) = protocol::parse_message(&text) else {
            continue;
        };
        if frame.path == protocol::PATH_INFO {
            if let Some(info) = frame.data.as_ref().and_then(Info::from_data) {
                return Ok((ws, info));
            }
            return Err("/system/info carried no identity".to_string());
        }
    }
}

#[async_trait]
impl crate::adapters::devices::DeviceFamily for StreamMagicAdapter {
    fn family(&self) -> &'static str {
        ADAPTER_NAME
    }

    async fn start_family(
        &self,
        context: crate::adapters::devices::FamilyContext,
    ) -> anyhow::Result<()> {
        let endpoints: Vec<Endpoint> = context
            .manual
            .into_iter()
            .map(|device| Endpoint {
                host: device.host,
                port: device.port.unwrap_or(protocol::DEFAULT_PORT),
            })
            .collect();
        if !endpoints.is_empty() {
            self.set_manual_endpoints(endpoints).await;
        }
        self.start_internal().await
    }

    async fn stop_family(&self) {
        self.stop_internal().await
    }
}

#[async_trait]
impl DeviceDriver for StreamMagicAdapter {
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
    power: Option<DevicePower>,
    source: Option<String>,
    pre_amp_mode: Option<bool>,
    volume_percent: Option<f32>,
    volume_limit: Option<f32>,
    muted: Option<bool>,
    inputs: Vec<DeviceInput>,
    channel: ControlChannel,
    detail: Option<String>,
}

impl UnitState {
    /// Volume is the unit's own only while its pre-amp is on.
    fn variable_output(&self) -> bool {
        self.pre_amp_mode == Some(true)
    }

    fn volume(&self) -> Option<f32> {
        if self.variable_output() {
            self.volume_percent
        } else {
            None
        }
    }

    fn apply(&mut self, frame: &Incoming, name: &mut String) {
        let Some(data) = frame.data.as_ref() else {
            return;
        };
        match frame.path.as_str() {
            protocol::PATH_INFO => {
                if let Some(info) = Info::from_data(data) {
                    if !info.name.is_empty() {
                        *name = info.name;
                    }
                }
            }
            protocol::PATH_SOURCES => {
                let inputs = protocol::parse_sources(data);
                if !inputs.is_empty() {
                    self.inputs = inputs;
                }
            }
            protocol::PATH_ZONE_STATE => {
                let state = ZoneState::from_data(data);
                if let Some(on) = state.power {
                    self.power = Some(if on {
                        DevicePower::On
                    } else {
                        DevicePower::Standby
                    });
                }
                if state.source.is_some() {
                    self.source = state.source;
                }
                if state.pre_amp_mode.is_some() {
                    self.pre_amp_mode = state.pre_amp_mode;
                }
                if state.volume_percent.is_some() {
                    self.volume_percent = state.volume_percent;
                }
                if state.mute.is_some() {
                    self.muted = state.mute;
                }
            }
            protocol::PATH_POWER => {
                if let Some(power) = protocol::parse_power(data) {
                    self.power = Some(power);
                }
            }
            protocol::PATH_AUDIO => {
                if let Some(limit) = protocol::parse_volume_limit(data) {
                    self.volume_limit = Some(limit);
                }
            }
            _ => {}
        }
    }

    /// Nothing reported over a closed session is still true.
    fn session_lost(&mut self, detail: String) {
        self.power = None;
        self.volume_percent = None;
        self.muted = None;
        self.source = None;
        self.channel = ControlChannel::None;
        self.detail = Some(detail);
    }
}

struct Unit {
    bus: SharedBus,
    endpoint: Endpoint,
    device_id: DeviceId,
    model: String,
    name: String,
    discovery: String,
    state: UnitState,
    /// Replies still owed for queries and subscriptions, per path. The protocol has no request
    /// ids, so a command's reply is the first one on its path *after* these.
    owed: HashMap<String, u32>,
}

/// Read the next frame if there is a session; pends forever otherwise.
async fn next_frame(
    ws: &mut Option<Ws>,
) -> Option<Result<Message, tokio_tungstenite::tungstenite::Error>> {
    match ws {
        Some(ws) => ws.next().await,
        None => std::future::pending().await,
    }
}

async fn send_text(ws: &mut Option<Ws>, text: String) -> Result<(), String> {
    let socket = ws.as_mut().ok_or_else(|| "no session".to_string())?;
    tokio::time::timeout(WRITE_TIMEOUT, socket.send(Message::Text(text.into())))
        .await
        .map_err(|_| "write timed out".to_string())?
        .map_err(|error| format!("write failed: {error}"))
}

impl Unit {
    /// Send a query or subscription and remember that its path owes a reply.
    async fn request(
        &mut self,
        ws: &mut Option<Ws>,
        path: &str,
        frame: String,
    ) -> Result<(), String> {
        send_text(ws, frame).await?;
        *self.owed.entry(path.to_string()).or_default() += 1;
        Ok(())
    }

    /// A reply arrived on `path`: one fewer owed. Returns whether it was owed.
    fn settle(&mut self, path: &str) -> bool {
        match self.owed.get_mut(path) {
            Some(count) if *count > 0 => {
                *count -= 1;
                true
            }
            _ => false,
        }
    }

    async fn run(
        mut self,
        first: Ws,
        mut commands: mpsc::Receiver<UnitCommand>,
        cancel: CancellationToken,
    ) {
        let mut ws: Option<Ws> = Some(first);
        let mut backoff = RECONNECT_MIN;
        let mut next_connect = Instant::now();
        let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
        let mut last_rx = Instant::now();
        // The probe socket is already open: ask for state and subscribe, then publish.
        self.open_session(&mut ws).await;

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                command = commands.recv() => {
                    let Some(command) = command else { break };
                    match command {
                        UnitCommand::Rehost(endpoint) => {
                            tracing::info!(device_id = %self.device_id, host = %endpoint.host, "Device moved to a new address");
                            self.endpoint = endpoint;
                            ws = None;
                            self.state.session_lost("address changed; reconnecting".to_string());
                            next_connect = Instant::now();
                            self.publish();
                        }
                        UnitCommand::Execute(work) => {
                            let Some((operation, reply)) = work.begin_dispatch() else { continue; };
                            let result = self.execute(&mut ws, &operation).await;
                            if ws.is_none() {
                                self.state.session_lost("session closed during a command".to_string());
                                next_connect = Instant::now() + backoff;
                            }
                            self.publish();
                            if reply.send(result).is_err() {
                                tracing::debug!(device_id = %self.device_id, "command caller went away before the reply");
                            }
                        }
                    }
                }
                frame = next_frame(&mut ws), if ws.is_some() => {
                    last_rx = Instant::now();
                    match frame {
                        Some(Ok(Message::Text(text))) => {
                            if let Some(incoming) = protocol::parse_message(&text) {
                                if incoming.is_response() {
                                    self.settle(&incoming.path);
                                }
                                self.state.apply(&incoming, &mut self.name);
                                if let Some(error) = incoming.error() {
                                    tracing::warn!(device_id = %self.device_id, path = %incoming.path, "StreamMagic rejected a request: {error}");
                                }
                                self.publish();
                            }
                        }
                        Some(Ok(Message::Close(_))) | None | Some(Err(_)) => {
                            tracing::info!(device_id = %self.device_id, "StreamMagic session closed; reconnecting");
                            ws = None;
                            self.state.session_lost("session closed; reconnecting".to_string());
                            next_connect = Instant::now() + backoff;
                            self.publish();
                        }
                        // Ping/Pong/binary: liveness only (tungstenite answers pings itself).
                        Some(Ok(_)) => {}
                    }
                }
                _ = tokio::time::sleep_until(next_connect), if ws.is_none() => {
                    match connect(&self.endpoint).await {
                        Ok(socket) => {
                            ws = Some(socket);
                            self.owed.clear();
                            backoff = RECONNECT_MIN;
                            last_rx = Instant::now();
                            self.open_session(&mut ws).await;
                        }
                        Err(detail) => {
                            self.state.detail = Some(detail);
                            self.state.channel = ControlChannel::None;
                            backoff = (backoff * 2).min(RECONNECT_MAX);
                            next_connect = Instant::now() + backoff;
                            self.publish();
                        }
                    }
                }
                _ = heartbeat.tick(), if ws.is_some() => {
                    if last_rx.elapsed() > SILENCE_LIMIT {
                        tracing::info!(device_id = %self.device_id, "StreamMagic session silent; reconnecting");
                        ws = None;
                        self.state.session_lost("no traffic; reconnecting".to_string());
                        next_connect = Instant::now();
                        self.publish();
                    } else if let Some(socket) = ws.as_mut() {
                        if let Err(error) = socket.send(Message::Ping(Vec::new().into())).await {
                            tracing::debug!(device_id = %self.device_id, "ping failed: {error}");
                        }
                    }
                }
            }
        }
        self.bus.publish(BusEvent::DeviceRemoved {
            device_id: self.device_id.clone(),
        });
    }

    /// Queries and subscriptions only: opening a session never changes the unit.
    async fn open_session(&mut self, ws: &mut Option<Ws>) {
        let mut frames: Vec<(&str, String)> = [
            protocol::PATH_INFO,
            protocol::PATH_SOURCES,
            protocol::PATH_ZONE_STATE,
            protocol::PATH_AUDIO,
        ]
        .iter()
        .map(|path| (*path, protocol::query(path)))
        .collect();
        frames.extend(
            protocol::SUBSCRIPTIONS
                .iter()
                .map(|path| (*path, protocol::subscribe(path))),
        );
        for (path, frame) in frames {
            if let Err(error) = self.request(ws, path, frame).await {
                tracing::debug!(device_id = %self.device_id, "session setup failed: {error}");
                *ws = None;
                self.state.session_lost(error);
                self.publish();
                return;
            }
        }
        self.state.channel = ControlChannel::Push;
        self.state.detail = None;
        self.publish();
    }

    /// Send one operation, wait for the unit's response to it, then ask for state so the
    /// device service sees a fresh report.
    async fn execute(
        &mut self,
        ws: &mut Option<Ws>,
        operation: &DeviceOperation,
    ) -> Result<(), String> {
        let (path, frame) = match operation {
            DeviceOperation::SetVolume(level) => {
                if !self.state.variable_output() {
                    return Err("volume is not supported by this device: it is not in \
                                variable-output (pre-amp) mode"
                        .to_string());
                }
                let frame = protocol::set_volume_percent(*level)
                    .ok_or_else(|| format!("{level} is not a valid StreamMagic volume (0-100)"))?;
                (protocol::PATH_ZONE_STATE, frame)
            }
            DeviceOperation::SetMute(muted) => {
                (protocol::PATH_ZONE_STATE, protocol::set_mute(*muted))
            }
            DeviceOperation::PowerOn => (protocol::PATH_POWER, protocol::power_on()),
            DeviceOperation::PowerStandby => {
                (protocol::PATH_POWER, protocol::power_network_standby())
            }
            DeviceOperation::SelectInput(id) => {
                (protocol::PATH_ZONE_STATE, protocol::set_source(id))
            }
        };
        send_text(ws, frame).await.inspect_err(|_| *ws = None)?;
        // Replies already owed on this path come first; the unit answers in order.
        let mut ahead = self.owed.get(path).copied().unwrap_or(0);
        let deadline = Instant::now() + WRITE_TIMEOUT;
        loop {
            let next = {
                let Some(socket) = ws.as_mut() else {
                    return Err("session closed".to_string());
                };
                tokio::time::timeout_at(deadline, socket.next()).await
            };
            match next {
                Err(_) => return Err("the unit did not answer the command".to_string()),
                Ok(Some(Ok(Message::Text(text)))) => {
                    let Some(incoming) = protocol::parse_message(&text) else {
                        continue;
                    };
                    self.state.apply(&incoming, &mut self.name);
                    if !incoming.is_response() {
                        continue;
                    }
                    if incoming.path == path && ahead > 0 {
                        ahead -= 1;
                        self.settle(path);
                        continue;
                    }
                    if incoming.path != path {
                        self.settle(&incoming.path);
                        continue;
                    }
                    // This path's owed replies are past: this one answers the command.
                    if let Some(error) = incoming.error() {
                        return Err(format!("the unit refused the command: {error}"));
                    }
                    break;
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(_))) | Ok(None) => {
                    *ws = None;
                    return Err("session closed".to_string());
                }
            }
        }
        self.request(
            ws,
            protocol::PATH_ZONE_STATE,
            protocol::query(protocol::PATH_ZONE_STATE),
        )
        .await
        .inspect_err(|_| *ws = None)
    }

    fn publish(&self) {
        let device = Device {
            device_id: self.device_id.clone(),
            vendor: "Cambridge Audio".to_string(),
            model: self.model.clone(),
            name: self.name.clone(),
            host: self.endpoint.host.clone(),
            discovery: self.discovery.clone(),
            maturity: DEVICE_CONTROL_MATURITY.to_string(),
            connection: DeviceConnection {
                control_channel: self.state.channel,
                detail: self.state.detail.clone(),
            },
            power: self.state.power.unwrap_or(DevicePower::Unknown),
            input: self.state.source.clone(),
            inputs: self.state.inputs.clone(),
            volume: DeviceVolume {
                scale: crate::bus::VolumeScale::Percentage,
                value: self.state.volume(),
                min: protocol::VOLUME_MIN_PERCENT,
                // The unit's own volume_limit_percent when it reports one, else the protocol's full scale.
                hardware_max: Some(
                    self.state
                        .volume_limit
                        .unwrap_or(protocol::VOLUME_MAX_PERCENT),
                ),
                ceiling: None,
                step: 1.0,
                is_muted: self.state.muted,
                display: None,
            },
            capabilities: capabilities_for(&self.model, self.state.variable_output()),
            revision: next_revision(),
            last_updated: now_ms(),
        };
        self.bus.publish(BusEvent::DeviceObserved {
            device: Box::new(device),
        });
    }
}
