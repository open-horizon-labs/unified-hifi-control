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
use serde_json::Value;
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
const COMMAND_DELIVERY_LEASE: Duration = Duration::from_secs(5);
const MAX_PAIRINGS: usize = 64;
const MAX_SESSIONS: usize = 16;
const MAX_COMMANDS: usize = 64;
const MAX_RESULTS: usize = 128;
const MAX_CONTENT_COMMANDS: usize = 32;
const MAX_CONTENT_RESULTS: usize = 64;
const MAX_BRIDGE_ID_LENGTH: usize = 128;

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
    /// The first player identity published by this companion. A paired
    /// companion may not later claim a second execution-owner zone.
    bound_player_id: Option<String>,
    commands: VecDeque<QueuedCommand>,
    results: HashMap<String, Option<std::result::Result<(), String>>>,
    content_commands: VecDeque<QueuedContentCommand>,
    content_results: HashMap<String, Option<std::result::Result<Value, String>>>,
    content_idempotency: HashMap<String, String>,
    content_completed: HashMap<String, std::result::Result<Value, String>>,
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

/// Internal owner-scoped liveness. The legacy HTTP status remains unchanged;
/// this classification is available to adapter/aggregator wiring without
/// pretending that a boolean `paired` means controllable playback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeOwnerLiveness {
    Unpaired,
    AwaitingSnapshot,
    Reachable,
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeOwnerStatus {
    pub player_id: String,
    pub bridge_id: Option<String>,
    pub last_seen: Option<u64>,
    pub has_snapshot: bool,
    pub liveness: BridgeOwnerLiveness,
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
    /// Commands are removed from the delivery stream after the first poll.
    /// This prevents a reconnecting companion from executing a transport
    /// command such as Next more than once. The result remains tracked until
    /// the adapter observes the acknowledgement or the command expires.
    delivered_at: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct CommandResult {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContentCommand {
    pub request_id: String,
    pub owner_id: String,
    pub operation: String,
    pub params: Value,
    pub idempotency_key: Option<String>,
    pub precondition: Option<Value>,
    pub confirm: bool,
    pub expires_at: u64,
}

#[derive(Debug, Clone)]
struct QueuedContentCommand {
    request: ContentCommand,
    delivered_at: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct ContentResult {
    pub outcome: String,
    #[serde(default)]
    pub data: Option<Value>,
    #[serde(default)]
    pub error: Option<ContentError>,
}

#[derive(Debug, Deserialize)]
pub struct ContentError {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
}

/// Outcomes accepted from a native Apple companion. Keep this vocabulary
/// aligned with `docs/apple-music-content-bridge.md` and the companion
/// packages. Rejecting unknown values at the bridge boundary prevents a
/// provider-specific string from becoming a durable UHC outcome.
const APPLE_CONTENT_OUTCOMES: &[&str] = &[
    "success",
    "unsupported",
    "unauthorized",
    "subscription_required",
    "restricted",
    "not_found",
    "offline",
    "rate_limited",
    "stale_owner",
    "conflict",
    "invalid",
    "failed",
];

fn is_apple_content_outcome(outcome: &str) -> bool {
    APPLE_CONTENT_OUTCOMES.contains(&outcome)
}

impl AppleBridgeRegistry {
    pub async fn create_pairing(&self, bridge_id: String) -> PairingResponse {
        let bridge_id = bridge_id
            .chars()
            .take(MAX_BRIDGE_ID_LENGTH)
            .collect::<String>();
        let pairing_code = random_token(24);
        let expires_at = now_secs() + PAIRING_TTL.as_secs();
        let mut state = self.inner.write().await;
        let now = now_secs();
        state.pairings.retain(|_, pairing| pairing.expires_at > now);
        state
            .sessions
            .retain(|_, session| session.last_seen + BRIDGE_LIVENESS_TTL.as_secs() > now);
        if state.pairings.len() >= MAX_PAIRINGS {
            let oldest = state
                .pairings
                .iter()
                .min_by_key(|(_, pairing)| pairing.expires_at)
                .map(|(code, _)| code.clone());
            if let Some(oldest) = oldest {
                state.pairings.remove(&oldest);
            }
        }
        state.pairings.insert(
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
        let now = now_secs();
        state.pairings.retain(|_, pairing| pairing.expires_at > now);
        state
            .sessions
            .retain(|_, session| session.last_seen + BRIDGE_LIVENESS_TTL.as_secs() > now);
        let pairing = state
            .pairings
            .remove(&request.pairing_code)
            .ok_or_else(|| anyhow!("pairing code is unknown or already used"))?;
        if pairing.expires_at <= now_secs() || pairing.bridge_id != request.bridge_id {
            bail!("pairing code is expired or belongs to another bridge");
        }
        let access_token = random_token(48);
        // A bridge identity represents one companion installation. Re-pairing
        // that installation supersedes its previous token, avoiding a stale
        // companion winning the "freshest session" selection race.
        state
            .sessions
            .retain(|_, session| session.bridge_id != request.bridge_id);
        if state.sessions.len() >= MAX_SESSIONS {
            bail!("Apple Music companion session capacity is full");
        }
        state.sessions.insert(
            access_token.clone(),
            BridgeSession {
                bridge_id: request.bridge_id.clone(),
                last_seen: now_secs(),
                snapshot: None,
                bound_player_id: None,
                commands: VecDeque::new(),
                results: HashMap::new(),
                content_commands: VecDeque::new(),
                content_results: HashMap::new(),
                content_idempotency: HashMap::new(),
                content_completed: HashMap::new(),
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
        let now = now_secs();
        let stale = state
            .sessions
            .get(token)
            .map(|session| session.last_seen + BRIDGE_LIVENESS_TTL.as_secs() <= now)
            .unwrap_or(false);
        if stale {
            state.sessions.remove(token);
            bail!("bridge token is stale");
        }
        let session = state
            .sessions
            .get_mut(token)
            .ok_or_else(|| anyhow!("bridge token is invalid"))?;
        session.last_seen = now_secs();
        Ok(f(session))
    }

    pub async fn update_snapshot(&self, token: &str, snapshot: MusicKitSnapshot) -> Result<()> {
        self.with_session(token, |session| {
            if let Some(bound) = &session.bound_player_id {
                if bound != &snapshot.player_id {
                    return Err(anyhow!(
                        "companion is already bound to player `{bound}`, not `{}`",
                        snapshot.player_id
                    ));
                }
            } else {
                session.bound_player_id = Some(snapshot.player_id.clone());
            }
            session.snapshot = Some(snapshot);
            Ok(())
        })
        .await
        .and_then(|result| result)
    }

    pub async fn snapshot(&self) -> Result<MusicKitSnapshot> {
        let state = self.inner.read().await;
        let session = state
            .sessions
            .values()
            .filter(|session| session.last_seen + BRIDGE_LIVENESS_TTL.as_secs() > now_secs())
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

    /// Return snapshots for every live paired execution owner. This is the
    /// owner-scoped counterpart to [`Self::snapshot`], whose freshest-session
    /// behavior is retained for legacy single-owner callers.
    pub async fn snapshots(&self) -> Result<Vec<MusicKitSnapshot>> {
        let now = now_secs();
        let state = self.inner.read().await;
        let snapshots = state
            .sessions
            .values()
            .filter(|session| session.last_seen + BRIDGE_LIVENESS_TTL.as_secs() > now)
            .filter_map(|session| session.snapshot.clone())
            .collect::<Vec<_>>();
        if snapshots.is_empty() {
            bail!("Apple Music companion is not paired");
        }
        Ok(snapshots)
    }

    /// Resolve the live companion bound to one player identity.
    pub async fn snapshot_for_player(&self, player_id: &str) -> Result<MusicKitSnapshot> {
        let now = now_secs();
        let state = self.inner.read().await;
        state
            .sessions
            .values()
            .filter(|session| session.last_seen + BRIDGE_LIVENESS_TTL.as_secs() > now)
            .filter(|session| session.bound_player_id.as_deref() == Some(player_id))
            .max_by_key(|session| session.last_seen)
            .and_then(|session| session.snapshot.clone())
            .ok_or_else(|| anyhow!("Apple Music player `{player_id}` is not paired or live"))
    }

    /// Classify one execution owner without collapsing pairing, first-state,
    /// reachability, and stale state into a single connected flag.
    pub async fn owner_status(&self, player_id: &str) -> BridgeOwnerStatus {
        let now = now_secs();
        let state = self.inner.read().await;
        let session = state
            .sessions
            .values()
            .filter(|session| {
                session.bridge_id == player_id
                    || session.bound_player_id.as_deref() == Some(player_id)
            })
            .max_by_key(|session| session.last_seen);
        let Some(session) = session else {
            return BridgeOwnerStatus {
                player_id: player_id.to_string(),
                bridge_id: None,
                last_seen: None,
                has_snapshot: false,
                liveness: BridgeOwnerLiveness::Unpaired,
            };
        };
        let has_snapshot = session.snapshot.is_some();
        let live = session.last_seen + BRIDGE_LIVENESS_TTL.as_secs() > now;
        BridgeOwnerStatus {
            player_id: player_id.to_string(),
            bridge_id: Some(session.bridge_id.clone()),
            last_seen: Some(session.last_seen),
            has_snapshot,
            liveness: if !live {
                BridgeOwnerLiveness::Stale
            } else if has_snapshot {
                BridgeOwnerLiveness::Reachable
            } else {
                BridgeOwnerLiveness::AwaitingSnapshot
            },
        }
    }

    pub async fn enqueue(&self, command: MusicKitCommand) -> Result<String> {
        let mut state = self.inner.write().await;
        let session = state
            .sessions
            .values_mut()
            .filter(|session| session.last_seen + BRIDGE_LIVENESS_TTL.as_secs() > now_secs())
            .max_by_key(|session| session.last_seen)
            .ok_or_else(|| anyhow!("Apple Music companion is not paired"))?;
        if session.last_seen + BRIDGE_LIVENESS_TTL.as_secs() <= now_secs() {
            bail!("Apple Music companion is not live");
        }
        if session.commands.len() >= MAX_COMMANDS || session.results.len() >= MAX_RESULTS {
            bail!("Apple Music companion command capacity is full");
        }
        let command_id = random_token(20);
        let expires_at = now_secs() + COMMAND_TTL.as_secs();
        session.commands.push_back(QueuedCommand {
            command_id: command_id.clone(),
            command,
            expires_at,
            delivered_at: None,
        });
        session.results.insert(command_id.clone(), None);
        Ok(command_id)
    }

    /// Queue a command only on the companion that owns `player_id`.
    pub async fn enqueue_for_player(
        &self,
        player_id: &str,
        command: MusicKitCommand,
    ) -> Result<String> {
        let mut state = self.inner.write().await;
        let now = now_secs();
        let session = state
            .sessions
            .values_mut()
            .filter(|session| session.last_seen + BRIDGE_LIVENESS_TTL.as_secs() > now)
            .filter(|session| session.bound_player_id.as_deref() == Some(player_id))
            .max_by_key(|session| session.last_seen)
            .ok_or_else(|| anyhow!("Apple Music player `{player_id}` is not paired or live"))?;
        if session.commands.len() >= MAX_COMMANDS || session.results.len() >= MAX_RESULTS {
            bail!("Apple Music companion command capacity is full");
        }
        let command_id = random_token(20);
        let expires_at = now + COMMAND_TTL.as_secs();
        session.commands.push_back(QueuedCommand {
            command_id: command_id.clone(),
            command,
            expires_at,
            delivered_at: None,
        });
        session.results.insert(command_id.clone(), None);
        Ok(command_id)
    }

    pub async fn enqueue_content(&self, operation: &str, params: Value) -> Result<String> {
        let mut state = self.inner.write().await;
        let now = now_secs();
        let session = state
            .sessions
            .values_mut()
            .filter(|session| session.last_seen + BRIDGE_LIVENESS_TTL.as_secs() > now)
            .max_by_key(|session| session.last_seen)
            .ok_or_else(|| anyhow!("Apple Music companion is not paired or live"))?;
        enqueue_content_in_session(session, operation, params)
    }

    pub async fn enqueue_content_for_player(
        &self,
        player_id: &str,
        operation: &str,
        params: Value,
    ) -> Result<String> {
        let mut state = self.inner.write().await;
        let now = now_secs();
        let session = state
            .sessions
            .values_mut()
            .filter(|session| session.last_seen + BRIDGE_LIVENESS_TTL.as_secs() > now)
            .filter(|session| session.bound_player_id.as_deref() == Some(player_id))
            .max_by_key(|session| session.last_seen)
            .ok_or_else(|| anyhow!("Apple Music player `{player_id}` is not paired or live"))?;
        enqueue_content_in_session(session, operation, params)
    }

    pub async fn poll_content(&self, token: &str) -> Result<Vec<ContentCommand>> {
        self.with_session(token, |session| {
            let now = now_secs();
            let expired = session
                .content_commands
                .iter()
                .filter(|command| command.request.expires_at <= now)
                .map(|command| command.request.request_id.clone())
                .collect::<Vec<_>>();
            session
                .content_commands
                .retain(|command| command.request.expires_at > now);
            for request_id in expired {
                session.content_results.remove(&request_id);
            }
            session
                .content_commands
                .iter_mut()
                .filter(|command| {
                    command
                        .delivered_at
                        .map(|delivered_at| {
                            now.saturating_sub(delivered_at) >= COMMAND_DELIVERY_LEASE.as_secs()
                        })
                        .unwrap_or(true)
                })
                .map(|command| {
                    command.delivered_at = Some(now);
                    command.request.clone()
                })
                .collect()
        })
        .await
    }

    pub async fn acknowledge_content(
        &self,
        token: &str,
        request_id: &str,
        result: ContentResult,
    ) -> Result<()> {
        self.with_session(token, |session| {
            if !session.content_results.contains_key(request_id) {
                return Err(anyhow!("content request id is unknown or expired"));
            }
            if !is_apple_content_outcome(&result.outcome) {
                return Err(anyhow!(
                    "unknown Apple Music content outcome `{}`",
                    result.outcome
                ));
            }
            session
                .content_commands
                .retain(|command| command.request.request_id != request_id);
            let outcome = result.outcome.as_str();
            let stored = if outcome == "success" {
                Ok(result.data.unwrap_or(Value::Null))
            } else {
                let message = result
                    .error
                    .map(|error| format!("{}: {}", error.code, error.message))
                    .unwrap_or_else(|| format!("Apple Music content outcome: {outcome}"));
                Err(message)
            };
            session
                .content_results
                .insert(request_id.to_string(), Some(stored));
            Ok(())
        })
        .await??;
        Ok(())
    }

    pub async fn wait_for_content_result(
        &self,
        request_id: &str,
        timeout: Duration,
    ) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            {
                let mut state = self.inner.write().await;
                for session in state.sessions.values_mut() {
                    if let Some(result) = session.content_completed.get(request_id) {
                        return result.clone().map_err(anyhow::Error::msg);
                    }
                    if let Some(Some(result)) = session.content_results.get(request_id) {
                        let result = result.clone();
                        if session.content_completed.len() >= MAX_CONTENT_RESULTS {
                            if let Some(oldest) = session.content_completed.keys().next().cloned() {
                                session.content_completed.remove(&oldest);
                            }
                        }
                        session
                            .content_completed
                            .insert(request_id.to_string(), result.clone());
                        session.content_results.remove(request_id);
                        return result.map_err(anyhow::Error::msg);
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("Apple Music companion did not acknowledge content request");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    pub async fn poll_commands(&self, token: &str) -> Result<Vec<BridgeCommand>> {
        self.with_session(token, |session| {
            let now = now_secs();
            let expired = session
                .commands
                .iter()
                .filter(|command| command.expires_at <= now)
                .map(|command| command.command_id.clone())
                .collect::<Vec<_>>();
            session.commands.retain(|command| command.expires_at > now);
            for command_id in expired {
                session.results.remove(&command_id);
            }
            let commands = session
                .commands
                .iter_mut()
                .filter(|command| {
                    command
                        .delivered_at
                        .map(|delivered_at| {
                            now.saturating_sub(delivered_at) >= COMMAND_DELIVERY_LEASE.as_secs()
                        })
                        .unwrap_or(true)
                })
                .map(|command| {
                    command.delivered_at = Some(now);
                    BridgeCommand {
                        command_id: command.command_id.clone(),
                        command: command.command.clone(),
                        expires_at: command.expires_at,
                    }
                })
                .collect();
            commands
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
            session
                .commands
                .retain(|command| command.command_id != command_id);
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
                let mut state = self.inner.write().await;
                for session in state.sessions.values_mut() {
                    if let Some(Some(result)) = session.results.get(command_id) {
                        let result = result.clone();
                        session.results.remove(command_id);
                        return result.map_err(anyhow::Error::msg);
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
            .filter(|session| session.last_seen + BRIDGE_LIVENESS_TTL.as_secs() > now_secs())
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

    async fn snapshots(&self) -> Result<Vec<MusicKitSnapshot>> {
        self.registry.snapshots().await
    }

    async fn execute(&self, command: MusicKitCommand) -> Result<()> {
        let command_id = self.registry.enqueue(command).await?;
        self.registry
            .wait_for_result(&command_id, COMMAND_TTL)
            .await
    }

    async fn execute_for_player(&self, player_id: &str, command: MusicKitCommand) -> Result<()> {
        let command_id = self.registry.enqueue_for_player(player_id, command).await?;
        self.registry
            .wait_for_result(&command_id, COMMAND_TTL)
            .await
    }

    async fn content(&self, operation: &str, params: &Value) -> Result<Value> {
        let request_id = self
            .registry
            .enqueue_content(operation, params.clone())
            .await?;
        self.registry
            .wait_for_content_result(&request_id, COMMAND_TTL)
            .await
    }

    async fn content_for_player(
        &self,
        player_id: &str,
        operation: &str,
        params: &Value,
    ) -> Result<Value> {
        let request_id = self
            .registry
            .enqueue_content_for_player(player_id, operation, params.clone())
            .await?;
        self.registry
            .wait_for_content_result(&request_id, COMMAND_TTL)
            .await
    }
}

pub async fn pair(
    State(state): State<AppState>,
    Json(request): Json<PairRequest>,
) -> Result<Json<PairingResponse>, (StatusCode, Json<ErrorBody>)> {
    validate_bridge_id(&request.bridge_id)
        .map_err(|message| error(StatusCode::BAD_REQUEST, &message, "pairing_failed"))?;
    Ok(Json(
        state.apple_bridges.create_pairing(request.bridge_id).await,
    ))
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

pub async fn content(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<ContentCommand>>, (StatusCode, Json<ErrorBody>)> {
    let token = bearer(&headers)?;
    state
        .apple_bridges
        .poll_content(&token)
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

pub async fn acknowledge_content(
    State(state): State<AppState>,
    Path(request_id): Path<String>,
    headers: HeaderMap,
    Json(result): Json<ContentResult>,
) -> Result<StatusCode, (StatusCode, Json<ErrorBody>)> {
    let token = bearer(&headers)?;
    state
        .apple_bridges
        .acknowledge_content(&token, &request_id, result)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(|e| {
            error(
                StatusCode::BAD_REQUEST,
                &e.to_string(),
                "content_ack_failed",
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

fn validate_bridge_id(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("bridge_id must not be empty".to_string());
    }
    if value.chars().count() > MAX_BRIDGE_ID_LENGTH {
        return Err(format!(
            "bridge_id must be at most {MAX_BRIDGE_ID_LENGTH} characters"
        ));
    }
    if value.contains(':') {
        return Err("bridge_id must not contain ':'".to_string());
    }
    if value
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err("bridge_id must not contain whitespace or control characters".to_string());
    }
    Ok(())
}

fn enqueue_content_in_session(
    session: &mut BridgeSession,
    operation: &str,
    params: Value,
) -> Result<String> {
    if operation.is_empty() || operation.chars().count() > 128 {
        bail!("Apple Music content operation is empty or too long");
    }
    if serde_json::to_vec(&params)
        .map(|bytes| bytes.len() > 16 * 1024)
        .unwrap_or(true)
    {
        bail!("Apple Music content parameters are too large");
    }
    let idempotency_key = params
        .get("idempotency_key")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(key) = &idempotency_key {
        if key.is_empty() || key.chars().count() > 128 {
            bail!("Apple Music idempotency_key is empty or too long");
        }
        if let Some(request_id) = session.content_idempotency.get(key) {
            return Ok(request_id.clone());
        }
    }
    let mutation = matches!(
        operation,
        "playlist_create"
            | "playlist_add"
            | "playlist_update"
            | "playlist_remove"
            | "favorite_add"
            | "favorite_remove"
            | "rating_set"
    );
    if mutation
        && !params
            .get("confirm")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        bail!("Apple Music mutation requires confirm=true");
    }
    if mutation && idempotency_key.is_none() {
        bail!("Apple Music mutation requires idempotency_key");
    }
    let precondition = params.get("precondition").cloned();
    let confirm = params
        .get("confirm")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if session.content_commands.len() >= MAX_CONTENT_COMMANDS
        || session.content_results.len() >= MAX_CONTENT_RESULTS
    {
        bail!("Apple Music content command capacity is full");
    }
    let request_id = random_token(20);
    let owner_id = session
        .bound_player_id
        .clone()
        .unwrap_or_else(|| session.bridge_id.clone());
    let request = ContentCommand {
        request_id: request_id.clone(),
        owner_id,
        operation: operation.to_string(),
        params,
        idempotency_key: idempotency_key.clone(),
        precondition,
        confirm,
        expires_at: now_secs() + COMMAND_TTL.as_secs(),
    };
    session.content_commands.push_back(QueuedContentCommand {
        request,
        delivered_at: None,
    });
    session.content_results.insert(request_id.clone(), None);
    if let Some(key) = idempotency_key {
        session.content_idempotency.insert(key, request_id.clone());
    }
    Ok(request_id)
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
        snapshot_for("application")
    }

    fn snapshot_for(player_id: &str) -> MusicKitSnapshot {
        MusicKitSnapshot {
            player_id: player_id.to_string(),
            display_name: format!("Apple Music {player_id}"),
            state: crate::adapters::apple_music::MusicKitPlaybackState::Paused,
            track: None,
            volume: Some(0.5),
            is_muted: false,
        }
    }

    #[test]
    fn pairing_bridge_ids_are_owner_safe() {
        assert!(validate_bridge_id("iphone-01").is_ok());
        for invalid in ["", "applemusic:iphone", "iphone 01", "iphone\n01"] {
            assert!(validate_bridge_id(invalid).is_err(), "accepted {invalid:?}");
        }
        assert!(validate_bridge_id(&"x".repeat(MAX_BRIDGE_ID_LENGTH + 1)).is_err());
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
        assert!(registry
            .poll_commands(&claim.access_token)
            .await
            .expect("poll after acknowledgement succeeds")
            .is_empty());
    }

    #[tokio::test]
    async fn content_requests_are_owner_scoped_and_return_normalized_data() {
        let registry = AppleBridgeRegistry::default();
        let pairing = registry.create_pairing("iphone".to_string()).await;
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
            .expect("owner snapshot binds the session");

        let request_id = registry
            .enqueue_content_for_player(
                "application",
                "catalog_search",
                serde_json::json!({"query": "Miles Davis", "limit": 10}),
            )
            .await
            .expect("content request queues for its owner");
        let requests = registry
            .poll_content(&claim.access_token)
            .await
            .expect("content poll succeeds");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].request_id, request_id);
        assert_eq!(requests[0].owner_id, "application");
        assert_eq!(requests[0].operation, "catalog_search");

        registry
            .acknowledge_content(
                &claim.access_token,
                &request_id,
                ContentResult {
                    outcome: "success".to_string(),
                    data: Some(serde_json::json!({"items": []})),
                    error: None,
                },
            )
            .await
            .expect("content acknowledgement succeeds");
        assert_eq!(
            registry
                .wait_for_content_result(&request_id, Duration::from_millis(100))
                .await
                .expect("adapter receives content result"),
            serde_json::json!({"items": []})
        );
        assert!(registry
            .enqueue_content_for_player(
                "other-owner",
                "catalog_search",
                serde_json::json!({"query": "x"}),
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn content_acknowledgement_rejects_unknown_outcomes_before_storage() {
        let registry = AppleBridgeRegistry::default();
        let pairing = registry.create_pairing("iphone".to_string()).await;
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
            .expect("owner snapshot binds the session");
        let request_id = registry
            .enqueue_content_for_player(
                "application",
                "catalog_search",
                serde_json::json!({"query": "Miles Davis"}),
            )
            .await
            .expect("content request queues");
        registry
            .poll_content(&claim.access_token)
            .await
            .expect("content poll succeeds");

        let error = registry
            .acknowledge_content(
                &claim.access_token,
                &request_id,
                ContentResult {
                    outcome: "provider_specific_error".to_string(),
                    data: None,
                    error: None,
                },
            )
            .await
            .expect_err("unknown outcome must be rejected at the bridge boundary");
        assert!(error.to_string().contains("unknown Apple Music content outcome"));

        // Rejection does not consume the request or create a durable result;
        // the companion can retry with a member of the documented vocabulary.
        assert_eq!(
            registry
                .poll_content(&claim.access_token)
                .await
                .expect("request remains available")
                .len(),
            0,
            "the delivery lease suppresses an immediate duplicate poll"
        );
        registry
            .acknowledge_content(
                &claim.access_token,
                &request_id,
                ContentResult {
                    outcome: "offline".to_string(),
                    data: None,
                    error: Some(ContentError {
                        code: "offline".to_string(),
                        message: "companion offline".to_string(),
                        retryable: true,
                    }),
                },
            )
            .await
            .expect("documented outcome is accepted");
        let error = registry
            .wait_for_content_result(&request_id, Duration::from_millis(100))
            .await
            .expect_err("offline is a refusal, not successful data");
        assert!(error.to_string().contains("offline: companion offline"));
    }

    #[tokio::test]
    async fn content_mutations_require_confirmation_and_are_idempotent() {
        let registry = AppleBridgeRegistry::default();
        let pairing = registry.create_pairing("iphone".to_string()).await;
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
            .expect("owner snapshot binds the session");

        let missing_confirmation = registry
            .enqueue_content_for_player(
                "application",
                "playlist_add",
                serde_json::json!({"playlist_ref": "playlist:1", "item_ref": "song:1", "idempotency_key": "add-1"}),
            )
            .await
            .expect_err("mutations must be explicitly confirmed");
        assert!(missing_confirmation.to_string().contains("confirm=true"));

        let missing_key = registry
            .enqueue_content_for_player(
                "application",
                "playlist_add",
                serde_json::json!({"playlist_ref": "playlist:1", "item_ref": "song:1", "confirm": true}),
            )
            .await
            .expect_err("mutations must carry an idempotency key");
        assert!(missing_key.to_string().contains("idempotency_key"));

        let params = serde_json::json!({
            "playlist_ref": "playlist:1",
            "item_ref": "song:1",
            "confirm": true,
            "idempotency_key": "add-1",
            "precondition": {"playlist_version": 3}
        });
        let request_id = registry
            .enqueue_content_for_player("application", "playlist_add", params.clone())
            .await
            .expect("confirmed mutation queues");
        let retry_id = registry
            .enqueue_content_for_player("application", "playlist_add", params)
            .await
            .expect("retry is accepted");
        assert_eq!(
            retry_id, request_id,
            "same idempotency key must not duplicate work"
        );

        let requests = registry
            .poll_content(&claim.access_token)
            .await
            .expect("content poll succeeds");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].request_id, request_id);
        assert!(requests[0].confirm);
        assert_eq!(requests[0].idempotency_key.as_deref(), Some("add-1"));
        assert_eq!(
            requests[0].precondition,
            Some(serde_json::json!({"playlist_version": 3}))
        );

        registry
            .acknowledge_content(
                &claim.access_token,
                &request_id,
                ContentResult {
                    outcome: "success".to_string(),
                    data: Some(serde_json::json!({"changed": true})),
                    error: None,
                },
            )
            .await
            .expect("mutation acknowledgement succeeds");
        let first = registry
            .wait_for_content_result(&request_id, Duration::from_millis(100))
            .await
            .expect("first caller receives result");
        let second = registry
            .wait_for_content_result(&retry_id, Duration::from_millis(100))
            .await
            .expect("idempotent retry receives the cached result");
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn legacy_enqueue_is_bounded_like_owner_scoped_enqueue() {
        let registry = AppleBridgeRegistry::default();
        let pairing = registry.create_pairing("iphone".to_string()).await;
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

        for _ in 0..MAX_COMMANDS {
            registry
                .enqueue(MusicKitCommand::Play)
                .await
                .expect("command remains within the bounded capacity");
        }
        let error = registry
            .enqueue(MusicKitCommand::Play)
            .await
            .expect_err("legacy enqueue must reject an unbounded queue");
        assert!(error.to_string().contains("capacity is full"));
    }

    #[tokio::test]
    async fn polling_delivers_each_command_once() {
        let registry = AppleBridgeRegistry::default();
        let pairing = registry.create_pairing("iphone".to_string()).await;
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
        registry
            .enqueue(MusicKitCommand::Next)
            .await
            .expect("command queued");

        assert_eq!(
            registry
                .poll_commands(&claim.access_token)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(registry
            .poll_commands(&claim.access_token)
            .await
            .expect("second poll succeeds")
            .is_empty());
    }

    #[tokio::test]
    async fn re_pairing_a_bridge_identity_revokes_the_previous_session() {
        let registry = AppleBridgeRegistry::default();
        let first = registry.create_pairing("iphone".to_string()).await;
        let first_claim = registry
            .claim(ClaimRequest {
                bridge_id: first.bridge_id,
                pairing_code: first.pairing_code,
            })
            .await
            .expect("first claim succeeds");
        let second = registry.create_pairing("iphone".to_string()).await;
        registry
            .claim(ClaimRequest {
                bridge_id: second.bridge_id,
                pairing_code: second.pairing_code,
            })
            .await
            .expect("second claim succeeds");
        assert!(registry
            .update_snapshot(&first_claim.access_token, snapshot())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_companion_cannot_claim_a_second_player_zone() {
        let registry = AppleBridgeRegistry::default();
        let pairing = registry.create_pairing("iphone".to_string()).await;
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
            .expect("first player identity binds");
        let mut other = snapshot();
        other.player_id = "another-player".to_string();
        assert!(registry
            .update_snapshot(&claim.access_token, other)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn owner_status_distinguishes_unpaired_awaiting_snapshot_and_reachable() {
        let registry = AppleBridgeRegistry::default();
        assert_eq!(
            registry.owner_status("iphone").await.liveness,
            BridgeOwnerLiveness::Unpaired
        );
        let pairing = registry.create_pairing("iphone".to_string()).await;
        let claim = registry
            .claim(ClaimRequest {
                bridge_id: pairing.bridge_id,
                pairing_code: pairing.pairing_code,
            })
            .await
            .expect("claim succeeds");
        // A paired owner is known, but its player identity is not bound until
        // its first state publication.
        assert_eq!(
            registry.owner_status("iphone").await.liveness,
            BridgeOwnerLiveness::AwaitingSnapshot
        );
        registry
            .update_snapshot(&claim.access_token, snapshot_for("system"))
            .await
            .expect("snapshot binds owner");
        let status = registry.owner_status("system").await;
        assert_eq!(status.bridge_id.as_deref(), Some("iphone"));
        assert_eq!(status.liveness, BridgeOwnerLiveness::Reachable);
        assert!(status.has_snapshot);
    }

    #[tokio::test]
    async fn live_companions_are_scoped_by_player_for_snapshots_and_commands() {
        let registry = AppleBridgeRegistry::default();
        let first = registry.create_pairing("iphone-a".to_string()).await;
        let first_claim = registry
            .claim(ClaimRequest {
                bridge_id: first.bridge_id,
                pairing_code: first.pairing_code,
            })
            .await
            .expect("first claim succeeds");
        registry
            .update_snapshot(&first_claim.access_token, snapshot_for("player-a"))
            .await
            .expect("first snapshot succeeds");

        let second = registry.create_pairing("iphone-b".to_string()).await;
        let second_claim = registry
            .claim(ClaimRequest {
                bridge_id: second.bridge_id,
                pairing_code: second.pairing_code,
            })
            .await
            .expect("second claim succeeds");
        registry
            .update_snapshot(&second_claim.access_token, snapshot_for("player-b"))
            .await
            .expect("second snapshot succeeds");

        let snapshots = registry.snapshots().await.expect("both owners are live");
        assert_eq!(snapshots.len(), 2);
        assert!(snapshots.iter().any(|item| item.player_id == "player-a"));
        assert!(snapshots.iter().any(|item| item.player_id == "player-b"));
        assert_eq!(
            registry
                .snapshot_for_player("player-a")
                .await
                .expect("owner A snapshot")
                .player_id,
            "player-a"
        );

        registry
            .enqueue_for_player("player-a", MusicKitCommand::Next)
            .await
            .expect("owner A command queues");
        let first_commands = registry
            .poll_commands(&first_claim.access_token)
            .await
            .expect("owner A polls");
        assert_eq!(first_commands.len(), 1);
        assert!(registry
            .poll_commands(&second_claim.access_token)
            .await
            .expect("owner B poll")
            .is_empty());

        registry
            .enqueue_for_player("player-b", MusicKitCommand::Previous)
            .await
            .expect("owner B command queues");
        assert!(registry
            .poll_commands(&first_claim.access_token)
            .await
            .expect("owner A second poll")
            .is_empty());
        let second_commands = registry
            .poll_commands(&second_claim.access_token)
            .await
            .expect("owner B second poll");
        assert_eq!(second_commands.len(), 1);
        assert_eq!(second_commands[0].command, MusicKitCommand::Previous);
    }

    #[tokio::test]
    async fn revoked_or_stale_owner_cannot_receive_scoped_commands() {
        let registry = AppleBridgeRegistry::default();
        let pairing = registry.create_pairing("iphone".to_string()).await;
        let claim = registry
            .claim(ClaimRequest {
                bridge_id: pairing.bridge_id,
                pairing_code: pairing.pairing_code,
            })
            .await
            .expect("claim succeeds");
        registry
            .update_snapshot(&claim.access_token, snapshot_for("player"))
            .await
            .expect("snapshot succeeds");
        registry
            .revoke(&claim.access_token)
            .await
            .expect("revoke succeeds");
        assert!(registry.snapshot_for_player("player").await.is_err());
        assert!(registry
            .enqueue_for_player("player", MusicKitCommand::Play)
            .await
            .is_err());

        let pairing = registry.create_pairing("iphone".to_string()).await;
        let claim = registry
            .claim(ClaimRequest {
                bridge_id: pairing.bridge_id,
                pairing_code: pairing.pairing_code,
            })
            .await
            .expect("replacement claim succeeds");
        registry
            .update_snapshot(&claim.access_token, snapshot_for("player"))
            .await
            .expect("replacement snapshot succeeds");
        registry
            .inner
            .write()
            .await
            .sessions
            .get_mut(&claim.access_token)
            .expect("session exists")
            .last_seen = now_secs() - BRIDGE_LIVENESS_TTL.as_secs() - 1;
        assert!(registry.snapshot_for_player("player").await.is_err());
        assert!(registry
            .enqueue_for_player("player", MusicKitCommand::Play)
            .await
            .is_err());
        assert!(registry
            .update_snapshot(&claim.access_token, snapshot_for("player"))
            .await
            .is_err());
        assert!(registry.poll_commands(&claim.access_token).await.is_err());
    }

    #[tokio::test]
    async fn abandoned_pairings_are_pruned_and_bounded() {
        let registry = AppleBridgeRegistry::default();
        for index in 0..(MAX_PAIRINGS + 8) {
            registry.create_pairing(format!("bridge-{index}")).await;
        }
        assert!(registry.inner.read().await.pairings.len() <= MAX_PAIRINGS);
    }
}
