//! LMS (Logitech Media Server) JSON-RPC Client
//!
//! Implements the JSON-RPC protocol over HTTP.
//! Documentation: http://HOST:9000/html/docs/cli-api.html

use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::interval;
use tokio_util::sync::CancellationToken;

use crate::bus::{BusEvent, PlaybackState, SharedBus, VolumeControl, Zone};
use crate::config::get_config_dir;

const LMS_CONFIG_FILE: &str = "lms-config.json";

/// Saved config for persistence
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedLmsConfig {
    host: String,
    port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<String>,
}

fn config_path() -> PathBuf {
    get_config_dir().join(LMS_CONFIG_FILE)
}

const DEFAULT_PORT: u16 = 9000;
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Shared JSON-RPC client operations for LMS
/// Extracted to avoid code duplication between LmsAdapter and the polling task
#[derive(Clone)]
struct LmsRpc {
    state: Arc<RwLock<LmsState>>,
    client: Client,
}

impl LmsRpc {
    fn new(state: Arc<RwLock<LmsState>>, client: Client) -> Self {
        Self { state, client }
    }

    async fn base_url(&self) -> Result<String> {
        let state = self.state.read().await;
        let host = state
            .host
            .as_ref()
            .ok_or_else(|| anyhow!("LMS host not configured"))?;
        Ok(format!("http://{}:{}", host, state.port))
    }

    async fn execute(&self, player_id: Option<&str>, params: Vec<Value>) -> Result<Value> {
        let base_url = self.base_url().await?;
        let url = format!("{}/jsonrpc.js", base_url);

        let body = json!({
            "id": 1,
            "method": "slim.request",
            "params": [player_id.unwrap_or(""), params]
        });

        let mut request = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body);

        // Add basic auth if configured
        {
            let state = self.state.read().await;
            if let (Some(username), Some(password)) = (&state.username, &state.password) {
                request = request.basic_auth(username, Some(password));
            }
        }

        let response = request.send().await?;

        if !response.status().is_success() {
            return Err(anyhow!("LMS request failed: {}", response.status()));
        }

        let data: Value = response.json().await?;

        if let Some(error) = data.get("error") {
            if !error.is_null() {
                return Err(anyhow!("LMS error: {}", error));
            }
        }

        Ok(data.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn get_player_status(&self, player_id: &str) -> Result<LmsPlayer> {
        let base_url = self.base_url().await?;
        let result = self
            .execute(
                Some(player_id),
                vec![json!("status"), json!("-"), json!(1), json!("tags:aAdltKc")],
            )
            .await?;

        let playlist_loop = result
            .get("playlist_loop")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .cloned()
            .unwrap_or(Value::Null);

        let mode = result
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("stop");
        let state = match mode {
            "play" => "playing",
            "pause" => "paused",
            _ => "stopped",
        };

        // Handle artwork URL
        let mut artwork_url = playlist_loop
            .get("artwork_url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if let Some(ref url) = artwork_url {
            if url.starts_with('/') {
                artwork_url = Some(format!("{}{}", base_url, url));
            }
        }

        let artwork_id = playlist_loop
            .get("coverid")
            .or_else(|| playlist_loop.get("artwork_track_id"))
            .or_else(|| playlist_loop.get("id"))
            .and_then(|v| {
                // Try string first, then try numeric conversion
                v.as_str()
                    .map(|s| s.to_string())
                    .or_else(|| v.as_i64().map(|n| n.to_string()))
            });

        Ok(LmsPlayer {
            playerid: player_id.to_string(),
            state: state.to_string(),
            mode: mode.to_string(),
            power: result.get("power").and_then(|v| v.as_i64()).unwrap_or(0) == 1,
            volume: result
                .get("mixer volume")
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32,
            playlist_tracks: result
                .get("playlist_tracks")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32,
            playlist_cur_index: result
                .get("playlist_cur_index")
                .and_then(|v| v.as_u64())
                .map(|n| n as u32),
            time: result.get("time").and_then(|v| v.as_f64()).unwrap_or(0.0),
            duration: playlist_loop
                .get("duration")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0),
            title: playlist_loop
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            artist: playlist_loop
                .get("artist")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            album: playlist_loop
                .get("album")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            artwork_track_id: artwork_id.clone(),
            coverid: artwork_id,
            artwork_url,
            ..Default::default()
        })
    }

