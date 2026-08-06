//! Paired Apple MusicKit companion contract.
//!
//! The companion owns MusicKit authorization. UHC only receives snapshots and
//! queues commands for a short-lived, explicitly paired companion session.

use crate::adapters::apple_music::{MusicKitCommand, MusicKitCompanion, MusicKitSnapshot};
use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    Json,
};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::RwLock;

use super::AppState;

const PAIRING_TTL: Duration = Duration::from_secs(300);
const COMMAND_TTL: Duration = Duration::from_secs(30);
const BRIDGE_LIVENESS_TTL: Duration = Duration::from_secs(30);

#[derive(Clone, Default)]
pub struct AppleBridgeRegistry {
    inner: Arc<RwLock<BridgeState>>,
}

#[derive(Default)]
struct BridgeState {
    pairings: HashMap<String, Pairing>,
    sessions: HashMap<String, BridgeSession>,
}

struct Pairing {
    bridge_id: String,
    expires_at: u64,
}

struct BridgeSession {
    bridge_id: String,
    last_seen: u64,
    snapshot: Option<MusicKitSnapshot>,
    commands: VecDeque<QueuedCommand>,
    results: HashMap<String, Option<std::result::Result<(), String>>>,
}

#[derive(Debug, Serialize)]
pub struct PairingResponse {
    pub bridge_id: String,
    pub pairing_code: String,
    pub expires_at: u64,
}

#[derive(Debug, Deserialize)]
pub struct PairRequest {
    pub bridge_id: String,
}

#[derive(Debug, Deserialize)]
pub struct ClaimRequest {
    pub bridge_id: String,
    pub pairing_code: String,
}

#[derive(Debug, Serialize)]
pub struct ClaimResponse {
    pub bridge_id: String,
    pub access_token: String,
}

#[derive(Debug, Serialize)]
pub struct BridgeStatus {
    pub paired: bool,
    pub bridge_id: Option<String>,
    pub last_seen: Option<u64>,
    pub has_snapshot: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BridgeCommand {
    pub command_id: String,
    pub command: MusicKitCommand,
    pub expires_at: u64,
}

#[derive(Debug, Clone)]
struct QueuedCommand {
    command_id: String,
    command: MusicKitCommand,
    expires_at: u64,
}

#[derive(Debug, Deserialize)]
pub struct CommandResult {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
}

impl AppleBridgeRegistry {
    pub async fn create_pairing(&self, bridge_id: String) -> PairingResponse {
        let pairing_code = random_token(24);
        let expires_at = now_secs() + PAIRING_TTL.as_secs();
        self.inner.write().await.pairings.insert(
            pairing_code.clone(),
            Pairing {
                bridge_id: bridge_id.clone(),
                expires_at,
            },
        );
        PairingResponse {
            bridge_id,
            pairing_code,
            expires_at,
        }
    }

    pub async fn claim(&self, request: ClaimRequest) -> Result<ClaimResponse> {
        let mut state = self.inner.write().await;
        let pairing = state
            .pairings
            .remove(&request.pairing_code)
            .ok_or_else(|| anyhow!("pairing code is unknown or already used"))?;
        if pairing.expires_at <= now_secs() || pairing.bridge_id != request.bridge_id {
            bail!("pairing code is expired or belongs to another bridge");
        }
        let access_token = random_token(48);
        state.sessions.insert(
            access_token.clone(),
            BridgeSession {
                bridge_id: request.bridge_id.clone(),
                last_seen: now_secs(),
                snapshot: None,
                commands: VecDeque::new(),
                results: HashMap::new(),
            },
        );
        Ok(ClaimResponse {
            bridge_id: request.bridge_id,
            access_token,
        })
    }

    async fn with_session<R>(
        &self,
        token: &str,
        f: impl FnOnce(&mut BridgeSession) -> R,
    ) -> Result<R> {
        let mut state = self.inner.write().await;
        let session = state
            .sessions
            .get_mut(token)
            .ok_or_else(|| anyhow!("bridge token is invalid"))?;
        session.last_seen = now_secs();
        Ok(f(session))
    }

    pub async fn update_snapshot(&self, token: &str, snapshot: MusicKitSnapshot) -> Result<()> {
        self.with_session(token, |session| session.snapshot = Some(snapshot))
            .await
    }

    pub async fn snapshot(&self) -> Result<MusicKitSnapshot> {
        let state = self.inner.read().await;
        let session = state
            .sessions
            .values()
            .max_by_key(|session| session.last_seen)
            .ok_or_else(|| anyhow!("Apple Music companion is not paired"))?;
        if session.last_seen + BRIDGE_LIVENESS_TTL.as_secs() <= now_secs() {
            bail!("Apple Music companion is not live");
        }
        session
            .snapshot
            .clone()
            .ok_or_else(|| anyhow!("Apple Music companion has not published a snapshot"))
    }

