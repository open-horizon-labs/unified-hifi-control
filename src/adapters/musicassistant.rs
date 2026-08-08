//! Music Assistant HTTP API adapter.
//!
//! Music Assistant is an optional peer integration.  UHC does not use MA as a
//! transport for any other provider; this adapter only exposes players owned
//! by a configured MA server.  MA's JSON API is intentionally used through
//! its documented command boundary (`POST /api`) instead of depending on
//! private Python implementation details.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use reqwest::Client;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, RwLock};
use tokio::time::sleep;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::adapters::traits::{
    AdapterCommand, AdapterCommandResponse, AdapterContext, AdapterLogic, LibraryAdapter,
    LibrarySearchResult,
};
use crate::adapters::{AdapterHandle, RetryConfig, Startable};
use crate::bus::{
    BusEvent, NowPlaying, PlaybackState, PrefixedZoneId, SharedBus, VolumeControl, VolumeScale,
    Zone,
};

const DEFAULT_PORT: u16 = 8095;
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// A queue-mode read is supplementary state. Never let a slow MA queue delay
/// player discovery or prevent other zones from being published.
const QUEUE_MODE_READ_TIMEOUT: Duration = Duration::from_secs(2);
/// Avoid stampeding an MA instance with one queue request per active player on
/// every poll. Queue modes are supplemental; a small bounded batch keeps zone
/// discovery responsive while allowing independent queues to hydrate together.
const MAX_CONCURRENT_QUEUE_MODE_READS: usize = 4;
/// `hifi_queue` has no pagination parameters, so keep the adapter response
/// bounded while still returning a useful window of the active MA queue.
const QUEUE_READ_ITEM_LIMIT: usize = 100;

/// Configuration for a Music Assistant server.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MusicAssistantConfig {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// A long-lived MA access token.  The token is never emitted in logs or
    /// zone state; callers are responsible for storing it securely.
    pub token: String,
    #[serde(default = "default_tls")]
    pub tls: bool,
    /// Permit plaintext HTTP only when the operator has explicitly opted in
    /// for a trusted local development network.
    #[serde(default)]
    pub allow_insecure_http: bool,
}

impl fmt::Debug for MusicAssistantConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MusicAssistantConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("token", &"[REDACTED]")
            .field("tls", &self.tls)
            .field("allow_insecure_http", &self.allow_insecure_http)
            .finish()
    }
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_tls() -> bool {
    true
}

impl Default for MusicAssistantConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: DEFAULT_PORT,
            token: String::new(),
            tls: true,
            allow_insecure_http: false,
        }
    }
}

impl MusicAssistantConfig {
    fn base_url(&self) -> String {
        let scheme = if self.tls { "https" } else { "http" };
        format!("{}://{}:{}/api", scheme, self.host, self.port)
    }

