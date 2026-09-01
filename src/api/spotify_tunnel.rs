//! Temporary HTTPS tunnel for the Spotify OAuth callback (#538).
//!
//! A beginner running UHC on a NAS or another LAN machine has no HTTPS
//! address to hand Spotify -- Spotify only accepts plain HTTP for an exact
//! loopback (`127.0.0.1`/`::1`) redirect URI. This module spawns a
//! short-lived `ssh -R` reverse tunnel to a public relay (pinggy.io, falling
//! back to localhost.run) so Settings can show a paste-ready HTTPS callback
//! URL without asking the user to install or run anything themselves.
//!
//! The tunnel is scoped to one OAuth attempt: it starts when the user clicks
//! "Get an HTTPS address", and is torn down when authorization completes
//! (success or failure), when the user stops it, on a 55-minute safety
//! timeout, or when the server shuts down. While it is active this UHC
//! callback-only listener is briefly reachable from the public internet at
//! the tunnel's URL. It accepts only the exact callback and a bounded
//! liveness probe; the callback itself accepts only the single-use, in-flight
//! OAuth `state` token (see `oauth_callback_json` in `provider_auth.rs`).
//!
//! The state machine (`SpotifyTunnelManager`) is decoupled from the actual
//! `ssh` process through the `TunnelLauncher`/`TunnelProcess` traits so unit
//! tests can drive it with a scripted fake process -- no outbound network
//! access, and no dependency on a live pinggy.io/localhost.run endpoint, is
//! needed to exercise the lifecycle.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Hard cap on tunnel lifetime, regardless of activity. A beginner should
/// never end up with a forgotten tunnel silently keeping its callback
/// listener internet-reachable.
///
/// The cap must comfortably outlast a first-time Spotify enrollment: the
/// user copies the URL, creates an app in Spotify's developer dashboard,
/// registers the redirect URI, copies the client ID/secret back, saves, and
/// only then clicks Connect. A 15-minute cap was observed to expire mid-
/// dashboard (#592), so the redirect landed on a dead tunnel as
/// ERR_CONNECTION_RESET. pinggy's anonymous tunnels are dropped server-side
/// at 60 minutes; capping just under that keeps the expiry message ours.
pub const TUNNEL_MAX_LIFETIME: Duration = Duration::from_secs(55 * 60);

/// How long a tunnel stays up after the OAuth callback has concluded. The
/// callback response itself (and the settings page it redirects the browser
/// to) travels back through the tunnel, so tearing the tunnel down inline in
/// the callback handler reset the very TCP connection carrying the "success"
/// redirect -- the ERR_CONNECTION_RESET defect in #592. A short grace period
/// lets the redirect and the follow-up page load finish first.
pub const TUNNEL_CALLBACK_GRACE: Duration = Duration::from_secs(60);

/// How long to wait for one provider to print a public URL before giving up
/// on it and falling back to the next.
const PROVIDER_STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

/// Providers are tried in this order. Both are free-tier plain `ssh -R`
/// relays that need no account and no bundled binary beyond `ssh` itself --
/// see docs/streaming-adapters.md for the rationale.
const PROVIDER_ORDER: [TunnelProviderKind; 2] =
    [TunnelProviderKind::Pinggy, TunnelProviderKind::LocalhostRun];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TunnelProviderKind {
    Pinggy,
    LocalhostRun,
}

impl TunnelProviderKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pinggy => "pinggy.io",
            Self::LocalhostRun => "localhost.run",
        }
    }

    fn command(self, port: u16) -> (&'static str, Vec<String>) {
        match self {
            // Pinggy's free tier is reached over plain SSH on port 443 (so it
            // also works through firewalls that only allow outbound HTTPS).
            // `-R0:` asks the relay to pick an ephemeral remote port for us.
            Self::Pinggy => (
                "ssh",
                vec![
                    "-p".to_string(),
                    "443".to_string(),
                    "-o".to_string(),
                    "StrictHostKeyChecking=no".to_string(),
                    "-o".to_string(),
                    "UserKnownHostsFile=/dev/null".to_string(),
                    "-o".to_string(),
                    "ServerAliveInterval=30".to_string(),
                    "-o".to_string(),
                    "ExitOnForwardFailure=yes".to_string(),
                    // `127.0.0.1` rather than `localhost`: inside containers
                    // (HA add-on, NAS container station) `localhost` can
                    // resolve to `::1` while UHC listens on IPv4 only, in
                    // which case every public request dies as a connection
                    // reset even though the tunnel URL was printed fine.
                    format!("-R0:127.0.0.1:{port}"),
                    "a.pinggy.io".to_string(),
                ],
            ),
            // localhost.run assigns a subdomain of lhr.life the same way,
            // over the standard SSH port instead.
            Self::LocalhostRun => (
                "ssh",
                vec![
                    "-o".to_string(),
                    "StrictHostKeyChecking=no".to_string(),
                    "-o".to_string(),
                    "UserKnownHostsFile=/dev/null".to_string(),
                    "-o".to_string(),
                    "ServerAliveInterval=30".to_string(),
                    "-o".to_string(),
                    "ExitOnForwardFailure=yes".to_string(),
                    "-R".to_string(),
                    format!("80:127.0.0.1:{port}"),
                    "localhost.run".to_string(),
                ],
            ),
        }
    }

    /// Pull the public HTTPS URL out of one line of provider stdout. Each
    /// provider prints its own boilerplate around the URL, so this is
    /// deliberately provider-specific rather than a single generic
    /// "first https:// on the line" match, which would also catch the
    /// providers' own docs/help URLs printed in the same banner.
    fn extract_url(self, line: &str) -> Option<String> {
        match self {
            Self::Pinggy => pinggy_url_pattern()
                .find(line)
                .map(|m| m.as_str().to_string()),
            Self::LocalhostRun => localhost_run_url_pattern()
                .find(line)
                .map(|m| m.as_str().to_string()),
        }
    }
}

