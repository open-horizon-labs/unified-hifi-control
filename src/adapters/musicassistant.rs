//! Music Assistant HTTP API adapter.
//!
//! Music Assistant is an optional peer integration.  UHC does not use MA as a
//! transport for any other provider; this adapter only exposes players owned
//! by a configured MA server.  MA's JSON API is intentionally used through
//! its documented command boundary (`POST /api`) instead of depending on
//! private Python implementation details.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
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
use tokio_util::sync::CancellationToken;

use crate::adapters::traits::{
    AdapterCommand, AdapterCommandResponse, AdapterContext, AdapterLogic, LibraryAdapter,
    LibrarySearchResult,
};
use crate::adapters::{AdapterHandle, RetryConfig};
use crate::bus::{
    BusEvent, NowPlaying, PlaybackState, PrefixedZoneId, SharedBus, VolumeControl, VolumeScale,
    Zone,
};

const DEFAULT_PORT: u16 = 8095;
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
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
/// and active-queue reads are available through its authenticated JSON API.
/// Browse, playlist management, queue mutation, playback modes, and grouping
/// remain separate capabilities until their MA contracts are wired end to end.
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
            return Err(anyhow!("Music Assistant returned HTTP {status}: {body}"));
        }
        serde_json::from_str(&body)
            .with_context(|| format!("decode Music Assistant response for {command}"))
    }

    async fn discover_players(&self) -> Result<Vec<PlayerSnapshot>> {
        let players: Vec<Value> = self.command("players/all", None).await?;
        players
            .into_iter()
            .map(parse_player)
            .collect::<Result<Vec<_>>>()
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

    async fn queue_id_for_zone(&self, zone_id: &str) -> Result<String> {
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
        queue
            .get("queue_id")
            .and_then(Value::as_str)
            .filter(|queue_id| !queue_id.trim().is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| {
                anyhow!("Music Assistant returned no active queue for player {player_id}")
            })
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

    Ok(PlayerSnapshot {
        id,
        name,
        state,
        available,
        volume,
        muted,
        now_playing,
        seekable,
    })
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

fn now_playing_changed(previous: Option<&NowPlaying>, current: Option<&NowPlaying>) -> bool {
    match (previous, current) {
        (None, None) => false,
        (Some(previous), Some(current)) => {
            previous.title != current.title
                || previous.artist != current.artist
                || previous.album != current.album
                || previous.image_key != current.image_key
                || previous.duration != current.duration
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
        let request = match command {
            AdapterCommand::Play => ("players/cmd/play", None),
            AdapterCommand::Pause => ("players/cmd/pause", None),
            AdapterCommand::PlayPause => ("players/cmd/play_pause", None),
            AdapterCommand::Stop => ("players/cmd/stop", None),
            AdapterCommand::Next => ("players/cmd/next", None),
            AdapterCommand::Previous => ("players/cmd/previous", None),
            AdapterCommand::VolumeAbsolute(value) => (
                "players/cmd/volume_set",
                Some(json!({ "volume_level": value.clamp(0, 100) })),
            ),
            AdapterCommand::VolumeRelative(delta) if delta >= 0 => ("players/cmd/volume_up", None),
            AdapterCommand::VolumeRelative(_) => ("players/cmd/volume_down", None),
            AdapterCommand::Mute(muted) => {
                ("players/cmd/volume_mute", Some(json!({ "muted": muted })))
            }
            AdapterCommand::SetRepeat(_) | AdapterCommand::SetShuffle(_) => {
                return Ok(AdapterCommandResponse {
                    success: false,
                    error: Some(
                        "Repeat and shuffle are not implemented by the Music Assistant adapter"
                            .to_string(),
                    ),
                });
            }
        };

        match self.control(player_id, request.0, request.1).await {
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

    async fn read_queue(&self, zone_id: &str) -> Result<Value> {
        self.read_active_queue(zone_id).await
    }
}

crate::impl_startable!(MusicAssistantAdapter, "musicassistant", is_configured);

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Bytes, extract::State, http::HeaderMap, response::IntoResponse, routing::post, Json,
        Router,
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

        if request["command"] == "players/all" {
            Json(json!([{
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
            }]))
        } else if request["command"] == "player_queues/get_active_queue" {
            // Deliberately differs from player_id: MA grouped children must
            // resolve their active queue rather than infer it from membership.
            Json(json!({"queue_id": "group-living-room", "items": 234}))
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
        assert_eq!(requests.len(), 11);
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