    fn permits_insecure_local_http(&self) -> bool {
        if self.tls || !self.allow_insecure_http {
            return false;
        }
        let host = self.host.trim().trim_matches(['[', ']']);
        if host.eq_ignore_ascii_case("localhost") || host.ends_with(".local") {
            return true;
        }
        let Ok(address) = host.parse::<IpAddr>() else {
            return false;
        };
        match address {
            IpAddr::V4(address) => {
                address.is_loopback() || address.is_private() || address.is_link_local()
            }
            IpAddr::V6(address) => {
                address.is_loopback()
                    || address.is_unicast_link_local()
                    || (address.segments()[0] & 0xfe00) == 0xfc00
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
struct PlayerSnapshot {
    id: String,
    name: String,
    state: PlaybackState,
    available: bool,
    volume: Option<f32>,
    muted: bool,
    now_playing: Option<NowPlaying>,
    seekable: bool,
    /// MA's current group owner, if this player is a member of an active group.
    active_group: Option<String>,
    /// MA reports group members on the leader/group player. It may include the
    /// leader itself; the normalized readback removes that duplicate.
    group_members: Vec<String>,
    /// `SET_MEMBERS` is per leader, not a global MA capability.
    can_set_members: bool,
}

#[derive(Debug, Serialize)]
struct ApiRequest<'a> {
    message_id: String,
    command: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    args: Option<Value>,
}

/// Direct adapter for an already-running Music Assistant server.
///
/// MA player discovery, control, catalog search, exact playback, queue-add,
/// active-queue reads, and playback modes are available through its
/// authenticated JSON API. Browse, playlist management, queue mutation, and
/// grouping remain separate capabilities until their MA contracts are wired
/// end to end.
#[derive(Clone)]
pub struct MusicAssistantAdapter {
    bus: SharedBus,
    http: Client,
    base_url: String,
    token: String,
    poll_interval: Duration,
    sequence: Arc<Mutex<u64>>,
    players: Arc<RwLock<HashMap<String, PlayerSnapshot>>>,
    shutdown: Arc<RwLock<CancellationToken>>,
    running: Arc<RwLock<bool>>,
}

/// Stable registry/lifecycle owner for a Music Assistant connection.
///
/// The configured peer can change after UHC has started, while the adapter
/// registry and coordinator must retain one stable `musicassistant` identity.
/// This façade keeps that ownership boundary intact and forwards only to the
/// currently installed outbound client.
#[derive(Clone)]
pub struct ReconfigurableMusicAssistant {
    bus: SharedBus,
    current: Arc<RwLock<Option<Arc<MusicAssistantAdapter>>>>,
    config: Arc<RwLock<Option<MusicAssistantConfig>>>,
    running: Arc<RwLock<bool>>,
}

impl ReconfigurableMusicAssistant {
    pub fn new(bus: SharedBus) -> Self {
        Self {
            bus,
            current: Arc::new(RwLock::new(None)),
            config: Arc::new(RwLock::new(None)),
            running: Arc::new(RwLock::new(false)),
        }
    }

    /// Install a validated client. Start the candidate before disturbing the
    /// current client so a lifecycle failure leaves the live configuration
    /// intact; only then replace the projection owner.
    pub async fn install(
        &self,
        adapter: Arc<MusicAssistantAdapter>,
        config: MusicAssistantConfig,
    ) -> Result<()> {
        if *self.running.read().await {
            adapter.start().await?;
        }
        let old = self.current.write().await.replace(adapter);
        *self.config.write().await = Some(config);
        if let Some(old) = old {
            old.stop().await;
        }
        Ok(())
    }

    pub async fn clear(&self) {
        if let Some(old) = self.current.write().await.take() {
            old.stop().await;
        }
        *self.config.write().await = None;
    }

    pub async fn is_configured(&self) -> bool {
        self.current.read().await.is_some()
    }

    pub async fn is_running(&self) -> bool {
        *self.running.read().await
    }

    /// Server-side bootstrap/status use only. Callers must never serialize
    /// the bearer token held in this configuration.
    pub async fn configuration(&self) -> Option<MusicAssistantConfig> {
        self.config.read().await.clone()
    }

    async fn adapter(&self) -> Result<Arc<MusicAssistantAdapter>> {
        self.current
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow!("Music Assistant is not configured"))
    }
}

impl MusicAssistantAdapter {
    pub fn new(bus: SharedBus, config: MusicAssistantConfig) -> Result<Self> {
        if config.host.trim().is_empty() {
            return Err(anyhow!("Music Assistant host cannot be empty"));
        }
        if config.token.trim().is_empty() {
            return Err(anyhow!("Music Assistant access token cannot be empty"));
        }
        if !config.tls && !config.permits_insecure_local_http() {
            return Err(anyhow!(
                "Music Assistant requires HTTPS; plaintext is allowed only for an explicitly opted-in localhost, .local, private, or link-local development peer"
            ));
        }

        Ok(Self {
            bus,
            http: Client::builder()
                .user_agent("unified-hifi-control/musicassistant")
                .build()
                .context("build Music Assistant HTTP client")?,
            base_url: config.base_url(),
            token: config.token,
            poll_interval: DEFAULT_POLL_INTERVAL,
            sequence: Arc::new(Mutex::new(0)),
            players: Arc::new(RwLock::new(HashMap::new())),
            shutdown: Arc::new(RwLock::new(CancellationToken::new())),
            running: Arc::new(RwLock::new(false)),
        })
    }

    /// Override the polling period for tests or installations with a large
    /// player inventory.
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval.max(Duration::from_millis(250));
        self
    }

    pub async fn snapshot(&self) -> Vec<(String, String)> {
        self.players
            .read()
            .await
            .values()
            .map(|player| (player.id.clone(), player.name.clone()))
            .collect()
    }

    pub async fn is_configured(&self) -> bool {
        !self.base_url.is_empty() && !self.token.trim().is_empty()
    }

    /// Verify the endpoint and bearer token without publishing a partial
    /// zone snapshot. The peer response body is intentionally not surfaced.
    pub async fn probe(&self) -> Result<()> {
        let _: Vec<Value> = self.command("players/all", None).await?;
        Ok(())
    }

    async fn start_internal(&self) -> Result<()> {
        {
            let mut running = self.running.write().await;
            if *running {
                return Ok(());
            }
            *running = true;
        }

        let shutdown = CancellationToken::new();
        *self.shutdown.write().await = shutdown.clone();
        let adapter = self.clone();
        let bus = self.bus.clone();
        tokio::spawn(async move {
            let handle = AdapterHandle::new(adapter, bus, shutdown);
            if let Err(error) = handle.run_with_retry(RetryConfig::default()).await {
                tracing::error!("Music Assistant adapter stopped: {error}");
            }
        });
        Ok(())
    }

    async fn stop_internal(&self) {
        self.shutdown.read().await.cancel();
        *self.running.write().await = false;
        self.bus.publish(BusEvent::AdapterStopping {
            adapter: self.prefix().to_string(),
            reason: Some("requested".to_string()),
        });
    }

    async fn next_message_id(&self) -> String {
        let mut sequence = self.sequence.lock().await;
        *sequence += 1;
        format!("uhc-{}", *sequence)
    }

    async fn command<T: DeserializeOwned>(&self, command: &str, args: Option<Value>) -> Result<T> {
        let request = ApiRequest {
            message_id: self.next_message_id().await,
            command,
            args,
        };
        let response = self
            .http
            .post(&self.base_url)
            .bearer_auth(&self.token)
            .json(&request)
            .send()
            .await
            .with_context(|| format!("Music Assistant command {command}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("read Music Assistant response")?;
        if !status.is_success() {
            // MA's error body is peer-controlled and may reflect credentials
            // or other sensitive request details. The status remains an
            // actionable boundary without exporting that body into UHC errors.
            return Err(anyhow!("Music Assistant returned HTTP {status}"));
        }
        serde_json::from_str(&body)
            .with_context(|| format!("decode Music Assistant response for {command}"))
    }

    async fn discover_players(&self) -> Result<Vec<PlayerSnapshot>> {
        let players: Vec<Value> = self.command("players/all", None).await?;
        let mut snapshots = players
            .into_iter()
            .map(parse_player)
            .collect::<Result<Vec<_>>>()?;
        // Playback modes belong to MA's active queue rather than the player.
        // In particular, a grouped child uses its group's queue, so never infer
        // that identity from player_id.
        let playing_player_ids = snapshots
            .iter()
            .filter(|snapshot| snapshot.now_playing.is_some())
            .map(|snapshot| snapshot.id.clone())
            .collect::<Vec<_>>();
        let queue_modes =
            stream::iter(playing_player_ids.into_iter().map(|player_id| async move {
                let result = timeout(
                    QUEUE_MODE_READ_TIMEOUT,
                    self.active_queue_for_zone(&zone_id_string(&player_id)),
                )
                .await;
                (player_id, result)
            }))
            .buffer_unordered(MAX_CONCURRENT_QUEUE_MODE_READS)
            .collect::<Vec<_>>()
            .await;
        for (player_id, result) in queue_modes {
            let Some(snapshot) = snapshots
                .iter_mut()
                .find(|snapshot| snapshot.id == player_id)
            else {
                continue;
            };
            match result {
                Ok(Ok(queue)) => apply_queue_playback_modes(snapshot, &queue),
                Ok(Err(error)) => {
                    tracing::debug!(%player_id, "Music Assistant queue mode read failed: {error}")
                }
                Err(_) => tracing::debug!(%player_id, "Music Assistant queue mode read timed out"),
            }
        }
        Ok(snapshots)
    }

    async fn publish_snapshot(&self, snapshots: Vec<PlayerSnapshot>) {
        let mut players = self.players.write().await;
        let mut next = HashMap::new();

        for snapshot in snapshots {
            let zone = snapshot_to_zone(&snapshot);
            let id = snapshot.id.clone();
            if let Some(previous) = players.get(&id) {
                self.bus.publish(BusEvent::ZoneUpdated {
                    zone_id: zone_id(&id),
                    display_name: snapshot.name.clone(),
                    state: snapshot.state.to_string(),
                });
                if now_playing_changed(previous.now_playing.as_ref(), snapshot.now_playing.as_ref())
                {
                    self.bus.publish(BusEvent::NowPlayingChanged {
                        zone_id: zone_id(&id),
                        title: snapshot
                            .now_playing
                            .as_ref()
                            .map(|track| track.title.clone()),
                        artist: snapshot
                            .now_playing
                            .as_ref()
                            .map(|track| track.artist.clone()),
                        album: snapshot
                            .now_playing
                            .as_ref()
                            .map(|track| track.album.clone()),
                        image_key: snapshot
                            .now_playing
                            .as_ref()
                            .and_then(|track| track.image_key.clone()),
                    });
                }
                let previous_modes = previous
                    .now_playing
                    .as_ref()
                    .map(|track| (track.repeat_mode, track.shuffle));
                let current_modes = snapshot
                    .now_playing
                    .as_ref()
                    .map(|track| (track.repeat_mode, track.shuffle));
                if previous_modes != current_modes {
                    self.bus.publish(BusEvent::PlaybackModesChanged {
                        zone_id: zone_id(&id),
                        repeat_mode: current_modes.and_then(|modes| modes.0),
                        shuffle: current_modes.and_then(|modes| modes.1),
                    });
                }
                let previous_position = previous
                    .now_playing
                    .as_ref()
                    .and_then(|track| track.seek_position);
                let current_position = snapshot
                    .now_playing
                    .as_ref()
                    .and_then(|track| track.seek_position);
                if previous_position != current_position {
                    if let Some(position) = current_position {
                        self.bus.publish(BusEvent::SeekPositionChanged {
                            zone_id: zone_id(&id),
                            position: position.max(0.0) as i64,
                        });
                    }
                }
                if previous.volume != snapshot.volume || previous.muted != snapshot.muted {
                    if let Some(volume) = snapshot.volume {
                        self.bus.publish(BusEvent::VolumeChanged {
                            output_id: zone_id_string(&id),
                            value: volume,
                            is_muted: snapshot.muted,
                        });
                    }
                }
            } else {
                self.bus.publish(BusEvent::ZoneDiscovered { zone });
            }
            next.insert(id, snapshot);
        }

        for id in players.keys().filter(|id| !next.contains_key(*id)) {
            self.bus.publish(BusEvent::ZoneRemoved {
                zone_id: zone_id(id),
            });
        }
        *players = next;
    }

    async fn control(&self, player_id: &str, command: &str, args: Option<Value>) -> Result<()> {
        let mut payload = serde_json::Map::new();
        payload.insert(
            "player_id".to_string(),
            Value::String(player_id.to_string()),
        );
        if let Some(Value::Object(extra)) = args {
            payload.extend(extra);
        }
        let _: Value = self.command(command, Some(Value::Object(payload))).await?;
        Ok(())
    }

    async fn active_queue_for_zone(&self, zone_id: &str) -> Result<Value> {
        let player_id = zone_id.strip_prefix("musicassistant:").unwrap_or(zone_id);
        if player_id.trim().is_empty() {
            return Err(anyhow!("Music Assistant zone_id must name a player"));
        }
        // `active_group` is a player relationship, not the queue identity.
        // Ask MA for its current active queue so a grouped child never sends a
        // play request to the wrong queue.
        let queue: Value = self
            .command(
                "player_queues/get_active_queue",
                Some(json!({ "player_id": player_id })),
            )
            .await?;
        if queue
            .get("queue_id")
            .and_then(Value::as_str)
            .filter(|queue_id| !queue_id.trim().is_empty())
            .is_none()
        {
            return Err(anyhow!(
                "Music Assistant returned no active queue for player {player_id}"
            ));
        }
        Ok(queue)
    }

    async fn queue_id_for_zone(&self, zone_id: &str) -> Result<String> {
        let queue = self.active_queue_for_zone(zone_id).await?;
        queue
            .get("queue_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| anyhow!("Music Assistant active queue had no queue_id"))
    }

    async fn search_catalog(&self, query: &str, limit: usize) -> Result<Vec<LibrarySearchResult>> {
        let response: Value = self
            .command(
                "music/search",
                Some(json!({
                    "search_query": query,
                    "limit": limit.clamp(1, 50),
                })),
            )
            .await?;
        Ok(parse_search_results(&response, limit))
    }

    async fn play_media_uri(&self, zone_id: &str, uri: &str) -> Result<String> {
        // MA's default enqueue option is configurable per media type and may
        // merely append or stage an item. UHC's `play` contract must always
        // start the selected item, regardless of that server-side preference.
        self.play_media_uri_with_option(zone_id, uri, Some("play"))
            .await
    }

    async fn queue_media_uri(&self, zone_id: &str, uri: &str) -> Result<()> {
        self.play_media_uri_with_option(zone_id, uri, Some("add"))
            .await
            .map(|_| ())
    }

    async fn read_active_queue(&self, zone_id: &str) -> Result<Value> {
        let queue_id = self.queue_id_for_zone(zone_id).await?;
        let queue: Value = self
            .command("player_queues/get", Some(json!({ "queue_id": queue_id })))
            .await?;
        if queue.is_null() {
            return Err(anyhow!(
                "Music Assistant active queue {queue_id} was not found"
            ));
        }
        let items: Value = self
            .command(
                "player_queues/items",
                Some(json!({
                    "queue_id": queue_id,
                    "limit": QUEUE_READ_ITEM_LIMIT,
                    "offset": 0,
                })),
            )
            .await?;
        if !items.is_array() {
            return Err(anyhow!(
                "Music Assistant returned non-list queue items for active queue {queue_id}"
            ));
        }
        Ok(json!({ "queue": queue, "items": items }))
    }

    async fn read_multiroom_players(&self) -> Result<Vec<PlayerSnapshot>> {
        let players: Vec<Value> = self.command("players/all", None).await?;
        players.into_iter().map(parse_player).collect()
    }

    async fn multiroom_status(&self) -> Result<Value> {
        let players = self.read_multiroom_players().await?;
        Ok(multiroom_status_from_players(&players))
    }

    async fn set_group_members(
        &self,
        leader_zone_id: &str,
        add_zone_ids: &[String],
        remove_zone_ids: &[String],
    ) -> Result<Value> {
        if add_zone_ids.is_empty() && remove_zone_ids.is_empty() {
            return Err(anyhow!(
                "Music Assistant group membership needs at least one member"
            ));
        }
        let leader_id = musicassistant_player_id(leader_zone_id, "leader_zone_id")?;
        let add_ids = musicassistant_player_ids(add_zone_ids, "member_zone_ids_to_add")?;
        let remove_ids = musicassistant_player_ids(remove_zone_ids, "member_zone_ids_to_remove")?;
        let players = self.read_multiroom_players().await?;
        let leader = players
            .iter()
            .find(|player| player.id == leader_id)
            .ok_or_else(|| anyhow!("Music Assistant group leader was not found"))?;
        if !leader.available {
            return Err(anyhow!("Music Assistant group leader is unavailable"));
        }
        if !leader.can_set_members {
            return Err(anyhow!(
                "Music Assistant group leader does not support membership changes"
            ));
        }
        for player_id in add_ids.iter().chain(remove_ids.iter()) {
            if player_id == &leader_id {
                return Err(anyhow!(
                    "Music Assistant group leader cannot be its own member input"
                ));
            }
            let player = players
                .iter()
                .find(|player| player.id == *player_id)
                .ok_or_else(|| anyhow!("Music Assistant group member was not found"))?;
            if !player.available {
                return Err(anyhow!("Music Assistant group member is unavailable"));
            }
        }
        let mut args = serde_json::Map::new();
        args.insert("target_player".to_string(), Value::String(leader_id));
        if !add_ids.is_empty() {
            args.insert("player_ids_to_add".to_string(), json!(add_ids));
        }
        if !remove_ids.is_empty() {
            args.insert("player_ids_to_remove".to_string(), json!(remove_ids));
        }
        let _: Value = self
            .command("players/cmd/set_members", Some(Value::Object(args)))
            .await?;
        self.multiroom_status().await
    }

    async fn ungroup_members(&self, member_zone_ids: &[String]) -> Result<Value> {
        let player_ids = musicassistant_player_ids(member_zone_ids, "member_zone_ids")?;
        if player_ids.is_empty() {
            return Err(anyhow!("Music Assistant ungroup needs at least one member"));
        }
        let players = self.read_multiroom_players().await?;
        for player_id in &player_ids {
            if !players.iter().any(|player| player.id == *player_id) {
                return Err(anyhow!("Music Assistant group member was not found"));
            }
        }
        let _: Value = self
            .command(
                "players/cmd/ungroup_many",
                Some(json!({"player_ids": player_ids})),
            )
            .await?;
        self.multiroom_status().await
    }

    async fn play_media_uri_with_option(
        &self,
        zone_id: &str,
        uri: &str,
        option: Option<&str>,
    ) -> Result<String> {
        if uri.trim().is_empty() {
            return Err(anyhow!("Music Assistant media URI cannot be empty"));
        }
        let queue_id = self.queue_id_for_zone(zone_id).await?;
        let mut args = serde_json::Map::new();
        args.insert("queue_id".to_string(), Value::String(queue_id));
        args.insert("media".to_string(), Value::String(uri.to_string()));
        if let Some(option) = option {
            args.insert("option".to_string(), Value::String(option.to_string()));
        }
        let _: Value = self
            .command("player_queues/play_media", Some(Value::Object(args)))
            .await?;
        Ok("Music Assistant item started".to_string())
    }
}

fn zone_id_string(raw: &str) -> String {
    format!("musicassistant:{raw}")
}

fn zone_id(raw: &str) -> PrefixedZoneId {
    PrefixedZoneId::musicassistant(raw)
}

fn value_string(value: &Value, fields: &[&str]) -> Option<String> {
    fields.iter().find_map(|field| {
        value
            .get(*field)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    })
}

fn parse_player(value: Value) -> Result<PlayerSnapshot> {
    let id = value_string(&value, &["player_id", "id"])
        .ok_or_else(|| anyhow!("Music Assistant player has no player_id"))?;
    let name = value_string(&value, &["name", "display_name"]).unwrap_or_else(|| id.clone());
    let state = value_string(&value, &["playback_state", "state"])
        .map(|state| PlaybackState::from(state.as_str()))
        .unwrap_or_default();
    let volume = value
        .get("volume_level")
        .or_else(|| value.get("volume"))
        .and_then(Value::as_f64)
        .map(|volume| volume as f32);
    let muted = value
        .get("volume_muted")
        .or_else(|| value.get("muted"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let available = value
        .get("available")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let media = value.get("current_media").or_else(|| value.get("media"));
    let now_playing = media.and_then(parse_now_playing);
    let seekable = media
        .and_then(|media| media.get("duration"))
        .and_then(Value::as_f64)
        .is_some();
    let group_members = player_string_list(&value, "group_members");
    let active_group = player_string(&value, "active_group");
    let can_set_members = player_string_list(&value, "supported_features")
        .iter()
        .any(|feature| feature.eq_ignore_ascii_case("set_members"));

    Ok(PlayerSnapshot {
        id,
        name,
        state,
        available,
        volume,
        muted,
        now_playing,
        seekable,
        active_group,
        group_members,
        can_set_members,
    })
}

/// MA serializes player state at the top level today, but older/newer server
/// shapes may nest it under `state`. Keep the adapter's read model tolerant at
/// this protocol boundary rather than duplicating that tolerance at callers.
fn player_string(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .or_else(|| value.get("state").and_then(|state| state.get(field)))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
}

fn player_string_list(value: &Value, field: &str) -> Vec<String> {
    value
        .get(field)
        .or_else(|| value.get("state").and_then(|state| state.get(field)))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn musicassistant_player_id(zone_id: &str, parameter: &str) -> Result<String> {
    zone_id
        .strip_prefix("musicassistant:")
        .filter(|value| !value.is_empty() && !value.contains(':'))
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("{parameter} must be a Music Assistant zone id"))
}

fn musicassistant_player_ids(zone_ids: &[String], parameter: &str) -> Result<Vec<String>> {
    let mut ids = Vec::with_capacity(zone_ids.len());
    for zone_id in zone_ids {
        let id = musicassistant_player_id(zone_id, parameter)?;
        if ids.contains(&id) {
            return Err(anyhow!("{parameter} must not contain duplicate zones"));
        }
        ids.push(id);
    }
    Ok(ids)
}

fn multiroom_status_from_players(players: &[PlayerSnapshot]) -> Value {
    let mut groups = std::collections::BTreeMap::<String, Vec<String>>::new();
    for player in players {
        if !player.group_members.is_empty() {
            let members = groups.entry(player.id.clone()).or_default();
            for member in &player.group_members {
                if member != &player.id && !members.contains(member) {
                    members.push(member.clone());
                }
            }
        }
        // Some MA providers only expose membership on children. Keep that
        // relationship rather than pretending an empty leader list is truth.
        if let Some(leader_id) = player.active_group.as_ref() {
            if leader_id != &player.id {
                let members = groups.entry(leader_id.clone()).or_default();
                if !members.contains(&player.id) {
                    members.push(player.id.clone());
                }
            }
        }
    }
    let groups = groups
        .into_iter()
        .filter(|(_, members)| !members.is_empty())
        .map(|(leader_id, members)| {
            let can_set_members = players
                .iter()
                .find(|player| player.id == leader_id)
                .is_some_and(|player| player.can_set_members);
            json!({
                "leader_zone_id": zone_id_string(&leader_id),
                "member_zone_ids": members.iter().map(|member| zone_id_string(member)).collect::<Vec<_>>(),
                "can_set_members": can_set_members,
            })
        })
        .collect::<Vec<_>>();
    json!({"groups": groups})
}

/// Convert MA's grouped `SearchResults` payload to the provider-neutral
/// library contract. Preserve MA's URI unchanged: it is the durable handle the
/// documented `player_queues/play_media` command accepts.
fn parse_search_results(value: &Value, limit: usize) -> Vec<LibrarySearchResult> {
    const GROUPS: &[&str] = &[
        "tracks",
        "albums",
        "artists",
        "playlists",
        "radio",
        "podcasts",
        "audiobooks",
        "sound_effects",
    ];
    let mut results = Vec::new();
    for group in GROUPS {
        let Some(items) = value.get(*group).and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            if results.len() >= limit {
                return results;
            }
            let Some(uri) = item
                .get("uri")
                .and_then(Value::as_str)
                .filter(|uri| !uri.is_empty())
            else {
                continue;
            };
            let title = value_string(item, &["name", "title"]).unwrap_or_else(|| uri.to_string());
            let subtitle = search_subtitle(item);
            results.push(LibrarySearchResult {
                title,
                subtitle,
                uri: uri.to_string(),
            });
        }
    }
    results
}

fn search_subtitle(item: &Value) -> Option<String> {
    if let Some(artists) = item.get("artists").and_then(Value::as_array) {
        let names: Vec<&str> = artists
            .iter()
            .filter_map(|artist| artist.get("name").and_then(Value::as_str))
            .collect();
        if !names.is_empty() {
            return Some(names.join(", "));
        }
    }
    value_string(item, &["artist", "album", "media_type"])
}

fn parse_now_playing(value: &Value) -> Option<NowPlaying> {
    let title = value_string(value, &["title", "name"])?;
    let artist = value_string(value, &["artist", "artist_name"]).unwrap_or_default();
    let album = value_string(value, &["album", "album_name"]).unwrap_or_default();
    let image_key = value_string(value, &["image_url", "artwork_url", "image"]);
    let seek_position = value
        .get("elapsed_time")
        .and_then(Value::as_f64)
        .or_else(|| value.get("position").and_then(Value::as_f64));
    let duration = value.get("duration").and_then(Value::as_f64);
    Some(NowPlaying {
        title,
        artist,
        album,
        image_key,
        seek_position,
        duration,
        metadata: None,
        repeat_mode: None,
        shuffle: None,
    })
}

/// Project queue-owned playback modes onto UHC's currently-playing state.
/// MA has one queue per active player/group, whereas UHC displays these modes
/// alongside a zone's now-playing metadata.
fn apply_queue_playback_modes(snapshot: &mut PlayerSnapshot, queue: &Value) {
    let Some(now_playing) = snapshot.now_playing.as_mut() else {
        return;
    };
    now_playing.repeat_mode = match queue.get("repeat_mode").and_then(Value::as_str) {
        Some("off") => Some(crate::bus::RepeatMode::Off),
        Some("one") => Some(crate::bus::RepeatMode::One),
        Some("all") => Some(crate::bus::RepeatMode::All),
        _ => None,
    };
    now_playing.shuffle = queue.get("shuffle_enabled").and_then(Value::as_bool);
}

fn now_playing_changed(previous: Option<&NowPlaying>, current: Option<&NowPlaying>) -> bool {
    match (previous, current) {
        (None, None) => false,
        (Some(previous), Some(current)) => {
            previous.title != current.title
                || previous.artist != current.artist
                || previous.album != current.album
                || previous.image_key != current.image_key
                || previous.duration != current.duration
                || previous.repeat_mode != current.repeat_mode
                || previous.shuffle != current.shuffle
        }
        _ => true,
    }
}

fn snapshot_to_zone(snapshot: &PlayerSnapshot) -> Zone {
    Zone {
        zone_id: zone_id_string(&snapshot.id),
        zone_name: snapshot.name.clone(),
        state: snapshot.state,
        volume_control: snapshot.volume.map(|volume| VolumeControl {
            value: volume,
            min: 0.0,
            max: 100.0,
            step: 1.0,
            is_muted: snapshot.muted,
            scale: VolumeScale::Percentage,
            output_id: Some(zone_id_string(&snapshot.id)),
        }),
        now_playing: snapshot.now_playing.clone(),
        source: "musicassistant".to_string(),
        is_controllable: snapshot.available,
        is_seekable: snapshot.seekable,
        last_updated: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        is_play_allowed: snapshot.state != PlaybackState::Playing,
        is_pause_allowed: snapshot.state == PlaybackState::Playing,
        is_next_allowed: snapshot.available,
        is_previous_allowed: snapshot.available,
    }
}

#[async_trait]
impl AdapterLogic for MusicAssistantAdapter {
    fn prefix(&self) -> &'static str {
        "musicassistant"
    }

    async fn run(&self, ctx: AdapterContext) -> Result<()> {
        loop {
            if ctx.shutdown.is_cancelled() {
                return Ok(());
            }
            match self.discover_players().await {
                Ok(snapshots) => self.publish_snapshot(snapshots).await,
                Err(error) => tracing::warn!("Music Assistant poll failed: {error}"),
            }
            tokio::select! {
                _ = ctx.shutdown.cancelled() => return Ok(()),
                _ = sleep(self.poll_interval) => {}
            }
        }
    }

    async fn handle_command(
        &self,
        zone_id: &str,
        command: AdapterCommand,
    ) -> Result<AdapterCommandResponse> {
        let player_id = zone_id.strip_prefix("musicassistant:").unwrap_or(zone_id);
        let result = match command {
            AdapterCommand::Play => self.control(player_id, "players/cmd/play", None).await,
            AdapterCommand::Pause => self.control(player_id, "players/cmd/pause", None).await,
            AdapterCommand::PlayPause => {
                self.control(player_id, "players/cmd/play_pause", None)
                    .await
            }
            AdapterCommand::Stop => self.control(player_id, "players/cmd/stop", None).await,
            AdapterCommand::Next => self.control(player_id, "players/cmd/next", None).await,
            AdapterCommand::Previous => self.control(player_id, "players/cmd/previous", None).await,
            AdapterCommand::VolumeAbsolute(value) => {
                self.control(
                    player_id,
                    "players/cmd/volume_set",
                    Some(json!({ "volume_level": value.clamp(0, 100) })),
                )
                .await
            }
            AdapterCommand::VolumeRelative(delta) if delta >= 0 => {
                self.control(player_id, "players/cmd/volume_up", None).await
            }
            AdapterCommand::VolumeRelative(_) => {
                self.control(player_id, "players/cmd/volume_down", None)
                    .await
            }
            AdapterCommand::Mute(muted) => {
                self.control(
                    player_id,
                    "players/cmd/volume_mute",
                    Some(json!({ "muted": muted })),
                )
                .await
            }
            AdapterCommand::SetRepeat(mode) => {
                let queue_id = self.queue_id_for_zone(zone_id).await?;
                let repeat_mode = match mode {
                    crate::bus::RepeatMode::Off => "off",
                    crate::bus::RepeatMode::One => "one",
                    crate::bus::RepeatMode::All => "all",
                };
                self.command::<Value>(
                    "player_queues/repeat",
                    Some(json!({ "queue_id": queue_id, "repeat_mode": repeat_mode })),
                )
                .await
                .map(|_| ())
            }
            AdapterCommand::SetShuffle(enabled) => {
                let queue_id = self.queue_id_for_zone(zone_id).await?;
                self.command::<Value>(
                    "player_queues/shuffle",
                    Some(json!({ "queue_id": queue_id, "shuffle_enabled": enabled })),
                )
                .await
                .map(|_| ())
            }
        };

        match result {
            Ok(()) => Ok(AdapterCommandResponse {
                success: true,
                error: None,
            }),
            Err(error) => Ok(AdapterCommandResponse {
                success: false,
                error: Some(error.to_string()),
            }),
        }
    }
}

#[async_trait]
impl LibraryAdapter for MusicAssistantAdapter {
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<LibrarySearchResult>> {
        self.search_catalog(query, limit).await
    }

    async fn play_uri(&self, zone_id: &str, uri: &str) -> Result<String> {
        self.play_media_uri(zone_id, uri).await
    }

    async fn queue_uri(&self, zone_id: &str, uri: &str) -> Result<()> {
        self.queue_media_uri(zone_id, uri).await
    }

    async fn play_next_uri(&self, zone_id: &str, uri: &str) -> Result<()> {
        self.play_media_uri_with_option(zone_id, uri, Some("next"))
            .await
            .map(|_| ())
    }

    async fn read_queue(&self, zone_id: &str) -> Result<Value> {
        self.read_active_queue(zone_id).await
    }

    async fn content(&self, operation: &str, params: &Value) -> Result<Value> {
        if operation.starts_with("collections_") {
            return self.collections_content(operation, params).await;
        }
        match operation {
            "multiroom_status" => return self.multiroom_status().await,
            "multiroom_set_members" => {
                let leader = params
                    .get("leader_zone_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        anyhow!("Music Assistant group operation requires leader_zone_id")
                    })?;
                let add = params
                    .get("member_zone_ids_to_add")
                    .or_else(|| params.get("member_zone_ids"))
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        anyhow!("Music Assistant group operation requires member_zone_ids")
                    })?
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>();
                let remove = params
                    .get("member_zone_ids_to_remove")
                    .and_then(Value::as_array)
                    .map(|members| {
                        members
                            .iter()
                            .filter_map(Value::as_str)
                            .map(ToOwned::to_owned)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                return self.set_group_members(leader, &add, &remove).await;
            }
            "multiroom_ungroup" => {
                let members = params
                    .get("member_zone_ids")
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow!("Music Assistant ungroup requires member_zone_ids"))?
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>();
                return self.ungroup_members(&members).await;
            }
            _ => {}
        }
        let zone_id = params
            .get("zone_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("Music Assistant queue operation requires zone_id"))?;
        let before = self.read_active_queue(zone_id).await?;
        let queue_id = before["queue"]["queue_id"]
            .as_str()
            .ok_or_else(|| anyhow!("Music Assistant active queue had no queue_id"))?
            .to_string();
        let mut args = serde_json::Map::new();
        args.insert("queue_id".to_string(), Value::String(queue_id));
        let command = match operation {
            "queue_jump" => {
                args.insert("index".to_string(), required_param(params, "item_id")?);
                "player_queues/play_index"
            }
            "queue_reorder" => {
                args.insert(
                    "queue_item_id".to_string(),
                    required_param(params, "item_id")?,
                );
                let target = required_param(params, "position")?
                    .as_i64()
                    .ok_or_else(|| {
                        anyhow!("Music Assistant reorder position must be an integer")
                    })?;
                let item_id = args["queue_item_id"].as_str().unwrap_or_default();
                let current = before["items"]
                    .as_array()
                    .and_then(|items| items.iter().position(|item| item["queue_item_id"].as_str() == Some(item_id)))
                    .ok_or_else(|| anyhow!("Music Assistant queue item {item_id} was not found in fresh queue state"))? as i64;
                args.insert("pos_shift".to_string(), json!(target - current));
                "player_queues/move_item"
            }
            "queue_remove" => {
                args.insert(
                    "item_id_or_index".to_string(),
                    required_param(params, "item_id")?,
                );
                "player_queues/delete_item"
            }
            "queue_clear" => "player_queues/clear",
            _ => {
                return Err(anyhow!(
                    "unsupported Music Assistant queue operation {operation}"
                ))
            }
        };
        let _: Value = self.command(command, Some(Value::Object(args))).await?;
        self.read_active_queue(zone_id).await
    }
}

impl MusicAssistantAdapter {
    async fn collections_content(&self, operation: &str, params: &Value) -> Result<Value> {
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(20)
            .clamp(1, 50);
        let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(0);
        let (command, args) = match operation {
            "collections_browse" => (
                "music/browse",
                json!({"path": params.get("path").and_then(Value::as_str).unwrap_or("root")}),
            ),
            "collections_playlists" => (
                "music/playlists/library_items",
                json!({"limit": limit, "offset": offset}),
            ),
            "collections_favorites" => {
                let media_type = params
                    .get("media_type")
                    .and_then(Value::as_str)
                    .unwrap_or("tracks");
                if !matches!(
                    media_type,
                    "tracks"
                        | "albums"
                        | "artists"
                        | "playlists"
                        | "radio"
                        | "podcasts"
                        | "audiobooks"
                ) {
                    return Err(anyhow!(
                        "unsupported Music Assistant favorites media_type {media_type}"
                    ));
                }
                let command = match media_type {
                    "tracks" => "music/tracks/library_items",
                    "albums" => "music/albums/library_items",
                    "artists" => "music/artists/library_items",
                    "playlists" => "music/playlists/library_items",
                    "radio" => "music/radio/library_items",
                    "podcasts" => "music/podcasts/library_items",
                    _ => "music/audiobooks/library_items",
                };
                (
                    command,
                    json!({"favorite": true, "limit": limit, "offset": offset}),
                )
            }
            _ => {
                return Err(anyhow!(
                    "unsupported Music Assistant collection operation {operation}"
                ))
            }
        };
        let raw: Value = self.command(command, Some(args)).await?;
        let raw_items = raw
            .as_array()
            .ok_or_else(|| anyhow!("Music Assistant collection response was not a list"))?;
        // `library_items` applies offset/limit server-side. `music/browse`
        // does not expose paging, so page that one locally without leaking MA's
        // provider path format through the MCP response.
        let page: Box<dyn Iterator<Item = &Value>> = if operation == "collections_browse" {
            Box::new(raw_items.iter().skip(offset as usize).take(limit as usize))
        } else {
            Box::new(raw_items.iter())
        };
        let mut items = Vec::new();
        for item in page {
            let title =
                value_string(item, &["name", "title"]).unwrap_or_else(|| "Untitled".to_string());
            let subtitle = search_subtitle(item);
            let mut mapped = serde_json::Map::new();
            mapped.insert("title".to_string(), Value::String(title));
            if let Some(subtitle) = subtitle {
                mapped.insert("subtitle".to_string(), Value::String(subtitle));
            }
            if let Some(uri) = item.get("uri").and_then(Value::as_str) {
                mapped.insert("uri".to_string(), Value::String(uri.to_string()));
            }
            if let Some(path) = item.get("path").and_then(Value::as_str) {
                mapped.insert("path".to_string(), Value::String(path.to_string()));
            }
            items.push(Value::Object(mapped));
        }
        let next_offset = if operation == "collections_browse" {
            (raw_items.len() > (offset + limit) as usize).then_some(offset + limit)
        } else {
            (raw_items.len() == limit as usize).then_some(offset + limit)
        };
        Ok(json!({"items": items, "next_offset": next_offset}))
    }
}

fn required_param(params: &Value, name: &str) -> Result<Value> {
    params
        .get(name)
        .filter(|value| !value.is_null())
        .cloned()
        .ok_or_else(|| anyhow!("Music Assistant queue operation requires {name}"))
}

#[async_trait]
impl AdapterLogic for ReconfigurableMusicAssistant {
    fn prefix(&self) -> &'static str {
        "musicassistant"
    }

    async fn run(&self, ctx: AdapterContext) -> Result<()> {
        ctx.shutdown.cancelled().await;
        Ok(())
    }

    async fn handle_command(
        &self,
        zone_id: &str,
        command: AdapterCommand,
    ) -> Result<AdapterCommandResponse> {
        self.adapter().await?.handle_command(zone_id, command).await
    }
}

#[async_trait]
impl LibraryAdapter for ReconfigurableMusicAssistant {
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<LibrarySearchResult>> {
        self.adapter().await?.search(query, limit).await
    }

    async fn search_for_zone(
        &self,
        zone_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<LibrarySearchResult>> {
        self.adapter()
            .await?
            .search_for_zone(zone_id, query, limit)
            .await
    }

    async fn play_uri(&self, zone_id: &str, uri: &str) -> Result<String> {
        self.adapter().await?.play_uri(zone_id, uri).await
    }

    async fn queue_uri(&self, zone_id: &str, uri: &str) -> Result<()> {
        self.adapter().await?.queue_uri(zone_id, uri).await
    }

    async fn play_next_uri(&self, zone_id: &str, uri: &str) -> Result<()> {
        self.adapter().await?.play_next_uri(zone_id, uri).await
    }

    async fn read_queue(&self, zone_id: &str) -> Result<Value> {
        self.adapter().await?.read_queue(zone_id).await
    }

    async fn content(&self, operation: &str, params: &Value) -> Result<Value> {
        self.adapter().await?.content(operation, params).await
    }
}

#[async_trait]
impl crate::adapters::Startable for ReconfigurableMusicAssistant {
    fn name(&self) -> &'static str {
        "musicassistant"
    }

    async fn start(&self) -> Result<()> {
        let adapter = self.adapter().await?;
        adapter.start().await?;
        *self.running.write().await = true;
        Ok(())
    }

    async fn stop(&self) {
        *self.running.write().await = false;
        if let Some(adapter) = self.current.read().await.clone() {
            adapter.stop().await;
        } else {
            self.bus.publish(BusEvent::AdapterStopping {
                adapter: self.prefix().to_string(),
                reason: Some("requested".to_string()),
            });
        }
    }

    async fn can_start(&self) -> bool {
        self.is_configured().await
    }
}

crate::impl_startable!(MusicAssistantAdapter, "musicassistant", is_configured);

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Bytes,
        extract::State,
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::post,
        Json, Router,
    };
    use std::sync::{Arc, Mutex as StdMutex};
    use tokio::net::TcpListener;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct RecordedRequest {
        authorization: Option<String>,
        body: Value,
    }