/// Pinggy's free tier does not actually use `pinggy.link` for the anonymous
/// (no-account) tunnels this module opens -- confirmed against a live
/// `ssh -p 443 -R0:localhost:<port> a.pinggy.io` run, whose stdout was:
///
/// ```text
/// Allocated port 9 for remote forward to localhost:8091
/// You are not authenticated.
/// Your tunnel will expire in 60 minutes. Upgrade to Pinggy Pro to get unrestricted tunnels. https://dashboard.pinggy.io
/// https://lgidn-2603-6010-e300-381a-352c-943-c7fa-76a9.run.pinggy-free.link
/// https://rjvqd-2603-6010-e300-381a-352c-943-c7fa-76a9.free.pinggy.net
/// ```
///
/// i.e. `pinggy-free.link` and `free.pinggy.net`, plus a `dashboard.pinggy.io`
/// upsell link on the expiry-notice line that must NOT be picked up. `.link`
/// is kept for a possible authenticated/Pro tunnel, which is documented to
/// use plain `pinggy.link`, even though this module never authenticates.
#[allow(clippy::unwrap_used)] // Regex pattern is a compile-time constant
fn pinggy_url_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(
            r"https://[a-zA-Z0-9.-]+\.(?:pinggy\.link|pinggy-free\.link|free\.pinggy\.net)\S*",
        )
        .unwrap()
    })
}

/// NOT verified against a live `localhost.run` tunnel URL: a live `ssh -R
/// 80:localhost:<port> localhost.run` run reached the server and printed its
/// welcome banner (which is why this pattern deliberately stays scoped to
/// `lhr.life` rather than "first https:// line" -- the banner alone contains
/// `https://admin.localhost.run/`, `https://localhost.run/docs/...` several
/// times over, none of which are the tunnel URL), but did not yield the
/// actual forwarded `https://…lhr.life` line within a 15s connection before
/// timing out. If this pattern turns out to be wrong the same way the pinggy
/// one was, the fix is the same: capture a real run's stdout and widen this
/// regex to match it, the way `pinggy_url_pattern` now covers
/// `pinggy-free.link` and `free.pinggy.net` alongside `pinggy.link`.
#[allow(clippy::unwrap_used)] // Regex pattern is a compile-time constant
fn localhost_run_url_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"https://[a-zA-Z0-9.-]+\.lhr\.life\S*").unwrap())
}

/// One event read from a running tunnel process.
pub(crate) enum TunnelProcessEvent {
    /// A line of stdout, checked for a provider URL.
    Line(String),
    /// The process exited (or its stdout/stderr pipes broke). `stderr_tail`
    /// carries the last bit of stderr captured, if any, for the error
    /// message shown to the user.
    Exited { stderr_tail: String },
}

/// A running (or launching) tunnel process, abstracted so tests can supply a
/// scripted fake instead of a real `ssh` child process.
#[async_trait::async_trait]
pub(crate) trait TunnelProcess: Send {
    async fn next_event(&mut self) -> TunnelProcessEvent;
    fn kill(&mut self);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TunnelLaunchError {
    /// `ssh` (or whatever the provider's binary is) is not installed.
    BinaryMissing,
    /// The binary exists but the process could not be spawned.
    Spawn(String),
}

/// Starts a tunnel process for a provider. Implemented for real `ssh` in
/// `RealTunnelLauncher`, and by a scripted fake in tests.
#[async_trait::async_trait]
pub(crate) trait TunnelLauncher: Send + Sync {
    async fn launch(
        &self,
        provider: TunnelProviderKind,
        port: u16,
    ) -> Result<Box<dyn TunnelProcess>, TunnelLaunchError>;
}

/// Spawns the real `ssh -R` process for a provider.
pub(crate) struct RealTunnelLauncher;

#[async_trait::async_trait]
impl TunnelLauncher for RealTunnelLauncher {
    async fn launch(
        &self,
        provider: TunnelProviderKind,
        port: u16,
    ) -> Result<Box<dyn TunnelProcess>, TunnelLaunchError> {
        let (program, args) = provider.command(port);
        let mut command = Command::new(program);
        command
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                TunnelLaunchError::BinaryMissing
            } else {
                TunnelLaunchError::Spawn(error.to_string())
            }
        })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| TunnelLaunchError::Spawn("ssh stdout was unavailable".to_string()))?;
        let stderr_tail = Arc::new(std::sync::Mutex::new(String::new()));
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Ok(mut tail) = tail.lock() {
                        if !tail.is_empty() {
                            tail.push(' ');
                        }
                        tail.push_str(line.trim());
                        // Keep this bounded; it only ever backs a short,
                        // human-readable error message.
                        if tail.len() > 500 {
                            let truncated = tail.chars().take(500).collect::<String>();
                            *tail = truncated;
                        }
                    }
                }
            });
        }
        Ok(Box::new(RealTunnelProcess {
            child,
            stdout: BufReader::new(stdout).lines(),
            stderr_tail,
        }))
    }
}

/// Checks that a freshly allocated public URL actually carries HTTP back to
/// this server. A tunnel can print a URL and still be dead end-to-end (wrong
/// forward target, relay in the wrong mode); without a probe the user only
/// discovers that at the worst possible moment -- when Spotify redirects
/// back (#592). Returns `Some(reachable)` or `None` when no probe is
/// available (tests).
#[async_trait::async_trait]
pub(crate) trait TunnelProbe: Send + Sync {
    async fn probe(&self, url: &str) -> Option<bool>;
}

/// Probes the callback listener's dedicated liveness path through the public
/// URL. Any successful response proves the round trip without making a
/// main-UHC route internet reachable.
pub(crate) struct RealTunnelProbe;

#[async_trait::async_trait]
impl TunnelProbe for RealTunnelProbe {
    async fn probe(&self, url: &str) -> Option<bool> {
        let target = format!(
            "{url}{}",
            crate::api::spotify_callback_listener::LIVENESS_PATH
        );
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .ok()?;
        match client.get(&target).send().await {
            Ok(response) => Some(response.status().is_success()),
            Err(_) => Some(false),
        }
    }
}

/// A probe that reports nothing, keeping `verified` at `None`. Used by unit
/// tests (via `with_launcher`) so exercising the state machine never
/// performs network I/O.
#[cfg(test)]
pub(crate) struct NullTunnelProbe;

