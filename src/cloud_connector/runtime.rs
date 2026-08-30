//! Opt-in outbound connector for the authenticated HiPhi relay.
//!
//! The relay is contacted over WSS only.  It sends a challenge first; the
//! installation grant is carried in the signed proof body, never a bearer
//! header or URL.  All control dispatch goes through the existing semantic
//! MQTT command router, which keeps the aggregator/command gateway boundary.

use futures::{SinkExt, StreamExt};
use sha2::{Digest as _, Sha256};
use tokio::task::JoinHandle;
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{protocol::WebSocketConfig, Message},
};
use uuid::Uuid;

use super::{
    commands::{CommandGrantVerifier, CommandLedger, CommandOutcome},
    config::CloudConnectorConfig,
    identity::InstallationIdentity,
    protocol::{
        ArtworkChunk, ArtworkRelayRequest, ArtworkRelayResponse, CommandResult, CommandStatus,
        ConnectorHello, ConnectorMessage, RelayMessage, StateSnapshot, MAX_ARTWORK_CHUNK_BYTES,
        MAX_ARTWORK_SOURCE_BYTES, MAX_STRING_BYTES, PROTOCOL_VERSION, UHC_AUDIENCE,
    },
    session::sign_installation_session_proof,
    state::{snapshot_from_aggregator, StateStore},
    transport::RelayEndpoint,
};

pub fn spawn_from_env(
    state: crate::api::AppState,
    config_dir: impl Into<std::path::PathBuf>,
) -> anyhow::Result<Option<JoinHandle<()>>> {
    let Some(config) = CloudConnectorConfig::from_env(config_dir)? else {
        return Ok(None);
    };
    let identity = if config.key_path.exists() {
        InstallationIdentity::load(&config.key_path, config.installation_id.clone())?
    } else {
        let identity = InstallationIdentity::generate(config.installation_id.clone())?;
        if let Some(parent) = config.key_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        identity.save(&config.key_path)?;
        identity
    };
    Ok(Some(tokio::spawn(run(state, config, identity))))
}

async fn run(
    state: crate::api::AppState,
    config: CloudConnectorConfig,
    identity: InstallationIdentity,
) {
    let mut backoff = super::transport::Backoff::default();
    let shutdown = state.shutdown.clone();
    let mut store = StateStore::default();
    loop {
        let delay = backoff.next_delay();
        tokio::select! { _ = shutdown.cancelled() => break, _ = tokio::time::sleep(delay) => {} }
        let grant = match request_session_grant(&config, &identity).await {
            Ok(grant) => grant,
            Err(error) => {
                tracing::debug!("HiPhi Cloud session grant unavailable: {error}");
                continue;
            }
        };
        let websocket_limits = WebSocketConfig {
            max_message_size: Some(super::protocol::MAX_MESSAGE_BYTES),
            max_frame_size: Some(super::protocol::MAX_MESSAGE_BYTES),
            ..WebSocketConfig::default()
        };
        match connect_async_with_config(config.endpoint.as_str(), Some(websocket_limits), false)
            .await
        {
            Ok((socket, _)) => {
                backoff.reset();
                if let Err(error) =
                    run_connection(&state, &config, &identity, &grant, &mut store, socket).await
                {
                    tracing::warn!("HiPhi Cloud relay disconnected: {error}");
                }
            }
            Err(error) => tracing::debug!("HiPhi Cloud relay unavailable: {error}"),
        }
    }
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn request_session_grant(
    config: &CloudConnectorConfig,
    identity: &InstallationIdentity,
) -> anyhow::Result<String> {
    #[derive(serde::Deserialize)]
    struct GrantResponse {
        grant: String,
    }
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
    let response = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(10))
        .build()?
        .post(url)
        .json(&request)
        .send()
        .await?;
    if !response.status().is_success() {
        anyhow::bail!("session grant request returned {}", response.status());
    }
    let grant = response.json::<GrantResponse>().await?.grant;
    if !(32..=4096).contains(&grant.len()) {
        anyhow::bail!("relay returned an invalid session grant");
    }
    Ok(grant)
}