    #[derive(Clone, Default)]
    struct MockMusicAssistantState {
        requests: Arc<StdMutex<Vec<RecordedRequest>>>,
        players: Arc<StdMutex<Option<Value>>>,
        failure_response: Arc<StdMutex<Option<(StatusCode, String)>>>,
        slow_active_queue_prefix: Arc<StdMutex<Option<String>>>,
        slow_active_queue_delay: Arc<StdMutex<Option<Duration>>>,
        active_queue_in_flight: Arc<AtomicUsize>,
        max_active_queue_in_flight: Arc<AtomicUsize>,
    }

    async fn musicassistant_mock(
        State(state): State<MockMusicAssistantState>,
        headers: HeaderMap,
        body: Bytes,
    ) -> impl IntoResponse {
        let request: Value = serde_json::from_slice(&body).expect("valid MA request JSON");
        state
            .requests
            .lock()
            .expect("mock request lock")
            .push(RecordedRequest {
                authorization: headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .map(ToOwned::to_owned),
                body: request.clone(),
            });

        if let Some((status, response)) = state
            .failure_response
            .lock()
            .expect("failure response lock")
            .clone()
        {
            return (status, response).into_response();
        }

        if request["command"] == "players/all" {
            let players = state.players.lock().expect("players lock").clone().unwrap_or_else(|| json!([{
                "player_id": "sonos-kitchen",
                "name": "Kitchen",
                "playback_state": "playing",
                "available": true,
                "volume_level": 42,
                "volume_muted": false,
                "current_media": {
                    "title": "Kind of Blue",
                    "artist": "Miles Davis",
                    "album": "Kind of Blue",
                    "elapsed_time": 12.5,
                    "duration": 300
                }
            }]));
            Json(players)
        } else if request["command"] == "player_queues/get_active_queue" {
            let delayed = state
                .slow_active_queue_prefix
                .lock()
                .expect("slow player lock")
                .as_ref()
                .is_some_and(|prefix| request["args"]["player_id"].as_str().is_some_and(|id| id.starts_with(prefix)));
            if delayed {
                let in_flight = state.active_queue_in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                state
                    .max_active_queue_in_flight
                    .fetch_max(in_flight, Ordering::SeqCst);
                let delay = state
                    .slow_active_queue_delay
                    .lock()
                    .expect("slow queue delay lock")
                    .unwrap_or(QUEUE_MODE_READ_TIMEOUT + Duration::from_millis(50));
                sleep(delay).await;
                state.active_queue_in_flight.fetch_sub(1, Ordering::SeqCst);
            }
            // Deliberately differs from player_id: MA grouped children must
            // resolve their active queue rather than infer it from membership.
            Json(json!({
                "queue_id": "group-living-room",
                "items": 234,
                "repeat_mode": "all",
                "shuffle_enabled": true
            }))
        } else if request["command"] == "player_queues/get" {
            Json(json!({
                "queue_id": "group-living-room",
                "display_name": "Living Room group",
                "items": 234,
                "current_index": 3
            }))
        } else if request["command"] == "player_queues/items" {
            Json(json!([
                {"queue_item_id": "item-4", "name": "So What", "uri": "library://track/4"},
                {"queue_item_id": "item-5", "name": "Freddie Freeloader", "uri": "library://track/5"}
            ]))
        } else {
            Json(json!({"ok": true}))
        }
        .into_response()
    }