    async fn get_players(&self) -> Result<Vec<LmsPlayer>> {
        let result = self
            .execute(None, vec![json!("players"), json!(0), json!(100)])
            .await?;

        let players_loop = result
            .get("players_loop")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        Ok(players_loop
            .into_iter()
            .map(|p| LmsPlayer {
                playerid: p
                    .get("playerid")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                name: p
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                model: p
                    .get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown")
                    .to_string(),
                connected: p.get("connected").and_then(|v| v.as_i64()).unwrap_or(0) == 1,
                power: p.get("power").and_then(|v| v.as_i64()).unwrap_or(0) == 1,
                ip: p.get("ip").and_then(|v| v.as_str()).map(|s| s.to_string()),
                ..Default::default()
            })
            .collect())
    }
}

/// LMS Player information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LmsPlayer {
    pub playerid: String,
    pub name: String,
    pub model: String,
    pub connected: bool,
    pub power: bool,
    pub ip: Option<String>,
    // Status fields
    pub state: String,
    pub mode: String,
    pub volume: i32,
    pub playlist_tracks: u32,
    pub playlist_cur_index: Option<u32>,
    pub time: f64,
    pub duration: f64,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub artwork_track_id: Option<String>,
    pub coverid: Option<String>,
    pub artwork_url: Option<String>,
}

impl Default for LmsPlayer {
    fn default() -> Self {
        Self {
            playerid: String::new(),
            name: String::new(),
            model: "Unknown".to_string(),
            connected: false,
            power: false,
            ip: None,
            state: "stopped".to_string(),
            mode: "stop".to_string(),
            volume: 0,
            playlist_tracks: 0,
            playlist_cur_index: None,
            time: 0.0,
            duration: 0.0,
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            artwork_track_id: None,
            coverid: None,
            artwork_url: None,
        }
    }
}

/// LMS connection status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LmsStatus {
    pub connected: bool,
    pub host: Option<String>,
    pub port: u16,
    pub player_count: usize,
    pub players: Vec<LmsPlayerInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LmsPlayerInfo {
    pub playerid: String,
    pub name: String,
    pub state: String,
    pub connected: bool,
}

/// Internal state
struct LmsState {
    host: Option<String>,
    port: u16,
    username: Option<String>,
    password: Option<String>,
    connected: bool,
    running: bool,
    players: HashMap<String, LmsPlayer>,
}

impl Default for LmsState {
    fn default() -> Self {
        Self {
            host: None,
            port: DEFAULT_PORT,
            username: None,
            password: None,
            connected: false,
            running: false,
            players: HashMap::new(),
        }
    }
}

/// LMS Adapter
pub struct LmsAdapter {
    state: Arc<RwLock<LmsState>>,
    rpc: LmsRpc,
    bus: SharedBus,
    /// Wrapped in RwLock to allow creating fresh token on restart
    shutdown: Arc<RwLock<CancellationToken>>,
}

impl LmsAdapter {
    pub fn new(bus: SharedBus) -> Self {
        let state = Arc::new(RwLock::new(LmsState::default()));
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("Failed to create HTTP client");
        let rpc = LmsRpc::new(state.clone(), client);
        let adapter = Self {
            state,
            rpc,
            bus,
            shutdown: Arc::new(RwLock::new(CancellationToken::new())),
        };
        // Load saved config synchronously at startup
        adapter.load_config_sync();
        adapter
    }

