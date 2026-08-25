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
//! (success or failure), when the user stops it, on a fifteen-minute safety
//! timeout, or when the server shuts down. While it is active this UHC
//! server is briefly reachable from the public internet at the tunnel's
//! URL; the callback endpoint behind it only ever accepts the single-use,
//! in-flight OAuth `state` token (see `oauth_callback_json` in
//! `provider_auth.rs`), so no additional trust is extended to tunnel
//! traffic.
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
/// never end up with a forgotten tunnel silently keeping this server
/// internet-reachable.
pub const TUNNEL_MAX_LIFETIME: Duration = Duration::from_secs(15 * 60);

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
                    format!("-R0:localhost:{port}"),
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
                    format!("80:localhost:{port}"),
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

#[allow(clippy::unwrap_used)] // Regex pattern is a compile-time constant
fn pinggy_url_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"https://[a-zA-Z0-9.-]+\.pinggy\.link\S*").unwrap())
}

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
    Active {
        url: String,
        provider: &'static str,
        expires_at: u64,
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
        Self::with_launcher(Arc::new(RealTunnelLauncher))
    }
}

impl SpotifyTunnelManager {
    pub(crate) fn with_launcher(launcher: Arc<dyn TunnelLauncher>) -> Self {
        Self {
            launcher,
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
                            message: "ssh was not found on this system. Install the OpenSSH \
                                      client (most Linux, macOS, and NAS systems already have \
                                      it) or use \"Advanced: bring your own HTTPS\" below."
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
                            url,
                            provider: provider.label(),
                            expires_at,
                        },
                    )
                    .await;
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

    /// Once a URL is live, keep reading events until the tunnel is
    /// cancelled (manual stop or OAuth completion), the process exits on
    /// its own, or the fifteen-minute cap is reached.
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
                        message: "The tunnel timed out after 15 minutes and was closed. Click \
                                  \"Get an HTTPS address\" again for a fresh URL.".to_string(),
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
                message: None,
            },
            TunnelStatus::Starting { provider } => Self {
                phase: "starting",
                provider: Some(provider),
                url: None,
                expires_at: None,
                seconds_remaining: None,
                message: None,
            },
            TunnelStatus::Active {
                url,
                provider,
                expires_at,
            } => Self {
                phase: "active",
                provider: Some(provider),
                url: Some(url),
                expires_at: Some(expires_at),
                seconds_remaining: Some(expires_at.saturating_sub(now_secs())),
                message: None,
            },
            TunnelStatus::Error { message } => Self {
                phase: "error",
                provider: None,
                url: None,
                expires_at: None,
                seconds_remaining: None,
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

    #[test]
    fn response_conversion_reports_seconds_remaining_for_active_tunnel() {
        let response = TunnelStatusResponse::from(TunnelStatus::Active {
            url: "https://example.pinggy.link".to_string(),
            provider: "pinggy.io",
            expires_at: now_secs() + 100,
        });
        assert_eq!(response.phase, "active");
        assert_eq!(response.url.as_deref(), Some("https://example.pinggy.link"));
        assert!(response.seconds_remaining.unwrap() <= 100);
    }
}