    async fn mock_musicassistant_server() -> (
        MusicAssistantConfig,
        MockMusicAssistantState,
        tokio::task::JoinHandle<()>,
    ) {
        let state = MockMusicAssistantState::default();
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("mock bind");
        let address = listener.local_addr().expect("mock address");
        let router = Router::new()
            .route("/api", post(musicassistant_mock))
            .with_state(state.clone());
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.expect("mock serve");
        });
        (
            MusicAssistantConfig {
                host: "127.0.0.1".to_string(),
                port: address.port(),
                token: "ma-test-token".to_string(),
                tls: false,
                allow_insecure_http: true,
            },
            state,
            handle,
        )
    }

    #[test]
    fn config_defaults_to_ma_port_and_https() {
        let config: MusicAssistantConfig = serde_json::from_value(json!({
            "host": "ma.local",
            "token": "secret"
        }))
        .unwrap_or_default();
        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.base_url(), "https://ma.local:8095/api");
        assert!(!config.allow_insecure_http);
    }

    #[test]
    fn plaintext_requires_explicit_development_opt_in() {
        let config = MusicAssistantConfig {
            host: "ma.local".to_string(),
            port: DEFAULT_PORT,
            token: "secret".to_string(),
            tls: false,
            allow_insecure_http: false,
        };
        assert!(MusicAssistantAdapter::new(crate::bus::create_bus(), config).is_err());
    }

    #[test]
    fn explicit_plaintext_opt_in_builds_http_url() {
        let config = MusicAssistantConfig {
            host: "127.0.0.1".to_string(),
            port: DEFAULT_PORT,
            token: "secret".to_string(),
            tls: false,
            allow_insecure_http: true,
        };
        assert_eq!(config.base_url(), "http://127.0.0.1:8095/api");
        assert!(MusicAssistantAdapter::new(crate::bus::create_bus(), config).is_ok());
    }

    #[test]
    fn plaintext_opt_in_rejects_public_hosts() {
        let config = MusicAssistantConfig {
            host: "ma.example.com".to_string(),
            port: DEFAULT_PORT,
            token: "secret".to_string(),
            tls: false,
            allow_insecure_http: true,
        };
        assert!(MusicAssistantAdapter::new(crate::bus::create_bus(), config).is_err());
    }

    #[test]
    fn player_state_and_metadata_map_to_zone_snapshot() {
        let player = parse_player(json!({
            "player_id": "sonos-kitchen",
            "name": "Kitchen",
            "playback_state": "playing",
            "available": true,
            "volume_level": 42,
            "volume_muted": false,
            "current_media": {
                "title": "Kind of Blue",
                "artist": "Miles Davis",
                "album": "Kind of Blue",
                "image_url": "https://example.invalid/art.jpg",
                "elapsed_time": 12.5,
                "duration": 300
            }
        }))
        .unwrap_or_default();

        assert_eq!(player.id, "sonos-kitchen");
        assert_eq!(player.state, PlaybackState::Playing);
        assert_eq!(player.volume, Some(42.0));
        assert!(player.seekable);
        assert_eq!(
            player
                .now_playing
                .as_ref()
                .map(|track| track.title.as_str()),
            Some("Kind of Blue")
        );
        let zone = snapshot_to_zone(&player);
        assert_eq!(zone.zone_id, "musicassistant:sonos-kitchen");
        assert_eq!(zone.source, "musicassistant");
    }

    #[tokio::test]
    async fn multiroom_content_reads_membership_and_uses_guarded_ma_commands() {
        let (config, state, server) = mock_musicassistant_server().await;
        *state.players.lock().expect("players lock") = Some(json!([
            {
                "player_id": "living-room",
                "name": "Living Room",
                "available": true,
                "supported_features": ["set_members"],
                "group_members": ["living-room", "kitchen"]
            },
            {
                "player_id": "kitchen",
                "name": "Kitchen",
                "available": true,
                "active_group": "living-room"
            }
        ]));
        let adapter =
            MusicAssistantAdapter::new(crate::bus::create_bus(), config).expect("valid MA adapter");

        let status = adapter
            .content("multiroom_status", &json!({}))
            .await
            .expect("group status");
        assert_eq!(
            status,
            json!({"groups": [{
                "leader_zone_id": "musicassistant:living-room",
                "member_zone_ids": ["musicassistant:kitchen"],
                "can_set_members": true
            }]})
        );

        adapter
            .content(
                "multiroom_set_members",
                &json!({
                    "leader_zone_id": "musicassistant:living-room",
                    "member_zone_ids": ["musicassistant:kitchen"]
                }),
            )
            .await
            .expect("set members");
        adapter
            .content(
                "multiroom_ungroup",
                &json!({"member_zone_ids": ["musicassistant:kitchen"]}),
            )
            .await
            .expect("ungroup");

        let requests = state.requests.lock().expect("requests").clone();
        let set_members = requests
            .iter()
            .find(|request| request.body["command"] == "players/cmd/set_members")
            .expect("set_members command");
        assert_eq!(
            set_members.body["args"],
            json!({"target_player": "living-room", "player_ids_to_add": ["kitchen"]})
        );
        let ungroup = requests
            .iter()
            .find(|request| request.body["command"] == "players/cmd/ungroup_many")
            .expect("ungroup command");
        assert_eq!(ungroup.body["args"], json!({"player_ids": ["kitchen"]}));
        assert!(
            requests
                .iter()
                .filter(|request| request.body["command"] == "players/all")
                .count()
                >= 3,
            "status and both writes must take a fresh membership readback"
        );
        server.abort();
    }

    #[tokio::test]
    async fn multiroom_set_members_refuses_unmanageable_or_cross_provider_inputs_before_write() {
        let (config, state, server) = mock_musicassistant_server().await;
        *state.players.lock().expect("players lock") = Some(json!([
            {"player_id": "living-room", "available": true},
            {"player_id": "kitchen", "available": true}
        ]));
        let adapter =
            MusicAssistantAdapter::new(crate::bus::create_bus(), config).expect("valid MA adapter");

        let unsupported = adapter
            .content(
                "multiroom_set_members",
                &json!({
                    "leader_zone_id": "musicassistant:living-room",
                    "member_zone_ids": ["musicassistant:kitchen"]
                }),
            )
            .await
            .expect_err("leader without SET_MEMBERS must refuse");
        assert!(unsupported.to_string().contains("does not support"));
        let cross_provider = adapter
            .content(
                "multiroom_ungroup",
                &json!({"member_zone_ids": ["spotify:kitchen"]}),
            )
            .await
            .expect_err("cross-provider member must refuse");
        assert!(cross_provider
            .to_string()
            .contains("Music Assistant zone id"));
        assert!(!state
            .requests
            .lock()
            .expect("requests")
            .iter()
            .any(
                |request| request.body["command"] == "players/cmd/set_members"
                    || request.body["command"] == "players/cmd/ungroup_many"
            ));
        server.abort();
    }

    #[test]
    fn catalog_search_keeps_ma_uris_for_exact_playback_and_limits_across_groups() {
        let results = parse_search_results(
            &json!({
                "tracks": [{
                    "name": "Kind of Blue",
                    "uri": "library://track/42",
                    "artists": [{"name": "Miles Davis"}]
                }],
                "albums": [{
                    "name": "Blue Train",
                    "uri": "library://album/7",
                    "artists": [{"name": "John Coltrane"}]
                }]
            }),
            1,
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Kind of Blue");
        assert_eq!(results[0].subtitle.as_deref(), Some("Miles Davis"));
        assert_eq!(results[0].uri, "library://track/42");
    }

    #[test]
    fn player_without_media_is_still_a_valid_zone() {
        let player = parse_player(json!({
            "player_id": "empty",
            "display_name": "Empty",
            "state": "idle",
            "available": false
        }))
        .unwrap_or_default();
        assert_eq!(player.state, PlaybackState::Unknown);
        assert!(player.now_playing.is_none());
        assert!(!snapshot_to_zone(&player).is_controllable);
    }

    #[test]
    fn control_request_keeps_ma_args_flat_and_never_serializes_the_token() {
        let request = ApiRequest {
            message_id: "uhc-1".to_string(),
            command: "players/cmd/volume_set",
            args: Some(json!({ "player_id": "kitchen", "volume_level": 55 })),
        };
        let wire = serde_json::to_value(request).unwrap_or_default();
        assert_eq!(wire["command"], "players/cmd/volume_set");
        assert_eq!(wire["args"]["player_id"], "kitchen");
        assert_eq!(wire["args"]["volume_level"], 55);
        assert!(wire.get("token").is_none());
    }

    #[tokio::test]
    async fn queue_read_resolves_grouped_player_active_queue_then_reads_bounded_items() {
        let (config, state, server) = mock_musicassistant_server().await;
        let adapter =
            MusicAssistantAdapter::new(crate::bus::create_bus(), config).expect("valid MA adapter");

        let queue = adapter
            .read_queue("musicassistant:sonos-kitchen")
            .await
            .expect("queue read");

        assert_eq!(queue["queue"]["queue_id"], "group-living-room");
        assert_eq!(queue["items"][0]["uri"], "library://track/4");

        let requests = state.requests.lock().expect("mock request lock").clone();
        assert_eq!(
            requests
                .iter()
                .map(|request| request.body["command"].as_str().expect("command"))
                .collect::<Vec<_>>(),
            [
                "player_queues/get_active_queue",
                "player_queues/get",
                "player_queues/items"
            ]
        );
        assert_eq!(
            requests[0].body["args"],
            json!({"player_id": "sonos-kitchen"})
        );
        assert_eq!(
            requests[1].body["args"],
            json!({"queue_id": "group-living-room"})
        );
        assert_eq!(
            requests[2].body["args"],
            json!({"queue_id": "group-living-room", "limit": 100, "offset": 0})
        );
        for request in requests {
            assert_eq!(
                request.authorization.as_deref(),
                Some("Bearer ma-test-token")
            );
        }

        server.abort();
    }

    #[tokio::test]
    async fn catalog_play_and_queue_use_explicit_distinct_ma_options() {
        let (config, state, server) = mock_musicassistant_server().await;
        let adapter =
            MusicAssistantAdapter::new(crate::bus::create_bus(), config).expect("valid MA adapter");

        adapter
            .play_media_uri("musicassistant:sonos-kitchen", "library://track/play")
            .await
            .expect("play request");
        adapter
            .queue_media_uri("musicassistant:sonos-kitchen", "library://track/queue")
            .await
            .expect("queue request");

        let requests = state.requests.lock().expect("mock request lock").clone();
        let play_media_requests: Vec<_> = requests
            .iter()
            .filter(|request| request.body["command"] == "player_queues/play_media")
            .collect();
        assert_eq!(play_media_requests.len(), 2);
        assert_eq!(
            play_media_requests[0].body["args"],
            json!({
                "queue_id": "group-living-room",
                "media": "library://track/play",
                "option": "play"
            })
        );
        assert_eq!(
            play_media_requests[1].body["args"],
            json!({
                "queue_id": "group-living-room",
                "media": "library://track/queue",
                "option": "add"
            })
        );

        server.abort();
    }

    #[tokio::test]
    async fn repeat_and_shuffle_resolve_the_active_group_queue_and_use_ma_wire_names() {
        let (config, state, server) = mock_musicassistant_server().await;
        let adapter =
            MusicAssistantAdapter::new(crate::bus::create_bus(), config).expect("valid MA adapter");

        for command in [
            AdapterCommand::SetRepeat(crate::bus::RepeatMode::Off),
            AdapterCommand::SetRepeat(crate::bus::RepeatMode::One),
            AdapterCommand::SetRepeat(crate::bus::RepeatMode::All),
            AdapterCommand::SetShuffle(true),
            AdapterCommand::SetShuffle(false),
        ] {
            assert!(
                adapter
                    .handle_command("musicassistant:sonos-kitchen", command)
                    .await
                    .expect("MA request")
                    .success
            );
        }

        let requests = state.requests.lock().expect("mock request lock").clone();
        let commands: Vec<_> = requests
            .iter()
            .map(|request| request.body["command"].as_str().expect("command"))
            .collect();
        assert_eq!(
            commands,
            [
                "player_queues/get_active_queue",
                "player_queues/repeat",
                "player_queues/get_active_queue",
                "player_queues/repeat",
                "player_queues/get_active_queue",
                "player_queues/repeat",
                "player_queues/get_active_queue",
                "player_queues/shuffle",
                "player_queues/get_active_queue",
                "player_queues/shuffle",
            ]
        );
        assert_eq!(
            requests[1].body["args"],
            json!({"queue_id": "group-living-room", "repeat_mode": "off"})
        );
        assert_eq!(
            requests[3].body["args"],
            json!({"queue_id": "group-living-room", "repeat_mode": "one"})
        );
        assert_eq!(
            requests[5].body["args"],
            json!({"queue_id": "group-living-room", "repeat_mode": "all"})
        );
        assert_eq!(
            requests[7].body["args"],
            json!({"queue_id": "group-living-room", "shuffle_enabled": true})
        );
        assert_eq!(
            requests[9].body["args"],
            json!({"queue_id": "group-living-room", "shuffle_enabled": false})
        );

        server.abort();
    }

    #[test]
    fn active_queue_repeat_and_shuffle_are_projected_into_now_playing() {
        let mut player = parse_player(json!({
            "player_id": "sonos-kitchen",
            "current_media": {"title": "Kind of Blue"}
        }))
        .expect("player");

        apply_queue_playback_modes(
            &mut player,
            &json!({"repeat_mode": "one", "shuffle_enabled": true}),
        );

        let now_playing = player.now_playing.expect("now playing");
        assert_eq!(now_playing.repeat_mode, Some(crate::bus::RepeatMode::One));
        assert_eq!(now_playing.shuffle, Some(true));
    }

    #[test]
    fn queue_mode_changes_count_as_now_playing_changes() {
        let mut previous = parse_now_playing(&json!({"title": "Kind of Blue"})).expect("track");
        let mut current = previous.clone();
        previous.repeat_mode = Some(crate::bus::RepeatMode::Off);
        current.repeat_mode = Some(crate::bus::RepeatMode::All);
        current.shuffle = Some(true);

        assert!(now_playing_changed(Some(&previous), Some(&current)));
    }

    #[tokio::test]
    async fn slow_queue_mode_reads_are_bounded_and_concurrent() {
        let (config, state, server) = mock_musicassistant_server().await;
        *state.players.lock().expect("players lock") = Some(json!([
            {"player_id": "slow-one", "current_media": {"title": "One"}},
            {"player_id": "slow-two", "current_media": {"title": "Two"}}
        ]));
        *state
            .slow_active_queue_prefix
            .lock()
            .expect("slow player lock") = Some("slow-".to_string());
        let adapter =
            MusicAssistantAdapter::new(crate::bus::create_bus(), config).expect("adapter");

        let started = std::time::Instant::now();
        let players = adapter
            .discover_players()
            .await
            .expect("discovery survives queue timeouts");
        assert_eq!(players.len(), 2);
        assert!(
            started.elapsed() < QUEUE_MODE_READ_TIMEOUT + Duration::from_millis(500),
            "two slow queue reads must share one timeout window"
        );
        assert!(players.iter().all(|player| player
            .now_playing
            .as_ref()
            .is_some_and(|track| track.repeat_mode.is_none() && track.shuffle.is_none())));

        server.abort();
    }

    #[tokio::test]
    async fn queue_mutations_use_active_queue_and_return_fresh_readback() {
        let (config, state, server) = mock_musicassistant_server().await;
        let adapter =
            MusicAssistantAdapter::new(crate::bus::create_bus(), config).expect("adapter");
        for (operation, params, command, args) in [
            (
                "queue_jump",
                json!({"item_id": "item-5"}),
                "player_queues/play_index",
                json!({"index": "item-5"}),
            ),
            (
                "queue_reorder",
                json!({"item_id": "item-5", "position": 1}),
                "player_queues/move_item",
                json!({"queue_item_id": "item-5", "pos_shift": 0}),
            ),
            (
                "queue_remove",
                json!({"item_id": "item-5"}),
                "player_queues/delete_item",
                json!({"item_id_or_index": "item-5"}),
            ),
            ("queue_clear", json!({}), "player_queues/clear", json!({})),
        ] {
            let mut params = params;
            params["zone_id"] = json!("musicassistant:sonos-kitchen");
            adapter.content(operation, &params).await.expect("mutation");
            let requests = state.requests.lock().expect("requests").clone();
            let mutation = requests
                .iter()
                .find(|request| request.body["command"] == command)
                .expect("mutation command");
            let mut expected = args;
            expected["queue_id"] = json!("group-living-room");
            assert_eq!(mutation.body["args"], expected);
        }
        server.abort();
    }

    #[tokio::test]
    async fn queue_reorder_refuses_a_stale_item_before_writing() {
        let (config, state, server) = mock_musicassistant_server().await;
        let adapter =
            MusicAssistantAdapter::new(crate::bus::create_bus(), config).expect("adapter");
        let error = adapter
            .content("queue_reorder", &json!({"zone_id": "musicassistant:sonos-kitchen", "item_id": "gone", "position": 0}))
            .await
            .expect_err("stale id");
        assert!(error.to_string().contains("not found"));
        assert!(!state
            .requests
            .lock()
            .expect("requests")
            .iter()
            .any(|request| request.body["command"] == "player_queues/move_item"));
        server.abort();
    }

    #[tokio::test]
    async fn queue_mode_hydration_caps_in_flight_requests() {
        let (config, state, server) = mock_musicassistant_server().await;
        *state.players.lock().expect("players lock") = Some(json!([
            {"player_id": "slow-one", "current_media": {"title": "One"}},
            {"player_id": "slow-two", "current_media": {"title": "Two"}},
            {"player_id": "slow-three", "current_media": {"title": "Three"}},
            {"player_id": "slow-four", "current_media": {"title": "Four"}},
            {"player_id": "slow-five", "current_media": {"title": "Five"}}
        ]));
        *state
            .slow_active_queue_prefix
            .lock()
            .expect("slow player lock") = Some("slow-".to_string());
        *state
            .slow_active_queue_delay
            .lock()
            .expect("slow queue delay lock") = Some(Duration::from_millis(100));
        let adapter =
            MusicAssistantAdapter::new(crate::bus::create_bus(), config).expect("adapter");

        adapter
            .discover_players()
            .await
            .expect("discovery survives queue timeouts");

        assert!(
            state.max_active_queue_in_flight.load(Ordering::SeqCst)
                <= MAX_CONCURRENT_QUEUE_MODE_READS,
            "queue-mode hydration must cap concurrent MA requests"
        );

        server.abort();
    }

    #[test]
    fn config_debug_redacts_the_access_token() {
        let config = MusicAssistantConfig {
            host: "ma.local".to_string(),
            token: "ma-test-token".to_string(),
            ..Default::default()
        };

        assert!(!format!("{config:?}").contains("ma-test-token"));
    }

    #[tokio::test]
    async fn upstream_error_is_actionable_without_leaking_a_reflected_token() {
        let (config, state, server) = mock_musicassistant_server().await;
        *state
            .failure_response
            .lock()
            .expect("failure response lock") = Some((
            StatusCode::UNAUTHORIZED,
            "invalid bearer ma-test-token; retry after authentication".to_string(),
        ));
        let adapter =
            MusicAssistantAdapter::new(crate::bus::create_bus(), config).expect("valid MA adapter");

        let response = adapter
            .handle_command("musicassistant:sonos-kitchen", AdapterCommand::Play)
            .await
            .expect("adapter returns provider refusal");

        assert!(!response.success);
        let error = response.error.expect("actionable failure");
        assert!(error.contains("Music Assistant returned HTTP 401"));
        assert!(!error.contains("ma-test-token"));
        assert!(!error.contains("invalid bearer"));

        server.abort();
    }

    #[tokio::test]
    async fn wire_contract_discovers_players_and_sends_authenticated_flat_controls() {
        let (config, state, server) = mock_musicassistant_server().await;
        let bus = crate::bus::create_bus();
        let mut events = bus.subscribe();
        let adapter = MusicAssistantAdapter::new(bus, config).expect("valid MA adapter");

        let players = adapter.discover_players().await.expect("discover players");
        assert_eq!(players.len(), 1);
        adapter.publish_snapshot(players).await;
        let BusEvent::ZoneDiscovered { zone } = events.recv().await.expect("zone event") else {
            panic!("expected a discovered MA zone");
        };
        assert_eq!(zone.zone_id, "musicassistant:sonos-kitchen");
        assert_eq!(zone.zone_name, "Kitchen");
        assert_eq!(
            zone.volume_control.as_ref().map(|volume| volume.value),
            Some(42.0)
        );
        assert_eq!(
            zone.now_playing.as_ref().map(|track| track.title.as_str()),
            Some("Kind of Blue")
        );
        assert_eq!(
            zone.now_playing
                .as_ref()
                .and_then(|track| track.repeat_mode),
            Some(crate::bus::RepeatMode::All)
        );
        assert_eq!(
            zone.now_playing.as_ref().and_then(|track| track.shuffle),
            Some(true)
        );

        for command in [
            AdapterCommand::Play,
            AdapterCommand::Pause,
            AdapterCommand::PlayPause,
            AdapterCommand::Stop,
            AdapterCommand::Next,
            AdapterCommand::Previous,
            AdapterCommand::VolumeAbsolute(125),
            AdapterCommand::VolumeRelative(1),
            AdapterCommand::VolumeRelative(-1),
            AdapterCommand::Mute(true),
        ] {
            assert!(
                adapter
                    .handle_command("musicassistant:sonos-kitchen", command)
                    .await
                    .expect("control response")
                    .success
            );
        }

        let requests = state.requests.lock().expect("mock request lock").clone();
        assert_eq!(requests.len(), 12);
        for request in &requests {
            assert_eq!(
                request.authorization.as_deref(),
                Some("Bearer ma-test-token")
            );
            assert!(request.body.get("token").is_none());
            assert!(request.body.get("authorization").is_none());
            assert!(request.body["message_id"]
                .as_str()
                .is_some_and(|id| id.starts_with("uhc-")));
        }
        assert_eq!(
            requests[0].body,
            json!({"message_id": "uhc-1", "command": "players/all"})
        );
        let commands: Vec<_> = requests[1..]
            .iter()
            .map(|request| request.body["command"].as_str().expect("command"))
            .collect();
        assert_eq!(
            commands,
            [
                "player_queues/get_active_queue",
                "players/cmd/play",
                "players/cmd/pause",
                "players/cmd/play_pause",
                "players/cmd/stop",
                "players/cmd/next",
                "players/cmd/previous",
                "players/cmd/volume_set",
                "players/cmd/volume_up",
                "players/cmd/volume_down",
                "players/cmd/volume_mute",
            ]
        );
        assert_eq!(
            requests[1].body["args"],
            json!({"player_id": "sonos-kitchen"})
        );
        let request_for = |command: &str| {
            requests
                .iter()
                .find(|request| request.body["command"] == command)
                .expect("recorded command")
        };
        assert_eq!(
            request_for("players/cmd/volume_set").body["args"],
            json!({"player_id": "sonos-kitchen", "volume_level": 100})
        );
        assert_eq!(
            request_for("players/cmd/volume_up").body["args"],
            json!({"player_id": "sonos-kitchen"})
        );
        assert_eq!(
            request_for("players/cmd/volume_down").body["args"],
            json!({"player_id": "sonos-kitchen"})
        );
        assert_eq!(
            request_for("players/cmd/volume_mute").body["args"],
            json!({"player_id": "sonos-kitchen", "muted": true})
        );

        server.abort();
    }
}
