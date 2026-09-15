use crate::control;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Write},
    net::{Shutdown, SocketAddr, TcpStream},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
pub const VIRTUAL_ID: &str = "hiphi:router";
/// Whole /api/select budget when HQPlayer control is configured.
const SELECT_DEADLINE: Duration = Duration::from_secs(10);
/// Each individual control round trip is bounded separately inside the budget.
const CONTROL_STEP: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(50);
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Route {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub device_id: String,
}
#[derive(Deserialize)]
pub struct NewRoute {
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub device_id: String,
}
fn default_port() -> u16 {
    43210
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub routes: Vec<Route>,
    pub selected_route_id: Option<String>,
}
#[derive(Clone, Serialize)]
pub struct Device {
    pub id: String,
    pub description: String,
}
#[derive(Clone, Serialize)]
pub struct DacObservation {
    pub host: String,
    pub port: u16,
    pub observed_at: u64,
    pub devices: Vec<Device>,
}
#[derive(Clone, Serialize)]
pub struct SessionView {
    pub state: String,
    pub route_id: String,
    pub peer: String,
    pub connected_at: u64,
    pub bytes_to_naa: u64,
    pub bytes_from_naa: u64,
    /// Downstream initialize reply with explicit success (result="1").
    pub initialized: bool,
    /// Downstream start reply with explicit success (result="1").
    pub started: bool,
    /// Current stream audio-section payload bytes forwarded (reset on start;
    /// excludes headers, side sections,
    /// control and auth), so header-only or end-marker traffic never counts.
    pub audio_bytes: u64,
}
pub struct Session {
    pub id: u64,
    pub view: SessionView,
    pub upstream: TcpStream,
    pub downstream: Option<TcpStream>,
}
/// Cached HQPlayer control view. GET /api/state never performs network requests.
#[derive(Clone, Serialize)]
pub struct ControlView {
    pub address: String,
    pub enabled: bool,
    pub phase: String,
    pub transport_state: Option<String>,
    pub track: Option<String>,
    pub position: Option<String>,
    /// null: no restore attempted (nothing playing, or position 0); true: Seek
    /// accepted and Status confirmed the same track and position; false:
    /// attempted but unconfirmed/refused/mismatched, see last_error.
    pub position_restored: Option<bool>,
    pub last_error: Option<String>,
}
pub struct ControlState {
    pub view: ControlView,
    /// Control sockets of the in-flight operation; Stop or a newer selection
    /// shuts them down so no continuation can wait on the network.
    sockets: Vec<TcpStream>,
}
pub struct Inner {
    pub config: Config,
    /// Bumped by every disconnect/stop/select/edit/remove and by every new
    /// control operation; doubles as the operation token that continuations check.
    pub generation: u64,
    pub session: Option<Session>,
    pub last_error: Option<String>,
    pub discovered_devices: Vec<Device>,
    pub dac_catalog: Vec<DacObservation>,
    pub control: Option<ControlState>,
    /// Closed after a failed automatic resume so the selected route stays
    /// visible but cannot start playing unexpectedly until the next selection.
    pub routing_enabled: bool,
    next_session: u64,
    /// Protocol workers reserved but not yet finished, including superseded ones.
    workers: usize,
}
const MAX_WORKERS: usize = 16;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
pub struct Router {
    pub name: String,
    pub naa_addr: SocketAddr,
    pub control_addr: SocketAddr,
    pub hqp_control: Option<SocketAddr>,
    pub inner: Mutex<Inner>,
    config_path: Option<PathBuf>,
}
impl Router {
    pub fn new(
        name: String,
        naa_addr: SocketAddr,
        control_addr: SocketAddr,
        config_path: Option<PathBuf>,
        hqp_control: Option<SocketAddr>,
    ) -> io::Result<Self> {
        let config: Config = match &config_path {
            Some(p) if p.exists() => {
                if fs::metadata(p)?.len() > 1024 * 1024 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "config exceeds 1 MiB",
                    ));
                }
                serde_json::from_slice(&fs::read(p)?).map_err(io::Error::other)?
            }
            _ => Config::default(),
        };
        if config.routes.len() > 128 {
            return Err(io::Error::other("at most 128 routes"));
        }
        let mut ids = std::collections::HashSet::new();
        for r in &config.routes {
            validate(&NewRoute {
                name: r.name.clone(),
                host: r.host.clone(),
                port: r.port,
                device_id: r.device_id.clone(),
            })
            .map_err(io::Error::other)?;
            if r.id.is_empty() || !ids.insert(r.id.clone()) {
                return Err(io::Error::other("duplicate or empty route id"));
            }
        }
        if config
            .selected_route_id
            .as_ref()
            .is_some_and(|id| !ids.contains(id))
        {
            return Err(io::Error::other("selected route does not exist"));
        }
        let control = hqp_control.map(|addr| ControlState {
            view: ControlView {
                address: addr.to_string(),
                enabled: true,
                phase: "idle".into(),
                transport_state: None,
                track: None,
                position: None,
                position_restored: None,
                last_error: None,
            },
            sockets: vec![],
        });
        Ok(Self {
            name,
            naa_addr,
            control_addr,
            hqp_control,
            config_path,
            inner: Mutex::new(Inner {
                config,
                generation: 0,
                session: None,
                last_error: None,
                discovered_devices: vec![],
                dac_catalog: vec![],
                control,
                routing_enabled: true,
                next_session: 0,
                workers: 0,
            }),
        })
    }
    fn save(&self, c: &Config) -> Result<(), String> {
        let Some(path) = &self.config_path else {
            return Ok(());
        };
        let dir = config_dir(path);
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        // Unique per process and per attempt: another live router sharing the
        // directory is never touched, and create_new cannot collide with our own
        // earlier attempt. A leftover from a crash is harmless.
        let tmp = dir.join(format!(
            ".naa-router-{}-{}.tmp",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| -> io::Result<()> {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(c).map_err(io::Error::other)?)?;
            f.write_all(b"\n")?;
            f.sync_all()?;
            fs::rename(&tmp, path)?;
            Ok(())
        })();
        if result.is_err() {
            // Only the file this attempt created.
            let _ = fs::remove_file(&tmp);
        }
        result.map_err(|e| format!("cannot persist route configuration: {e}"))
    }
    pub fn state(&self) -> serde_json::Value {
        let s = self.inner.lock().unwrap();
        self.view(&s)
    }
    fn view(&self, s: &Inner) -> serde_json::Value {
        serde_json::json!({"name":self.name,"virtual_device_id":VIRTUAL_ID,"selected_route_id":s.config.selected_route_id,"route":s.config.routes.iter().find(|r| Some(&r.id)==s.config.selected_route_id.as_ref()),"session":s.session.as_ref().map(|s| &s.view),"last_error":s.last_error,"generation":s.generation,"naa_addr":self.naa_addr.to_string(),"control_addr":self.control_addr.to_string(),"discovered_devices":s.discovered_devices,"dac_catalog":s.dac_catalog,"hqp_control":s.control.as_ref().map(|c| &c.view),"routing_enabled":s.routing_enabled})
    }
    pub fn add(&self, r: NewRoute) -> Result<Route, String> {
        validate(&r)?;
        let mut s = self.inner.lock().unwrap();
        if s.config.routes.len() >= 128 {
            return Err("at most 128 routes".into());
        }
        let mut n = s.config.routes.len() + 1;
        while s.config.routes.iter().any(|r| r.id == format!("route-{n}")) {
            n += 1;
        }
        let route = Route {
            id: format!("route-{n}"),
            name: r.name.trim().into(),
            host: r.host,
            port: r.port,
            device_id: r.device_id,
        };
        let mut config = s.config.clone();
        config.routes.push(route.clone());
        self.save(&config)?;
        s.config = config;
        Ok(route)
    }
    pub fn edit(&self, id: &str, r: NewRoute) -> Result<Route, String> {
        validate(&r)?;
        let mut s = self.inner.lock().unwrap();
        let mut config = s.config.clone();
        let route = config
            .routes
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or("unknown route_id")?;
        *route = Route {
            id: id.into(),
            name: r.name.trim().into(),
            host: r.host,
            port: r.port,
            device_id: r.device_id,
        };
        let result = route.clone();
        self.save(&config)?;
        if config.selected_route_id.as_deref() == Some(id) {
            Self::disconnect(&mut s);
            s.discovered_devices.clear();
            s.last_error = None;
        }
        s.config = config;
        Ok(result)
    }
    pub fn remove(&self, id: &str) -> Result<serde_json::Value, String> {
        let mut s = self.inner.lock().unwrap();
        let mut config = s.config.clone();
        if !config.routes.iter().any(|r| r.id == id) {
            return Err("unknown route_id".into());
        }
        config.routes.retain(|r| r.id != id);
        let selected = config.selected_route_id.as_deref() == Some(id);
        if selected {
            config.selected_route_id = None;
        }
        self.save(&config)?;
        if selected {
            Self::disconnect(&mut s);
            s.discovered_devices.clear();
            s.last_error = None;
        }
        s.config = config;
        Ok(self.view(&s))
    }
    pub fn select(self: &Arc<Self>, id: &str) -> Result<serde_json::Value, String> {
        // With no NAA session HQPlayer is not connected through this router, so
        // nothing can be playing here: change the route without any native
        // control traffic. The controller is consulted only while a session exists.
        let active = self.inner.lock().unwrap().session.is_some();
        match self.hqp_control {
            Some(addr) if active => self.select_controlled(addr, id),
            _ => {
                let mut s = self.inner.lock().unwrap();
                self.commit(&mut s, id)?;
                if let Some(c) = s.control.as_mut() {
                    c.view.phase = "idle".into();
                    c.view.last_error = None;
                }
                Ok(self.view(&s))
            }
        }
    }
    /// Commit a selection: persist, tear down the current NAA pair and reopen the
    /// routing gate. Bumps `generation`. Caller holds the lock.
    fn commit(&self, s: &mut Inner, id: &str) -> Result<(), String> {
        if !s.config.routes.iter().any(|r| r.id == id) {
            return Err("unknown route_id".into());
        }
        let mut config = s.config.clone();
        config.selected_route_id = Some(id.into());
        self.save(&config)?;
        Self::disconnect(s);
        s.config = config;
        s.discovered_devices.clear();
        s.last_error = None;
        s.routing_enabled = true;
        Ok(())
    }
    /// Stop is an immediate audio action: the session is torn down and the
    /// selection cleared in memory first. A persistence failure is reported in
    /// `last_error` (the on-disk file still names the old route, so a restart
    /// would reconnect) instead of leaving audio flowing. When HQPlayer control
    /// is configured a bounded native Stop follows the local halt.
    pub fn stop(&self) -> Result<serde_json::Value, String> {
        let (token, had_session) = {
            let mut s = self.inner.lock().unwrap();
            let had_session = s.session.is_some();
            let mut config = s.config.clone();
            config.selected_route_id = None;
            Self::disconnect(&mut s);
            s.config = config.clone();
            s.discovered_devices.clear();
            s.routing_enabled = true;
            s.last_error = self.save(&config).err().map(|e| {
                format!("Stopped, but the selection could not be saved; a restart would reconnect to the previous route. {e}")
            });
            if let Some(c) = s.control.as_mut() {
                c.view.phase = if had_session { "stopping" } else { "idle" }.into();
                c.view.last_error = None;
            }
            (s.generation, had_session)
        };
        if let (Some(addr), true) = (self.hqp_control, had_session) {
            // Local audio already halted; this only tidies HQPlayer's transport.
            let deadline = Instant::now() + CONTROL_STEP;
            let outcome = self.call(addr, token, deadline, "Stop", &[]).and_then(|_| {
                let reply = self.call(addr, token, deadline, "State", &[])?;
                if reply.attr("state") == Some("0") {
                    Ok(())
                } else {
                    Err(format!(
                        "HQPlayer did not confirm stopped state after Stop (state={})",
                        reply.attr("state").unwrap_or("missing")
                    ))
                }
            });
            let mut s = self.inner.lock().unwrap();
            if s.generation == token {
                if let Some(c) = s.control.as_mut() {
                    c.sockets.clear();
                    match outcome {
                        Ok(_) => {
                            c.view.phase = "idle".into();
                            c.view.transport_state = Some("0".into());
                        }
                        Err(e) => {
                            c.view.phase = "error".into();
                            c.view.last_error = Some(format!("Audio routing stopped locally, but HQPlayer transport Stop failed: {e}"));
                        }
                    }
                }
            }
            return Ok(self.view(&s));
        }
        let s = self.inner.lock().unwrap();
        Ok(self.view(&s))
    }
    fn disconnect(s: &mut Inner) {
        s.generation += 1;
        if let Some(session) = s.session.take() {
            let _ = session.upstream.shutdown(Shutdown::Both);
            if let Some(d) = session.downstream {
                let _ = d.shutdown(Shutdown::Both);
            }
        }
        if let Some(c) = s.control.as_mut() {
            for socket in c.sockets.drain(..) {
                let _ = socket.shutdown(Shutdown::Both);
            }
        }
    }
    /// Register an in-flight control socket under `token`; false once superseded.
    fn register_control(&self, token: u64, stream: &TcpStream) -> bool {
        let mut s = self.inner.lock().unwrap();
        if s.generation != token {
            return false;
        }
        match (s.control.as_mut(), stream.try_clone()) {
            (Some(c), Ok(clone)) => {
                c.sockets.push(clone);
                true
            }
            _ => false,
        }
    }
    fn call(
        &self,
        addr: SocketAddr,
        token: u64,
        deadline: Instant,
        command: &str,
        attrs: &[(&str, &str)],
    ) -> Result<control::Reply, String> {
        control::request(addr, command, attrs, deadline, &mut |s| {
            self.register_control(token, s)
        })
        .map_err(|e| format!("HQPlayer <{command}>: {e}"))
    }
    /// Apply a control-view change only while `token` is still current.
    fn control_view(&self, token: u64, f: impl FnOnce(&mut ControlView)) -> bool {
        let mut s = self.inner.lock().unwrap();
        if s.generation != token {
            return false;
        }
        if let Some(c) = s.control.as_mut() {
            f(&mut c.view);
        }
        true
    }
    /// One-click selection through HQPlayer's native transport. Proven live
    /// sequence: if HQPlayer is playing, Stop it and verify state 0, commit the
    /// route (closing the NAA pair), wait for a fresh successful initialize on
    /// the new route, then a single Play and verify accepted start, forwarded
    /// audio and native state 2. Stopped or paused sources only change the
    /// route; nothing is ever auto-started. Router.inner is never held across a
    /// network wait; every continuation re-checks its operation token.
    fn select_controlled(
        self: &Arc<Self>,
        addr: SocketAddr,
        id: &str,
    ) -> Result<serde_json::Value, String> {
        let deadline = Instant::now() + SELECT_DEADLINE;
        let token = {
            let mut s = self.inner.lock().unwrap();
            if !s.config.routes.iter().any(|r| r.id == id) {
                return Err("unknown route_id".into());
            }
            // Supersede any pending operation and cancel its control sockets
            // without disturbing the currently routed audio yet.
            s.generation += 1;
            if let Some(c) = s.control.as_mut() {
                for socket in c.sockets.drain(..) {
                    let _ = socket.shutdown(Shutdown::Both);
                }
                c.view.phase = "checking".into();
                c.view.last_error = None;
            }
            s.generation
        };
        let superseded = || "selection superseded by Stop or a newer request".to_string();
        let step = |d: Instant| d.min(deadline);
        // 1. HQPlayer must be reachable and its state known before anything changes.
        let state = self
            .call(
                addr,
                token,
                step(Instant::now() + CONTROL_STEP),
                "State",
                &[],
            )
            .and_then(|r| match r.attr("state") {
                Some(value @ ("0" | "1" | "2")) => Ok(value.to_string()),
                Some(value) => Err(format!("HQPlayer <State> returned unknown state {value:?}")),
                None => Err("HQPlayer <State> reply has no state attribute".to_string()),
            });
        let state = match state {
            Ok(v) => v,
            Err(e) => {
                let msg = format!("Route unchanged. {e}");
                self.control_view(token, |c| {
                    c.phase = "error".into();
                    c.last_error = Some(msg.clone());
                });
                return Err(msg);
            }
        };
        // Track/position are informational; their absence never blocks routing.
        let status = self
            .call(
                addr,
                token,
                step(Instant::now() + CONTROL_STEP),
                "Status",
                &[("subscribe", "0")],
            )
            .ok();
        if !self.control_view(token, |c| {
            c.transport_state = Some(state.clone());
            c.track = status
                .as_ref()
                .and_then(|r| r.attr("track"))
                .map(str::to_string);
            c.position = status
                .as_ref()
                .and_then(|r| r.attr("position"))
                .map(str::to_string);
            c.position_restored = None;
        }) {
            return Err(superseded());
        }
        let playing = state == "2";
        // 2. Stop playing or paused transport before detaching its NAA.
        // Embedded may stall on native commands if a paused session is
        // disconnected directly. Only an originally playing source resumes.
        if state != "0" {
            if !self.control_view(token, |c| c.phase = "stopping".into()) {
                return Err(superseded());
            }
            let stopped = self
                .call(
                    addr,
                    token,
                    step(Instant::now() + CONTROL_STEP),
                    "Stop",
                    &[],
                )
                .and_then(|_| {
                    let until = step(Instant::now() + CONTROL_STEP);
                    loop {
                        let reply = self.call(addr, token, until, "State", &[])?;
                        if reply.attr("state") == Some("0") {
                            return Ok(());
                        }
                        if Instant::now() >= until {
                            return Err(format!(
                                "HQPlayer reported state {} after Stop",
                                reply.attr("state").unwrap_or("?")
                            ));
                        }
                        thread::sleep(POLL);
                    }
                });
            if let Err(e) = stopped {
                let msg = format!("Route unchanged. {e}");
                self.control_view(token, |c| {
                    c.phase = "error".into();
                    c.last_error = Some(msg.clone());
                });
                return Err(msg);
            }
        }
        // 3. Commit under the lock, re-checking the token; commit bumps generation.
        let token = {
            let mut s = self.inner.lock().unwrap();
            if s.generation != token {
                return Err(superseded());
            }
            if let Err(e) = self.commit(&mut s, id) {
                if let Some(c) = s.control.as_mut() {
                    c.view.phase = "error".into();
                    c.view.last_error = Some(e.clone());
                }
                return Err(e);
            }
            if let Some(c) = s.control.as_mut() {
                c.view.transport_state = Some("0".into());
                c.view.phase = if playing { "waiting_for_naa" } else { "idle" }.into();
            }
            s.generation
        };
        if playing {
            // The route is committed and HQPlayer is stopped; the resume
            // continuation runs in the background within the same budget so
            // this request, GET state and Stop all stay responsive. Progress is
            // visible through hqp_control.phase / last_error.
            let me = self.clone();
            let route = id.to_string();
            thread::spawn(move || me.resume(addr, token, &route, deadline, status));
        }
        let s = self.inner.lock().unwrap();
        Ok(self.view(&s))
    }
    /// Steps 4–6 of the one-click sequence, after the route commit.
    fn resume(
        &self,
        addr: SocketAddr,
        token: u64,
        id: &str,
        deadline: Instant,
        status: Option<control::Reply>,
    ) -> Result<serde_json::Value, String> {
        let superseded = || "selection superseded by Stop or a newer request".to_string();
        let step = |d: Instant| d.min(deadline);
        // 4. Wait for a fresh successful initialize on the newly selected route.
        // The commit tore down the previous pair, so any session carrying this
        // route id was created after the commit.
        let initialized = self.await_session(token, id, deadline, |v| v.initialized);
        if let Err(e) = initialized {
            // HQPlayer is stopped; nothing plays. Route stays as selected.
            return self.resume_failed(
                addr,
                token,
                false,
                format!("Route selected but playback was not resumed: {e}"),
            );
        }
        // 5. Single Play, then verify accepted start, forwarded audio and state 2.
        if !self.control_view(token, |c| c.phase = "resuming".into()) {
            return Err(superseded());
        }
        if let Err(e) = self.call(
            addr,
            token,
            step(Instant::now() + CONTROL_STEP),
            "Play",
            &[("last", "0")],
        ) {
            return self.resume_failed(
                addr,
                token,
                true,
                format!("Route selected but Play failed: {e}"),
            );
        }
        // Accepted start plus positive audio-section bytes; auth, control,
        // headers and end markers alone never qualify.
        if let Err(e) = self.await_session(token, id, deadline, |v| v.started && v.audio_bytes > 0)
        {
            return self.resume_failed(addr, token, true, format!("Route selected and Play accepted, but audio did not start on the new route: {e}"));
        }
        let verified = loop {
            match self.call(
                addr,
                token,
                step(Instant::now() + CONTROL_STEP),
                "State",
                &[],
            ) {
                Ok(r) if r.attr("state") == Some("2") => break Ok(()),
                Ok(r) if Instant::now() < deadline => {
                    let _ = r;
                    thread::sleep(Duration::from_millis(200));
                }
                Ok(r) => {
                    break Err(format!(
                        "HQPlayer reported state {} after Play",
                        r.attr("state").unwrap_or("?")
                    ))
                }
                Err(e) => break Err(e),
            }
        };
        if let Err(e) = verified {
            return self.resume_failed(
                addr,
                token,
                true,
                format!("Audio reached the new route but HQPlayer did not confirm playing: {e}"),
            );
        }
        // 6. Restore the captured source position once the new stream is
        // actually playing. Stop reset it to zero; Seek before Play is ignored.
        // Refusal (non-seekable source), a track change or a timeout leave the
        // working route playing and are reported as a visible partial outcome.
        let captured_track = status
            .as_ref()
            .and_then(|r| r.attr("track"))
            .map(str::to_string);
        let seconds = status
            .as_ref()
            .and_then(|r| r.attr("position"))
            .and_then(|p| p.parse::<f64>().ok())
            .filter(|p| p.is_finite() && *p >= 1.0)
            .map(|p| p.floor() as u64);
        let (restored, warning) = match (captured_track, seconds) {
            (Some(track), Some(seconds)) => {
                let outcome = self
                    .call(
                        addr,
                        token,
                        step(Instant::now() + CONTROL_STEP),
                        "Status",
                        &[("subscribe", "0")],
                    )
                    .and_then(|now| {
                        if now.attr("track") != Some(track.as_str()) {
                            return Err(format!(
                                "the track changed (was {track}, now {}); position not restored",
                                now.attr("track").unwrap_or("?")
                            ));
                        }
                        let seek_started = Instant::now();
                        let until = step(seek_started + CONTROL_STEP);
                        self.call(
                            addr,
                            token,
                            until,
                            "Seek",
                            &[("position", &seconds.to_string())],
                        )?;
                        loop {
                            let after =
                                self.call(addr, token, until, "Status", &[("subscribe", "0")])?;
                            if after.attr("track") != Some(track.as_str()) {
                                return Err(
                                    "the track changed after Seek; position not confirmed".into()
                                );
                            }
                            let position =
                                after.attr("position").and_then(|p| p.parse::<f64>().ok());
                            // The integer-second target is floored. Allow elapsed
                            // playback plus modest reporting latency, never an
                            // unchanged zero or an unrelated later position.
                            if position.is_some_and(|p| {
                                p.is_finite()
                                    && p >= seconds as f64
                                    && p <= seconds as f64
                                        + seek_started.elapsed().as_secs_f64()
                                        + 1.0
                            }) {
                                return Ok(());
                            }
                            if Instant::now() >= until {
                                return Err(format!(
                                    "Status did not confirm position {seconds}s after Seek"
                                ));
                            }
                            thread::sleep(POLL);
                        }
                    });
                match outcome {
                    Ok(()) => (Some(true), None),
                    Err(e) => (
                        Some(false),
                        Some(format!("Route switched and playing, but the source position ({seconds}s) could not be confirmed; check playback position. {e}")),
                    ),
                }
            }
            _ => (None, None),
        };
        let mut s = self.inner.lock().unwrap();
        if s.generation != token {
            return Err(superseded());
        }
        if let Some(c) = s.control.as_mut() {
            c.sockets.clear();
            c.view.phase = "idle".into();
            c.view.transport_state = Some("2".into());
            c.view.position_restored = restored;
            c.view.last_error = warning;
        }
        Ok(self.view(&s))
    }
    /// Poll the cached session view until `accept` matches, the session fails,
    /// the token is superseded or the deadline passes. Never holds the lock
    /// across a sleep.
    fn await_session(
        &self,
        token: u64,
        route_id: &str,
        deadline: Instant,
        accept: impl Fn(&SessionView) -> bool,
    ) -> Result<(), String> {
        loop {
            {
                let s = self.inner.lock().unwrap();
                if s.generation != token {
                    return Err("selection superseded by Stop or a newer request".into());
                }
                if let Some(v) = s.session.as_ref().filter(|v| v.view.route_id == route_id) {
                    if accept(&v.view) {
                        return Ok(());
                    }
                } else if let Some(e) = &s.last_error {
                    return Err(e.clone());
                }
            }
            if Instant::now() >= deadline {
                return Err("timed out within the 10 second selection budget".into());
            }
            thread::sleep(POLL);
        }
    }
    /// After a failed automatic resume the selected route stays visible, but if
    /// Play was already issued the local pair is torn down and the routing gate
    /// closed so nothing can keep playing unexpectedly until the next selection.
    fn resume_failed(
        &self,
        addr: SocketAddr,
        token: u64,
        play_sent: bool,
        message: String,
    ) -> Result<serde_json::Value, String> {
        // Play was issued but never verified: ask HQPlayer to stop so its
        // transport does not sit in state 2 aimed at a gated router. Bounded,
        // token-checked, and only informative if it fails.
        let native_stop = if play_sent {
            Some(self.call(addr, token, Instant::now() + CONTROL_STEP, "Stop", &[]))
        } else {
            None
        };
        let mut s = self.inner.lock().unwrap();
        if s.generation != token {
            return Err("selection superseded by Stop or a newer request".into());
        }
        let mut message = message;
        if play_sent {
            Self::disconnect(&mut s);
            s.routing_enabled = false;
        }
        if let Some(c) = s.control.as_mut() {
            c.sockets.clear();
            c.view.phase = "error".into();
            match native_stop {
                Some(Ok(_)) => c.view.transport_state = Some("0".into()),
                Some(Err(e)) => {
                    c.view.transport_state = None;
                    message.push_str(&format!(
                        " HQPlayer transport state is unknown; Stop failed: {e}"
                    ));
                }
                None => {}
            }
            c.view.last_error = Some(message.clone());
        }
        s.last_error = Some(message);
        Ok(self.view(&s))
    }
    pub fn reserve(&self, client: &TcpStream) -> Result<(u64, Route), String> {
        let mut s = self.inner.lock().unwrap();
        if s.session.is_some() {
            return Err("router busy".into());
        }
        // Superseded workers may still be inside DNS resolution or connect_timeout
        // for a route that is no longer selected. Bound how many can pile up
        // during rapid route changes before HQPlayer's next attempt is refused.
        if s.workers >= MAX_WORKERS {
            return Err("too many session workers still shutting down".into());
        }
        let Some(route) = s
            .config
            .routes
            .iter()
            .find(|r| Some(&r.id) == s.config.selected_route_id.as_ref())
            .cloned()
        else {
            // HQPlayer retries automatically after Stop. The routine hint must
            // not erase a more important standing error such as an unsaved Stop.
            if s.last_error.is_none() {
                s.last_error = Some("Select a route before connecting HQPlayer".into());
            }
            return Err("no route selected".into());
        };
        if !s.routing_enabled {
            return Err("routing disabled after a failed resume; select the route again".into());
        }
        let upstream = client.try_clone().map_err(|e| e.to_string())?;
        s.next_session += 1;
        s.workers += 1;
        let id = s.next_session;
        s.session = Some(Session {
            id,
            upstream,
            downstream: None,
            view: SessionView {
                state: "connecting".into(),
                route_id: route.id.clone(),
                peer: client.peer_addr().map_err(|e| e.to_string())?.to_string(),
                connected_at: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                bytes_to_naa: 0,
                bytes_from_naa: 0,
                initialized: false,
                started: false,
                audio_bytes: 0,
            },
        });
        s.last_error = None;
        Ok((id, route))
    }
    /// Record a stream lifecycle transition or explicitly successful acknowledgement.
    pub fn milestone(&self, id: u64, name: &str) {
        let mut s = self.inner.lock().unwrap();
        if let Some(v) = s.session.as_mut().filter(|v| v.id == id) {
            match name {
                "initializing" => {
                    v.view.initialized = false;
                    v.view.started = false;
                    v.view.audio_bytes = 0;
                }
                "starting" => {
                    v.view.started = false;
                    v.view.audio_bytes = 0;
                }
                "stopped" => v.view.started = false,
                "initialized" => v.view.initialized = true,
                "started" => v.view.started = true,
                _ => {}
            }
            v.view.state = name.into();
        }
    }
    /// Count forwarded audio-section payload bytes only.
    pub fn audio(&self, id: u64, n: usize) {
        let mut s = self.inner.lock().unwrap();
        if let Some(v) = s.session.as_mut().filter(|v| v.id == id) {
            v.view.audio_bytes += n as u64;
        }
    }
    pub fn attach(&self, id: u64, downstream: &TcpStream) -> io::Result<()> {
        let mut s = self.inner.lock().unwrap();
        let session = s
            .session
            .as_mut()
            .filter(|v| v.id == id)
            .ok_or_else(|| io::Error::other("route changed during connect"))?;
        session.downstream = Some(downstream.try_clone()?);
        session.view.state = "authenticating".into();
        Ok(())
    }
    pub fn update(&self, id: u64, to: bool, n: usize, status: Option<&str>) {
        let mut s = self.inner.lock().unwrap();
        if let Some(v) = s.session.as_mut().filter(|v| v.id == id) {
            if to {
                v.view.bytes_to_naa += n as u64
            } else {
                v.view.bytes_from_naa += n as u64
            };
            if let Some(status) = status {
                v.view.state = status.into();
            }
        }
    }
    pub fn devices(&self, id: u64, devices: Vec<Device>) {
        let mut s = self.inner.lock().unwrap();
        if s.session.as_ref().is_some_and(|v| v.id == id) {
            let route_id = &s.session.as_ref().unwrap().view.route_id;
            if let Some(route) = s.config.routes.iter().find(|r| &r.id == route_id).cloned() {
                s.dac_catalog
                    .retain(|entry| entry.host != route.host || entry.port != route.port);
                if s.dac_catalog.len() >= 256 {
                    s.dac_catalog.remove(0);
                }
                s.dac_catalog.push(DacObservation {
                    host: route.host,
                    port: route.port,
                    observed_at: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    devices: devices.clone(),
                });
            }
            s.discovered_devices = devices;
        }
    }
    pub fn finish(&self, id: u64, error: Option<String>) {
        let mut s = self.inner.lock().unwrap();
        // Every reserved worker calls finish exactly once, matched or superseded.
        s.workers = s.workers.saturating_sub(1);
        if s.session.as_ref().is_some_and(|v| v.id == id) {
            if let Some(v) = s.session.take() {
                let _ = v.upstream.shutdown(Shutdown::Both);
                if let Some(d) = v.downstream {
                    let _ = d.shutdown(Shutdown::Both);
                }
            }
            s.last_error = error;
        }
    }
}
fn config_dir(path: &std::path::Path) -> &std::path::Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."))
}
fn validate(r: &NewRoute) -> Result<(), String> {
    if r.name.trim().is_empty() || r.name.len() > 256 || r.name.chars().any(char::is_control) {
        return Err("name must contain 1–256 printable bytes".into());
    }
    if r.host.is_empty()
        || r.host.len() > 253
        || r.host
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "/\\@?#".contains(c))
        || r.port == 0
    {
        return Err(
            "host must be an explicit hostname or IP address and port must be nonzero".into(),
        );
    }
    if r.device_id.len() > 1024 || r.device_id.chars().any(char::is_control) {
        return Err("invalid device_id".into());
    }
    Ok(())
}
