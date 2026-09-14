//! UHC-owned NAA relay: one stable virtual NAA identity in front of an explicitly selected
//! physical endpoint.
//!
//! Extracted from the private PoC (`experiments/naa-router/native/naa-router/src/state.rs`,
//! preserved verbatim there). What is deliberately **not** carried over: the browser/HTTP control
//! listener and the standalone native HQPlayer controller. This relay has no listener of its own
//! except the NAA data socket HQPlayer connects to; every mutation is a method on [`NaaRelay`],
//! which is constructed and driven only by the owning `HqpAdapter` composition. There is therefore
//! exactly one command owner for routing, and no second user-facing control application.
//!
//! # Ownership model
//!
//! Two objects, on purpose:
//!
//! * [`RelayCore`] is the shared worker state (routes, selection, session sockets, observations).
//!   The accept loop and every session worker hold `Arc<RelayCore>`. It has no `Drop` that joins
//!   anything, so a worker can never be the last reference to an object whose destructor waits for
//!   that worker.
//! * [`NaaRelay`] is the external owner guard. Only the composition holds it. Its `Drop` runs
//!   [`NaaRelay::stop_listener`]: stop flag, socket shutdown, **join the accept loop first**, then
//!   drain the final worker set. Because workers never hold the guard, the last external drop
//!   always runs on the owner's thread and can never self-join.
//!
//! Threading: the accept loop and every relay session run on dedicated blocking `std::thread`s,
//! exactly as the PoC did, never on the Tokio runtime. Cancellation is socket shutdown.

use std::collections::HashSet;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::discovery;
use super::frame::MetadataPayload;
use super::outputs::{
    HqpDacDevice, HqpDacObservation, HqpDiscoveryObservation, HqpEndpointRef,
    HqpOutputAvailability, HqpOutputRoute, HqpRelayConfigView, HqpRelaySessionView,
    NaaRelaySettings, DEFAULT_NAA_PORT, VIRTUAL_DEVICE_ID,
};
use super::protocol;

/// Superseded workers may still be inside DNS resolution or connect for a route that is no longer
/// selected. Bound how many can pile up before HQPlayer's next attempt is refused.
const MAX_WORKERS: usize = 16;
/// Bound on retained per-endpoint DAC observations.
const MAX_DAC_OBSERVATIONS: usize = 256;
/// Bound on saved routes.
const MAX_ROUTES: usize = 128;
/// How long `stop` waits for session workers to notice their sockets were shut down. A worker
/// still inside DNS resolution or `connect_timeout` (bounded at 2 s in the protocol worker) can
/// outlive this; it is then reported as detached, never silently forgotten.
const WORKER_JOIN_BUDGET: Duration = Duration::from_secs(3);
/// Accept-loop wake interval while idle. Bounds `stop` latency.
const ACCEPT_POLL: Duration = Duration::from_millis(25);

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Persisted routes and selection for one instance's relay.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RelayRoutesFile {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub routes: Vec<HqpOutputRoute>,
    #[serde(default)]
    pub selected_route_id: Option<String>,
}

/// Validation the PoC applied to every saved or requested route.
pub fn validate_route(
    name: &str,
    host: &str,
    port: u16,
    device_id: Option<&str>,
) -> Result<(), String> {
    if name.trim().is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
        return Err("name must contain 1–256 printable bytes".into());
    }
    if host.is_empty()
        || host.len() > 253
        || host
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "/\\@?#".contains(c))
        || port == 0
    {
        return Err(
            "host must be an explicit hostname or IP address and port must be nonzero".into(),
        );
    }
    if let Some(device_id) = device_id {
        if device_id.len() > 1024 || device_id.chars().any(char::is_control) {
            return Err("invalid device_id".into());
        }
    }
    Ok(())
}