    pub async fn enqueue(&self, command: MusicKitCommand) -> Result<String> {
        let mut state = self.inner.write().await;
        let session = state
            .sessions
            .values_mut()
            .max_by_key(|session| session.last_seen)
            .ok_or_else(|| anyhow!("Apple Music companion is not paired"))?;
        if session.last_seen + BRIDGE_LIVENESS_TTL.as_secs() <= now_secs() {
            bail!("Apple Music companion is not live");
        }
        let command_id = random_token(20);
        let expires_at = now_secs() + COMMAND_TTL.as_secs();
        session.commands.push_back(QueuedCommand {
            command_id: command_id.clone(),
            command,
            expires_at,
        });
        session.results.insert(command_id.clone(), None);
        Ok(command_id)
    }

    pub async fn poll_commands(&self, token: &str) -> Result<Vec<BridgeCommand>> {
        self.with_session(token, |session| {
            let now = now_secs();
            session.commands.retain(|command| command.expires_at > now);
            session
                .commands
                .iter()
                .map(|command| BridgeCommand {
                    command_id: command.command_id.clone(),
                    command: command.command.clone(),
                    expires_at: command.expires_at,
                })
                .collect()
        })
        .await
    }

    pub async fn acknowledge(
        &self,
        token: &str,
        command_id: &str,
        result: CommandResult,
    ) -> Result<()> {
        self.with_session(token, |session| {
            if !session.results.contains_key(command_id) {
                return Err(anyhow!("command id is unknown or expired"));
            }
            session.results.insert(
                command_id.to_string(),
                Some(if result.ok {
                    Ok(())
                } else {
                    Err(result
                        .error
                        .unwrap_or_else(|| "companion rejected command".to_string()))
                }),
            );
            Ok(())
        })
        .await??;
        Ok(())
    }