async fn run_connection(
    state: &crate::api::AppState,
    config: &CloudConnectorConfig,
    identity: &InstallationIdentity,
    grant: &str,
    store: &mut StateStore,
    mut socket: Socket,
) -> anyhow::Result<()> {
    let challenge = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        next_json::<RelayMessage>(&mut socket),
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
    socket
        .send(Message::Text(serde_json::to_string(&proof)?))
        .await?;
    let epoch = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        next_json::<RelayMessage>(&mut socket),
    )
    .await
    .map_err(|_| anyhow::anyhow!("relay proof response timed out"))??
    {
        Some(RelayMessage::SessionEstablished { epoch }) if epoch != 0 => epoch,
        _ => anyhow::bail!("relay rejected installation proof"),
    };

    let projection = snapshot_from_aggregator(
        &state.aggregator,
        store,
        config.installation_id.clone(),
        epoch,
        1,
        now_ms() as u64,
    )
    .await;
    let hello = ConnectorMessage::Hello(ConnectorHello {
        protocol_version: PROTOCOL_VERSION,
        message_id: message_id(),
        connector_version: env!("CARGO_PKG_VERSION").to_owned(),
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

    let issuer_key = ed25519_dalek::VerifyingKey::from_bytes(
        config
            .issuer_public_key
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("issuer key must be 32 bytes"))?,
    )?;
    let mut verifier = CommandGrantVerifier::new_without_node(
        "hiphi-command-authorization",
        UHC_AUDIENCE,
        config.installation_id.clone(),
        epoch,
        session_grant_generation(grant)?,
    );
    verifier.pin_key(config.issuer_key_id.clone(), issuer_key);
    let mut ledger = CommandLedger::default();
    let mut refresh = tokio::time::interval(std::time::Duration::from_secs(20));
    refresh.tick().await;
    loop {
        let message = tokio::select! {
            _ = refresh.tick() => {
                let projection = snapshot_from_aggregator(&state.aggregator, store, config.installation_id.clone(), epoch, store.latest().map_or(1, |p| p.revision.saturating_add(1)), now_ms() as u64).await;
                send_json(&mut socket, &ConnectorMessage::Snapshot(StateSnapshot { protocol_version: PROTOCOL_VERSION, message_id: message_id(), installation_id: projection.installation_id, epoch: projection.epoch, revision: projection.revision, observed_at: projection.observed_at as i64, expires_at: projection.expires_at as i64, zones: projection.zones, now_playing: projection.now_playing })).await?;
                continue;
            }
            message = socket.next() => {
                let Some(message) = message else { break; };
                message?
            }
        };
        match message {
            Message::Text(text) => match serde_json::from_str::<RelayMessage>(&text)? {
                RelayMessage::Heartbeat {
                    epoch: message_epoch,
                    ..
                } if message_epoch == epoch => {
                    send_json(
                        &mut socket,
                        &ConnectorMessage::Heartbeat {
                            epoch,
                            sent_at: now_ms(),
                        },
                    )
                    .await?;
                }
                RelayMessage::Command(command) => {
                    let request_id = command.request_id;
                    // The idempotency key is part of the signed command
                    // binding.  Include it in the local response ledger so a
                    // compromised relay cannot replay a request id with a
                    // different key and bypass grant verification.
                    let ledger_key = format!("{request_id}:{}", command.idempotency_key);
                    let outcome = if let Some(previous) = ledger.get(&ledger_key) {
                        previous
                    } else {
                        let outcome = match verifier.verify(&command, now_ms()) {
                            Ok(verified) => dispatch(state, store, &verified.payload).await,
                            Err(error) => {
                                tracing::debug!("rejecting relay command: {error}");
                                CommandOutcome::Forbidden
                            }
                        };
                        ledger.record_at(ledger_key, outcome, now_ms() as u64);
                        outcome
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
                    respond_artwork(state, store, config, epoch, &mut socket, request).await?;
                }
                RelayMessage::Revoke { reason_code } => {
                    anyhow::bail!("relay revoked connector: {reason_code}")
                }
                _ => {}
            },
            Message::Ping(bytes) => {
                socket.send(Message::Pong(bytes)).await?;
            }
            Message::Close(_) => break,
            Message::Binary(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    }
    Ok(())
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

async fn next_json<T: serde::de::DeserializeOwned>(
    socket: &mut Socket,
) -> anyhow::Result<Option<T>> {
    while let Some(message) = socket.next().await {
        match message? {
            Message::Text(text) => return Ok(Some(serde_json::from_str(&text)?)),
            Message::Ping(bytes) => {
                socket.send(Message::Pong(bytes)).await?;
            }
            Message::Close(_) => return Ok(None),
            _ => {}
        }
    }
    Ok(None)
}
async fn send_json<T: serde::Serialize>(socket: &mut Socket, value: &T) -> anyhow::Result<()> {
    socket
        .send(Message::Text(serde_json::to_string(value)?))
        .await?;
    Ok(())
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
        super::protocol::CommandAction::VolumeUp => crate::mqtt::command::ParsedAction::VolumeUp,
        super::protocol::CommandAction::VolumeDown => {
            crate::mqtt::command::ParsedAction::VolumeDown
        }
        super::protocol::CommandAction::VolumeAbsolute { value } => {
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

async fn send_command_result(
    socket: &mut Socket,
    config: &CloudConnectorConfig,
    epoch: u64,
    request_id: Uuid,
    idempotency_key: Uuid,
    outcome: CommandOutcome,
) -> anyhow::Result<()> {
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

async fn respond_artwork(
    state: &crate::api::AppState,
    store: &StateStore,
    config: &CloudConnectorConfig,
    epoch: u64,
    socket: &mut Socket,
    request: ArtworkRelayRequest,
) -> anyhow::Result<()> {
    if request.protocol_version != PROTOCOL_VERSION
        || request.installation_id != config.installation_id
        || request.epoch != epoch
        || request.max_source_bytes == 0
    {
        return Ok(());
    }
    let Some(provider_id) = store.provider_id(&request.zone_handle) else {
        return Ok(());
    };
    let Some(image_key) = store.artwork_key(&request.zone_handle, &request.artwork_revision) else {
        return Ok(());
    };
    let limit = request.max_source_bytes.min(MAX_ARTWORK_SOURCE_BYTES);
    let image = state
        .get_image(provider_id, image_key, None, None, None, None)
        .await?;
    // The connector bounds the source transfer. The private cloud artwork
    // service performs hostile-image validation and emits the smaller Garmin
    // representation; applying its output cap here would reject legitimate
    // source images before that validation/re-encode boundary.
    if image.data.len() > limit {
        return Ok(());
    }
    let count = image
        .data
        .len()
        .div_ceil(MAX_ARTWORK_CHUNK_BYTES)
        .try_into()
        .map_err(|_| anyhow::anyhow!("artwork has too many chunks"))?;
    let count: u16 = if image.data.is_empty() { 0 } else { count };
    if count == 0 {
        return Ok(());
    }
    send_json(
        socket,
        &ConnectorMessage::ArtworkResponse(ArtworkRelayResponse {
            protocol_version: PROTOCOL_VERSION,
            message_id: message_id(),
            installation_id: config.installation_id.clone(),
            epoch,
            request_id: request.request_id,
            artwork_revision: request.artwork_revision,
            content_type: image.content_type,
            total_bytes: image.data.len(),
            chunk_count: count,
            sha256: hex::encode(Sha256::digest(&image.data)),
        }),
    )
    .await?;
    for (index, bytes) in image.data.chunks(MAX_ARTWORK_CHUNK_BYTES).enumerate() {
        socket
            .send(Message::Binary(
                ArtworkChunk {
                    request_id: request.request_id,
                    index: index.try_into().unwrap_or(u16::MAX),
                    count,
                    bytes: bytes.to_vec(),
                }
                .encode()
                .map_err(|error| anyhow::anyhow!(error))?,
            ))
            .await?;
    }
    Ok(())
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

fn session_grant_generation(grant: &str) -> anyhow::Result<u64> {
    use base64::Engine as _;
    let mut parts = grant.split('.');
    let (Some(_header), Some(encoded_claims), Some(_signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        anyhow::bail!("session grant is not a compact JWS");
    };
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded_claims)
        .map_err(|_| anyhow::anyhow!("session grant claims are not base64url"))?;
    serde_json::from_slice::<serde_json::Value>(&bytes)?
        .get("grant_generation")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("session grant has no authorization generation"))
}

#[cfg(test)]
mod tests {
    use super::{validate_challenge, RelayMessage};
    use crate::cloud_connector::protocol::SessionChallengeMessage;
    use uuid::Uuid;

    fn challenge(endpoint: &str, expires_at: i64) -> SessionChallengeMessage {
        SessionChallengeMessage {
            protocol_version: 1,
            challenge_id: Uuid::from_u128(1),
            endpoint: endpoint.to_owned(),
            nonce: "nonce_012345678901234567890123456789".to_owned(),
            expires_at,
        }
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
}