/// The live relay session view plus the sockets the owner can shut down.
struct Session {
    id: u64,
    view: HqpRelaySessionView,
    /// Physical endpoint this session was reserved for (observed forwarding destination).
    endpoint: HqpEndpointRef,
    upstream: TcpStream,
    downstream: Option<TcpStream>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ListenerState {
    Disabled,
    Bound(SocketAddr),
    Failed { reason: String, since: u64 },
}

struct RelayInner {
    settings: NaaRelaySettings,
    routes: Vec<HqpOutputRoute>,
    selected_route_id: Option<String>,
    /// Bumped by every disconnect/stop/select/edit/remove. Doubles as the operation token
    /// continuations check before acting.
    generation: u64,
    session: Option<Session>,
    last_error: Option<String>,
    dac_observations: Vec<HqpDacObservation>,
    discovery: Option<HqpDiscoveryObservation>,
    /// Closed after a failed automatic resume so the selected route stays visible but cannot
    /// start playing unexpectedly until the next selection.
    routing_enabled: bool,
    next_session: u64,
    /// Protocol workers reserved but not yet finished, including superseded ones.
    workers: usize,
    worker_handles: Vec<JoinHandle<()>>,
    listener: ListenerState,
    /// Bound UDP discovery responder address, when the settings enable discovery.
    responder: Option<SocketAddr>,
    /// Test seam: makes the accept loop fail on its next iteration.
    injected_accept_failure: Option<String>,
    /// Latest metadata projection for this HQPlayer instance. The owner may update this while a
    /// session is active; the protocol worker reads a clone per audio frame.
    metadata: Option<MetadataPayload>,
}

/// Shared worker state. Never joins threads; see the module docs for why.
pub struct RelayCore {
    inner: Mutex<RelayInner>,
    persist_path: Option<PathBuf>,
    /// Woken on every observable change so the owner can republish without polling hot.
    changed: tokio::sync::Notify,
    /// Test seam: called by the accept loop after a worker is spawned and before it is tracked.
    before_track_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

struct AcceptLoop {
    stop: Arc<AtomicBool>,
    join: JoinHandle<()>,
}

struct Responder {
    stop: Arc<AtomicBool>,
    join: JoinHandle<()>,
}

/// Snapshot of relay state for the owner's projection builder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayObservation {
    pub relay: HqpRelayConfigView,
    pub availability: HqpOutputAvailability,
    /// Bound UDP discovery responder, when advertising.
    pub responder: Option<SocketAddr>,
    pub routes: Vec<HqpOutputRoute>,
    pub selected_route_id: Option<String>,
    pub generation: u64,
    pub desired_destination: Option<HqpEndpointRef>,
    pub observed_forwarding_destination: Option<HqpEndpointRef>,
    pub session: Option<HqpRelaySessionView>,
    pub discovery: Option<HqpDiscoveryObservation>,
    pub dac_observations: Vec<HqpDacObservation>,
    pub routing_enabled: bool,
    pub last_error: Option<String>,
    pub observed_at: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelayStopReport {
    pub workers_joined: usize,
    /// Workers that had not observed their shutdown inside the join budget. They hold only the
    /// shared core and end when their bounded connect/DNS attempt returns; a non-zero value is
    /// reported, never hidden.
    pub workers_detached: usize,
    pub accept_loop_panicked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayStopOutcome {
    pub had_session: bool,
    pub generation: u64,
    pub persist_error: Option<String>,
}

/// Opaque handle to the shared worker state, for lifecycle tests that need to stand in for a
/// worker holding it while the external owner drops. Holding it never keeps a listener alive.
#[doc(hidden)]
pub struct RelaySharedState(#[allow(dead_code)] Arc<RelayCore>);

/// One instance's managed relay: the external owner guard. Constructed only by the owning
/// adapter composition.
pub struct NaaRelay {
    core: Arc<RelayCore>,
    accept_loop: Mutex<Option<AcceptLoop>>,
    responder: Mutex<Option<Responder>>,
    stop_in_progress: AtomicBool,
}

impl NaaRelay {
    /// Update the effective metadata projection used by active sessions.
    pub fn set_metadata(&self, metadata: Option<super::frame::MetadataPayload>) {
        self.core.set_metadata(metadata);
    }
    /// Load routes (if a persistence path is given) and construct an idle relay. No socket is
    /// opened here regardless of `settings.enabled`; the owner starts the listener explicitly.
    pub fn new(settings: NaaRelaySettings, persist_path: Option<PathBuf>) -> Result<Self, String> {
        let file = match &persist_path {
            Some(path) if path.exists() => {
                let metadata = std::fs::metadata(path).map_err(|e| e.to_string())?;
                if metadata.len() > 1024 * 1024 {
                    return Err("route file exceeds 1 MiB".into());
                }
                let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
                serde_json::from_slice::<RelayRoutesFile>(&bytes).map_err(|e| e.to_string())?
            }
            _ => RelayRoutesFile::default(),
        };
        if file.routes.len() > MAX_ROUTES {
            return Err(format!("at most {MAX_ROUTES} routes"));
        }
        let mut ids = HashSet::new();
        for route in &file.routes {
            validate_route(
                &route.name,
                &route.host,
                route.port,
                route.device_id.as_deref(),
            )?;
            if route.route_id.is_empty() || !ids.insert(route.route_id.clone()) {
                return Err("duplicate or empty route id".into());
            }
        }
        if file
            .selected_route_id
            .as_ref()
            .is_some_and(|id| !ids.contains(id))
        {
            return Err("selected route does not exist".into());
        }
        Ok(Self {
            core: Arc::new(RelayCore {
                inner: Mutex::new(RelayInner {
                    settings,
                    routes: file.routes,
                    selected_route_id: file.selected_route_id,
                    generation: 0,
                    session: None,
                    last_error: None,
                    dac_observations: Vec::new(),
                    discovery: None,
                    routing_enabled: true,
                    next_session: 0,
                    workers: 0,
                    worker_handles: Vec::new(),
                    listener: ListenerState::Disabled,
                    responder: None,
                    injected_accept_failure: None,
                    metadata: None,
                }),
                persist_path,
                changed: tokio::sync::Notify::new(),
                before_track_hook: Mutex::new(None),
            }),
            accept_loop: Mutex::new(None),
            responder: Mutex::new(None),
            stop_in_progress: AtomicBool::new(false),
        })
    }

    /// Stable display name HQPlayer sees.
    pub fn name(&self) -> String {
        self.core.name()
    }

    pub fn virtual_id(&self) -> &'static str {
        VIRTUAL_DEVICE_ID
    }

    pub fn settings(&self) -> NaaRelaySettings {
        lock(&self.core.inner).settings.clone()
    }

    /// Replace settings. The caller restarts the listener when they change.
    pub fn set_settings(&self, settings: NaaRelaySettings) {
        lock(&self.core.inner).settings = settings;
        self.core.changed.notify_one();
    }

    /// Wait for the next observable change.
    pub async fn changed(&self) {
        self.core.changed.notified().await;
    }

    pub fn generation(&self) -> u64 {
        lock(&self.core.inner).generation
    }

    pub fn listener_addr(&self) -> Option<SocketAddr> {
        match &lock(&self.core.inner).listener {
            ListenerState::Bound(addr) => Some(*addr),
            _ => None,
        }
    }

    /// Bound UDP discovery responder address while discovery is enabled and listening.
    pub fn responder_addr(&self) -> Option<SocketAddr> {
        lock(&self.core.inner).responder
    }

    pub fn availability(&self) -> HqpOutputAvailability {
        RelayCore::availability_of(&lock(&self.core.inner))
    }

    // ------------------------------------------------------------------------------------------
    // Listener lifecycle
    // ------------------------------------------------------------------------------------------

    /// Bind the NAA listener from the current settings and start the accept loop. An empty
    /// allow-list accepts any NAA peer; a populated list restricts connections to those IPs.
    pub fn start_listener(&self) -> Result<SocketAddr, String> {
        if let Some(addr) = self.listener_addr() {
            return Ok(addr);
        }
        // A previous accept loop that ended (owner stop or failure) is joined before rebinding so
        // two loops can never coexist.
        if let Some(previous) = lock(&self.accept_loop).take() {
            previous.stop.store(true, Ordering::Release);
            let _ = previous.join.join();
        }
        let settings = self.settings();
        if !settings.enabled {
            return Err("relay is disabled for this instance".into());
        }
        let bind: SocketAddr = settings
            .bind
            .parse()
            .map_err(|e| format!("invalid relay bind address {:?}: {e}", settings.bind))?;
        let allow = settings
            .hqp_allow
            .iter()
            .map(|ip| {
                ip.parse::<IpAddr>()
                    .map_err(|e| format!("invalid hqp_allow entry {ip:?}: {e}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let listener = match TcpListener::bind(bind) {
            Ok(listener) => listener,
            Err(e) => {
                let reason = format!("cannot bind NAA listener {bind}: {e}");
                self.core.mark_failed(reason.clone());
                return Err(reason);
            }
        };
        if let Err(e) = listener.set_nonblocking(true) {
            let reason = format!("cannot configure NAA listener {bind}: {e}");
            self.core.mark_failed(reason.clone());
            return Err(reason);
        }
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        // The accept loop holds only the shared core, never this owner guard.
        let core = Arc::clone(&self.core);
        let stop_flag = stop.clone();
        let join = thread::Builder::new()
            .name(format!("naa-relay-accept-{}", addr.port()))
            .spawn({
                let allow = allow.clone();
                move || RelayCore::accept_loop(core, listener, allow, stop_flag)
            })
            .map_err(|e| format!("cannot spawn NAA accept thread: {e}"))?;
        {
            let mut inner = lock(&self.core.inner);
            inner.listener = ListenerState::Bound(addr);
            inner.last_error = None;
        }
        *lock(&self.accept_loop) = Some(AcceptLoop { stop, join });
        self.start_responder(&settings, addr, &allow);
        self.core.changed.notify_one();
        Ok(addr)
    }

    /// Advertise the stable virtual NAA on UDP discovery (same port as the TCP listener, as the
    /// protocol requires) when the settings name an explicit interface. Failure to bind is
    /// recorded, never hidden; the TCP relay keeps working without discovery.
    fn start_responder(&self, settings: &NaaRelaySettings, tcp: SocketAddr, allow: &[IpAddr]) {
        let Some(interface) = settings.discovery_interface.as_deref() else {
            return;
        };
        let interface: Ipv4Addr = match interface.parse() {
            Ok(ip) => ip,
            Err(e) => {
                lock(&self.core.inner).last_error =
                    Some(format!("discovery interface {interface:?} is invalid: {e}"));
                return;
            }
        };
        if interface.is_unspecified() || interface.is_multicast() {
            lock(&self.core.inner).last_error =
                Some("discovery requires an explicit local IPv4 interface".to_string());
            return;
        }
        if !tcp.ip().is_unspecified() && tcp.ip() != IpAddr::V4(interface) {
            lock(&self.core.inner).last_error = Some(format!(
                "discovery interface {interface} must match the relay bind address {}",
                tcp.ip()
            ));
            return;
        }
        // HQPlayer's scanner asks the standard discovery port and connects to the port the
        // answering socket used. Advertising a relay whose TCP listener is elsewhere would name
        // an unreachable endpoint, so discovery requires the two to agree.
        if tcp.port() != settings.discovery_port {
            lock(&self.core.inner).last_error = Some(format!(
                "discovery requires the relay TCP port ({}) to equal discovery_port ({})",
                tcp.port(),
                settings.discovery_port
            ));
            return;
        }
        let advertisement = match discovery::advertisement(&settings.adapter_name) {
            Ok(bytes) => bytes,
            Err(e) => {
                lock(&self.core.inner).last_error = Some(e);
                return;
            }
        };
        // A loopback-configured relay must not be reachable from the LAN through its discovery
        // socket either: bind the responder to loopback. A LAN interface needs the wildcard bind
        // to receive multicast, and its optional allow-list gates replies.
        let bind_ip = if interface.is_loopback() {
            interface
        } else {
            Ipv4Addr::UNSPECIFIED
        };
        let socket = match UdpSocket::bind((bind_ip, tcp.port())) {
            Ok(socket) => socket,
            Err(e) => {
                lock(&self.core.inner).last_error = Some(format!(
                    "cannot bind NAA discovery responder on UDP {}:{}: {e}",
                    bind_ip,
                    tcp.port()
                ));
                return;
            }
        };
        if !interface.is_loopback() {
            for group in discovery::GROUPS {
                if let Err(e) = socket.join_multicast_v4(&group, &interface) {
                    tracing::warn!(%group, %interface, %e, "NAA discovery multicast join failed");
                }
            }
        }
        let allow: Vec<IpAddr> = if allow.is_empty() && interface.is_loopback() {
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]
        } else {
            allow.to_vec()
        };
        if socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .is_err()
        {
            return;
        }
        let bound = match socket.local_addr() {
            Ok(addr) => addr,
            Err(_) => return,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let join = match thread::Builder::new()
            .name(format!("naa-relay-discovery-{}", tcp.port()))
            .spawn(move || {
                let mut buf = [0u8; 4097];
                while !stop_flag.load(Ordering::Acquire) {
                    match socket.recv_from(&mut buf) {
                        Ok((n, peer)) => {
                            if (allow.is_empty() || allow.contains(&peer.ip()))
                                && discovery::valid_request(&buf[..n])
                            {
                                let _ = socket.send_to(&advertisement, peer);
                            }
                        }
                        Err(e)
                            if matches!(
                                e.kind(),
                                io::ErrorKind::WouldBlock
                                    | io::ErrorKind::TimedOut
                                    | io::ErrorKind::Interrupted
                            ) => {}
                        Err(_) => break,
                    }
                }
                // Socket drops here, releasing the UDP port.
            }) {
            Ok(join) => join,
            Err(_) => return,
        };
        *lock(&self.responder) = Some(Responder { stop, join });
        lock(&self.core.inner).responder = Some(bound);
    }

    fn stop_responder(&self) {
        if let Some(responder) = lock(&self.responder).take() {
            responder.stop.store(true, Ordering::Release);
            let _ = responder.join.join();
        }
        lock(&self.core.inner).responder = None;
    }

    /// Stop the accept loop, tear down the active pair, and wait (bounded) for every session
    /// worker to observe its shutdown. Releases the bind address. Idempotent.
    ///
    /// Order matters: the accept loop is joined **before** the worker set is drained, so nothing
    /// can be spawned or tracked after collection.
    pub fn stop_listener(&self) -> RelayStopReport {
        self.stop_in_progress.store(true, Ordering::Release);
        self.stop_responder();
        let accept = lock(&self.accept_loop).take();
        if let Some(accept) = &accept {
            accept.stop.store(true, Ordering::Release);
        }
        // Close the pair now so a blocked worker wakes while the accept loop winds down.
        {
            let mut inner = lock(&self.core.inner);
            RelayCore::disconnect_locked(&mut inner);
        }
        let mut report = RelayStopReport::default();
        if let Some(accept) = accept {
            if accept.join.join().is_err() {
                report.accept_loop_panicked = true;
            }
        }
        // The accept loop is gone: this is the final worker set. Any session it reserved in the
        // window between the first shutdown and its exit is closed here.
        let handles = {
            let mut inner = lock(&self.core.inner);
            RelayCore::disconnect_locked(&mut inner);
            std::mem::take(&mut inner.worker_handles)
        };
        let deadline = Instant::now() + WORKER_JOIN_BUDGET;
        let mut pending = handles;
        while !pending.is_empty() && Instant::now() < deadline {
            let (finished, still): (Vec<_>, Vec<_>) =
                pending.into_iter().partition(|h| h.is_finished());
            for handle in finished {
                let _ = handle.join();
                report.workers_joined += 1;
            }
            pending = still;
            if !pending.is_empty() {
                thread::sleep(Duration::from_millis(5));
            }
        }
        report.workers_detached = pending.len();
        {
            let mut inner = lock(&self.core.inner);
            if !matches!(inner.listener, ListenerState::Failed { .. }) {
                inner.listener = ListenerState::Disabled;
            }
        }
        self.stop_in_progress.store(false, Ordering::Release);
        self.core.changed.notify_one();
        report
    }

    // ------------------------------------------------------------------------------------------
    // Routes and selection (relay-local; native transport coordination lives in the owner)
    // ------------------------------------------------------------------------------------------

    pub fn routes(&self) -> Vec<HqpOutputRoute> {
        lock(&self.core.inner).routes.clone()
    }

    pub fn route(&self, route_id: &str) -> Option<HqpOutputRoute> {
        lock(&self.core.inner)
            .routes
            .iter()
            .find(|r| r.route_id == route_id)
            .cloned()
    }

    pub fn selected_route_id(&self) -> Option<String> {
        lock(&self.core.inner).selected_route_id.clone()
    }

    pub fn add_route(
        &self,
        name: &str,
        host: &str,
        port: Option<u16>,
        device_id: Option<String>,
    ) -> Result<HqpOutputRoute, String> {
        let port = port.unwrap_or(DEFAULT_NAA_PORT);
        let device_id = device_id.filter(|d| !d.is_empty());
        validate_route(name, host, port, device_id.as_deref())?;
        let mut inner = lock(&self.core.inner);
        if inner.routes.len() >= MAX_ROUTES {
            return Err(format!("at most {MAX_ROUTES} routes"));
        }
        let route = HqpOutputRoute {
            route_id: uuid::Uuid::new_v4().to_string(),
            name: name.trim().to_string(),
            host: host.to_string(),
            port,
            device_id,
            imported_from: None,
        };
        let mut routes = inner.routes.clone();
        routes.push(route.clone());
        self.core.persist(&routes, &inner.selected_route_id)?;
        inner.routes = routes;
        drop(inner);
        self.core.changed.notify_one();
        Ok(route)
    }

    /// Add already-validated routes with their identifiers preserved (import). Never selects.
    pub fn add_imported_routes(&self, imported: Vec<HqpOutputRoute>) -> Result<(), String> {
        let mut inner = lock(&self.core.inner);
        let mut routes = inner.routes.clone();
        for route in imported {
            if routes.iter().any(|r| r.route_id == route.route_id) {
                return Err(format!("route id {:?} already exists", route.route_id));
            }
            routes.push(route);
        }
        if routes.len() > MAX_ROUTES {
            return Err(format!("at most {MAX_ROUTES} routes"));
        }
        self.core.persist(&routes, &inner.selected_route_id)?;
        inner.routes = routes;
        drop(inner);
        self.core.changed.notify_one();
        Ok(())
    }

    pub fn update_route(
        &self,
        route_id: &str,
        name: &str,
        host: &str,
        port: Option<u16>,
        device_id: Option<String>,
    ) -> Result<HqpOutputRoute, String> {
        let port = port.unwrap_or(DEFAULT_NAA_PORT);
        let device_id = device_id.filter(|d| !d.is_empty());
        validate_route(name, host, port, device_id.as_deref())?;
        let mut inner = lock(&self.core.inner);
        let mut routes = inner.routes.clone();
        let route = routes
            .iter_mut()
            .find(|r| r.route_id == route_id)
            .ok_or("unknown route_id")?;
        route.name = name.trim().to_string();
        route.host = host.to_string();
        route.port = port;
        route.device_id = device_id;
        let result = route.clone();
        self.core.persist(&routes, &inner.selected_route_id)?;
        if inner.selected_route_id.as_deref() == Some(route_id) {
            RelayCore::disconnect_locked(&mut inner);
            inner.last_error = None;
        }
        inner.routes = routes;
        drop(inner);
        self.core.changed.notify_one();
        Ok(result)
    }

    pub fn remove_route(&self, route_id: &str) -> Result<(), String> {
        let mut inner = lock(&self.core.inner);
        if !inner.routes.iter().any(|r| r.route_id == route_id) {
            return Err("unknown route_id".into());
        }
        let routes: Vec<_> = inner
            .routes
            .iter()
            .filter(|r| r.route_id != route_id)
            .cloned()
            .collect();
        let selected = inner.selected_route_id.as_deref() == Some(route_id);
        let selection = if selected {
            None
        } else {
            inner.selected_route_id.clone()
        };
        self.core.persist(&routes, &selection)?;
        if selected {
            RelayCore::disconnect_locked(&mut inner);
            inner.last_error = None;
        }
        inner.routes = routes;
        inner.selected_route_id = selection;
        drop(inner);
        self.core.changed.notify_one();
        Ok(())
    }

    /// Commit a selection: persist, tear down the current NAA pair and reopen the routing gate.
    /// Bumps the generation and returns it. The owner decides whether native transport work
    /// surrounds this commit.
    pub fn commit_selection(&self, route_id: &str) -> Result<u64, String> {
        let mut inner = lock(&self.core.inner);
        if !inner.routes.iter().any(|r| r.route_id == route_id) {
            return Err("unknown route_id".into());
        }
        let selection = Some(route_id.to_string());
        self.core.persist(&inner.routes, &selection)?;
        RelayCore::disconnect_locked(&mut inner);
        inner.selected_route_id = selection;
        inner.last_error = None;
        inner.routing_enabled = true;
        let generation = inner.generation;
        drop(inner);
        self.core.changed.notify_one();
        Ok(generation)
    }

    /// Local Stop: tear down the session and clear the selection first, then persist. A
    /// persistence failure is reported rather than allowed to leave audio flowing.
    pub fn clear_selection(&self) -> RelayStopOutcome {
        let mut inner = lock(&self.core.inner);
        let had_session = inner.session.is_some();
        RelayCore::disconnect_locked(&mut inner);
        inner.selected_route_id = None;
        inner.routing_enabled = true;
        let persist_error = self.core.persist(&inner.routes, &None).err().map(|e| {
            format!("Stopped, but the selection could not be saved; a restart would reconnect to the previous route. {e}")
        });
        inner.last_error = persist_error.clone();
        let generation = inner.generation;
        drop(inner);
        self.core.changed.notify_one();
        RelayStopOutcome {
            had_session,
            generation,
            persist_error,
        }
    }

    /// Supersede a pending operation without touching routed audio: bumps the generation.
    pub fn bump_generation(&self) -> u64 {
        let mut inner = lock(&self.core.inner);
        inner.generation += 1;
        inner.generation
    }

    /// Tear down the pair after a failed resume and close the routing gate so nothing can keep
    /// playing unexpectedly until the next selection. Only acts while `generation` is current.
    pub fn disable_routing_after_failed_resume(&self, generation: u64, message: String) -> bool {
        let mut inner = lock(&self.core.inner);
        if inner.generation != generation {
            return false;
        }
        RelayCore::disconnect_locked(&mut inner);
        inner.routing_enabled = false;
        inner.last_error = Some(message);
        drop(inner);
        self.core.changed.notify_one();
        true
    }

    /// Record a completed discovery scan (`endpoints == []` is an observed empty result).
    pub fn record_discovery(&self, observation: HqpDiscoveryObservation) {
        lock(&self.core.inner).discovery = Some(observation);
        self.core.changed.notify_one();
    }

    // ------------------------------------------------------------------------------------------
    // Observation
    // ------------------------------------------------------------------------------------------

    pub fn observe(&self) -> RelayObservation {
        self.core.observe()
    }

    /// Live session view for one generation, used by the owner's resume wait.
    pub fn session_for(&self, generation: u64) -> Result<Option<HqpRelaySessionView>, String> {
        let inner = lock(&self.core.inner);
        if inner.generation != generation {
            return Err("selection superseded by Stop or a newer request".into());
        }
        Ok(inner.session.as_ref().map(|s| s.view.clone()))
    }

    pub fn last_error(&self) -> Option<String> {
        lock(&self.core.inner).last_error.clone()
    }

    // ------------------------------------------------------------------------------------------
    // Test seams (labelled; no production caller)
    // ------------------------------------------------------------------------------------------

    /// A stand-in for a worker holding the shared state while the owner drops.
    #[doc(hidden)]
    pub fn shared_state_for_tests(&self) -> RelaySharedState {
        RelaySharedState(Arc::clone(&self.core))
    }

    /// Worker handles still tracked (not yet drained by `stop_listener`).
    #[doc(hidden)]
    pub fn tracked_worker_handles_for_tests(&self) -> usize {
        lock(&self.core.inner).worker_handles.len()
    }

    /// Make the accept loop fail on its next iteration, as an accept(2) error would.
    #[doc(hidden)]
    pub fn inject_accept_failure_for_tests(&self, reason: &str) {
        lock(&self.core.inner).injected_accept_failure = Some(reason.to_string());
    }

    /// Run `hook` on the accept thread after a session worker is spawned and before its handle is
    /// tracked, so a test can hold the accept loop exactly in the window a stop must survive.
    #[doc(hidden)]
    pub fn set_before_track_hook_for_tests(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *lock(&self.core.before_track_hook) = Some(hook);
    }

    /// Whether `stop_listener` has begun and not yet returned.
    #[doc(hidden)]
    pub fn stop_in_progress_for_tests(&self) -> bool {
        self.stop_in_progress.load(Ordering::Acquire)
    }
}

impl Drop for NaaRelay {
    fn drop(&mut self) {
        // The owner guard is never held by a worker, so this join cannot be a self-join. An owner
        // that forgets to stop still cannot leave a listener or a forwarding pair behind.
        self.stop_listener();
    }
}

impl RelayCore {
    /// Install the effective metadata projection used by active NAA sessions. Empty/absent
    /// metadata leaves HQPlayer's original sections untouched.
    pub fn set_metadata(&self, metadata: Option<MetadataPayload>) {
        lock(&self.inner).metadata = metadata;
        self.changed.notify_one();
    }

    pub(super) fn metadata(&self) -> Option<MetadataPayload> {
        lock(&self.inner).metadata.clone()
    }

    pub(super) fn name(&self) -> String {
        lock(&self.inner).settings.adapter_name.clone()
    }

    fn availability_of(inner: &RelayInner) -> HqpOutputAvailability {
        match &inner.listener {
            ListenerState::Disabled => HqpOutputAvailability::Disabled,
            ListenerState::Bound(_) => HqpOutputAvailability::Available,
            ListenerState::Failed { reason, since } => HqpOutputAvailability::Unavailable {
                reason: reason.clone(),
                since: *since,
            },
        }
    }

    fn mark_failed(&self, reason: String) {
        let mut inner = lock(&self.inner);
        inner.listener = ListenerState::Failed {
            reason,
            since: now(),
        };
        drop(inner);
        self.changed.notify_one();
    }

    fn accept_loop(
        core: Arc<Self>,
        listener: TcpListener,
        allow: Vec<IpAddr>,
        stop: Arc<AtomicBool>,
    ) {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            while !stop.load(Ordering::Acquire) {
                if let Some(reason) = lock(&core.inner).injected_accept_failure.take() {
                    return Err(format!("NAA accept failed: {reason}"));
                }
                match listener.accept() {
                    Ok((client, peer)) => {
                        if !allow.is_empty() && !allow.contains(&peer.ip()) {
                            let _ = client.shutdown(Shutdown::Both);
                            continue;
                        }
                        if client.set_nonblocking(false).is_err() {
                            let _ = client.shutdown(Shutdown::Both);
                            continue;
                        }
                        // Reserve the exclusive session before spawning: connection floods do
                        // not create unbounded workers.
                        match core.reserve(&client) {
                            Ok((id, route)) => {
                                let worker_core = Arc::clone(&core);
                                let spawned = thread::Builder::new()
                                    .name(format!("naa-relay-session-{id}"))
                                    .spawn(move || protocol::serve(client, worker_core, id, route));
                                if let Some(hook) = lock(&core.before_track_hook).clone() {
                                    hook();
                                }
                                match spawned {
                                    Ok(handle) => core.track_worker(handle),
                                    Err(e) => {
                                        core.finish(
                                            id,
                                            Some(format!("cannot spawn session worker: {e}")),
                                        );
                                    }
                                }
                            }
                            Err(_) => {
                                let _ = client.shutdown(Shutdown::Both);
                            }
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(ACCEPT_POLL);
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(format!("NAA accept failed: {e}")),
                }
            }
            Ok(())
        }));
        // The listener is dropped here, releasing the bind address.
        drop(listener);
        let stopped_by_owner = stop.load(Ordering::Acquire);
        let mut inner = lock(&core.inner);
        match (outcome, stopped_by_owner) {
            (Ok(Ok(())), true) => inner.listener = ListenerState::Disabled,
            (Ok(Ok(())), false) => {
                inner.listener = ListenerState::Failed {
                    reason: "NAA accept loop exited unexpectedly".to_string(),
                    since: now(),
                }
            }
            (Ok(Err(reason)), _) => {
                inner.listener = ListenerState::Failed {
                    reason,
                    since: now(),
                }
            }
            (Err(_), _) => {
                inner.listener = ListenerState::Failed {
                    reason: "NAA accept loop panicked".to_string(),
                    since: now(),
                }
            }
        }
        // Whatever ended the listener, no session may keep forwarding behind a dead owner.
        Self::disconnect_locked(&mut inner);
        drop(inner);
        core.changed.notify_one();
    }

    fn track_worker(&self, handle: JoinHandle<()>) {
        let mut inner = lock(&self.inner);
        inner.worker_handles.retain(|h| !h.is_finished());
        inner.worker_handles.push(handle);
    }

    fn persist(&self, routes: &[HqpOutputRoute], selected: &Option<String>) -> Result<(), String> {
        let Some(path) = &self.persist_path else {
            return Ok(());
        };
        let file = RelayRoutesFile {
            version: 1,
            routes: routes.to_vec(),
            selected_route_id: selected.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&file).map_err(|e| e.to_string())?;
        super::super::secure_atomic_write(path, &bytes)
            .map_err(|e| format!("cannot persist route configuration: {e}"))
    }

    fn disconnect_locked(inner: &mut RelayInner) {
        inner.generation += 1;
        if let Some(session) = inner.session.take() {
            let _ = session.upstream.shutdown(Shutdown::Both);
            if let Some(downstream) = session.downstream {
                let _ = downstream.shutdown(Shutdown::Both);
            }
        }
    }

    // ------------------------------------------------------------------------------------------
    // Session callbacks used by the protocol worker
    // ------------------------------------------------------------------------------------------

    pub(super) fn reserve(&self, client: &TcpStream) -> Result<(u64, HqpOutputRoute), String> {
        let mut inner = lock(&self.inner);
        if inner.session.is_some() {
            return Err("relay busy".into());
        }
        if inner.workers >= MAX_WORKERS {
            return Err("too many session workers still shutting down".into());
        }
        let Some(route) = inner
            .routes
            .iter()
            .find(|r| Some(&r.route_id) == inner.selected_route_id.as_ref())
            .cloned()
        else {
            // HQPlayer retries automatically after Stop. The routine hint must not erase a more
            // important standing error such as an unsaved Stop.
            if inner.last_error.is_none() {
                inner.last_error = Some("Select a route before connecting HQPlayer".into());
            }
            return Err("no route selected".into());
        };
        if !inner.routing_enabled {
            return Err("routing disabled after a failed resume; select the route again".into());
        }
        let upstream = client.try_clone().map_err(|e| e.to_string())?;
        inner.next_session += 1;
        inner.workers += 1;
        let id = inner.next_session;
        let generation = inner.generation;
        inner.session = Some(Session {
            id,
            upstream,
            downstream: None,
            endpoint: HqpEndpointRef {
                host: route.host.clone(),
                port: route.port,
                device_id: route.device_id.clone(),
            },
            view: HqpRelaySessionView {
                session_id: id,
                route_id: route.route_id.clone(),
                route_generation: generation,
                state: "connecting".into(),
                peer: client
                    .peer_addr()
                    .map(|a| a.to_string())
                    .unwrap_or_default(),
                connected_at: now(),
                bytes_to_naa: 0,
                bytes_from_naa: 0,
                initialized: false,
                started: false,
                current_stream_audio_bytes: 0,
            },
        });
        inner.last_error = None;
        drop(inner);
        self.changed.notify_one();
        Ok((id, route))
    }

    /// Record a stream lifecycle transition or explicitly successful acknowledgement.
    pub(super) fn milestone(&self, id: u64, name: &str) {
        let mut inner = lock(&self.inner);
        if let Some(session) = inner.session.as_mut().filter(|s| s.id == id) {
            match name {
                "initializing" => {
                    session.view.initialized = false;
                    session.view.started = false;
                    session.view.current_stream_audio_bytes = 0;
                }
                "starting" => {
                    session.view.started = false;
                    session.view.current_stream_audio_bytes = 0;
                }
                "stopped" => session.view.started = false,
                "initialized" => session.view.initialized = true,
                "started" => session.view.started = true,
                _ => {}
            }
            session.view.state = name.into();
            drop(inner);
            self.changed.notify_one();
        }
    }

    /// Count forwarded audio-section payload bytes only.
    pub(super) fn audio(&self, id: u64, n: usize) {
        let mut inner = lock(&self.inner);
        if let Some(session) = inner.session.as_mut().filter(|s| s.id == id) {
            let first = session.view.current_stream_audio_bytes == 0;
            session.view.current_stream_audio_bytes += n as u64;
            if first {
                drop(inner);
                self.changed.notify_one();
            }
        }
    }

    pub(super) fn attach(&self, id: u64, downstream: &TcpStream) -> io::Result<()> {
        let mut inner = lock(&self.inner);
        let session = inner
            .session
            .as_mut()
            .filter(|s| s.id == id)
            .ok_or_else(|| io::Error::other("route changed during connect"))?;
        session.downstream = Some(downstream.try_clone()?);
        session.view.state = "authenticating".into();
        drop(inner);
        self.changed.notify_one();
        Ok(())
    }

    pub(super) fn update(&self, id: u64, to_naa: bool, n: usize, status: Option<&str>) {
        let mut inner = lock(&self.inner);
        if let Some(session) = inner.session.as_mut().filter(|s| s.id == id) {
            if to_naa {
                session.view.bytes_to_naa += n as u64;
            } else {
                session.view.bytes_from_naa += n as u64;
            }
            if let Some(status) = status {
                if session.view.state != status {
                    session.view.state = status.into();
                    drop(inner);
                    self.changed.notify_one();
                }
            }
        }
    }

    /// Whole-list replacement of the physical DAC observation for the session's endpoint.
    pub(super) fn devices(&self, id: u64, devices: Vec<HqpDacDevice>) {
        let mut inner = lock(&self.inner);
        let Some(endpoint) = inner
            .session
            .as_ref()
            .filter(|s| s.id == id)
            .map(|s| s.endpoint.clone())
        else {
            return;
        };
        inner
            .dac_observations
            .retain(|entry| entry.host != endpoint.host || entry.port != endpoint.port);
        if inner.dac_observations.len() >= MAX_DAC_OBSERVATIONS {
            inner.dac_observations.remove(0);
        }
        inner.dac_observations.push(HqpDacObservation {
            host: endpoint.host,
            port: endpoint.port,
            observed_at: now(),
            session_id: id,
            provenance: "relayed-getdevices".to_string(),
            devices,
        });
        drop(inner);
        self.changed.notify_one();
    }

    pub(super) fn finish(&self, id: u64, error: Option<String>) {
        let mut inner = lock(&self.inner);
        // Every reserved worker calls finish exactly once, matched or superseded.
        inner.workers = inner.workers.saturating_sub(1);
        if inner.session.as_ref().is_some_and(|s| s.id == id) {
            if let Some(session) = inner.session.take() {
                let _ = session.upstream.shutdown(Shutdown::Both);
                if let Some(downstream) = session.downstream {
                    let _ = downstream.shutdown(Shutdown::Both);
                }
            }
            inner.last_error = error;
        }
        drop(inner);
        self.changed.notify_one();
    }

    pub(super) fn observe(&self) -> RelayObservation {
        let inner = lock(&self.inner);
        let selected = inner
            .routes
            .iter()
            .find(|r| Some(&r.route_id) == inner.selected_route_id.as_ref());
        RelayObservation {
            relay: HqpRelayConfigView {
                enabled: inner.settings.enabled,
                adapter_name: inner.settings.adapter_name.clone(),
                virtual_device_id: VIRTUAL_DEVICE_ID.to_string(),
                bind: match &inner.listener {
                    ListenerState::Bound(addr) => Some(addr.to_string()),
                    _ => Some(inner.settings.bind.clone()),
                },
                hqp_allow: inner.settings.hqp_allow.clone(),
                discovery_interface: inner.settings.discovery_interface.clone(),
                discovery_port: inner.settings.discovery_port,
                discovery_responder: inner.responder.map(|a| a.to_string()),
            },
            availability: Self::availability_of(&inner),
            responder: inner.responder,
            routes: inner.routes.clone(),
            selected_route_id: inner.selected_route_id.clone(),
            generation: inner.generation,
            desired_destination: selected.map(|r| HqpEndpointRef {
                host: r.host.clone(),
                port: r.port,
                device_id: r.device_id.clone(),
            }),
            observed_forwarding_destination: inner
                .session
                .as_ref()
                .filter(|s| s.view.initialized)
                .map(|s| s.endpoint.clone()),
            session: inner.session.as_ref().map(|s| s.view.clone()),
            discovery: inner.discovery.clone(),
            dac_observations: inner.dac_observations.clone(),
            routing_enabled: inner.routing_enabled,
            last_error: inner.last_error.clone(),
            observed_at: now(),
        }
    }
}
