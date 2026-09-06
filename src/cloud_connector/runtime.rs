//! Opt-in outbound connector for the authenticated HiPhi relay.
//!
//! The relay is contacted over WSS only.  It sends a challenge first; the
//! installation grant authenticates the HTTP upgrade and is then carried in
//! the signed proof body so possession of the installation key is still
//! required. The grant is never placed in a URL. All control dispatch goes through the existing semantic
//! MQTT command router, which keeps the aggregator/command gateway boundary.

use futures::{SinkExt, StreamExt};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, Mutex, Semaphore};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{
        client::IntoClientRequest as _,
        http::{header::AUTHORIZATION, HeaderValue},
        protocol::{frame::coding::CloseCode, CloseFrame, WebSocketConfig},
        Message,
    },
};
use uuid::Uuid;

use super::{
    commands::{CommandGrantVerifier, CommandLedger, CommandOutcome, GrantError, LedgerError},
    config::CloudConnectorConfig,
    identity::InstallationIdentity,
    protocol::{
        ArtworkChunk, ArtworkRelayRequest, ArtworkRelayResponse, CommandResult, CommandStatus,
        ConnectorHello, ConnectorMessage, RelayMessage, StateSnapshot, MAX_ARTWORK_CHUNK_BYTES,
        MAX_ARTWORK_SOURCE_BYTES, MAX_COMMAND_BYTES, MAX_STRING_BYTES, PROTOCOL_VERSION,
        UHC_AUDIENCE,
    },
    session::{sign_installation_session_proof, verify_installation_session_grant},
    state::{snapshot_from_aggregator, StateStore},
    transport::{
        PeerWatchdog, RelayEndpoint, SessionEpochGuard, CONNECT_TIMEOUT,
        PEER_HEARTBEAT_CHECK_INTERVAL, PEER_HEARTBEAT_TIMEOUT, SOCKET_WRITE_TIMEOUT,
    },
};

const ARTWORK_QUEUE_CAPACITY: usize = super::artwork::MAX_PENDING;
const ARTWORK_CONCURRENCY: usize = super::artwork::MAX_CONCURRENT;
const ARTWORK_FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const ARTWORK_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);
const MAX_SESSION_GRANT_RESPONSE_BYTES: usize = 16 * 1024;
const SHUTDOWN_CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

fn snapshot_refresh_interval() -> tokio::time::Interval {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(20));
    // A stalled task needs one current snapshot, not a burst of every missed tick.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantResponse {
    grant: String,
    endpoint: String,
    expires_at: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConnectionExit {
    Disconnected,
    Revoked,
    Shutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectorPhase {
    Unconfigured,
    Offline,
    Connecting,
    Online,
    Revoked,
    Paused,
    SafetyError,
}

impl ConnectorPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unconfigured => "unconfigured",
            Self::Offline => "offline",
            Self::Connecting => "connecting",
            Self::Online => "online",
            Self::Revoked => "revoked",
            Self::Paused | Self::SafetyError => "paused",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectorStatus {
    pub configured: bool,
    pub installation_id: Option<String>,
    pub phase: ConnectorPhase,
    pub pause_reason: Option<&'static str>,
    pub can_resume: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectorStart {
    NotConfigured,
    Started,
    AlreadyRunning,
}

#[derive(Default)]
struct SupervisorState {
    active: bool,
    generation: u64,
    installation_id: Option<String>,
    phase: Option<ConnectorPhase>,
}

/// Owns the single outbound connector task for this UHC process. Pairing can
/// activate it immediately, while startup reconstructs the same state from the
/// owner-only persisted binding without relying on a package launcher.
#[derive(Clone, Default)]
pub struct ConnectorSupervisor {
    inner: std::sync::Arc<Mutex<SupervisorState>>,
    operation: std::sync::Arc<Mutex<()>>,
}

#[derive(Clone, Copy)]
struct ConnectionLifecycle<'a> {
    supervisor: &'a ConnectorSupervisor,
    generation: u64,
}

impl ConnectorSupervisor {
    pub async fn start_from_runtime(
        &self,
        state: crate::api::AppState,
        config_dir: impl Into<std::path::PathBuf>,
    ) -> anyhow::Result<ConnectorStart> {
        let _operation_guard = self.operation.lock().await;
        let Some(config) = CloudConnectorConfig::from_runtime(config_dir)? else {
            return Ok(ConnectorStart::NotConfigured);
        };
        let identity = InstallationIdentity::load(&config.key_path, config.installation_id.clone())
            .map_err(|error| {
                anyhow::anyhow!("paired HiPhi installation key is unavailable: {error}")
            })?;

        self.start_config(state, config, identity).await
    }

    pub async fn resume_from_runtime(
        &self,
        state: crate::api::AppState,
        config_dir: impl Into<std::path::PathBuf>,
    ) -> anyhow::Result<ConnectorStart> {
        let _operation_guard = self.operation.lock().await;
        anyhow::ensure!(
            !self.inner.lock().await.active,
            "Cloud connector is already running."
        );
        anyhow::ensure!(!state.shutdown.is_cancelled(), "UHC is shutting down.");
        let config = CloudConnectorConfig::from_runtime(config_dir)?
            .ok_or_else(|| anyhow::anyhow!("This installation is not paired."))?;
        // Validate identity and replay protection before changing containment.
        let identity =
            InstallationIdentity::load(&config.key_path, config.installation_id.clone())?;
        SessionEpochGuard::load(&config.epoch_path)?;
        super::safety::resume(&config.epoch_path, now_ms() as u64)?;
        self.start_config(state, config, identity).await
    }

    async fn start_config(
        &self,
        state: crate::api::AppState,
        config: CloudConnectorConfig,
        identity: InstallationIdentity,
    ) -> anyhow::Result<ConnectorStart> {
        let generation = {
            let mut inner = self.inner.lock().await;
            if inner.active {
                if inner.installation_id.as_deref() == Some(config.installation_id.as_str()) {
                    return Ok(ConnectorStart::AlreadyRunning);
                }
                anyhow::bail!("a different HiPhi connector task is already active");
            }
            inner.generation = inner.generation.saturating_add(1);
            inner.active = true;
            inner.installation_id = Some(config.installation_id.clone());
            inner.phase = Some(ConnectorPhase::Connecting);
            inner.generation
        };

        let supervisor = self.clone();
        tokio::spawn(async move {
            let final_phase = run(state, config, identity, supervisor.clone(), generation).await;
            supervisor.finish(generation, final_phase).await;
        });
        Ok(ConnectorStart::Started)
    }

    pub async fn status_from_runtime(
        &self,
        config_dir: impl Into<std::path::PathBuf>,
    ) -> anyhow::Result<ConnectorStatus> {
        let Some(config) = CloudConnectorConfig::from_runtime(config_dir)? else {
            return Ok(ConnectorStatus {
                configured: false,
                installation_id: None,
                phase: ConnectorPhase::Unconfigured,
                pause_reason: None,
                can_resume: false,
            });
        };
        let inner = self.inner.lock().await;
        let same_installation =
            inner.installation_id.as_deref() == Some(config.installation_id.as_str());
        let active = inner.active && same_installation;
        let mut phase = if same_installation {
            inner.phase.unwrap_or(ConnectorPhase::Offline)
        } else {
            ConnectorPhase::Offline
        };
        let mut pause_reason = if active {
            None
        } else if phase == ConnectorPhase::SafetyError {
            Some("safety_state_unavailable")
        } else {
            super::safety::pause_reason(&config.epoch_path)
        };
        if !active && SessionEpochGuard::load(&config.epoch_path).is_err() {
            pause_reason = Some("safety_state_unavailable");
        }
        if pause_reason.is_some() {
            phase = ConnectorPhase::Paused;
        }
        let can_resume = !active
            && pause_reason == Some("cost_limit")
            && super::safety::can_resume(&config.epoch_path, now_ms() as u64);
        Ok(ConnectorStatus {
            configured: true,
            installation_id: Some(config.installation_id),
            phase,
            pause_reason,
            can_resume,
        })
    }

    async fn set_phase(&self, generation: u64, phase: ConnectorPhase) {
        let mut inner = self.inner.lock().await;
        if inner.active && inner.generation == generation {
            inner.phase = Some(phase);
        }
    }

    async fn finish(&self, generation: u64, phase: ConnectorPhase) {
        let mut inner = self.inner.lock().await;
        if inner.generation == generation {
            inner.active = false;
            inner.phase = Some(phase);
        }
    }
}

async fn run(
    state: crate::api::AppState,
    config: CloudConnectorConfig,
    identity: InstallationIdentity,
    supervisor: ConnectorSupervisor,
    generation: u64,
) -> ConnectorPhase {
    let mut backoff = super::transport::Backoff::default();
    let shutdown = state.shutdown.clone();
    let mut store = StateStore::default();
    let mut epoch_guard = match SessionEpochGuard::load(&config.epoch_path) {
        Ok(guard) => guard,
        Err(error) => {
            tracing::error!("HiPhi Cloud epoch state is unavailable: {error}");
            return ConnectorPhase::SafetyError;
        }
    };
    let mut ledger = CommandLedger::default();
    let mut final_phase = ConnectorPhase::Offline;
    loop {
        supervisor
            .set_phase(generation, ConnectorPhase::Connecting)
            .await;
        let delay = backoff.next_delay();
        tokio::select! { _ = shutdown.cancelled() => break, _ = tokio::time::sleep(delay) => {} }
        match super::safety::admit_reconnect(&config.epoch_path, now_ms() as u64) {
            Ok(true) => {}
            result => {
                tracing::error!(event = "cloud_cost_quarantined", "HiPhi remote access stopped: reconnect budget or persisted safety state unavailable; inspect connector safety files before recovery ({result:?})");
                final_phase = if result.is_err() {
                    ConnectorPhase::SafetyError
                } else {
                    ConnectorPhase::Paused
                };
                break;
            }
        }
        let grant = match tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            grant = request_session_grant(&config, &identity) => grant,
        } {
            Ok(grant) => grant,
            Err(error) => {
                tracing::debug!("HiPhi Cloud session grant unavailable: {error}");
                continue;
            }
        };
        let verified_grant = match verify_installation_session_grant(
            &grant,
            &config.session_issuer_keys,
            &identity,
            config.endpoint.as_str(),
            now_ms(),
        ) {
            Ok(claims) => claims,
            Err(error) => {
                tracing::warn!("HiPhi Cloud returned an invalid session grant: {error}");
                continue;
            }
        };
        let websocket_limits = WebSocketConfig {
            max_message_size: Some(super::protocol::MAX_MESSAGE_BYTES),
            max_frame_size: Some(super::protocol::MAX_MESSAGE_BYTES),
            ..WebSocketConfig::default()
        };
        let request = match websocket_upgrade_request(config.endpoint.as_str(), &grant) {
            Ok(request) => request,
            Err(error) => {
                tracing::error!("HiPhi Cloud relay request is invalid: {error}");
                break;
            }
        };
        let connection = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            connection = tokio::time::timeout(
                CONNECT_TIMEOUT,
                connect_async_with_config(request, Some(websocket_limits), false),
            ) => connection,
        };
        match connection {
            Ok(Ok((socket, _))) => {
                match run_connection(
                    &state,
                    &config,
                    &identity,
                    &grant,
                    verified_grant.grant_generation,
                    &mut store,
                    &mut epoch_guard,
                    &mut ledger,
                    ConnectionLifecycle {
                        supervisor: &supervisor,
                        generation,
                    },
                    socket,
                )
                .await
                {
                    // Reset only after the authenticated session completed its
                    // challenge/proof ceremony.  A socket that accepts TCP
                    // and then rejects the protocol must still back off.
                    Ok(ConnectionExit::Disconnected) => {
                        supervisor
                            .set_phase(generation, ConnectorPhase::Offline)
                            .await;
                        backoff.reset();
                    }
                    Ok(ConnectionExit::Revoked) => {
                        final_phase = ConnectorPhase::Revoked;
                        break;
                    }
                    Ok(ConnectionExit::Shutdown) => break,
                    Err(error) => {
                        if error.is::<SafetyPersistenceFailure>() {
                            final_phase = ConnectorPhase::SafetyError;
                            break;
                        }
                        supervisor
                            .set_phase(generation, ConnectorPhase::Offline)
                            .await;
                        tracing::warn!("HiPhi Cloud relay disconnected: {error}");
                    }
                }
            }
            Ok(Err(error)) => tracing::debug!("HiPhi Cloud relay unavailable: {error}"),
            Err(_) => tracing::debug!("HiPhi Cloud relay connect timed out"),
        }
    }
    final_phase
}