    /// Load config from disk (sync, for startup)
    fn load_config_sync(&self) {
        let path = config_path();
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => match serde_json::from_str::<SavedLmsConfig>(&content) {
                    Ok(saved) => {
                        // Use try_write to avoid async in sync context
                        if let Ok(mut state) = self.state.try_write() {
                            state.host = Some(saved.host.clone());
                            state.port = saved.port;
                            state.username = saved.username;
                            state.password = saved.password;
                            tracing::info!(
                                "Loaded LMS config from disk: {}:{}",
                                saved.host,
                                saved.port
                            );
                        }
                    }
                    Err(e) => tracing::warn!("Failed to parse LMS config: {}", e),
                },
                Err(e) => tracing::warn!("Failed to read LMS config: {}", e),
            }
        }
    }

    /// Save config to disk
    async fn save_config(&self) {
        let state = self.state.read().await;
        if let Some(ref host) = state.host {
            let saved = SavedLmsConfig {
                host: host.clone(),
                port: state.port,
                username: state.username.clone(),
                password: state.password.clone(),
            };
            let path = config_path();
            // Ensure config directory exists
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match serde_json::to_string_pretty(&saved) {
                Ok(json) => {
                    if let Err(e) = std::fs::write(&path, json) {
                        tracing::error!("Failed to save LMS config: {}", e);
                    } else {
                        tracing::info!("Saved LMS config to disk");
                    }
                }
                Err(e) => tracing::error!("Failed to serialize LMS config: {}", e),
            }
        }
    }

    /// Configure the LMS connection
    pub async fn configure(
        &self,
        host: String,
        port: Option<u16>,
        username: Option<String>,
        password: Option<String>,
    ) {
        {
            let mut state = self.state.write().await;
            state.host = Some(host);
            state.port = port.unwrap_or(DEFAULT_PORT);
            state.username = username;
            state.password = password;
            state.connected = false;
        }
        // Persist to disk
        self.save_config().await;
    }

    /// Check if configured
    pub async fn is_configured(&self) -> bool {
        self.state.read().await.host.is_some()
    }

    /// Get connection status
    pub async fn get_status(&self) -> LmsStatus {
        let state = self.state.read().await;
        LmsStatus {
            connected: state.connected,
            host: state.host.clone(),
            port: state.port,
            player_count: state.players.len(),
            players: state
                .players
                .values()
                .map(|p| LmsPlayerInfo {
                    playerid: p.playerid.clone(),
                    name: p.name.clone(),
                    state: p.state.clone(),
                    connected: p.connected,
                })
                .collect(),
        }
    }

    /// Get list of all players (delegates to shared RPC)
    pub async fn get_players(&self) -> Result<Vec<LmsPlayer>> {
        self.rpc.get_players().await
    }

    /// Get player status (delegates to shared RPC)
    pub async fn get_player_status(&self, player_id: &str) -> Result<LmsPlayer> {
        self.rpc.get_player_status(player_id).await
    }

    /// Start polling for player updates (internal - use Startable trait)
    async fn start_internal(&self) -> Result<()> {
        if !self.is_configured().await {
            return Err(anyhow!("LMS not configured"));
        }

        // Check if already running to prevent double-start
        {
            let mut state = self.state.write().await;
            if state.running {
                return Ok(());
            }
            state.running = true;
        }

        // Initial update - reset running flag on failure so we can retry
        if let Err(e) = self.update_players().await {
            let mut state = self.state.write().await;
            state.running = false;
            return Err(e);
        }

        {
            let mut state = self.state.write().await;
            state.connected = true;
        }

        let host = {
            let state = self.state.read().await;
            state.host.clone().unwrap_or_default()
        };

        tracing::info!("LMS client connected to {}", host);
        self.bus
            .publish(BusEvent::LmsConnected { host: host.clone() });

        // Create fresh cancellation token for this run (previous token may be cancelled)
        let shutdown = {
            let mut token = self.shutdown.write().await;
            *token = CancellationToken::new();
            token.clone()
        };

        // Spawn polling task using shared RPC
        let state = self.state.clone();
        let bus = self.bus.clone();
        let rpc = self.rpc.clone();

        tokio::spawn(async move {
            let mut poll_interval = interval(POLL_INTERVAL);

            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        tracing::info!("LMS polling shutting down");
                        break;
                    }
                    _ = poll_interval.tick() => {
                        if let Err(e) = update_players_internal(&rpc, &state, &bus).await {
                            tracing::error!("Failed to update LMS players: {}", e);
                        }
                    }
                }
            }

            tracing::info!("LMS polling stopped");
        });

        Ok(())
    }

    /// Update cached player information (delegates to shared helper)
    pub async fn update_players(&self) -> Result<()> {
        update_players_internal(&self.rpc, &self.state, &self.bus).await
    }

    /// Stop polling (internal - use Startable trait)
    async fn stop_internal(&self) {
        // Cancel background tasks first
        self.shutdown.read().await.cancel();

        let host = {
            let mut state = self.state.write().await;
            state.connected = false;
            state.running = false;
            state.host.clone()
        };

        if let Some(host) = host {
            self.bus.publish(BusEvent::LmsDisconnected { host });
        }
    }

    /// Control player
    pub async fn control(&self, player_id: &str, command: &str, value: Option<i32>) -> Result<()> {
        let params: Vec<Value> = match command {
            // LMS quirk: "play" starts from stopped but doesn't resume from pause.
            // "pause 0" resumes from pause but is a no-op from stopped.
            // Check cached mode to send the appropriate command.
            "play" => {
                let is_paused = self
                    .state
                    .read()
                    .await
                    .players
                    .get(player_id)
                    .map(|p| p.mode == "pause")
                    .unwrap_or(false);
                if is_paused {
                    vec![json!("pause"), json!(0)]
                } else {
                    vec![json!("play")]
                }
            }
            "pause" => vec![json!("pause"), json!(1)],
            "stop" => vec![json!("stop")],
            "play_pause" => vec![json!("pause")], // Toggle
            "next" => vec![json!("playlist"), json!("index"), json!("+1")],
            "previous" | "prev" => vec![json!("playlist"), json!("index"), json!("-1")],
            "volume" | "vol_abs" => {
                let v = value.unwrap_or(50);
                vec![json!("mixer"), json!("volume"), json!(v)]
            }
            "vol_rel" => {
                let v = value.unwrap_or(0);
                let prefix = if v > 0 { "+" } else { "" };
                vec![
                    json!("mixer"),
                    json!("volume"),
                    json!(format!("{}{}", prefix, v)),
                ]
            }
            _ => return Err(anyhow!("Unknown command: {}", command)),
        };

        self.rpc.execute(Some(player_id), params).await?;

        // Update status after command
        let player_id = player_id.to_string();
        let state = self.state.clone();
        let rpc = self.rpc.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(status) = rpc.get_player_status(&player_id).await {
                let mut state = state.write().await;
                if let Some(player) = state.players.get_mut(&player_id) {
                    player.state = status.state;
                    player.mode = status.mode;
                    player.volume = status.volume;
                    player.time = status.time;
                }
            }
        });

        Ok(())
    }

    /// Get artwork URL for a track
    pub async fn get_artwork_url(
        &self,
        coverid: &str,
        width: Option<u32>,
        height: Option<u32>,
    ) -> Result<String> {
        let base_url = self.rpc.base_url().await?;

        let suffix = match (width, height) {
            (Some(w), Some(h)) => format!("cover_{}x{}.jpg", w, h),
            (Some(w), None) => format!("cover_{}x{}.jpg", w, w),
            _ => "cover".to_string(),
        };

        Ok(format!("{}/music/{}/{}", base_url, coverid, suffix))
    }

    /// Fetch artwork image bytes
    /// If image_key is a URL, fetches directly. Otherwise treats as coverid.
    pub async fn get_artwork(
        &self,
        image_key: &str,
        width: Option<u32>,
        height: Option<u32>,
    ) -> Result<(String, Vec<u8>)> {
        let state = self.state.read().await;
        let username = state.username.clone();
        let password = state.password.clone();
        drop(state);

        // If image_key is a URL, fetch directly
        let url = if image_key.starts_with("http://") || image_key.starts_with("https://") {
            image_key.to_string()
        } else {
            // Otherwise treat as coverid
            self.get_artwork_url(image_key, width, height).await?
        };

        let mut req = self.rpc.client.get(&url);

        // Add basic auth if configured
        if let (Some(ref user), Some(ref pass)) = (username, password) {
            use base64::Engine;
            let auth =
                base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", user, pass));
            req = req.header("Authorization", format!("Basic {}", auth));
        }

        let response = req.send().await?;
        if !response.status().is_success() {
            return Err(anyhow!("Failed to fetch artwork: {}", response.status()));
        }

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("image/jpeg")
            .to_string();

        let body = response.bytes().await?.to_vec();
        Ok((content_type, body))
    }

    /// Get cached player
    pub async fn get_cached_player(&self, player_id: &str) -> Option<LmsPlayer> {
        self.state.read().await.players.get(player_id).cloned()
    }

    /// Get all cached players
    pub async fn get_cached_players(&self) -> Vec<LmsPlayer> {
        self.state.read().await.players.values().cloned().collect()
    }

    /// Change volume
    pub async fn change_volume(&self, player_id: &str, value: i32, relative: bool) -> Result<()> {
        let command = if relative { "vol_rel" } else { "vol_abs" };
        self.control(player_id, command, Some(value)).await
    }
}