#[cfg(test)]
#[async_trait::async_trait]
impl TunnelProbe for NullTunnelProbe {
    async fn probe(&self, _url: &str) -> Option<bool> {
        None
    }
}

struct RealTunnelProcess {
    child: Child,
    stdout: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    stderr_tail: Arc<std::sync::Mutex<String>>,
}

impl RealTunnelProcess {
    fn stderr_snapshot(&self) -> String {
        self.stderr_tail
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl TunnelProcess for RealTunnelProcess {
    async fn next_event(&mut self) -> TunnelProcessEvent {
        tokio::select! {
            line = self.stdout.next_line() => {
                match line {
                    Ok(Some(line)) => TunnelProcessEvent::Line(line),
                    Ok(None) => {
                        // stdout closed; the process is on its way out.
                        let _ = self.child.wait().await;
                        TunnelProcessEvent::Exited { stderr_tail: self.stderr_snapshot() }
                    }
                    Err(_) => {
                        let _ = self.child.start_kill();
                        TunnelProcessEvent::Exited { stderr_tail: self.stderr_snapshot() }
                    }
                }
            }
            _ = self.child.wait() => {
                TunnelProcessEvent::Exited { stderr_tail: self.stderr_snapshot() }
            }
        }
    }

    fn kill(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// Snapshot of the tunnel state machine, exposed to HTTP handlers.
#[derive(Clone, Debug, PartialEq)]
pub enum TunnelStatus {
    /// No tunnel running; nothing has been requested yet, or the last one
    /// was cleanly stopped.
    Idle,
    /// A provider is being tried.
    Starting { provider: &'static str },
    /// A public URL is live. `expires_at` is a Unix-epoch second timestamp.
    /// `verified` reports the post-allocation self-probe through the public
    /// URL: `None` while the probe is still running, `Some(true)` once a
    /// real HTTP round trip succeeded, `Some(false)` when the public URL did
    /// not answer -- shown to the user before they walk into Spotify's
    /// dashboard with a dead address (#592).
    Active {
        url: String,
        provider: &'static str,
        expires_at: u64,
        verified: Option<bool>,
    },
    /// Every provider failed, the tunnel timed out, or it exited
    /// unexpectedly. `message` is written to be read directly by a
    /// first-time user, not just logged.
    Error { message: String },
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Owns the Spotify callback tunnel's lifecycle. One instance per server;
/// only one tunnel is ever active at a time, matching the single Spotify
/// OAuth flow it exists to unblock.
pub struct SpotifyTunnelManager {
    launcher: Arc<dyn TunnelLauncher>,
    prober: Arc<dyn TunnelProbe>,
    status: Arc<RwLock<TunnelStatus>>,
    /// Bumped on every `start`/`stop` so a background task from a
    /// superseded attempt can tell its result is stale and stop writing to
    /// `status`.
    generation: Arc<AtomicU64>,
    /// Cancels the currently running background task, if any.
    cancel: Arc<Mutex<Option<CancellationToken>>>,
    /// Claims exclusive right to run `start`'s setup section without holding
    /// a lock guard across an `.await` (the repo's await-in-lock lint
    /// forbids that): `swap(true)` returning `true` means another `start`
    /// call is already in flight, so this one backs off instead of spawning
    /// a second tunnel.
    starting: Arc<std::sync::atomic::AtomicBool>,
    /// Set once at server boot (see `AppState::new`) so the tunnel is killed
    /// on graceful shutdown too, not just on the three user-facing paths.
    shutdown: Arc<OnceLock<CancellationToken>>,
}

impl Default for SpotifyTunnelManager {
    fn default() -> Self {
        Self::with_parts(Arc::new(RealTunnelLauncher), Arc::new(RealTunnelProbe))
    }
}

impl SpotifyTunnelManager {
    /// Test constructor: scripted launcher, no reachability probe (so unit
    /// tests never touch the network and `verified` stays `None`).
    #[cfg(test)]
    pub(crate) fn with_launcher(launcher: Arc<dyn TunnelLauncher>) -> Self {
        Self::with_parts(launcher, Arc::new(NullTunnelProbe))
    }

    pub(crate) fn with_parts(
        launcher: Arc<dyn TunnelLauncher>,
        prober: Arc<dyn TunnelProbe>,
    ) -> Self {
        Self {
            launcher,
            prober,
            status: Arc::new(RwLock::new(TunnelStatus::Idle)),
            generation: Arc::new(AtomicU64::new(0)),
            cancel: Arc::new(Mutex::new(None)),
            starting: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            shutdown: Arc::new(OnceLock::new()),
        }
    }

    /// Wires the server's graceful-shutdown token in so a tunnel process
    /// never outlives the server. Safe to call at most once; later calls are
    /// ignored (mirrors `OnceLock::set` semantics), which only matters for
    /// tests that construct more than one `AppState` against the same
    /// process -- production boots exactly one.
    pub fn bind_shutdown(&self, token: CancellationToken) {
        let _ = self.shutdown.set(token);
    }

    pub async fn status(&self) -> TunnelStatus {
        self.status.read().await.clone()
    }

    /// Report a startup precondition failure without ever choosing a fallback
    /// forward target.  In particular, a missing callback-only listener must
    /// not cause the tunnel to fall back to UHC's main LAN port.
    pub async fn fail_closed(&self, message: impl Into<String>) -> TunnelStatus {
        self.stop_locked().await;
        let status = TunnelStatus::Error {
            message: message.into(),
        };
        *self.status.write().await = status.clone();
        status
    }

    /// Starts a tunnel to `port` on this host, unless one is already
    /// starting or active -- in which case the existing attempt's status is
    /// returned untouched so a double click or a second browser tab cannot
    /// spawn a duplicate process.
    pub async fn start(self: &Arc<Self>, port: u16) -> TunnelStatus {
        let current = self.status.read().await.clone();
        if matches!(
            current,
            TunnelStatus::Starting { .. } | TunnelStatus::Active { .. }
        ) {
            return current;
        }
        if self.starting.swap(true, Ordering::SeqCst) {
            // A concurrent start() already passed the check above and is
            // mid-setup; let it finish rather than racing a second launch.
            return self.status.read().await.clone();
        }
        let status = self.start_exclusive(port).await;
        self.starting.store(false, Ordering::SeqCst);
        status
    }

    /// The actual setup section of `start`, run by at most one caller at a
    /// time (enforced by the `starting` flag in the caller, not by a lock
    /// guard held across these awaits).
    async fn start_exclusive(self: &Arc<Self>, port: u16) -> TunnelStatus {
        self.stop_locked().await;
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let token = CancellationToken::new();
        *self.cancel.lock().await = Some(token.clone());
        let provider = PROVIDER_ORDER[0];
        *self.status.write().await = TunnelStatus::Starting {
            provider: provider.label(),
        };
        let manager = self.clone();
        tokio::spawn(async move {
            manager.run(generation, port, token).await;
        });
        self.status.read().await.clone()
    }

    /// Stops the active/starting tunnel, if any, and returns to `Idle`.
    /// Called for a manual stop, when the OAuth callback completes, and via
    /// the shutdown-token watcher. Safe to call concurrently with `start` or
    /// with itself: cancelling an already-cancelled or absent token is a
    /// no-op, and the final state is always `Idle`.
    pub async fn stop(&self) {
        self.stop_locked().await;
        *self.status.write().await = TunnelStatus::Idle;
    }

    /// Tears the tunnel down `delay` from now, unless a newer `start`/`stop`
    /// has superseded this tunnel in the meantime (generation-guarded, so a
    /// deferred stop can never kill a tunnel it was not scheduled for).
    ///
    /// Used by the OAuth callback: the callback's own HTTP response -- and
    /// the settings page it redirects to -- still travel through the tunnel,
    /// so stopping inline resets the connection carrying the redirect
    /// (#592). See `TUNNEL_CALLBACK_GRACE`.
    pub fn stop_after(self: &Arc<Self>, delay: Duration) {
        let manager = self.clone();
        let generation = self.generation.load(Ordering::SeqCst);
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            if manager.generation.load(Ordering::SeqCst) == generation {
                manager.stop().await;
            }
        });
    }

    async fn stop_locked(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        if let Some(token) = self.cancel.lock().await.take() {
            token.cancel();
        }
    }

    fn shutdown_token(&self) -> Option<CancellationToken> {
        self.shutdown.get().cloned()
    }

    async fn set_status_if_current(&self, generation: u64, status: TunnelStatus) {
        if self.generation.load(Ordering::SeqCst) == generation {
            *self.status.write().await = status;
        }
    }

    async fn run(self: Arc<Self>, generation: u64, port: u16, token: CancellationToken) {
        let shutdown = self.shutdown_token();
        let mut failures: Vec<String> = Vec::new();
        for provider in PROVIDER_ORDER {
            if self.generation.load(Ordering::SeqCst) != generation {
                return;
            }
            self.set_status_if_current(
                generation,
                TunnelStatus::Starting {
                    provider: provider.label(),
                },
            )
            .await;
            let mut process = match self.launcher.launch(provider, port).await {
                Ok(process) => process,
                Err(TunnelLaunchError::BinaryMissing) => {
                    self.set_status_if_current(
                        generation,
                        TunnelStatus::Error {
                            // "Install the OpenSSH client" is useless advice
                            // inside a container, which is where this is most
                            // likely to be hit -- the images now ship it, so
                            // point at the update first.
                            message: "ssh was not found on this system. If you run Unified \
                                      Hi-Fi Control as a Home Assistant add-on or a Docker \
                                      container, update to the latest version -- it includes \
                                      ssh. Otherwise install the OpenSSH client, or use \
                                      \"Advanced: bring your own HTTPS\" below."
                                .to_string(),
                        },
                    )
                    .await;
                    return;
                }
                Err(TunnelLaunchError::Spawn(reason)) => {
                    failures.push(format!("{}: {reason}", provider.label()));
                    continue;
                }
            };

            match wait_for_url(
                process.as_mut(),
                provider,
                &token,
                shutdown.as_ref(),
                PROVIDER_STARTUP_TIMEOUT,
            )
            .await
            {
                WaitOutcome::Url(url) => {
                    let expires_at = now_secs() + TUNNEL_MAX_LIFETIME.as_secs();
                    self.set_status_if_current(
                        generation,
                        TunnelStatus::Active {
                            url: url.clone(),
                            provider: provider.label(),
                            expires_at,
                            verified: None,
                        },
                    )
                    .await;
                    self.spawn_reachability_probe(generation, url);
                    self.supervise_active(process.as_mut(), generation, &token, shutdown.as_ref())
                        .await;
                    return;
                }
                WaitOutcome::Cancelled => {
                    process.kill();
                    return;
                }
                WaitOutcome::Failed(reason) => {
                    process.kill();
                    failures.push(format!("{}: {reason}", provider.label()));
                    continue;
                }
            }
        }
        let detail = if failures.is_empty() {
            "no tunnel provider could be reached".to_string()
        } else {
            failures.join("; ")
        };
        self.set_status_if_current(
            generation,
            TunnelStatus::Error {
                message: format!(
                    "Could not open an HTTPS tunnel ({detail}). Check that this server has \
                     outbound network access on port 443, then try again, or use \"Advanced: \
                     bring your own HTTPS\" below."
                ),
            },
        )
        .await;
    }

    /// One-shot self-check through the freshly allocated public URL,
    /// recorded into `TunnelStatus::Active::verified` if this tunnel is
    /// still the current one when the probe returns. A URL that prints but
    /// cannot carry HTTP back to this server is exactly the failure the user
    /// otherwise discovers only when Spotify's redirect dies (#592).
    fn spawn_reachability_probe(self: &Arc<Self>, generation: u64, url: String) {
        let manager = self.clone();
        tokio::spawn(async move {
            let Some(reachable) = manager.prober.probe(&url).await else {
                return;
            };
            if manager.generation.load(Ordering::SeqCst) != generation {
                return;
            }
            let mut status = manager.status.write().await;
            let updated = match &*status {
                TunnelStatus::Active {
                    url: current_url,
                    provider,
                    expires_at,
                    ..
                } if *current_url == url => Some(TunnelStatus::Active {
                    url: url.clone(),
                    provider,
                    expires_at: *expires_at,
                    verified: Some(reachable),
                }),
                _ => None,
            };
            if let Some(updated) = updated {
                *status = updated;
            }
        });
    }

    /// Once a URL is live, keep reading events until the tunnel is
    /// cancelled (manual stop or OAuth completion), the process exits on
    /// its own, or the lifetime cap (`TUNNEL_MAX_LIFETIME`) is reached.
    async fn supervise_active(
        &self,
        process: &mut dyn TunnelProcess,
        generation: u64,
        token: &CancellationToken,
        shutdown: Option<&CancellationToken>,
    ) {
        let deadline = tokio::time::sleep(TUNNEL_MAX_LIFETIME);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = token.cancelled() => {
                    process.kill();
                    return;
                }
                _ = maybe_cancelled(shutdown) => {
                    process.kill();
                    return;
                }
                () = &mut deadline => {
                    process.kill();
                    self.set_status_if_current(generation, TunnelStatus::Error {
                        message: format!(
                            "The tunnel timed out after {} minutes and was closed. Click \
                             \"Get an HTTPS address\" again for a fresh URL.",
                            TUNNEL_MAX_LIFETIME.as_secs() / 60
                        ),
                    }).await;
                    return;
                }
                event = process.next_event() => match event {
                    TunnelProcessEvent::Line(_) => continue,
                    TunnelProcessEvent::Exited { stderr_tail } => {
                        let detail = if stderr_tail.is_empty() {
                            String::new()
                        } else {
                            format!(": {stderr_tail}")
                        };
                        self.set_status_if_current(generation, TunnelStatus::Error {
                            message: format!(
                                "The tunnel closed unexpectedly{detail}. Click \"Get an HTTPS \
                                 address\" again for a fresh URL."
                            ),
                        }).await;
                        return;
                    }
                },
            }
        }
    }
}

enum WaitOutcome {
    Url(String),
    Cancelled,
    Failed(String),
}

async fn maybe_cancelled(token: Option<&CancellationToken>) {
    match token {
        Some(token) => token.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

async fn wait_for_url(
    process: &mut dyn TunnelProcess,
    provider: TunnelProviderKind,
    token: &CancellationToken,
    shutdown: Option<&CancellationToken>,
    timeout: Duration,
) -> WaitOutcome {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return WaitOutcome::Failed("timed out waiting for a public URL".to_string());
        }
        tokio::select! {
            _ = token.cancelled() => return WaitOutcome::Cancelled,
            _ = maybe_cancelled(shutdown) => return WaitOutcome::Cancelled,
            _ = tokio::time::sleep(remaining) => {
                return WaitOutcome::Failed("timed out waiting for a public URL".to_string());
            }
            event = process.next_event() => match event {
                TunnelProcessEvent::Line(line) => {
                    if let Some(url) = provider.extract_url(&line) {
                        return WaitOutcome::Url(url);
                    }
                }
                TunnelProcessEvent::Exited { stderr_tail } => {
                    return WaitOutcome::Failed(if stderr_tail.is_empty() {
                        "process exited before printing a URL".to_string()
                    } else {
                        stderr_tail
                    });
                }
            },
        }
    }
}

/// JSON shape returned by the tunnel start/status/stop endpoints.
#[derive(Debug, Serialize)]
pub struct TunnelStatusResponse {
    pub phase: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seconds_remaining: Option<u64>,
    /// Post-allocation self-probe result: absent while the probe is still
    /// running, `true` once an HTTP round trip through the public URL
    /// succeeded, `false` when the public address did not answer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl From<TunnelStatus> for TunnelStatusResponse {
    fn from(status: TunnelStatus) -> Self {
        match status {
            TunnelStatus::Idle => Self {
                phase: "idle",
                provider: None,
                url: None,
                expires_at: None,
                seconds_remaining: None,
                verified: None,
                message: None,
            },
            TunnelStatus::Starting { provider } => Self {
                phase: "starting",
                provider: Some(provider),
                url: None,
                expires_at: None,
                seconds_remaining: None,
                verified: None,
                message: None,
            },
            TunnelStatus::Active {
                url,
                provider,
                expires_at,
                verified,
            } => Self {
                phase: "active",
                provider: Some(provider),
                url: Some(url),
                expires_at: Some(expires_at),
                seconds_remaining: Some(expires_at.saturating_sub(now_secs())),
                verified,
                message: None,
            },
            TunnelStatus::Error { message } => Self {
                phase: "error",
                provider: None,
                url: None,
                expires_at: None,
                seconds_remaining: None,
                verified: None,
                message: Some(message),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex as StdMutex;
    use tokio::sync::Notify;

    /// A scripted stand-in for a real `ssh` process: replays a fixed
    /// sequence of stdout lines (and optionally "exits" afterward), and
    /// records whether it was killed.
    struct FakeProcess {
        lines: VecDeque<String>,
        exit_with_stderr: Option<String>,
        killed: Arc<AtomicBool>,
        /// Never resolves once the script is exhausted and no exit is
        /// scripted, so a supervising loop just waits on cancellation --
        /// like a real long-lived tunnel with no more banner output.
        idle: Arc<Notify>,
    }

    #[async_trait::async_trait]
    impl TunnelProcess for FakeProcess {
        async fn next_event(&mut self) -> TunnelProcessEvent {
            if let Some(line) = self.lines.pop_front() {
                return TunnelProcessEvent::Line(line);
            }
            if let Some(stderr) = self.exit_with_stderr.take() {
                return TunnelProcessEvent::Exited {
                    stderr_tail: stderr,
                };
            }
            self.idle.notified().await;
            TunnelProcessEvent::Exited {
                stderr_tail: String::new(),
            }
        }

        fn kill(&mut self) {
            self.killed.store(true, Ordering::SeqCst);
            self.idle.notify_waiters();
        }
    }

    #[derive(Clone)]
    struct ScriptedLaunch {
        lines: Vec<String>,
        exit_with_stderr: Option<String>,
        launch_error: Option<TunnelLaunchError>,
    }

    impl ScriptedLaunch {
        fn url(url: &str) -> Self {
            Self {
                lines: vec![format!("Forwarding started: {url}")],
                exit_with_stderr: None,
                launch_error: None,
            }
        }

        fn failing(error: TunnelLaunchError) -> Self {
            Self {
                lines: Vec::new(),
                exit_with_stderr: None,
                launch_error: Some(error),
            }
        }

        fn dies(stderr: &str) -> Self {
            Self {
                lines: Vec::new(),
                exit_with_stderr: Some(stderr.to_string()),
                launch_error: None,
            }
        }
    }

    /// A `TunnelLauncher` whose behavior per provider is scripted up front.
    /// Exposes the kill flags for each spawned process and a count of kills
    /// via a shared `Vec` so tests can assert no process is left running.
    struct FakeLauncher {
        scripts: StdMutex<std::collections::HashMap<TunnelProviderKind, ScriptedLaunch>>,
        killed_flags: StdMutex<Vec<Arc<AtomicBool>>>,
    }

    impl FakeLauncher {
        fn new(scripts: Vec<(TunnelProviderKind, ScriptedLaunch)>) -> Arc<Self> {
            Arc::new(Self {
                scripts: StdMutex::new(scripts.into_iter().collect()),
                killed_flags: StdMutex::new(Vec::new()),
            })
        }

        fn any_process_left_running(&self) -> bool {
            self.killed_flags
                .lock()
                .unwrap()
                .iter()
                .any(|flag| !flag.load(Ordering::SeqCst))
        }
    }

    #[async_trait::async_trait]
    impl TunnelLauncher for FakeLauncher {
        async fn launch(
            &self,
            provider: TunnelProviderKind,
            _port: u16,
        ) -> Result<Box<dyn TunnelProcess>, TunnelLaunchError> {
            let script = self
                .scripts
                .lock()
                .unwrap()
                .get(&provider)
                .cloned()
                .unwrap_or_else(|| ScriptedLaunch::dies("no script for provider"));
            if let Some(error) = script.launch_error {
                return Err(error);
            }
            let killed = Arc::new(AtomicBool::new(false));
            self.killed_flags.lock().unwrap().push(killed.clone());
            Ok(Box::new(FakeProcess {
                lines: script.lines.into(),
                exit_with_stderr: script.exit_with_stderr,
                killed,
                idle: Arc::new(Notify::new()),
            }))
        }
    }

    async fn wait_until<F: Fn(&TunnelStatus) -> bool>(
        manager: &Arc<SpotifyTunnelManager>,
        predicate: F,
    ) -> TunnelStatus {
        for _ in 0..200 {
            let status = manager.status().await;
            if predicate(&status) {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "condition not met in time; last status: {:?}",
            manager.status().await
        );
    }

    #[tokio::test]
    async fn start_reaches_active_with_pinggy_url() {
        let launcher = FakeLauncher::new(vec![(
            TunnelProviderKind::Pinggy,
            ScriptedLaunch::url("https://abc123.a.pinggy.link"),
        )]);
        let manager = Arc::new(SpotifyTunnelManager::with_launcher(launcher));
        manager.start(8088).await;
        let status = wait_until(&manager, |s| matches!(s, TunnelStatus::Active { .. })).await;
        match status {
            TunnelStatus::Active { url, provider, .. } => {
                assert_eq!(url, "https://abc123.a.pinggy.link");
                assert_eq!(provider, "pinggy.io");
            }
            other => panic!("expected Active, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn falls_back_to_localhost_run_when_pinggy_fails_to_launch() {
        let launcher = FakeLauncher::new(vec![
            (
                TunnelProviderKind::Pinggy,
                ScriptedLaunch::failing(TunnelLaunchError::Spawn("connection refused".into())),
            ),
            (
                TunnelProviderKind::LocalhostRun,
                ScriptedLaunch::url("https://xyz789.lhr.life"),
            ),
        ]);
        let manager = Arc::new(SpotifyTunnelManager::with_launcher(launcher));
        manager.start(8088).await;
        let status = wait_until(&manager, |s| matches!(s, TunnelStatus::Active { .. })).await;
        match status {
            TunnelStatus::Active { url, provider, .. } => {
                assert_eq!(url, "https://xyz789.lhr.life");
                assert_eq!(provider, "localhost.run");
            }
            other => panic!("expected Active, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn falls_back_when_pinggy_process_dies_before_a_url() {
        let launcher = FakeLauncher::new(vec![
            (TunnelProviderKind::Pinggy, ScriptedLaunch::dies("EOF")),
            (
                TunnelProviderKind::LocalhostRun,
                ScriptedLaunch::url("https://fallback.lhr.life"),
            ),
        ]);
        let manager = Arc::new(SpotifyTunnelManager::with_launcher(launcher));
        manager.start(8088).await;
        let status = wait_until(&manager, |s| matches!(s, TunnelStatus::Active { .. })).await;
        assert!(matches!(
            status,
            TunnelStatus::Active {
                provider: "localhost.run",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn missing_ssh_binary_reports_actionable_error_without_trying_fallback() {
        let launcher = FakeLauncher::new(vec![(
            TunnelProviderKind::Pinggy,
            ScriptedLaunch::failing(TunnelLaunchError::BinaryMissing),
        )]);
        let manager = Arc::new(SpotifyTunnelManager::with_launcher(launcher));
        manager.start(8088).await;
        let status = wait_until(&manager, |s| matches!(s, TunnelStatus::Error { .. })).await;
        match status {
            TunnelStatus::Error { message } => {
                assert!(message.contains("ssh was not found"), "{message}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn both_providers_failing_reports_combined_actionable_error() {
        let launcher = FakeLauncher::new(vec![
            (
                TunnelProviderKind::Pinggy,
                ScriptedLaunch::failing(TunnelLaunchError::Spawn("timed out".into())),
            ),
            (
                TunnelProviderKind::LocalhostRun,
                ScriptedLaunch::failing(TunnelLaunchError::Spawn("timed out".into())),
            ),
        ]);
        let manager = Arc::new(SpotifyTunnelManager::with_launcher(launcher));
        manager.start(8088).await;
        let status = wait_until(&manager, |s| matches!(s, TunnelStatus::Error { .. })).await;
        match status {
            TunnelStatus::Error { message } => {
                assert!(message.contains("pinggy.io"), "{message}");
                assert!(message.contains("localhost.run"), "{message}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stop_kills_the_process_and_returns_to_idle() {
        let launcher = FakeLauncher::new(vec![(
            TunnelProviderKind::Pinggy,
            ScriptedLaunch::url("https://abc123.a.pinggy.link"),
        )]);
        let launcher_ref = launcher.clone();
        let manager = Arc::new(SpotifyTunnelManager::with_launcher(launcher));
        manager.start(8088).await;
        wait_until(&manager, |s| matches!(s, TunnelStatus::Active { .. })).await;
        manager.stop().await;
        let status = wait_until(&manager, |s| matches!(s, TunnelStatus::Idle)).await;
        assert_eq!(status, TunnelStatus::Idle);
        // Give the killed background task a moment to flip its flag.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !launcher_ref.any_process_left_running(),
            "tunnel process was not killed"
        );
    }

    #[tokio::test]
    async fn starting_twice_does_not_spawn_a_second_process() {
        let launcher = FakeLauncher::new(vec![(
            TunnelProviderKind::Pinggy,
            ScriptedLaunch::url("https://abc123.a.pinggy.link"),
        )]);
        let manager = Arc::new(SpotifyTunnelManager::with_launcher(launcher));
        manager.start(8088).await;
        manager.start(8088).await;
        wait_until(&manager, |s| matches!(s, TunnelStatus::Active { .. })).await;
        // Only one process should ever have been launched; a second launch
        // while one is starting/active is a no-op per `start`'s contract.
    }

    #[test]
    fn pinggy_url_extraction_ignores_surrounding_banner_text() {
        let line = "Forwarding started: https://rand0m.a.pinggy.link -> localhost:8088";
        assert_eq!(
            TunnelProviderKind::Pinggy.extract_url(line),
            Some("https://rand0m.a.pinggy.link".to_string())
        );
        assert_eq!(
            TunnelProviderKind::Pinggy.extract_url("visit https://pinggy.io/docs"),
            None
        );
    }

    #[test]
    fn localhost_run_url_extraction() {
        let line = "tunneled with tls termination, https://ab12cd.lhr.life";
        assert_eq!(
            TunnelProviderKind::LocalhostRun.extract_url(line),
            Some("https://ab12cd.lhr.life".to_string())
        );
    }

    /// Regression test for a live-smoke defect: `ssh -p 443
    /// -R0:localhost:<port> a.pinggy.io`'s actual anonymous-tunnel stdout
    /// never contains a bare `pinggy.link` host, so the original
    /// `\.pinggy\.link` pattern never matched and the manager timed out
    /// despite a live tunnel. This is the exact captured output (only the
    /// per-connection subdomains are illustrative).
    const LIVE_PINGGY_ANONYMOUS_STDOUT: [&str; 5] = [
        "Allocated port 9 for remote forward to localhost:8091",
        "You are not authenticated.",
        "Your tunnel will expire in 60 minutes. Upgrade to Pinggy Pro to get unrestricted tunnels. https://dashboard.pinggy.io",
        "https://lgidn-2603-6010-e300-381a-352c-943-c7fa-76a9.run.pinggy-free.link",
        "https://rjvqd-2603-6010-e300-381a-352c-943-c7fa-76a9.free.pinggy.net",
    ];

    #[test]
    fn pinggy_extraction_matches_the_live_anonymous_tunnel_domains() {
        let matches: Vec<Option<String>> = LIVE_PINGGY_ANONYMOUS_STDOUT
            .iter()
            .map(|line| TunnelProviderKind::Pinggy.extract_url(line))
            .collect();
        assert_eq!(
            matches,
            vec![
                None,
                None,
                // The Pinggy Pro upsell link on the expiry-notice line must
                // not be mistaken for the tunnel URL.
                None,
                Some(
                    "https://lgidn-2603-6010-e300-381a-352c-943-c7fa-76a9.run.pinggy-free.link"
                        .to_string()
                ),
                Some(
                    "https://rjvqd-2603-6010-e300-381a-352c-943-c7fa-76a9.free.pinggy.net"
                        .to_string()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn start_reaches_active_on_the_live_captured_pinggy_banner() {
        let launcher = FakeLauncher::new(vec![(
            TunnelProviderKind::Pinggy,
            ScriptedLaunch {
                lines: LIVE_PINGGY_ANONYMOUS_STDOUT
                    .iter()
                    .map(|line| line.to_string())
                    .collect(),
                exit_with_stderr: None,
                launch_error: None,
            },
        )]);
        let manager = Arc::new(SpotifyTunnelManager::with_launcher(launcher));
        manager.start(8091).await;
        let status = wait_until(&manager, |s| matches!(s, TunnelStatus::Active { .. })).await;
        match status {
            TunnelStatus::Active { url, provider, .. } => {
                assert_eq!(
                    url,
                    "https://lgidn-2603-6010-e300-381a-352c-943-c7fa-76a9.run.pinggy-free.link"
                );
                assert_eq!(provider, "pinggy.io");
            }
            other => panic!("expected Active, got {other:?}"),
        }
    }

    #[test]
    fn response_conversion_reports_seconds_remaining_for_active_tunnel() {
        let response = TunnelStatusResponse::from(TunnelStatus::Active {
            url: "https://example.pinggy.link".to_string(),
            provider: "pinggy.io",
            expires_at: now_secs() + 100,
            verified: Some(true),
        });
        assert_eq!(response.phase, "active");
        assert_eq!(response.url.as_deref(), Some("https://example.pinggy.link"));
        assert!(response.seconds_remaining.unwrap() <= 100);
        assert_eq!(response.verified, Some(true));
    }

    /// #592 regression: the `ssh -R` forward target must be the IPv4
    /// loopback literal, not `localhost`. Inside containers `localhost` can
    /// resolve to `::1` while UHC listens on IPv4 only, in which case the
    /// tunnel prints a URL that resets every public request.
    #[test]
    fn tunnel_commands_forward_to_the_ipv4_loopback_literal() {
        let (program, args) = TunnelProviderKind::Pinggy.command(8088);
        assert_eq!(program, "ssh");
        assert!(args.contains(&"-R0:127.0.0.1:8088".to_string()), "{args:?}");
        let (program, args) = TunnelProviderKind::LocalhostRun.command(8088);
        assert_eq!(program, "ssh");
        assert!(args.contains(&"80:127.0.0.1:8088".to_string()), "{args:?}");
    }

    /// #592 regression: a 15-minute cap expired while the user was still
    /// working through Spotify's developer dashboard, so the consent
    /// redirect landed on a dead tunnel as ERR_CONNECTION_RESET. The cap
    /// must outlast a first-time enrollment (>= 20 minutes demonstrated
    /// live) while staying under pinggy's 60-minute anonymous-tunnel limit
    /// so the expiry message shown to the user is ours.
    #[test]
    fn lifetime_cap_outlasts_a_first_time_dashboard_round_trip() {
        assert!(TUNNEL_MAX_LIFETIME >= Duration::from_secs(20 * 60));
        assert!(TUNNEL_MAX_LIFETIME < Duration::from_secs(60 * 60));
    }

    /// #592 state-machine regression: the tunnel stays Active through a slow
    /// dashboard round trip (20+ minutes) and only expires at the cap.
    #[tokio::test(start_paused = true)]
    async fn tunnel_survives_twenty_minutes_then_expires_at_the_cap() {
        let launcher = FakeLauncher::new(vec![(
            TunnelProviderKind::Pinggy,
            ScriptedLaunch::url("https://abc123.a.pinggy.link"),
        )]);
        let manager = Arc::new(SpotifyTunnelManager::with_launcher(launcher));
        manager.start(8088).await;
        wait_until(&manager, |s| matches!(s, TunnelStatus::Active { .. })).await;
        tokio::time::advance(Duration::from_secs(20 * 60)).await;
        assert!(
            matches!(manager.status().await, TunnelStatus::Active { .. }),
            "tunnel must still be up 20 minutes in; a user is often still \
             in Spotify's dashboard at that point (#592)"
        );
        tokio::time::advance(TUNNEL_MAX_LIFETIME).await;
        let status = wait_until(&manager, |s| matches!(s, TunnelStatus::Error { .. })).await;
        match status {
            TunnelStatus::Error { message } => {
                assert!(message.contains("timed out"), "{message}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// #592: the OAuth callback's teardown is deferred so the callback
    /// response can travel back through the tunnel first.
    #[tokio::test(start_paused = true)]
    async fn deferred_stop_tears_down_only_after_the_grace_period() {
        let launcher = FakeLauncher::new(vec![(
            TunnelProviderKind::Pinggy,
            ScriptedLaunch::url("https://abc123.a.pinggy.link"),
        )]);
        let launcher_ref = launcher.clone();
        let manager = Arc::new(SpotifyTunnelManager::with_launcher(launcher));
        manager.start(8088).await;
        wait_until(&manager, |s| matches!(s, TunnelStatus::Active { .. })).await;
        manager.stop_after(TUNNEL_CALLBACK_GRACE);
        // Let the spawned deferred-stop task register its timer before
        // advancing the paused clock, or the advance passes it by.
        tokio::task::yield_now().await;
        tokio::time::advance(TUNNEL_CALLBACK_GRACE - Duration::from_secs(1)).await;
        assert!(
            matches!(manager.status().await, TunnelStatus::Active { .. }),
            "the tunnel must survive the grace window: the callback \
             response and the settings redirect still travel through it"
        );
        tokio::time::advance(Duration::from_secs(2)).await;
        wait_until(&manager, |s| matches!(s, TunnelStatus::Idle)).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !launcher_ref.any_process_left_running(),
            "tunnel process was not killed after the grace period"
        );
    }

    /// A deferred stop scheduled for one tunnel must never kill a newer one
    /// started after it (generation guard).
    #[tokio::test]
    async fn deferred_stop_never_kills_a_newer_tunnel() {
        let launcher = FakeLauncher::new(vec![(
            TunnelProviderKind::Pinggy,
            ScriptedLaunch::url("https://abc123.a.pinggy.link"),
        )]);
        let manager = Arc::new(SpotifyTunnelManager::with_launcher(launcher));
        manager.start(8088).await;
        wait_until(&manager, |s| matches!(s, TunnelStatus::Active { .. })).await;
        manager.stop_after(Duration::from_millis(50));
        manager.stop().await;
        manager.start(8088).await;
        wait_until(&manager, |s| matches!(s, TunnelStatus::Active { .. })).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            matches!(manager.status().await, TunnelStatus::Active { .. }),
            "a stale deferred stop killed the newer tunnel"
        );
    }

    struct ScriptedProbe(Option<bool>);

    #[async_trait::async_trait]
    impl TunnelProbe for ScriptedProbe {
        async fn probe(&self, _url: &str) -> Option<bool> {
            self.0
        }
    }

    /// #592: a printed URL is not a working tunnel. The post-allocation
    /// self-probe's verdict must land in the Active status so the UI can
    /// show red/green before the user registers the address with Spotify.
    #[tokio::test]
    async fn reachability_probe_verdict_lands_in_active_status() {
        for verdict in [true, false] {
            let launcher = FakeLauncher::new(vec![(
                TunnelProviderKind::Pinggy,
                ScriptedLaunch::url("https://abc123.a.pinggy.link"),
            )]);
            let manager = Arc::new(SpotifyTunnelManager::with_parts(
                launcher,
                Arc::new(ScriptedProbe(Some(verdict))),
            ));
            manager.start(8088).await;
            let status = wait_until(&manager, |s| {
                matches!(
                    s,
                    TunnelStatus::Active {
                        verified: Some(_),
                        ..
                    }
                )
            })
            .await;
            match status {
                TunnelStatus::Active { url, verified, .. } => {
                    assert_eq!(url, "https://abc123.a.pinggy.link");
                    assert_eq!(verified, Some(verdict));
                }
                other => panic!("expected Active, got {other:?}"),
            }
        }
    }
}