async fn request_session_grant(
    config: &CloudConnectorConfig,
    identity: &InstallationIdentity,
) -> anyhow::Result<String> {
    let mut url = url::Url::parse(config.endpoint.as_str())?;
    url.set_scheme("https")
        .map_err(|_| anyhow::anyhow!("relay endpoint cannot use HTTPS"))?;
    url.set_path("/v1/relay/session-grant");
    url.set_query(None);
    let request = super::session::sign_installation_grant_request(
        identity,
        config.endpoint.as_str().to_owned(),
        now_ms(),
    );
    let client = session_grant_client()?;
    request_session_grant_at(&client, url, &request, config.endpoint.as_str()).await
}

fn session_grant_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()?)
}

async fn request_session_grant_at(
    client: &reqwest::Client,
    url: url::Url,
    request: &super::session::InstallationGrantRequest,
    expected_relay_endpoint: &str,
) -> anyhow::Result<String> {
    let expected_response_url = url.clone();
    let response = client.post(url).json(&request).send().await?;
    if response.url().scheme() != "https" && !cfg!(test) {
        anyhow::bail!("session grant response used a non-HTTPS endpoint");
    }
    if response.url() != &expected_response_url {
        anyhow::bail!("session grant response came from an unexpected endpoint");
    }
    if !response.status().is_success() {
        anyhow::bail!("session grant request returned {}", response.status());
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    if !matches!(content_type, Some(value) if value.eq_ignore_ascii_case("application/json")) {
        anyhow::bail!("session grant response was not JSON");
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_SESSION_GRANT_RESPONSE_BYTES as u64)
    {
        anyhow::bail!("session grant response exceeded its byte limit");
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if bytes.len().saturating_add(chunk.len()) > MAX_SESSION_GRANT_RESPONSE_BYTES {
            anyhow::bail!("session grant response exceeded its byte limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    let response: GrantResponse = serde_json::from_slice(&bytes)?;
    if response.endpoint != expected_relay_endpoint || response.expires_at <= 0 {
        anyhow::bail!("session grant response did not match the requested relay");
    }
    if !(32..=4096).contains(&response.grant.len()) {
        anyhow::bail!("relay returned an invalid session grant");
    }
    Ok(response.grant)
}

async fn run_connection<S>(
    state: &crate::api::AppState,
    config: &CloudConnectorConfig,
    identity: &InstallationIdentity,
    grant: &str,
    grant_generation: u64,
    store: &mut StateStore,
    epoch_guard: &mut SessionEpochGuard,
    ledger: &mut CommandLedger,
    lifecycle: ConnectionLifecycle<'_>,
    mut socket: tokio_tungstenite::WebSocketStream<S>,
) -> anyhow::Result<ConnectionExit>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if state.shutdown.is_cancelled() {
        close_socket_for_shutdown(&mut socket).await;
        return Ok(ConnectionExit::Shutdown);
    }
    let challenge = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        next_relay_message(&mut socket),
    )
    .await
    .map_err(|_| anyhow::anyhow!("relay challenge timed out"))??
    {
        Some(RelayMessage::Challenge(challenge))
            if challenge.protocol_version == PROTOCOL_VERSION =>
        {
            challenge
        }
        _ => anyhow::bail!("relay did not begin with a valid challenge"),
    };
    validate_challenge(&challenge, config.endpoint.as_str(), now_ms())?;
    // Private relay API expects the proof object itself, not a tagged envelope.
    let proof = sign_installation_session_proof(identity, grant.to_owned(), &challenge, now_ms());
    send_message(&mut socket, Message::Text(serde_json::to_string(&proof)?)).await?;
    let epoch = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        next_relay_message(&mut socket),
    )
    .await
    .map_err(|_| anyhow::anyhow!("relay proof response timed out"))??
    {
        Some(RelayMessage::SessionEstablished { epoch }) if epoch != 0 => epoch,
        _ => anyhow::bail!("relay rejected installation proof"),
    };
    epoch_guard.accept_at(epoch, now_ms().try_into().unwrap_or_default())?;

    let projection = snapshot_from_aggregator(
        state,
        store,
        config.installation_id.clone(),
        epoch,
        1,
        now_ms() as u64,
    )
    .await?;
    let hello = ConnectorMessage::Hello(ConnectorHello {
        protocol_version: PROTOCOL_VERSION,
        message_id: message_id(),
        connector_version: env!("UHC_VERSION").to_owned(),
        installation_id: config.installation_id.clone(),
        epoch,
        capabilities: vec![
            "zones.read".into(),
            "now_playing.read".into(),
            "transport.play_pause".into(),
            "transport.next".into(),
            "transport.previous".into(),
            "volume.up".into(),
            "volume.down".into(),
            "volume.absolute".into(),
            "artwork.read".into(),
        ],
    });
    send_json(&mut socket, &hello).await?;
    let snapshot = ConnectorMessage::Snapshot(StateSnapshot {
        protocol_version: PROTOCOL_VERSION,
        message_id: message_id(),
        installation_id: projection.installation_id.clone(),
        epoch: projection.epoch,
        revision: projection.revision,
        observed_at: projection.observed_at as i64,
        expires_at: projection.expires_at as i64,
        zones: projection.zones,
        now_playing: projection.now_playing,
    });
    send_json(&mut socket, &snapshot).await?;
    lifecycle
        .supervisor
        .set_phase(lifecycle.generation, ConnectorPhase::Online)
        .await;

    let mut verifier = CommandGrantVerifier::new(
        "hiphi-command-authorization",
        UHC_AUDIENCE,
        config.installation_id.clone(),
        epoch,
        grant_generation,
    );
    for (key_id, key) in config.command_issuer_keys.iter() {
        verifier.pin_key(key_id.to_owned(), *key);
    }
    let (artwork_tx, mut artwork_rx) = mpsc::channel(ARTWORK_QUEUE_CAPACITY);
    let artwork_slots =
        std::sync::Arc::new(Semaphore::new(ARTWORK_QUEUE_CAPACITY + ARTWORK_CONCURRENCY));
    let artwork_active = std::sync::Arc::new(Semaphore::new(ARTWORK_CONCURRENCY));
    let mut refresh = snapshot_refresh_interval();
    let mut watchdog_check = tokio::time::interval(PEER_HEARTBEAT_CHECK_INTERVAL);
    let mut heartbeat_step = 0u32;
    let heartbeat_check = tokio::time::sleep(super::safety::heartbeat_delay(heartbeat_step));
    tokio::pin!(heartbeat_check);
    let mut traffic = super::safety::TrafficBudget::default();
    let mut peer_watchdog = PeerWatchdog::new(now_ms() as u64, PEER_HEARTBEAT_TIMEOUT);
    refresh.tick().await;
    loop {
        let message = tokio::select! {
            biased;
            _ = state.shutdown.cancelled() => {
                close_socket_for_shutdown(&mut socket).await;
                return Ok(ConnectionExit::Shutdown);
            }
            _ = watchdog_check.tick() => {
                if peer_watchdog.expired(now_ms() as u64) { anyhow::bail!("relay heartbeat timed out"); }
                continue;
            }
            _ = &mut heartbeat_check => {
                if peer_watchdog.expired(now_ms() as u64) {
                    anyhow::bail!("relay heartbeat timed out");
                }
                send_json(&mut socket, &ConnectorMessage::Heartbeat { epoch, sent_at: now_ms() }).await?;
                heartbeat_step = heartbeat_step.saturating_add(1);
                heartbeat_check.as_mut().reset(tokio::time::Instant::now() + super::safety::heartbeat_delay(heartbeat_step));
                continue;
            }
            message = socket.next() => {
                let Some(message) = message else { break; };
                message?
            }
            _ = refresh.tick() => {
                let projection = snapshot_from_aggregator(state, store, config.installation_id.clone(), epoch, store.latest().map_or(1, |p| p.revision.saturating_add(1)), now_ms() as u64).await?;
                send_json(&mut socket, &ConnectorMessage::Snapshot(StateSnapshot { protocol_version: PROTOCOL_VERSION, message_id: message_id(), installation_id: projection.installation_id, epoch: projection.epoch, revision: projection.revision, observed_at: projection.observed_at as i64, expires_at: projection.expires_at as i64, zones: projection.zones, now_playing: projection.now_playing })).await?;
                continue;
            }
            artwork = artwork_rx.recv() => {
                if let Some(artwork) = artwork {
                    send_artwork_message(&mut socket, artwork).await?;
                    continue;
                }
                // All artwork workers have gone away; keep servicing the relay.
                continue;
            }
        };
        if state.shutdown.is_cancelled() {
            close_socket_for_shutdown(&mut socket).await;
            return Ok(ConnectionExit::Shutdown);
        }
        if !traffic.admit(now_ms() as u64, message.len()) {
            quarantine_connection(&mut socket, &config.epoch_path).await?;
            return Ok(ConnectionExit::Disconnected);
        }
        match message {
            Message::Text(text) if text.len() > MAX_COMMAND_BYTES => {
                anyhow::bail!("relay command frame exceeds the command bound");
            }
            Message::Text(text) => match super::protocol::parse_relay_message(text.as_bytes())
                .map_err(|error| anyhow::anyhow!(error))?
            {
                RelayMessage::Heartbeat {
                    epoch: message_epoch,
                    ..
                } if message_epoch == epoch => {
                    peer_watchdog.observe(now_ms() as u64);
                }
                RelayMessage::Command(command) => {
                    let request_id = command.request_id;
                    let now = now_ms();
                    let payload_hash = command.payload.canonical_hash().ok();
                    let outcome = match verifier.verify(&command, now) {
                        Ok(verified) => {
                            let Some(payload_hash) = payload_hash.as_deref() else {
                                return Err(anyhow::anyhow!("command payload hash failed"));
                            };
                            match ledger.lookup_command_at(
                                &command.idempotency_key.to_string(),
                                payload_hash,
                                now as u64,
                            ) {
                                Ok(Some(previous)) => previous,
                                Err(LedgerError::Conflict) => CommandOutcome::Forbidden,
                                Ok(None) if ledger.is_full() => CommandOutcome::Busy,
                                Ok(None) => {
                                    let outcome = dispatch(state, store, &verified.payload).await;
                                    if ledger
                                        .record_command_at(
                                            command.idempotency_key.to_string(),
                                            payload_hash,
                                            outcome,
                                            now as u64,
                                        )
                                        .is_err()
                                    {
                                        CommandOutcome::Busy
                                    } else {
                                        outcome
                                    }
                                }
                                Err(LedgerError::AtCapacity) => CommandOutcome::Busy,
                            }
                        }
                        Err(GrantError::Replayed) => {
                            match payload_hash.as_deref().and_then(|payload_hash| {
                                ledger
                                    .lookup_command_at(
                                        &command.idempotency_key.to_string(),
                                        payload_hash,
                                        now as u64,
                                    )
                                    .ok()
                                    .flatten()
                            }) {
                                Some(previous) => previous,
                                None => CommandOutcome::Forbidden,
                            }
                        }
                        Err(GrantError::AtCapacity) => CommandOutcome::Busy,
                        Err(GrantError::Expired) => CommandOutcome::Expired,
                        Err(error) => {
                            tracing::debug!("rejecting relay command: {error}");
                            CommandOutcome::Forbidden
                        }
                    };
                    send_command_result(
                        &mut socket,
                        config,
                        epoch,
                        request_id,
                        command.idempotency_key,
                        outcome,
                    )
                    .await?;
                }
                RelayMessage::ArtworkRequest(request) => {
                    if !traffic.artwork() {
                        quarantine_connection(&mut socket, &config.epoch_path).await?;
                        return Ok(ConnectionExit::Disconnected);
                    }
                    schedule_artwork(
                        state,
                        store,
                        config,
                        epoch,
                        request,
                        artwork_tx.clone(),
                        artwork_slots.clone(),
                        artwork_active.clone(),
                    );
                }
                RelayMessage::SpotifyCallback(callback) => {
                    if let Err(error) =
                        crate::api::provider_auth::accept_cloud_spotify_callback(state, callback)
                            .await
                    {
                        tracing::warn!("HiPhi Spotify callback was refused: {error}");
                    }
                }
                RelayMessage::Revoke { reason_code } => {
                    tracing::warn!("relay revoked connector: {reason_code}");
                    return Ok(ConnectionExit::Revoked);
                }
                _ => {}
            },
            Message::Ping(bytes) => {
                send_message(&mut socket, Message::Pong(bytes)).await?;
            }
            Message::Close(Some(ref frame)) if u16::from(frame.code) == 4008 => {
                quarantine_connection(&mut socket, &config.epoch_path).await?;
                return Ok(ConnectionExit::Disconnected);
            }
            Message::Close(_) => break,
            Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    }
    Ok(ConnectionExit::Disconnected)
}

/// A challenge is untrusted relay input.  Do not sign a nonce or endpoint
/// until both are bound to the configured WSS authority and the challenge is
/// still live.  This prevents a compromised/redirecting relay from turning
/// the installation key into a proof for another endpoint.
fn validate_challenge(
    challenge: &super::protocol::SessionChallengeMessage,
    configured_endpoint: &str,
    now_ms: i64,
) -> anyhow::Result<()> {
    if challenge.protocol_version != PROTOCOL_VERSION
        || challenge.nonce.is_empty()
        || challenge.nonce.len() > MAX_STRING_BYTES
        || challenge.expires_at <= now_ms
    {
        anyhow::bail!("relay challenge is invalid or expired");
    }
    let configured = RelayEndpoint::parse(configured_endpoint)
        .map_err(|_| anyhow::anyhow!("configured relay endpoint is invalid"))?;
    let challenged = RelayEndpoint::parse(&challenge.endpoint)
        .map_err(|_| anyhow::anyhow!("relay challenge endpoint is invalid"))?;
    if challenged != configured {
        anyhow::bail!("relay challenge endpoint does not match configured endpoint");
    }
    Ok(())
}

async fn next_relay_message<S>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
) -> anyhow::Result<Option<RelayMessage>>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    while let Some(message) = socket.next().await {
        match message? {
            Message::Text(text) => {
                return Ok(Some(
                    super::protocol::parse_relay_message(text.as_bytes())
                        .map_err(|error| anyhow::anyhow!(error))?,
                ))
            }
            Message::Ping(bytes) => {
                send_message(socket, Message::Pong(bytes)).await?;
            }
            Message::Close(_) => return Ok(None),
            _ => {}
        }
    }
    Ok(None)
}
async fn send_message<S>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
    message: Message,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::time::timeout(SOCKET_WRITE_TIMEOUT, socket.send(message))
        .await
        .map_err(|_| anyhow::anyhow!("relay write timed out"))??;
    Ok(())
}

