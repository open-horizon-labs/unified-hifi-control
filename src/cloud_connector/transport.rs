use std::time::Duration;

/// The relay must prove liveness periodically after authentication.  These
/// bounds are deliberately conservative; a missed heartbeat cannot make a
/// command execute, but it must eventually release a black-holed session.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const SOCKET_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
pub const PEER_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(90);
pub const PEER_HEARTBEAT_CHECK_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelayEndpoint(String);

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
#[error("relay endpoint must be an HTTPS WebSocket URL")]
pub struct EndpointError;

impl RelayEndpoint {
    pub fn parse(value: &str) -> Result<Self, EndpointError> {
        let parsed = url::Url::parse(value).map_err(|_| EndpointError)?;
        if parsed.scheme() != "wss"
            || parsed.host_str().is_none()
            || parsed.username() != ""
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(EndpointError);
        }
        Ok(Self(value.trim_end_matches('/').to_owned()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionState {
    Offline,
    Connecting,
    Online { epoch: u64, snapshot_sent: bool },
    Revoked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunLoopEvent {
    Connect,
    Connected { epoch: u64 },
    Heartbeat,
    SnapshotRequired,
    Disconnected,
    Revoked,
}

#[derive(Clone, Debug)]
pub struct Backoff {
    attempt: u32,
    base: Duration,
    max: Duration,
}
impl Default for Backoff {
    fn default() -> Self {
        Self {
            attempt: 0,
            base: Duration::from_millis(250),
            max: Duration::from_secs(30),
        }
    }
}
impl Backoff {
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
    pub fn next_delay(&mut self) -> Duration {
        let delay = self
            .base
            .saturating_mul(2u32.saturating_pow(self.attempt.min(7)));
        self.attempt = self.attempt.saturating_add(1);
        delay.min(self.max)
    }
}

pub struct ConnectorRunLoop {
    state: ConnectionState,
    next_epoch: u64,
    backoff: Backoff,
    outbound: Vec<RunLoopEvent>,
    last_heartbeat_ms: Option<u64>,
}

/// Small deterministic liveness primitive used by the real socket loop.  It
/// stores milliseconds rather than an `Instant` so tests can exercise expiry
/// without sleeping. Saturating arithmetic prevents clock rollback from
/// underflowing; command grants independently fail their wall-clock checks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerWatchdog {
    last_seen_ms: u64,
    timeout_ms: u64,
}

impl PeerWatchdog {
    pub fn new(now_ms: u64, timeout: Duration) -> Self {
        Self {
            last_seen_ms: now_ms,
            timeout_ms: timeout.as_millis().try_into().unwrap_or(u64::MAX),
        }
    }

    pub fn observe(&mut self, now_ms: u64) {
        self.last_seen_ms = now_ms;
    }

    pub fn expired(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.last_seen_ms) >= self.timeout_ms
    }
}
impl Default for ConnectorRunLoop {
    fn default() -> Self {
        Self {
            state: ConnectionState::Offline,
            next_epoch: 0,
            backoff: Default::default(),
            outbound: Vec::new(),
            last_heartbeat_ms: None,
        }
    }
}
impl ConnectorRunLoop {
    pub fn connect(&mut self) {
        if !matches!(self.state, ConnectionState::Revoked) {
            self.state = ConnectionState::Connecting;
            self.outbound.push(RunLoopEvent::Connect);
        }
    }
    pub fn connected(&mut self) -> u64 {
        self.next_epoch = self.next_epoch.saturating_add(1);
        self.state = ConnectionState::Online {
            epoch: self.next_epoch,
            snapshot_sent: false,
        };
        self.backoff.reset();
        self.outbound.push(RunLoopEvent::Connected {
            epoch: self.next_epoch,
        });
        self.next_epoch
    }
    pub fn mark_snapshot_sent(&mut self) {
        if let ConnectionState::Online { epoch, .. } = self.state {
            self.state = ConnectionState::Online {
                epoch,
                snapshot_sent: true,
            };
        }
    }
    pub fn can_send_delta(&self) -> bool {
        matches!(
            self.state,
            ConnectionState::Online {
                snapshot_sent: true,
                ..
            }
        )
    }
    pub fn heartbeat(&mut self, now_ms: u64) {
        if matches!(self.state, ConnectionState::Online { .. }) {
            self.last_heartbeat_ms = Some(now_ms);
            self.outbound.push(RunLoopEvent::Heartbeat);
        }
    }
    pub fn disconnect(&mut self) {
        self.state = ConnectionState::Offline;
        self.outbound.clear();
        self.last_heartbeat_ms = None;
    }
    pub fn revoke(&mut self) {
        self.state = ConnectionState::Revoked;
        self.outbound.clear();
    }
    pub fn state(&self) -> ConnectionState {
        self.state
    }
    pub fn reconnect_delay(&mut self) -> Duration {
        self.backoff.next_delay()
    }
    pub fn drain_events(&mut self) -> Vec<RunLoopEvent> {
        std::mem::take(&mut self.outbound)
    }
}
