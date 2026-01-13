//! LMS (Logitech Media Server) JSON-RPC Client
//!
//! Implements the JSON-RPC protocol over HTTP.
//! Documentation: http://HOST:9000/html/docs/cli-api.html

use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::interval;

use crate::bus::{BusEvent, SharedBus};

const DEFAULT_PORT: u16 = 9000;
const POLL_INTERVAL: Duration = Duration::from_secs(2);

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
            players: HashMap::new(),
        }
    }
}

/// LMS Adapter
pub struct LmsAdapter {
    state: Arc<RwLock<LmsState>>,
    client: Client,
    bus: SharedBus,
}

impl LmsAdapter {
    pub fn new(bus: SharedBus) -> Self {
        Self {
            state: Arc::new(RwLock::new(LmsState::default())),
            client: Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("Failed to create HTTP client"),
            bus,
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
        let mut state = self.state.write().await;
        state.host = Some(host);
        state.port = port.unwrap_or(DEFAULT_PORT);
        state.username = username;
        state.password = password;
        state.connected = false;
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

    /// Get base URL
    async fn base_url(&self) -> Result<String> {
        let state = self.state.read().await;
        let host = state
            .host
            .as_ref()
            .ok_or_else(|| anyhow!("LMS host not configured"))?;
        Ok(format!("http://{}:{}", host, state.port))
    }

    /// Execute JSON-RPC command
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

    /// Get list of all players
    pub async fn get_players(&self) -> Result<Vec<LmsPlayer>> {
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

    /// Get player status
    pub async fn get_player_status(&self, player_id: &str) -> Result<LmsPlayer> {
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

    /// Start polling for player updates
    pub async fn start(&self) -> Result<()> {
        if !self.is_configured().await {
            return Err(anyhow!("LMS not configured"));
        }

        // Initial update
        self.update_players().await?;

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

        // Spawn polling task
        let state = self.state.clone();
        let bus = self.bus.clone();
        let adapter = self.clone_for_polling();

        tokio::spawn(async move {
            let mut poll_interval = interval(POLL_INTERVAL);

            loop {
                poll_interval.tick().await;

                // Check if we should stop
                let connected = { state.read().await.connected };
                if !connected {
                    tracing::info!("LMS polling stopped");
                    break;
                }

                if let Err(e) = adapter.update_players().await {
                    tracing::error!("Failed to update LMS players: {}", e);
                }
            }
        });

        Ok(())
    }

    fn clone_for_polling(&self) -> LmsAdapterPoller {
        LmsAdapterPoller {
            state: self.state.clone(),
            client: self.client.clone(),
            bus: self.bus.clone(),
        }
    }

    /// Update cached player information
    pub async fn update_players(&self) -> Result<()> {
        let base_url = self.base_url().await?;
        let players = self.get_players().await?;

        let previous_ids: std::collections::HashSet<String> =
            { self.state.read().await.players.keys().cloned().collect() };

        for mut player in players {
            match self.get_player_status(&player.playerid).await {
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

            let mut state = self.state.write().await;
            state.players.insert(player.playerid.clone(), player);
        }

        // Emit events for player set changes
        let current_ids: std::collections::HashSet<String> =
            { self.state.read().await.players.keys().cloned().collect() };

        if previous_ids != current_ids {
            // Log zone changes for debugging (events emitted via bus on player state change)
            let added: Vec<_> = current_ids.difference(&previous_ids).collect();
            let removed: Vec<_> = previous_ids.difference(&current_ids).collect();
            if !added.is_empty() {
                tracing::debug!("LMS players added: {:?}", added);
            }
            if !removed.is_empty() {
                tracing::debug!("LMS players removed: {:?}", removed);
            }
        }

        Ok(())
    }

    /// Stop polling
    pub async fn stop(&self) {
        let host = {
            let mut state = self.state.write().await;
            state.connected = false;
            state.host.clone()
        };

        if let Some(host) = host {
            self.bus.publish(BusEvent::LmsDisconnected { host });
        }
    }

    /// Control player
    pub async fn control(&self, player_id: &str, command: &str, value: Option<i32>) -> Result<()> {
        let params: Vec<Value> = match command {
            "play" => vec![json!("play")],
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

        self.execute(Some(player_id), params).await?;

        // Update status after command
        let player_id = player_id.to_string();
        let state = self.state.clone();
        let adapter = self.clone_for_polling();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(status) = adapter.get_player_status(&player_id).await {
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
        let base_url = self.base_url().await?;

        let suffix = match (width, height) {
            (Some(w), Some(h)) => format!("cover_{}x{}.jpg", w, h),
            (Some(w), None) => format!("cover_{}x{}.jpg", w, w),
            _ => "cover".to_string(),
        };

        Ok(format!("{}/music/{}/{}", base_url, coverid, suffix))
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

/// Internal polling helper
struct LmsAdapterPoller {
    state: Arc<RwLock<LmsState>>,
    client: Client,
    bus: SharedBus,
}

impl LmsAdapterPoller {
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
            .and_then(|v| v.as_str().or_else(|| v.as_i64().map(|_| "")))
            .map(|s| s.to_string())
            .or_else(|| {
                playlist_loop
                    .get("coverid")
                    .and_then(|v| v.as_i64())
                    .map(|n| n.to_string())
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

    async fn update_players(&self) -> Result<()> {
        let result = self
            .execute(None, vec![json!("players"), json!(0), json!(100)])
            .await?;

        let players_loop = result
            .get("players_loop")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        for p in players_loop {
            let playerid = p
                .get("playerid")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if playerid.is_empty() {
                continue;
            }

            let mut player = LmsPlayer {
                playerid: playerid.clone(),
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
            };

            if let Ok(status) = self.get_player_status(&playerid).await {
                player.state = status.state;
                player.mode = status.mode;
                player.volume = status.volume;
                player.time = status.time;
                player.duration = status.duration;
                player.title = status.title;
                player.artist = status.artist;
                player.album = status.album;
                player.artwork_url = status.artwork_url;
                player.coverid = status.coverid;
            }

            let mut state = self.state.write().await;
            state.players.insert(playerid, player);
        }

        Ok(())
    }
}