async fn close_socket_for_shutdown<S>(socket: &mut tokio_tungstenite::WebSocketStream<S>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let close = Message::Close(Some(CloseFrame {
        code: CloseCode::Away,
        reason: "connector shutdown".into(),
    }));
    let _ = tokio::time::timeout(SHUTDOWN_CLOSE_TIMEOUT, socket.send(close)).await;
    let _ = tokio::time::timeout(SHUTDOWN_CLOSE_TIMEOUT, async {
        while let Some(message) = socket.next().await {
            if matches!(message, Ok(Message::Close(_)) | Err(_)) {
                break;
            }
        }
    })
    .await;
}
async fn send_artwork_message<S>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
    message: Message,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::time::timeout(ARTWORK_WRITE_TIMEOUT, socket.send(message))
        .await
        .map_err(|_| {
            anyhow::anyhow!("relay artwork write exceeded the control-priority bound")
        })??;
    Ok(())
}
async fn quarantine_connection<S>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
    epoch_path: &std::path::Path,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let persisted = super::safety::quarantine(epoch_path);
    tracing::error!(event = "cloud_cost_quarantined", "HiPhi remote traffic stopped; local playback remains available. Inspect the relay before removing the quarantine file.");
    let _ = tokio::time::timeout(
        SHUTDOWN_CLOSE_TIMEOUT,
        socket.send(Message::Close(Some(CloseFrame {
            code: CloseCode::Library(4008),
            reason: "cloud cost guard".into(),
        }))),
    )
    .await;
    // Failure to persist must still stop reconnects in this process.
    if let Err(error) = persisted {
        tracing::error!("Unable to persist cloud quarantine: {error}");
        anyhow::bail!(SafetyPersistenceFailure);
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("cloud quarantine persistence failed")]
struct SafetyPersistenceFailure;

