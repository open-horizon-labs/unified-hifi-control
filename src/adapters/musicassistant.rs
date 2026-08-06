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
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, RwLock};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::adapters::traits::{
    AdapterCommand, AdapterCommandResponse, AdapterContext, AdapterLogic,
};
use crate::adapters::{AdapterHandle, RetryConfig};
use crate::bus::{
    BusEvent, NowPlaying, PlaybackState, PrefixedZoneId, SharedBus, VolumeControl, VolumeScale,
    Zone,
};

const DEFAULT_PORT: u16 = 8095;
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Configuration for a Music Assistant server.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct MusicAssistantConfig {
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// A long-lived MA access token.  The token is never emitted in logs or
    /// zone state; callers are responsible for storing it securely.
    pub token: String,
    #[serde(default)]
    pub tls: bool,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

impl MusicAssistantConfig {
    fn base_url(&self) -> String {
        let scheme = if self.tls { "https" } else { "http" };
        format!("{}://{}:{}/api", scheme, self.host, self.port)
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
/// MA player discovery and control are available through its authenticated
/// JSON API.  Library search and queue/playlist operations are deliberately
/// left to follow-up capabilities: the adapter's initial contract is player
/// discovery, now-playing state, and transport/volume control.
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

crate::impl_startable!(MusicAssistantAdapter, "musicassistant", is_configured);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_to_ma_port_and_builds_api_url() {
        let config: MusicAssistantConfig = serde_json::from_value(json!({
            "host": "ma.local",
            "token": "secret"
        }))
        .unwrap_or_default();
        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.base_url(), "http://ma.local:8095/api");
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
}