/// Convert an LMS player to a unified Zone representation
fn lms_player_to_zone(player: &LmsPlayer) -> Zone {
    Zone {
        zone_id: format!("lms:{}", player.playerid),
        zone_name: player.name.clone(),
        state: PlaybackState::from(player.state.as_str()),
        volume_control: Some(VolumeControl {
            value: player.volume as f32,
            min: 0.0,
            max: 100.0,
            step: 1.0,
            is_muted: false, // LMS doesn't expose mute via JSON-RPC status
            scale: crate::bus::VolumeScale::Percentage,
            output_id: Some(player.playerid.clone()),
        }),
        now_playing: if !player.title.is_empty() {
            Some(crate::bus::NowPlaying {
                title: player.title.clone(),
                artist: player.artist.clone(),
                album: player.album.clone(),
                image_key: player.artwork_url.clone().or(player.coverid.clone()),
                seek_position: Some(player.time),
                duration: Some(player.duration),
                metadata: None,
            })
        } else {
            None
        },
        source: "lms".to_string(),
        is_controllable: player.power && player.connected,
        is_seekable: true,
        last_updated: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
    }
}

/// Shared helper function for updating players from the polling task
/// Uses LmsRpc to avoid code duplication between LmsAdapter and background task
async fn update_players_internal(
    rpc: &LmsRpc,
    state: &Arc<RwLock<LmsState>>,
    bus: &SharedBus,
) -> Result<()> {
    let players = rpc.get_players().await?;

    let previous_ids: std::collections::HashSet<String> =
        { state.read().await.players.keys().cloned().collect() };

    for mut player in players {
        match rpc.get_player_status(&player.playerid).await {
            Ok(status) => {
                player.state = status.state;
                player.mode = status.mode;
                player.power = status.power;
                player.volume = status.volume;
                player.playlist_tracks = status.playlist_tracks;
                player.playlist_cur_index = status.playlist_cur_index;
                player.time = status.time;
                player.duration = status.duration;
                player.title = status.title;
                player.artist = status.artist;
                player.album = status.album;
                player.artwork_track_id = status.artwork_track_id;
                player.coverid = status.coverid;
                player.artwork_url = status.artwork_url;
            }
            Err(e) => {
                tracing::warn!("Failed to get status for player {}: {}", player.playerid, e);
            }
        }

        let mut state = state.write().await;
        state.players.insert(player.playerid.clone(), player);
    }

    // Emit events for player set changes
    let current_ids: std::collections::HashSet<String> =
        { state.read().await.players.keys().cloned().collect() };

    if previous_ids != current_ids {
        let added: Vec<_> = current_ids.difference(&previous_ids).cloned().collect();
        let removed: Vec<_> = previous_ids.difference(&current_ids).cloned().collect();

        // Emit zone discovered events for new players
        for player_id in &added {
            if let Some(player) = state.read().await.players.get(player_id) {
                tracing::debug!("LMS player discovered: {}", player_id);
                let zone = lms_player_to_zone(player);
                bus.publish(BusEvent::ZoneDiscovered { zone });
            }
        }

        // Emit zone removed events
        for player_id in &removed {
            tracing::debug!("LMS player removed: {}", player_id);
            bus.publish(BusEvent::ZoneRemoved {
                zone_id: format!("lms:{}", player_id),
            });
        }
    }

    Ok(())
}

// Startable trait implementation via macro
crate::impl_startable!(LmsAdapter, "lms", is_configured);