async fn send_json<S, T: serde::Serialize>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
    value: &T,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    send_message(socket, Message::Text(serde_json::to_string(value)?)).await
}

async fn dispatch(
    state: &crate::api::AppState,
    store: &StateStore,
    payload: &super::protocol::CommandPayload,
) -> CommandOutcome {
    let now = u64::try_from(now_ms()).unwrap_or_default();
    let Some(epoch) = store.latest().map(|projection| projection.epoch) else {
        return CommandOutcome::StaleState;
    };
    if !store.is_fresh(epoch, now) {
        return CommandOutcome::StaleState;
    }
    let Some(provider_id) = store.provider_id(&payload.zone_handle) else {
        return CommandOutcome::Forbidden;
    };
    if !store.accepts_transport_action(&payload.zone_handle, &payload.action) {
        return CommandOutcome::Rejected;
    }
    let action = match payload.action {
        super::protocol::CommandAction::PlayPause => {
            if store.state(&payload.zone_handle) == Some("playing") {
                crate::mqtt::command::ParsedAction::Pause
            } else {
                crate::mqtt::command::ParsedAction::Play
            }
        }
        super::protocol::CommandAction::Next => crate::mqtt::command::ParsedAction::Next,
        super::protocol::CommandAction::Previous => crate::mqtt::command::ParsedAction::Previous,
        super::protocol::CommandAction::VolumeUp => {
            if !store.supports_relative_volume(&payload.zone_handle) {
                return CommandOutcome::Rejected;
            }
            crate::mqtt::command::ParsedAction::VolumeUp
        }
        super::protocol::CommandAction::VolumeDown => {
            if !store.supports_relative_volume(&payload.zone_handle) {
                return CommandOutcome::Rejected;
            }
            crate::mqtt::command::ParsedAction::VolumeDown
        }
        super::protocol::CommandAction::VolumeAbsolute { value } => {
            if !store.accepts_absolute_volume(&payload.zone_handle, value) {
                return CommandOutcome::Rejected;
            }
            crate::mqtt::command::ParsedAction::VolumeNative(value)
        }
    };
    match crate::mqtt::command::dispatch(
        &state.adapter_registry,
        &state.aggregator,
        state.reliable_commands.as_ref(),
        provider_id,
        action,
    )
    .await
    {
        crate::mqtt::command::DispatchOutcome::Sent => CommandOutcome::Executed,
        crate::mqtt::command::DispatchOutcome::Refused(reason) if reason.contains("busy") => {
            CommandOutcome::Busy
        }
        crate::mqtt::command::DispatchOutcome::Refused(_) => CommandOutcome::Rejected,
        crate::mqtt::command::DispatchOutcome::Unsupported(_) => CommandOutcome::Forbidden,
    }
}

async fn send_command_result<S>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
    config: &CloudConnectorConfig,
    epoch: u64,
    request_id: Uuid,
    idempotency_key: Uuid,
    outcome: CommandOutcome,
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    send_json(
        socket,
        &ConnectorMessage::CommandResult(CommandResult {
            protocol_version: PROTOCOL_VERSION,
            request_id,
            idempotency_key,
            status: map_outcome(outcome),
            reason_code: None,
            epoch,
            completed_at: now_ms(),
        }),
    )
    .await?;
    let _ = config;
    Ok(())
}
fn map_outcome(outcome: CommandOutcome) -> CommandStatus {
    match outcome {
        CommandOutcome::Executed => CommandStatus::Executed,
        CommandOutcome::Rejected => CommandStatus::Rejected,
        CommandOutcome::Expired => CommandStatus::Expired,
        CommandOutcome::Busy => CommandStatus::Busy,
        CommandOutcome::InstallationOffline => CommandStatus::InstallationOffline,
        CommandOutcome::Forbidden => CommandStatus::Forbidden,
        CommandOutcome::UnknownOutcome => CommandStatus::UnknownOutcome,
        CommandOutcome::StaleState => CommandStatus::StaleState,
    }
}