    pub async fn wait_for_result(&self, command_id: &str, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let state = self.inner.read().await;
                for session in state.sessions.values() {
                    if let Some(Some(result)) = session.results.get(command_id) {
                        return result.clone().map_err(anyhow::Error::msg);
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("Apple Music companion did not acknowledge command");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub async fn revoke(&self, token: &str) -> Result<()> {
        let mut state = self.inner.write().await;
        state
            .sessions
            .remove(token)
            .map(|_| ())
            .ok_or_else(|| anyhow!("bridge token is invalid"))
    }

    pub async fn status(&self) -> BridgeStatus {
        let state = self.inner.read().await;
        let session = state
            .sessions
            .values()
            .max_by_key(|session| session.last_seen);
        BridgeStatus {
            paired: session
                .map(|session| session.last_seen + BRIDGE_LIVENESS_TTL.as_secs() > now_secs())
                .unwrap_or(false),
            bridge_id: session.map(|session| session.bridge_id.clone()),
            last_seen: session.map(|session| session.last_seen),
            has_snapshot: session
                .and_then(|session| session.snapshot.as_ref())
                .is_some(),
        }
    }
}

#[derive(Clone)]
pub struct PairedMusicKitCompanion {
    registry: AppleBridgeRegistry,
}

impl PairedMusicKitCompanion {
    pub fn new(registry: AppleBridgeRegistry) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl MusicKitCompanion for PairedMusicKitCompanion {
    async fn snapshot(&self) -> Result<MusicKitSnapshot> {
        self.registry.snapshot().await
    }

    async fn execute(&self, command: MusicKitCommand) -> Result<()> {
        let command_id = self.registry.enqueue(command).await?;
        self.registry
            .wait_for_result(&command_id, COMMAND_TTL)
            .await
    }
}

pub async fn pair(
    State(state): State<AppState>,
    Json(request): Json<PairRequest>,
) -> Json<PairingResponse> {
    Json(state.apple_bridges.create_pairing(request.bridge_id).await)
}

pub async fn claim(
    State(state): State<AppState>,
    Json(request): Json<ClaimRequest>,
) -> Result<Json<ClaimResponse>, (StatusCode, Json<ErrorBody>)> {
    let response = state
        .apple_bridges
        .claim(request)
        .await
        .map_err(|e| error(StatusCode::BAD_REQUEST, &e.to_string(), "pairing_failed"))?;
    if state.coordinator.is_enabled("applemusic").await {
        if let Err(error) = state.adapter_registry.start("applemusic").await {
            tracing::debug!("Apple Music adapter will start when registered: {error}");
        }
    } else {
        tracing::info!("Apple Music companion paired while adapter is disabled");
    }
    Ok(Json(response))
}

pub async fn status(State(state): State<AppState>) -> Json<BridgeStatus> {
    Json(state.apple_bridges.status().await)
}

pub async fn revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    let token = bearer(&headers)?;
    state
        .apple_bridges
        .revoke(&token)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(|e| {
            error(
                StatusCode::UNAUTHORIZED,
                &e.to_string(),
                "bridge_unauthorized",
            )
        })
}

pub async fn state(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(snapshot): Json<MusicKitSnapshot>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    let token = bearer(&headers)?;
    state
        .apple_bridges
        .update_snapshot(&token, snapshot)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(|e| {
            error(
                StatusCode::UNAUTHORIZED,
                &e.to_string(),
                "bridge_unauthorized",
            )
        })
}

pub async fn commands(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<BridgeCommand>>, (StatusCode, Json<ErrorBody>)> {
    let token = bearer(&headers)?;
    state
        .apple_bridges
        .poll_commands(&token)
        .await
        .map(Json)
        .map_err(|e| {
            error(
                StatusCode::UNAUTHORIZED,
                &e.to_string(),
                "bridge_unauthorized",
            )
        })
}

pub async fn acknowledge(
    State(state): State<AppState>,
    Path(command_id): Path<String>,
    headers: HeaderMap,
    Json(result): Json<CommandResult>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    let token = bearer(&headers)?;
    state
        .apple_bridges
        .acknowledge(&token, &command_id, result)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(|e| {
            error(
                StatusCode::BAD_REQUEST,
                &e.to_string(),
                "command_ack_failed",
            )
        })
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    error: String,
    code: &'static str,
}

fn bearer(headers: &HeaderMap) -> Result<String, (StatusCode, Json<ErrorBody>)> {
    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    value.ok_or_else(|| {
        error(
            StatusCode::UNAUTHORIZED,
            "Authorization: Bearer is required",
            "bridge_unauthorized",
        )
    })
}

fn random_token(length: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(length)
        .map(char::from)
        .collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn error(status: StatusCode, message: &str, code: &'static str) -> (StatusCode, Json<ErrorBody>) {
    (
        status,
        Json(ErrorBody {
            error: message.to_string(),
            code,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> MusicKitSnapshot {
        MusicKitSnapshot {
            player_id: "application".to_string(),
            display_name: "Mac Music".to_string(),
            state: crate::adapters::apple_music::MusicKitPlaybackState::Paused,
            track: None,
            volume: Some(0.5),
            is_muted: false,
        }
    }

    #[tokio::test]
    async fn pairing_is_single_use_and_snapshot_requires_bearer() {
        let registry = AppleBridgeRegistry::default();
        let pairing = registry.create_pairing("macbook".to_string()).await;
        let claim = registry
            .claim(ClaimRequest {
                bridge_id: pairing.bridge_id.clone(),
                pairing_code: pairing.pairing_code.clone(),
            })
            .await
            .expect("first claim succeeds");
        assert!(registry
            .claim(ClaimRequest {
                bridge_id: pairing.bridge_id,
                pairing_code: pairing.pairing_code,
            })
            .await
            .is_err());

        registry
            .update_snapshot(&claim.access_token, snapshot())
            .await
            .expect("valid bearer updates state");
        assert_eq!(
            registry.snapshot().await.expect("snapshot exists"),
            snapshot()
        );
        assert!(registry
            .update_snapshot("not-a-token", snapshot())
            .await
            .is_err());
        assert!(registry.revoke(&claim.access_token).await.is_ok());
        assert!(registry.snapshot().await.is_err());
    }

    #[tokio::test]
    async fn commands_are_queued_and_acknowledged() {
        let registry = AppleBridgeRegistry::default();
        let pairing = registry.create_pairing("macbook".to_string()).await;
        let claim = registry
            .claim(ClaimRequest {
                bridge_id: pairing.bridge_id,
                pairing_code: pairing.pairing_code,
            })
            .await
            .expect("claim succeeds");
        registry
            .update_snapshot(&claim.access_token, snapshot())
            .await
            .expect("bridge is live");
        let command_id = registry
            .enqueue(MusicKitCommand::Play)
            .await
            .expect("command queued");
        let commands = registry
            .poll_commands(&claim.access_token)
            .await
            .expect("bridge can poll");
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].command_id, command_id);
        registry
            .acknowledge(
                &claim.access_token,
                &command_id,
                CommandResult {
                    ok: true,
                    error: None,
                },
            )
            .await
            .expect("ack accepted");
        registry
            .wait_for_result(&command_id, Duration::from_millis(100))
            .await
            .expect("adapter sees acknowledgement");
    }
}