/// Schedule artwork away from the socket event loop. A relay can request
/// artwork frequently, but it must never make a slow provider image fetch
/// delay command verification or heartbeat processing. Ten bounded permits
/// represent two active fetches plus eight queued jobs; excess work is
/// intentionally dropped because artwork is decoration, not control state.
fn schedule_artwork(
    state: &crate::api::AppState,
    store: &StateStore,
    config: &CloudConnectorConfig,
    epoch: u64,
    request: ArtworkRelayRequest,
    artwork_tx: mpsc::Sender<Message>,
    artwork_slots: std::sync::Arc<Semaphore>,
    artwork_active: std::sync::Arc<Semaphore>,
) {
    if request.protocol_version != PROTOCOL_VERSION
        || request.installation_id != config.installation_id
        || request.epoch != epoch
        || request.max_source_bytes == 0
    {
        return;
    }
    let Some(provider_id) = store.provider_id(&request.zone_handle).map(str::to_owned) else {
        return;
    };
    let Some(image_key) = store
        .artwork_key(&request.zone_handle, &request.artwork_revision)
        .map(str::to_owned)
    else {
        return;
    };
    let Ok(slot) = artwork_slots.try_acquire_owned() else {
        return;
    };
    let limit = request.max_source_bytes.min(MAX_ARTWORK_SOURCE_BYTES);
    let state = state.clone();
    let installation_id = config.installation_id.clone();
    tokio::spawn(async move {
        let _slot = slot;
        let Ok(Ok(active)) =
            tokio::time::timeout(ARTWORK_FETCH_TIMEOUT, artwork_active.acquire_owned()).await
        else {
            return;
        };
        let Ok(Ok(image)) = tokio::time::timeout(
            ARTWORK_FETCH_TIMEOUT,
            state.get_image_bounded(&provider_id, &image_key, limit),
        )
        .await
        else {
            return;
        };
        // The connector bounds the source transfer. The private cloud artwork
        // service performs hostile-image validation and emits the smaller
        // Garmin representation, so the source cap is the relevant bound at
        // this side of the relay.
        if image.data.is_empty() || image.data.len() > limit {
            return;
        }
        drop(active);
        let Ok(count) = image
            .data
            .len()
            .div_ceil(MAX_ARTWORK_CHUNK_BYTES)
            .try_into()
        else {
            return;
        };
        let count: u16 = count;
        let response = ConnectorMessage::ArtworkResponse(ArtworkRelayResponse {
            protocol_version: PROTOCOL_VERSION,
            message_id: message_id(),
            installation_id,
            epoch,
            request_id: request.request_id,
            artwork_revision: request.artwork_revision,
            content_type: image.content_type,
            total_bytes: image.data.len(),
            chunk_count: count,
            sha256: hex::encode(Sha256::digest(&image.data)),
        });
        let Ok(text) = serde_json::to_string(&response) else {
            return;
        };
        if artwork_tx.send(Message::Text(text)).await.is_err() {
            return;
        }
        for (index, bytes) in image.data.chunks(MAX_ARTWORK_CHUNK_BYTES).enumerate() {
            let Ok(frame) = (ArtworkChunk {
                request_id: request.request_id,
                index: index.try_into().unwrap_or(u16::MAX),
                count,
                bytes: bytes.to_vec(),
            })
            .encode() else {
                return;
            };
            if artwork_tx.send(Message::Binary(frame)).await.is_err() {
                return;
            }
        }
    });
}
fn message_id() -> String {
    format!("msg_{}", Uuid::new_v4().simple())
}
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn websocket_upgrade_request(
    endpoint: &str,
    grant: &str,
) -> anyhow::Result<tokio_tungstenite::tungstenite::http::Request<()>> {
    let mut request = endpoint.into_client_request()?;
    request.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {grant}"))?,
    );
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::{
        request_session_grant_at, run_connection, session_grant_client, validate_challenge,
        websocket_upgrade_request, CommandLedger, ConnectionExit, ConnectionLifecycle,
        ConnectorSupervisor, RelayMessage, StateStore, MAX_SESSION_GRANT_RESPONSE_BYTES,
    };
    use crate::{
        adapters::{
            hqplayer::{HqpInstanceManager, HqpZoneLinkService},
            lms::LmsAdapter,
            openhome::OpenHomeAdapter,
            roon::RoonAdapter,
            upnp::UPnPAdapter,
            AdapterCommand, AdapterCommandResponse, AdapterContext, AdapterLogic,
        },
        aggregator::ZoneAggregator,
        api::AppState,
        bus::create_bus,
        cloud_connector::{
            config::{CloudConnectorConfig, IssuerVerifyingKeyRing},
            identity::InstallationIdentity,
            protocol::{
                CommandAction, CommandEnvelope, CommandGrantClaims, CommandPayload,
                ConnectorMessage, SessionChallengeMessage, PROTOCOL_VERSION,
            },
            state::{ControlEligibility, SemanticStateInput, SemanticZoneInput},
            transport::{RelayEndpoint, SessionEpochGuard},
            InstallationGrantRequest, InstallationSessionProof,
        },
        coordinator::AdapterCoordinator,
        knobs::KnobStore,
    };
    use base64::Engine as _;
    use ed25519_dalek::{Signer as _, SigningKey};
    use futures::{SinkExt as _, StreamExt as _};
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Instant,
    };
    use tokio_tungstenite::{
        tungstenite::{http::header::AUTHORIZATION, protocol::Role, Message},
        WebSocketStream,
    };
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    async fn serve_grant_app(app: axum::Router) -> url::Url {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        url::Url::parse(&format!("http://{address}/v1/relay/session-grant")).unwrap()
    }

    fn grant_request() -> InstallationGrantRequest {
        InstallationGrantRequest {
            installation_id: Uuid::from_u128(1).to_string(),
            request_id: Uuid::from_u128(2),
            endpoint: "wss://cloud.invalid/v1/relay/connect".into(),
            connector_version: env!("UHC_VERSION").into(),
            issued_at: 1_800_000_000_000,
            signature: "proof_012345678901234567890123456789".into(),
        }
    }

    fn signed_command(
        key_id: &str,
        key: &SigningKey,
        installation_id: &str,
        epoch: u64,
        generation: u64,
        now: i64,
    ) -> crate::cloud_connector::protocol::CommandEnvelope {
        let control_node_id = Uuid::new_v4();
        let request_id = Uuid::new_v4();
        let idempotency_key = Uuid::new_v4();
        let payload = CommandPayload {
            zone_handle: "zone_distinct_authority_test".into(),
            action: CommandAction::Next,
        };
        let claims = CommandGrantClaims {
            protocol_version: PROTOCOL_VERSION,
            issuer: "hiphi-command-authorization".into(),
            audience: super::UHC_AUDIENCE.into(),
            installation_id: installation_id.into(),
            control_node_id,
            request_id,
            idempotency_key,
            scope: payload.action.scope().into(),
            payload_sha256: payload.canonical_hash().unwrap(),
            epoch,
            issued_at: now - 100,
            expires_at: now + 5_000,
            grant_generation: generation,
        };
        let encoded_header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&serde_json::json!({
                "alg": "EdDSA",
                "kid": key_id,
            }))
            .unwrap(),
        );
        let encoded_claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap());
        let signature = key.sign(format!("{encoded_header}.{encoded_claims}").as_bytes());
        let grant = format!(
            "{encoded_header}.{encoded_claims}.{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.to_bytes())
        );
        CommandEnvelope {
            protocol_version: PROTOCOL_VERSION,
            installation_id: installation_id.into(),
            control_node_id,
            epoch,
            message_id: "message_distinct_authority_test".into(),
            request_id,
            idempotency_key,
            created_at: claims.issued_at,
            expires_at: claims.expires_at,
            payload,
            grant,
        }
    }

    async fn empty_app_state() -> AppState {
        let bus = create_bus();
        let roon = Arc::new(RoonAdapter::new_disconnected(bus.clone()));
        let hqp_instances = Arc::new(HqpInstanceManager::new(bus.clone()));
        let hqplayer = hqp_instances.get_default().await;
        let hqp_zone_links = Arc::new(HqpZoneLinkService::new(hqp_instances.clone()));
        let lms = Arc::new(LmsAdapter::new(bus.clone()));
        let openhome = Arc::new(OpenHomeAdapter::new(bus.clone()));
        let upnp = Arc::new(UPnPAdapter::new(bus.clone()));
        let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
        let coordinator = Arc::new(AdapterCoordinator::new(bus.clone()));
        AppState::new(
            roon,
            hqplayer,
            hqp_instances,
            hqp_zone_links,
            lms,
            openhome,
            upnp,
            KnobStore::new(),
            bus,
            aggregator,
            coordinator,
            vec![],
            Instant::now(),
            CancellationToken::new(),
        )
    }

    #[tokio::test(start_paused = true)]
    async fn missed_snapshot_ticks_do_not_burst_after_a_stall() {
        let mut refresh = super::snapshot_refresh_interval();
        refresh.tick().await;
        tokio::time::advance(std::time::Duration::from_secs(10 * 60)).await;
        refresh.tick().await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(1), refresh.tick())
                .await
                .is_err(),
            "missed snapshots must not catch up back-to-back"
        );
    }

    #[cfg(unix)]
    fn paired_directory() -> (tempfile::TempDir, String) {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let installation_id = Uuid::new_v4().to_string();
        let session_issuer = SigningKey::from_bytes(&[41; 32]);
        let command_issuer = SigningKey::from_bytes(&[42; 32]);
        let session_keys = serde_json::json!({
            "version": 1,
            "keys": [{
                "kid": "session-v1",
                "key": base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(session_issuer.verifying_key().to_bytes()),
            }],
        });
        let command_keys = serde_json::json!({
            "version": 1,
            "keys": [{
                "kid": "command-v1",
                "key": base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(command_issuer.verifying_key().to_bytes()),
            }],
        });
        let environment_path = directory.path().join("hiphi.env");
        std::fs::write(
            &environment_path,
            format!(
                "UHC_HIPHI_RELAY_URL=wss://cloud.invalid/v1/relay/connect\n\
                 UHC_HIPHI_INSTALLATION_ID={installation_id}\n\
                 UHC_HIPHI_SESSION_ISSUER_KEYS={session_keys}\n\
                 UHC_HIPHI_COMMAND_ISSUER_KEYS={command_keys}\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&environment_path, std::fs::Permissions::from_mode(0o600))
            .unwrap();

        (directory, installation_id)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn persisted_pairing_survives_restart_and_supervisor_is_duplicate_safe() {
        let (directory, installation_id) = paired_directory();
        let supervisor = ConnectorSupervisor::default();
        let status = supervisor
            .status_from_runtime(directory.path())
            .await
            .unwrap();
        assert!(status.configured);
        assert_eq!(
            status.installation_id.as_deref(),
            Some(installation_id.as_str())
        );
        assert_eq!(status.phase, super::ConnectorPhase::Offline);

        let state = empty_app_state().await;
        assert!(
            supervisor
                .start_from_runtime(state.clone(), directory.path())
                .await
                .is_err(),
            "a persisted binding must not replace a missing private key"
        );

        let identity = InstallationIdentity::generate(installation_id.clone()).unwrap();
        identity
            .save(&directory.path().join("hiphi-installation.key"))
            .unwrap();
        assert_eq!(
            supervisor
                .start_from_runtime(state.clone(), directory.path())
                .await
                .unwrap(),
            super::ConnectorStart::Started
        );
        assert_eq!(
            supervisor
                .start_from_runtime(state.clone(), directory.path())
                .await
                .unwrap(),
            super::ConnectorStart::AlreadyRunning
        );

        state.shutdown.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if !supervisor.inner.lock().await.active {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("connector should stop on process shutdown");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn recovery_serializes_clicks_preserves_identity_and_exposes_terminal_pause() {
        let (directory, installation_id) = paired_directory();
        let key_path = directory.path().join("hiphi-installation.key");
        InstallationIdentity::generate(installation_id)
            .unwrap()
            .save(&key_path)
            .unwrap();
        let key_before = std::fs::read(&key_path).unwrap();
        let epoch = directory.path().join("hiphi-relay-epoch");
        let now = super::now_ms() as u64;
        super::SessionEpochGuard::load(&epoch)
            .unwrap()
            .accept_at(now, now)
            .unwrap();
        super::super::safety::quarantine(&epoch).unwrap();
        let supervisor = ConnectorSupervisor::default();
        let status = supervisor
            .status_from_runtime(directory.path())
            .await
            .unwrap();
        assert_eq!(status.phase.as_str(), "paused");
        assert_eq!(status.pause_reason, Some("cost_limit"));
        assert!(status.can_resume);
        let state = empty_app_state().await;
        supervisor
            .start_from_runtime(state.clone(), directory.path())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while supervisor.inner.lock().await.active {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let stopped = supervisor
            .status_from_runtime(directory.path())
            .await
            .unwrap();
        assert_eq!(stopped.phase.as_str(), "paused");
        assert_eq!(stopped.pause_reason, Some("cost_limit"));
        let (first, second) = tokio::join!(
            supervisor.resume_from_runtime(state.clone(), directory.path()),
            supervisor.resume_from_runtime(state.clone(), directory.path()),
        );
        assert_ne!(
            first.is_ok(),
            second.is_ok(),
            "only one click may reset the budget"
        );
        assert_eq!(supervisor.inner.lock().await.generation, 2);
        assert_eq!(std::fs::read(&key_path).unwrap(), key_before);
        assert!(
            !state.shutdown.is_cancelled(),
            "recovery must not stop local playback"
        );
        assert!(supervisor.inner.lock().await.active);
        let mut replay = super::SessionEpochGuard::load(&epoch).unwrap();
        assert_eq!(replay.last(), now);
        assert!(
            replay.accept_at(now, now).is_err(),
            "resume must not allow replay"
        );
        state.shutdown.cancel();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn recovery_rejects_damaged_replay_state_without_clearing_quarantine() {
        let (directory, installation_id) = paired_directory();
        InstallationIdentity::generate(installation_id)
            .unwrap()
            .save(&directory.path().join("hiphi-installation.key"))
            .unwrap();
        let epoch = directory.path().join("hiphi-relay-epoch");
        std::fs::write(&epoch, "invalid replay state").unwrap();
        super::super::safety::quarantine(&epoch).unwrap();
        let supervisor = ConnectorSupervisor::default();
        let state = empty_app_state().await;
        assert!(supervisor
            .resume_from_runtime(state.clone(), directory.path())
            .await
            .is_err());
        assert!(epoch.with_extension("quarantine").exists());
        assert!(!epoch.with_extension("resume").exists());
        assert!(!supervisor.inner.lock().await.active);
        state.shutdown.cancel();
    }

    #[derive(Default)]
    struct RecordingSpotifyAdapter {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl AdapterLogic for RecordingSpotifyAdapter {
        fn prefix(&self) -> &'static str {
            "spotify"
        }

        async fn run(&self, _ctx: AdapterContext) -> anyhow::Result<()> {
            Ok(())
        }

        async fn handle_command(
            &self,
            _zone_id: &str,
            _command: AdapterCommand,
        ) -> anyhow::Result<AdapterCommandResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(AdapterCommandResponse {
                success: true,
                error: None,
            })
        }
    }

    fn challenge(endpoint: &str, expires_at: i64) -> SessionChallengeMessage {
        SessionChallengeMessage {
            protocol_version: 1,
            challenge_id: Uuid::from_u128(1),
            endpoint: endpoint.to_owned(),
            nonce: "nonce_012345678901234567890123456789".to_owned(),
            expires_at,
        }
    }

    #[tokio::test]
    async fn volume_disabled_spotify_zone_is_rejected_before_registry_dispatch() {
        let state = empty_app_state().await;
        let spotify = Arc::new(RecordingSpotifyAdapter::default());
        state.adapter_registry.register(spotify.clone()).await;

        let mut store = StateStore::default();
        let projection = store
            .snapshot(SemanticStateInput {
                installation_id: Uuid::new_v4().to_string(),
                epoch: 7,
                revision: 1,
                observed_at: u64::try_from(super::now_ms()).unwrap(),
                expires_at: u64::try_from(super::now_ms() + 30_000).unwrap(),
                zones: vec![SemanticZoneInput {
                    provider_id: "spotify:private-device-id".into(),
                    name: "Spotify without volume".into(),
                    state: "paused".into(),
                    control: ControlEligibility::all(),
                    volume: None,
                    now_playing: None,
                }],
            })
            .unwrap();
        let payload = CommandPayload {
            zone_handle: projection.zones[0].zone_handle.clone(),
            action: CommandAction::VolumeUp,
        };

        assert_eq!(
            super::dispatch(&state, &store, &payload).await,
            super::CommandOutcome::Rejected
        );
        assert_eq!(spotify.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn restricted_spotify_zone_is_rejected_before_registry_dispatch() {
        let state = empty_app_state().await;
        let spotify = Arc::new(RecordingSpotifyAdapter::default());
        state.adapter_registry.register(spotify.clone()).await;

        let mut store = StateStore::default();
        let projection = store
            .snapshot(SemanticStateInput {
                installation_id: Uuid::new_v4().to_string(),
                epoch: 7,
                revision: 1,
                observed_at: u64::try_from(super::now_ms()).unwrap(),
                expires_at: u64::try_from(super::now_ms() + 30_000).unwrap(),
                zones: vec![SemanticZoneInput {
                    provider_id: "spotify:private-restricted-device-id".into(),
                    name: "Restricted Spotify device".into(),
                    state: "paused".into(),
                    control: ControlEligibility {
                        play: false,
                        pause: false,
                        next: false,
                        previous: false,
                    },
                    volume: None,
                    now_playing: None,
                }],
            })
            .unwrap();
        let payload = CommandPayload {
            zone_handle: projection.zones[0].zone_handle.clone(),
            action: CommandAction::Next,
        };

        assert_eq!(
            super::dispatch(&state, &store, &payload).await,
            super::CommandOutcome::Rejected
        );
        assert_eq!(spotify.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn truthful_spotify_actions_dispatch_only_through_registry() {
        let state = empty_app_state().await;
        let spotify = Arc::new(RecordingSpotifyAdapter::default());
        state.adapter_registry.register(spotify.clone()).await;

        let mut store = StateStore::default();
        let projection = store
            .snapshot(SemanticStateInput {
                installation_id: Uuid::new_v4().to_string(),
                epoch: 7,
                revision: 1,
                observed_at: u64::try_from(super::now_ms()).unwrap(),
                expires_at: u64::try_from(super::now_ms() + 30_000).unwrap(),
                zones: vec![SemanticZoneInput {
                    provider_id: "spotify:private-device-id".into(),
                    name: "Spotify with volume".into(),
                    state: "paused".into(),
                    control: ControlEligibility::all(),
                    volume: Some(super::super::state::VolumeInput {
                        value: 42.0,
                        min: 0.0,
                        max: 100.0,
                        step: 1.0,
                        scale: "percent".into(),
                    }),
                    now_playing: None,
                }],
            })
            .unwrap();
        let handle = projection.zones[0].zone_handle.clone();

        for action in [CommandAction::Next, CommandAction::VolumeUp] {
            assert_eq!(
                super::dispatch(
                    &state,
                    &store,
                    &CommandPayload {
                        zone_handle: handle.clone(),
                        action,
                    },
                )
                .await,
                super::CommandOutcome::Executed
            );
        }
        assert_eq!(spotify.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn websocket_upgrade_carries_authority_only_in_the_header() {
        let request = websocket_upgrade_request(
            "wss://cloud.example/v1/relay/connect",
            "signed.session.grant",
        )
        .unwrap();
        assert_eq!(request.uri(), "wss://cloud.example/v1/relay/connect");
        assert_eq!(
            request.headers().get(AUTHORIZATION).unwrap(),
            "Bearer signed.session.grant"
        );
        assert!(!request.uri().to_string().contains("grant"));
        assert!(websocket_upgrade_request(
            "wss://cloud.example/v1/relay/connect",
            "invalid\r\ngrant"
        )
        .is_err());
    }

    #[tokio::test]
    async fn session_grant_http_boundary_accepts_only_bounded_exact_json() {
        use axum::{response::Redirect, routing::post, Json, Router};

        let relay_endpoint = "wss://cloud.invalid/v1/relay/connect";
        let grant = "g".repeat(64);
        let success_grant = grant.clone();
        let success_endpoint = relay_endpoint.to_owned();
        let success_url = serve_grant_app(Router::new().route(
            "/v1/relay/session-grant",
            post(move || {
                let grant = success_grant.clone();
                let endpoint = success_endpoint.clone();
                async move {
                    Json(serde_json::json!({
                        "grant": grant,
                        "endpoint": endpoint,
                        "expires_at": 1_800_000_060_000_i64,
                    }))
                }
            }),
        ))
        .await;
        let accepted = request_session_grant_at(
            &session_grant_client().unwrap(),
            success_url,
            &grant_request(),
            relay_endpoint,
        )
        .await
        .unwrap();
        assert_eq!(accepted, grant);

        let redirect_url = serve_grant_app(
            Router::new()
                .route(
                    "/v1/relay/session-grant",
                    post(|| async { Redirect::temporary("/redirected") }),
                )
                .route(
                    "/redirected",
                    post(|| async { Json(serde_json::json!({"grant":"should-not-be-read"})) }),
                ),
        )
        .await;
        let redirect_error = request_session_grant_at(
            &session_grant_client().unwrap(),
            redirect_url.clone(),
            &grant_request(),
            relay_endpoint,
        )
        .await
        .unwrap_err();
        assert!(redirect_error.to_string().contains("307"));
        let followed_redirect_error = request_session_grant_at(
            &reqwest::Client::new(),
            redirect_url,
            &grant_request(),
            relay_endpoint,
        )
        .await
        .unwrap_err();
        assert!(followed_redirect_error
            .to_string()
            .contains("unexpected endpoint"));

        let wrong_type_url = serve_grant_app(Router::new().route(
            "/v1/relay/session-grant",
            post(|| async { ([("content-type", "text/plain")], "{}") }),
        ))
        .await;
        let wrong_type_error = request_session_grant_at(
            &session_grant_client().unwrap(),
            wrong_type_url,
            &grant_request(),
            relay_endpoint,
        )
        .await
        .unwrap_err();
        assert!(wrong_type_error.to_string().contains("not JSON"));

        let oversized = "x".repeat(MAX_SESSION_GRANT_RESPONSE_BYTES + 1);
        let oversized_url = serve_grant_app(Router::new().route(
            "/v1/relay/session-grant",
            post(move || {
                let oversized = oversized.clone();
                async move { ([("content-type", "application/json")], oversized) }
            }),
        ))
        .await;
        let oversized_error = request_session_grant_at(
            &session_grant_client().unwrap(),
            oversized_url,
            &grant_request(),
            relay_endpoint,
        )
        .await
        .unwrap_err();
        assert!(oversized_error.to_string().contains("byte limit"));

        let unexpected_url = serve_grant_app(Router::new().route(
            "/v1/relay/session-grant",
            post(|| async {
                Json(serde_json::json!({
                    "grant": "g".repeat(64),
                    "endpoint": "wss://cloud.invalid/v1/relay/connect",
                    "expires_at": 1_800_000_060_000_i64,
                    "extra": true,
                }))
            }),
        ))
        .await;
        let unexpected_error = request_session_grant_at(
            &session_grant_client().unwrap(),
            unexpected_url,
            &grant_request(),
            relay_endpoint,
        )
        .await
        .unwrap_err();
        assert!(unexpected_error.to_string().contains("unknown field"));
    }

    #[test]
    fn challenge_must_match_configured_authority_and_be_live() {
        let good = challenge("wss://cloud.invalid/v1/relay", 2_000);
        assert!(validate_challenge(&good, "wss://cloud.invalid/v1/relay/", 1_000).is_ok());
        assert!(validate_challenge(
            &challenge("wss://attacker.invalid/v1/relay", 2_000),
            "wss://cloud.invalid/v1/relay",
            1_000
        )
        .is_err());
        assert!(validate_challenge(
            &challenge("wss://cloud.invalid/v1/relay", 1_000),
            "wss://cloud.invalid/v1/relay",
            1_000
        )
        .is_err());
    }

    // Keep the protocol import in this module tied to the exact tagged relay
    // family: an accidental legacy envelope cannot reach the runtime.
    #[test]
    fn runtime_accepts_only_typed_relay_messages() {
        let raw = serde_json::json!({"type":"command","body":{}});
        assert!(serde_json::from_value::<RelayMessage>(raw).is_err());
    }

    #[tokio::test]
    async fn production_connection_uses_distinct_command_authority_then_honors_revocation() {
        let installation_id = Uuid::new_v4().to_string();
        let identity = InstallationIdentity::generate(installation_id.clone()).unwrap();
        let previous_session_issuer = SigningKey::from_bytes(&[28; 32]);
        let session_issuer = SigningKey::from_bytes(&[30; 32]);
        let previous_command_issuer = SigningKey::from_bytes(&[29; 32]);
        let command_issuer = SigningKey::from_bytes(&[31; 32]);
        let temp = tempfile::tempdir().unwrap();
        let endpoint = "wss://cloud.invalid/v1/relay/connect";
        let config = CloudConnectorConfig {
            endpoint: RelayEndpoint::parse(endpoint).unwrap(),
            installation_id: installation_id.clone(),
            key_path: temp.path().join("installation.key"),
            epoch_path: temp.path().join("epoch"),
            session_issuer_keys: IssuerVerifyingKeyRing::from_entries([
                (
                    "session-issuer-1".into(),
                    previous_session_issuer.verifying_key(),
                ),
                ("session-issuer-2".into(), session_issuer.verifying_key()),
            ])
            .unwrap(),
            command_issuer_keys: IssuerVerifyingKeyRing::from_entries([
                (
                    "command-issuer-1".into(),
                    previous_command_issuer.verifying_key(),
                ),
                ("command-issuer-2".into(), command_issuer.verifying_key()),
            ])
            .unwrap(),
        };
        let state = empty_app_state().await;
        let (client_io, server_io) = tokio::io::duplex(128 * 1024);
        let client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
        let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
        let now = super::now_ms();
        let challenge = RelayMessage::Challenge(SessionChallengeMessage {
            protocol_version: PROTOCOL_VERSION,
            challenge_id: Uuid::new_v4(),
            endpoint: endpoint.into(),
            nonce: "nonce_012345678901234567890123456789".into(),
            expires_at: now + 30_000,
        });
        let epoch = u64::try_from(now).unwrap();
        let command = signed_command(
            "command-issuer-2",
            &command_issuer,
            &installation_id,
            epoch,
            0,
            now,
        );
        let server_task = tokio::spawn(async move {
            server
                .send(Message::Text(
                    serde_json::to_string(&challenge).unwrap().into(),
                ))
                .await
                .unwrap();
            let Message::Text(proof) = server.next().await.unwrap().unwrap() else {
                panic!("expected installation proof")
            };
            let proof: InstallationSessionProof = serde_json::from_str(&proof).unwrap();
            assert_eq!(proof.grant, "test.session.grant");
            server
                .send(Message::Text(
                    serde_json::to_string(&RelayMessage::SessionEstablished { epoch })
                        .unwrap()
                        .into(),
                ))
                .await
                .unwrap();
            let Message::Text(hello) = server.next().await.unwrap().unwrap() else {
                panic!("expected connector hello")
            };
            let hello: ConnectorMessage = serde_json::from_str(&hello).unwrap();
            assert!(matches!(hello, ConnectorMessage::Hello(ref value)
                if value.installation_id == installation_id
                    && value.connector_version == env!("UHC_VERSION")));
            let Message::Text(snapshot) = server.next().await.unwrap().unwrap() else {
                panic!("expected initial snapshot")
            };
            let snapshot: ConnectorMessage = serde_json::from_str(&snapshot).unwrap();
            assert!(matches!(snapshot, ConnectorMessage::Snapshot(ref value)
                if value.installation_id == installation_id && value.epoch == epoch));
            server
                .send(Message::Text(
                    serde_json::to_string(&RelayMessage::Command(command))
                        .unwrap()
                        .into(),
                ))
                .await
                .unwrap();
            let Message::Text(result) = server.next().await.unwrap().unwrap() else {
                panic!("expected command result")
            };
            let result: ConnectorMessage = serde_json::from_str(&result).unwrap();
            assert!(matches!(result, ConnectorMessage::CommandResult(_)));
            server
                .send(Message::Text(
                    serde_json::to_string(&RelayMessage::Revoke {
                        reason_code: "owner_revoked".into(),
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
        });
        let mut store = StateStore::default();
        let mut epoch_guard = SessionEpochGuard::load(&config.epoch_path).unwrap();
        let mut ledger = CommandLedger::default();
        let supervisor = ConnectorSupervisor::default();
        let exit = run_connection(
            &state,
            &config,
            &identity,
            "test.session.grant",
            0,
            &mut store,
            &mut epoch_guard,
            &mut ledger,
            ConnectionLifecycle {
                supervisor: &supervisor,
                generation: 1,
            },
            client,
        )
        .await
        .unwrap();
        assert_eq!(exit, ConnectionExit::Revoked);
        assert_eq!(ledger.len(), 1, "the command authority grant was accepted");
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn received_heartbeat_does_not_create_an_echo_loop() {
        let installation_id = Uuid::new_v4().to_string();
        let identity = InstallationIdentity::generate(installation_id.clone()).unwrap();
        let previous_session_issuer = SigningKey::from_bytes(&[28; 32]);
        let session_issuer = SigningKey::from_bytes(&[30; 32]);
        let previous_command_issuer = SigningKey::from_bytes(&[29; 32]);
        let command_issuer = SigningKey::from_bytes(&[31; 32]);
        let temp = tempfile::tempdir().unwrap();
        let endpoint = "wss://cloud.invalid/v1/relay/connect";
        let config = CloudConnectorConfig {
            endpoint: RelayEndpoint::parse(endpoint).unwrap(),
            installation_id: installation_id.clone(),
            key_path: temp.path().join("installation.key"),
            epoch_path: temp.path().join("epoch"),
            session_issuer_keys: IssuerVerifyingKeyRing::from_entries([
                (
                    "session-issuer-1".into(),
                    previous_session_issuer.verifying_key(),
                ),
                ("session-issuer-2".into(), session_issuer.verifying_key()),
            ])
            .unwrap(),
            command_issuer_keys: IssuerVerifyingKeyRing::from_entries([
                (
                    "command-issuer-1".into(),
                    previous_command_issuer.verifying_key(),
                ),
                ("command-issuer-2".into(), command_issuer.verifying_key()),
            ])
            .unwrap(),
        };
        let state = empty_app_state().await;
        let (client_io, server_io) = tokio::io::duplex(128 * 1024);
        let client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
        let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
        let now = super::now_ms();
        let challenge = RelayMessage::Challenge(SessionChallengeMessage {
            protocol_version: PROTOCOL_VERSION,
            challenge_id: Uuid::new_v4(),
            endpoint: endpoint.into(),
            nonce: "nonce_012345678901234567890123456789".into(),
            expires_at: now + 30_000,
        });
        let epoch = u64::try_from(now).unwrap();
        let command = signed_command(
            "command-issuer-2",
            &command_issuer,
            &installation_id,
            epoch,
            0,
            now,
        );
        let server_task = tokio::spawn(async move {
            server
                .send(Message::Text(
                    serde_json::to_string(&challenge).unwrap().into(),
                ))
                .await
                .unwrap();
            let Message::Text(proof) = server.next().await.unwrap().unwrap() else {
                panic!("expected installation proof")
            };
            let proof: InstallationSessionProof = serde_json::from_str(&proof).unwrap();
            assert_eq!(proof.grant, "test.session.grant");
            server
                .send(Message::Text(
                    serde_json::to_string(&RelayMessage::SessionEstablished { epoch })
                        .unwrap()
                        .into(),
                ))
                .await
                .unwrap();
            let Message::Text(hello) = server.next().await.unwrap().unwrap() else {
                panic!("expected connector hello")
            };
            let hello: ConnectorMessage = serde_json::from_str(&hello).unwrap();
            assert!(matches!(hello, ConnectorMessage::Hello(ref value)
                if value.installation_id == installation_id
                    && value.connector_version == env!("UHC_VERSION")));
            let Message::Text(snapshot) = server.next().await.unwrap().unwrap() else {
                panic!("expected initial snapshot")
            };
            let snapshot: ConnectorMessage = serde_json::from_str(&snapshot).unwrap();
            assert!(matches!(snapshot, ConnectorMessage::Snapshot(ref value)
                if value.installation_id == installation_id && value.epoch == epoch));
            server
                .send(Message::Text(
                    serde_json::to_string(&RelayMessage::Heartbeat {
                        epoch,
                        sent_at: super::now_ms(),
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(100), server.next())
                    .await
                    .is_err(),
                "receiving a heartbeat must not immediately echo another heartbeat"
            );
            server
                .send(Message::Text(
                    serde_json::to_string(&RelayMessage::Command(command))
                        .unwrap()
                        .into(),
                ))
                .await
                .unwrap();
            let Message::Text(result) = server.next().await.unwrap().unwrap() else {
                panic!("expected command result")
            };
            let result: ConnectorMessage = serde_json::from_str(&result).unwrap();
            assert!(matches!(result, ConnectorMessage::CommandResult(_)));
            server
                .send(Message::Text(
                    serde_json::to_string(&RelayMessage::Revoke {
                        reason_code: "owner_revoked".into(),
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
        });
        let mut store = StateStore::default();
        let mut epoch_guard = SessionEpochGuard::load(&config.epoch_path).unwrap();
        let mut ledger = CommandLedger::default();
        let supervisor = ConnectorSupervisor::default();
        let exit = run_connection(
            &state,
            &config,
            &identity,
            "test.session.grant",
            0,
            &mut store,
            &mut epoch_guard,
            &mut ledger,
            ConnectionLifecycle {
                supervisor: &supervisor,
                generation: 1,
            },
            client,
        )
        .await
        .unwrap();
        assert_eq!(exit, ConnectionExit::Revoked);
        assert_eq!(ledger.len(), 1, "the command authority grant was accepted");
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn relay_flood_is_quarantined_before_processing_unbounded_messages() {
        let installation_id = Uuid::new_v4().to_string();
        let identity = InstallationIdentity::generate(installation_id.clone()).unwrap();
        let previous_session_issuer = SigningKey::from_bytes(&[28; 32]);
        let session_issuer = SigningKey::from_bytes(&[30; 32]);
        let previous_command_issuer = SigningKey::from_bytes(&[29; 32]);
        let command_issuer = SigningKey::from_bytes(&[31; 32]);
        let temp = tempfile::tempdir().unwrap();
        let endpoint = "wss://cloud.invalid/v1/relay/connect";
        let config = CloudConnectorConfig {
            endpoint: RelayEndpoint::parse(endpoint).unwrap(),
            installation_id: installation_id.clone(),
            key_path: temp.path().join("installation.key"),
            epoch_path: temp.path().join("epoch"),
            session_issuer_keys: IssuerVerifyingKeyRing::from_entries([
                (
                    "session-issuer-1".into(),
                    previous_session_issuer.verifying_key(),
                ),
                ("session-issuer-2".into(), session_issuer.verifying_key()),
            ])
            .unwrap(),
            command_issuer_keys: IssuerVerifyingKeyRing::from_entries([
                (
                    "command-issuer-1".into(),
                    previous_command_issuer.verifying_key(),
                ),
                ("command-issuer-2".into(), command_issuer.verifying_key()),
            ])
            .unwrap(),
        };
        let state = empty_app_state().await;
        let (client_io, server_io) = tokio::io::duplex(128 * 1024);
        let client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
        let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
        let now = super::now_ms();
        let challenge = RelayMessage::Challenge(SessionChallengeMessage {
            protocol_version: PROTOCOL_VERSION,
            challenge_id: Uuid::new_v4(),
            endpoint: endpoint.into(),
            nonce: "nonce_012345678901234567890123456789".into(),
            expires_at: now + 30_000,
        });
        let epoch = u64::try_from(now).unwrap();
        let _command = signed_command(
            "command-issuer-2",
            &command_issuer,
            &installation_id,
            epoch,
            0,
            now,
        );
        let server_task = tokio::spawn(async move {
            server
                .send(Message::Text(
                    serde_json::to_string(&challenge).unwrap().into(),
                ))
                .await
                .unwrap();
            let Message::Text(proof) = server.next().await.unwrap().unwrap() else {
                panic!("expected installation proof")
            };
            let proof: InstallationSessionProof = serde_json::from_str(&proof).unwrap();
            assert_eq!(proof.grant, "test.session.grant");
            server
                .send(Message::Text(
                    serde_json::to_string(&RelayMessage::SessionEstablished { epoch })
                        .unwrap()
                        .into(),
                ))
                .await
                .unwrap();
            let Message::Text(hello) = server.next().await.unwrap().unwrap() else {
                panic!("expected connector hello")
            };
            let hello: ConnectorMessage = serde_json::from_str(&hello).unwrap();
            assert!(matches!(hello, ConnectorMessage::Hello(ref value)
                if value.installation_id == installation_id
                    && value.connector_version == env!("UHC_VERSION")));
            let Message::Text(snapshot) = server.next().await.unwrap().unwrap() else {
                panic!("expected initial snapshot")
            };
            let snapshot: ConnectorMessage = serde_json::from_str(&snapshot).unwrap();
            assert!(matches!(snapshot, ConnectorMessage::Snapshot(ref value)
                if value.installation_id == installation_id && value.epoch == epoch));
            // Buffer one burst before flushing; closure during the burst is the expected outcome.
            for _ in 0..200 {
                server
                    .feed(Message::Text(
                        serde_json::to_string(&RelayMessage::Heartbeat {
                            epoch,
                            sent_at: super::now_ms(),
                        })
                        .unwrap()
                        .into(),
                    ))
                    .await
                    .unwrap();
            }
            server.flush().await.unwrap();
            let reply = tokio::time::timeout(std::time::Duration::from_secs(1), server.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(
                matches!(reply, Message::Close(Some(ref frame)) if u16::from(frame.code) == 4008),
                "a flood must close and quarantine the session, got {reply:?}"
            );
        });
        let mut store = StateStore::default();
        let mut epoch_guard = SessionEpochGuard::load(&config.epoch_path).unwrap();
        let mut ledger = CommandLedger::default();
        let supervisor = ConnectorSupervisor::default();
        let exit = run_connection(
            &state,
            &config,
            &identity,
            "test.session.grant",
            0,
            &mut store,
            &mut epoch_guard,
            &mut ledger,
            ConnectionLifecycle {
                supervisor: &supervisor,
                generation: 1,
            },
            client,
        )
        .await
        .unwrap();
        assert!(config.epoch_path.with_extension("quarantine").exists());
        assert_eq!(exit, ConnectionExit::Disconnected);
        assert_eq!(ledger.len(), 0);
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn authenticated_connection_closes_without_executing_or_reconnecting_on_shutdown() {
        let installation_id = Uuid::new_v4().to_string();
        let identity = InstallationIdentity::generate(installation_id.clone()).unwrap();
        let session_issuer = SigningKey::from_bytes(&[50; 32]);
        let command_issuer = SigningKey::from_bytes(&[51; 32]);
        let temp = tempfile::tempdir().unwrap();
        let endpoint = "wss://cloud.invalid/v1/relay/connect";
        let config = CloudConnectorConfig {
            endpoint: RelayEndpoint::parse(endpoint).unwrap(),
            installation_id: installation_id.clone(),
            key_path: temp.path().join("installation.key"),
            epoch_path: temp.path().join("epoch"),
            session_issuer_keys: IssuerVerifyingKeyRing::from_entries([(
                "session-issuer-1".into(),
                session_issuer.verifying_key(),
            )])
            .unwrap(),
            command_issuer_keys: IssuerVerifyingKeyRing::from_entries([(
                "command-issuer-1".into(),
                command_issuer.verifying_key(),
            )])
            .unwrap(),
        };
        let state = empty_app_state().await;
        let shutdown = state.shutdown.clone();
        let (client_io, server_io) = tokio::io::duplex(128 * 1024);
        let client = WebSocketStream::from_raw_socket(client_io, Role::Client, None).await;
        let mut server = WebSocketStream::from_raw_socket(server_io, Role::Server, None).await;
        let now = super::now_ms();
        let challenge = RelayMessage::Challenge(SessionChallengeMessage {
            protocol_version: PROTOCOL_VERSION,
            challenge_id: Uuid::new_v4(),
            endpoint: endpoint.into(),
            nonce: "nonce_012345678901234567890123456789".into(),
            expires_at: now + 30_000,
        });
        let epoch = u64::try_from(now).unwrap();
        let command = signed_command(
            "command-issuer-1",
            &command_issuer,
            &installation_id,
            epoch,
            0,
            now,
        );
        let (authenticated_tx, authenticated_rx) = tokio::sync::oneshot::channel();
        let (send_after_shutdown_tx, send_after_shutdown_rx) = tokio::sync::oneshot::channel();
        let server_task = tokio::spawn(async move {
            server
                .send(Message::Text(
                    serde_json::to_string(&challenge).unwrap().into(),
                ))
                .await
                .unwrap();
            assert!(matches!(
                server.next().await.unwrap().unwrap(),
                Message::Text(_)
            ));
            server
                .send(Message::Text(
                    serde_json::to_string(&RelayMessage::SessionEstablished { epoch })
                        .unwrap()
                        .into(),
                ))
                .await
                .unwrap();
            assert!(matches!(
                server.next().await.unwrap().unwrap(),
                Message::Text(_)
            ));
            assert!(matches!(
                server.next().await.unwrap().unwrap(),
                Message::Text(_)
            ));
            authenticated_tx.send(()).unwrap();
            send_after_shutdown_rx.await.unwrap();
            server
                .send(Message::Text(
                    serde_json::to_string(&RelayMessage::Command(command))
                        .unwrap()
                        .into(),
                ))
                .await
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(1), server.next())
                .await
                .expect("connector must terminate the active socket within the shutdown bound")
                .expect("connector must send a WebSocket close frame")
                .expect("close frame must be readable")
        });
        let client_task = tokio::spawn(async move {
            let mut store = StateStore::default();
            let mut epoch_guard = SessionEpochGuard::load(&config.epoch_path).unwrap();
            let mut ledger = CommandLedger::default();
            let supervisor = ConnectorSupervisor::default();
            let exit = run_connection(
                &state,
                &config,
                &identity,
                "test.session.grant",
                0,
                &mut store,
                &mut epoch_guard,
                &mut ledger,
                ConnectionLifecycle {
                    supervisor: &supervisor,
                    generation: 1,
                },
                client,
            )
            .await;
            (exit, ledger)
        });

        authenticated_rx.await.unwrap();
        shutdown.cancel();
        send_after_shutdown_tx.send(()).unwrap();
        let (exit, ledger) = tokio::time::timeout(std::time::Duration::from_secs(1), client_task)
            .await
            .expect("connector shutdown must be bounded")
            .unwrap();
        assert_eq!(exit.unwrap(), ConnectionExit::Shutdown);
        assert_eq!(
            ledger.len(),
            0,
            "shutdown must win before command verification"
        );
        let close = server_task.await.unwrap();
        assert!(matches!(
            close,
            Message::Close(Some(frame))
                if frame.code == tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Away
        ));
    }
}
