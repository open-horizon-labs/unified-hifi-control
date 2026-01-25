//! LMS (Logitech Media Server) JSON-RPC Client
//!
//! Implements the JSON-RPC protocol over HTTP and CLI event subscription over TCP.
//! Documentation: http://HOST:9000/html/docs/cli-api.html
//!
//! ## Architecture (Issue #165)
//!
//! This module contains two logically separate concerns that share state:
//!
//! 1. **Polling** (HTTP JSON-RPC on port 9000)
//!    - Discovers LMS server and players
//!    - Polls player status at configurable interval
//!    - Primary mechanism - always works
//!
//! 2. **CLI Subscription** (TCP telnet on port 9090)
//!    - Subscribes to real-time events: playlist, mixer, power, client
//!    - Enhancement for faster updates and lower CPU
//!    - Optional - polling continues if CLI unavailable
//!
//! ## Interaction Model
//!
//! The two paths coordinate via a single shared flag: `cli_subscription_active`
//!
//! ```text
//! CLI connects    → flag = true  → Polling slows to 30s interval
//! CLI fails/exits → flag = false → Polling speeds to 2s interval (immediate)
//! CLI reconnects  → flag = true  → Polling slows again
//! ```
//!
//! Currently both run within a single adapter. Future refactor (Issue #165) will
//! split into two independent adapters, each with AdapterHandle retry, sharing
//! only the `cli_subscription_active` flag.
//!
//! ## Configuration
//!
//! - `LMS_POLL_INTERVAL`: Base poll interval in seconds (default: 2)
//! - When CLI active, polling runs at 15x base interval (default: 30s)

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::RwLock;
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::adapters::handle::{AdapterHandle, RetryConfig};
use crate::adapters::lms_discovery::discover_lms_servers;
use crate::adapters::traits::{
    AdapterCommand, AdapterCommandResponse, AdapterContext, AdapterLogic,
};
use crate::bus::{BusEvent, PlaybackState, PrefixedZoneId, SharedBus, VolumeControl, Zone};
use crate::config::{get_config_file_path, read_config_file};

const LMS_CONFIG_FILE: &str = "lms-config.json";
/// Request ID for LMS JSON-RPC calls (aids debugging in LMS logs)
const LMS_REQUEST_ID: i32 = 217;

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
    get_config_file_path(LMS_CONFIG_FILE)
}

const DEFAULT_PORT: u16 = 9000;
/// CLI telnet port for event subscription
const CLI_PORT: u16 = 9090;
/// Default poll interval in seconds (when no subscription active)
const DEFAULT_POLL_INTERVAL_SECS: u64 = 2;
/// Multiplier for poll interval when subscription is active (15x base interval)
const SUBSCRIPTION_INTERVAL_MULTIPLIER: u64 = 15;

/// Get the poll interval from LMS_POLL_INTERVAL env var, or use default
fn get_poll_interval() -> Duration {
    std::env::var("LMS_POLL_INTERVAL")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(DEFAULT_POLL_INTERVAL_SECS))
}

/// Get the poll interval when subscription is active (15x base interval)
fn get_poll_interval_with_subscription() -> Duration {
    let base = get_poll_interval();
    Duration::from_secs(base.as_secs() * SUBSCRIPTION_INTERVAL_MULTIPLIER)
}
/// TCP read timeout for CLI subscription (detect unresponsive LMS)
const CLI_READ_TIMEOUT: Duration = Duration::from_secs(120);

// =============================================================================
// CLI Event Parsing
// =============================================================================

/// Now playing update data for bus emission
struct NowPlayingUpdate {
    player_id: String,
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    image_key: Option<String>,
}

/// Parsed CLI event from LMS
#[derive(Debug, Clone, PartialEq)]
pub enum CliEvent {
    /// Playlist changed (newsong, play, stop, pause, etc.)
    Playlist {
        player_id: String,
        command: String,
        /// Track name for newsong events
        track_name: Option<String>,
        /// Playlist index for newsong events
        index: Option<u32>,
    },
    /// Mixer changed (volume, muting)
    Mixer {
        player_id: String,
        param: String,
        /// Value is None when parsing fails (avoids silent conversion to 0)
        value: Option<i32>,
    },
    /// Power state changed
    Power { player_id: String, state: bool },
    /// Client connected/disconnected/new
    Client { player_id: String, action: String },
    /// Unknown/unparsed event (logged but not acted upon)
    Unknown { raw_line: String },
}

/// Parse a raw CLI event line from LMS
///
/// LMS CLI events are URL-encoded, space-separated lines:
/// `<playerid> <command> <args...>`
///
/// Example events:
/// - `00%3A04%3A20%3Axx%3Ayy%3Azz playlist newsong Track%20Name 5`
/// - `00%3A04%3A20%3Axx%3Ayy%3Azz mixer volume 75`
/// - `00%3A04%3A20%3Axx%3Ayy%3Azz power 1`
/// - `00%3A04%3A20%3Axx%3Ayy%3Azz client new`
pub fn parse_cli_event(line: &str) -> CliEvent {
    let line = line.trim();
    if line.is_empty() {
        return CliEvent::Unknown {
            raw_line: line.to_string(),
        };
    }

    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 2 {
        return CliEvent::Unknown {
            raw_line: line.to_string(),
        };
    }

    // Decode player ID (URL-encoded MAC address)
    let player_id = urlencoding::decode(parts[0])
        .map(|s| s.into_owned())
        .unwrap_or_else(|_| parts[0].to_string());

    let command = parts[1];

    match command {
        "playlist" => {
            let subcommand = parts.get(2).copied().unwrap_or("");
            let track_name = parts.get(3).and_then(|s| {
                urlencoding::decode(s)
                    .ok()
                    .map(|decoded| decoded.into_owned())
            });
            let index = parts.get(4).and_then(|s| s.parse().ok());

            CliEvent::Playlist {
                player_id,
                command: subcommand.to_string(),
                track_name,
                index,
            }
        }
        "mixer" => {
            let param = parts.get(2).copied().unwrap_or("volume");
            let value = parts.get(3).and_then(|s| s.parse().ok());

            CliEvent::Mixer {
                player_id,
                param: param.to_string(),
                value,
            }
        }
        "power" => {
            let state = parts.get(2).is_some_and(|s| *s == "1");

            CliEvent::Power { player_id, state }
        }
        "client" => {
            let action = parts.get(2).copied().unwrap_or("unknown");

            CliEvent::Client {
                player_id,
                action: action.to_string(),
            }
        }
        _ => CliEvent::Unknown {
            raw_line: line.to_string(),
        },
    }
}

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
            "id": LMS_REQUEST_ID,
            "method": "slim.request",
            "params": [player_id.unwrap_or(""), params]
        });

        debug!(
            player_id = player_id.unwrap_or("<server>"),
            params = ?body["params"][1],
            "LMS request"
        );

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

        debug!(
            player_id = player_id.unwrap_or("<server>"),
            result = ?data.get("result"),
            "LMS response"
        );

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
    /// Whether CLI subscription is active (real-time events vs polling)
    pub cli_subscription_active: bool,
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
    /// Whether CLI subscription is active (for reduced polling frequency)
    cli_subscription_active: bool,
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
            cli_subscription_active: false,
        }
    }
}

/// LMS Adapter
#[derive(Clone)]
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
        #[allow(clippy::expect_used)] // HTTP client creation only fails if TLS setup fails
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
    /// Issue #76: Uses read_config_file for backwards-compatible fallback
    fn load_config_sync(&self) {
        // read_config_file checks subdir first, falls back to root for legacy files
        if let Some(content) = read_config_file(LMS_CONFIG_FILE) {
            match serde_json::from_str::<SavedLmsConfig>(&content) {
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

    /// Attempt auto-discovery and configure if exactly one server is found.
    /// Returns Ok(true) if auto-configured, Ok(false) if no single server found, Err on failure.
    ///
    /// Only auto-configures if:
    /// - No existing configuration (host is None)
    /// - Exactly one LMS server responds to discovery
    pub async fn auto_discover_and_configure(&self) -> Result<bool> {
        // Don't auto-configure if already configured
        if self.is_configured().await {
            tracing::debug!("LMS already configured, skipping auto-discovery");
            return Ok(false);
        }

        tracing::info!("Attempting LMS auto-discovery...");

        let servers = discover_lms_servers(None).await?;

        match servers.len() {
            0 => {
                tracing::info!("LMS auto-discovery: no servers found");
                Ok(false)
            }
            1 => {
                let server = &servers[0];
                tracing::info!(
                    "LMS auto-discovery: found single server '{}' at {}:{}",
                    server.name,
                    server.host,
                    server.json_port
                );
                // Auto-configure with discovered settings
                self.configure(server.host.clone(), Some(server.json_port), None, None)
                    .await;
                Ok(true)
            }
            n => {
                tracing::info!(
                    "LMS auto-discovery: found {} servers, not auto-configuring (manual selection required)",
                    n
                );
                for server in &servers {
                    tracing::info!(
                        "  - '{}' at {}:{}",
                        server.name,
                        server.host,
                        server.json_port
                    );
                }
                Ok(false)
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
            cli_subscription_active: state.cli_subscription_active,
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
        // Check if already running and set running=true atomically to prevent race
        {
            let mut state = self.state.write().await;
            if state.running {
                return Ok(());
            }
            state.running = true;
        }

        // Create fresh cancellation token for this run (previous token may be cancelled)
        let shutdown = {
            let mut token = self.shutdown.write().await;
            *token = CancellationToken::new();
            token.clone()
        };

        // Create AdapterHandle and spawn run_with_retry
        let adapter = self.clone();
        let bus = self.bus.clone();
        let handle = AdapterHandle::new(adapter, bus, shutdown);

        tokio::spawn(async move { handle.run_with_retry(RetryConfig::default()).await });

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
            // Per real-world testing (issue #68), "play" handles both start and resume.
            // No need to check cached state - just send the command directly.
            "play" => vec![json!("play")],
            // "pause" without args toggles pause state - matches expected UI behavior
            "pause" => vec![json!("pause")],
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

    /// Change volume (f32 for fractional step support)
    pub async fn change_volume(&self, player_id: &str, value: f32, relative: bool) -> Result<()> {
        let command = if relative { "vol_rel" } else { "vol_abs" };
        // LMS uses integer volume 0-100, round at the last moment
        self.control(player_id, command, Some(value.round() as i32))
            .await
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
            // LMS hardcodes $increment = 2.5 in Slim/Player/Client.pm:755
            // This is not queryable via CLI/JSON-RPC, so we use the constant.
            step: 2.5,
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
        // LMS always allows playback controls when powered and connected
        is_play_allowed: player.state != "playing",
        is_pause_allowed: player.state == "playing",
        is_next_allowed: true,
        is_previous_allowed: true,
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

    // Collect updates to emit after releasing the lock
    let mut now_playing_updates: Vec<NowPlayingUpdate> = Vec::new();
    // LmsPlayerStateChanged: (player_id, state)
    let mut state_updates: Vec<(String, String)> = Vec::new();
    // VolumeChanged: (player_id, volume)
    let mut volume_updates: Vec<(String, i32)> = Vec::new();

    // Helper to convert empty strings to None (metadata cleared)
    let to_option = |s: &str| {
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    };

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

        // Check what changed for this player
        let (now_playing_changed, state_changed, volume_changed) = {
            let s = state.read().await;
            if let Some(old_player) = s.players.get(&player.playerid) {
                let np_changed = old_player.title != player.title
                    || old_player.artist != player.artist
                    || old_player.album != player.album
                    || old_player.artwork_url != player.artwork_url
                    || old_player.coverid != player.coverid;
                let state_changed = old_player.state != player.state;
                let volume_changed = old_player.volume != player.volume;
                (np_changed, state_changed, volume_changed)
            } else {
                // New player - will be handled by ZoneDiscovered
                (false, false, false)
            }
        };

        if now_playing_changed {
            // Emit even when metadata clears (all fields empty) so UI can update
            now_playing_updates.push(NowPlayingUpdate {
                player_id: player.playerid.clone(),
                title: to_option(&player.title),
                artist: to_option(&player.artist),
                album: to_option(&player.album),
                image_key: player.artwork_url.clone().or(player.coverid.clone()),
            });
        }

        if state_changed {
            state_updates.push((player.playerid.clone(), player.state.clone()));
        }

        if volume_changed {
            volume_updates.push((player.playerid.clone(), player.volume));
        }

        let mut s = state.write().await;
        s.players.insert(player.playerid.clone(), player);
    }

    // Emit NowPlayingChanged events for updated players (including metadata clearing)
    for update in now_playing_updates {
        debug!(
            "Polling detected now_playing change for {}: {:?}",
            update.player_id,
            update.title.as_deref().unwrap_or("<cleared>")
        );
        bus.publish(BusEvent::NowPlayingChanged {
            zone_id: PrefixedZoneId::lms(&update.player_id),
            title: update.title,
            artist: update.artist,
            album: update.album,
            image_key: update.image_key,
        });
    }

    // Emit LmsPlayerStateChanged events for state changes (play/pause/stop)
    for (player_id, state) in state_updates {
        debug!("Polling detected state change for {}: {}", player_id, state);
        bus.publish(BusEvent::LmsPlayerStateChanged { player_id, state });
    }

    // Emit VolumeChanged events for volume changes
    for (player_id, volume) in volume_updates {
        debug!(
            "Polling detected volume change for {}: {}",
            player_id, volume
        );
        bus.publish(BusEvent::VolumeChanged {
            output_id: format!("lms:{}", player_id),
            value: volume as f32,
            is_muted: false, // LMS doesn't expose mute via JSON-RPC
        });
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
                zone_id: PrefixedZoneId::lms(player_id),
            });
        }
    }

    Ok(())
}

// =============================================================================
// CLI Subscription (Event-Driven Updates)
// =============================================================================

/// Maximum consecutive poll failures before triggering restart
const MAX_CONSECUTIVE_POLL_FAILURES: u32 = 3;

/// Run the polling loop (extracted helper for AdapterLogic)
/// Returns Err after consecutive failures to trigger adapter restart
async fn run_polling_loop(
    state: Arc<RwLock<LmsState>>,
    bus: SharedBus,
    rpc: LmsRpc,
    shutdown: CancellationToken,
) -> Result<()> {
    // Start with fast polling; will switch to slow when subscription is active
    let mut current_interval = get_poll_interval();
    let mut poll_timer = interval(current_interval);
    let mut consecutive_failures: u32 = 0;

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                info!("LMS polling shutting down");
                break;
            }
            _ = poll_timer.tick() => {
                // Check if we need to adjust polling interval
                let subscription_active = state.read().await.cli_subscription_active;
                let target_interval = if subscription_active {
                    get_poll_interval_with_subscription()
                } else {
                    get_poll_interval()
                };

                if target_interval != current_interval {
                    debug!(
                        "Adjusting poll interval: {:?} -> {:?} (subscription_active={})",
                        current_interval, target_interval, subscription_active
                    );
                    current_interval = target_interval;
                    poll_timer = interval(current_interval);
                }

                match update_players_internal(&rpc, &state, &bus).await {
                    Ok(()) => {
                        // Reset failure counter on success
                        if consecutive_failures > 0 {
                            debug!("LMS poll succeeded, resetting failure counter");
                            consecutive_failures = 0;
                        }
                    }
                    Err(e) => {
                        consecutive_failures += 1;
                        if consecutive_failures >= MAX_CONSECUTIVE_POLL_FAILURES {
                            tracing::error!(
                                "LMS poll failed {} consecutive times, triggering restart: {}",
                                consecutive_failures, e
                            );
                            return Err(anyhow!("LMS unreachable after {} consecutive poll failures", consecutive_failures));
                        } else {
                            warn!(
                                "LMS poll failed ({}/{}): {}",
                                consecutive_failures, MAX_CONSECUTIVE_POLL_FAILURES, e
                            );
                        }
                    }
                }
            }
        }
    }

    info!("LMS polling stopped");
    Ok(())
}

/// Run CLI subscription once (calls connect_and_subscribe directly)
async fn run_cli_subscription_once(
    host: &str,
    state: &Arc<RwLock<LmsState>>,
    bus: &SharedBus,
    rpc: &LmsRpc,
    shutdown: &CancellationToken,
) -> Result<()> {
    info!("Connecting to LMS CLI at {}:{}", host, CLI_PORT);
    connect_and_subscribe(host, state, bus, rpc, shutdown).await
}

/// Connect to LMS CLI and process events
async fn connect_and_subscribe(
    host: &str,
    state: &Arc<RwLock<LmsState>>,
    bus: &SharedBus,
    rpc: &LmsRpc,
    shutdown: &CancellationToken,
) -> Result<()> {
    let addr = format!("{}:{}", host, CLI_PORT);
    let stream = TcpStream::connect(&addr).await?;

    info!("Connected to LMS CLI at {}", addr);

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Send subscription command
    // Subscribe to: playlist, mixer, power, client events
    let subscribe_cmd = "subscribe playlist,mixer,power,client\n";
    writer.write_all(subscribe_cmd.as_bytes()).await?;
    writer.flush().await?;

    info!("Subscribed to LMS CLI events");

    // Mark subscription as active
    {
        let mut s = state.write().await;
        s.cli_subscription_active = true;
    }

    // Process events
    let mut line = String::new();

    loop {
        line.clear();

        tokio::select! {
            _ = shutdown.cancelled() => {
                info!("CLI subscription received shutdown signal");
                return Ok(());
            }
            result = tokio::time::timeout(CLI_READ_TIMEOUT, reader.read_line(&mut line)) => {
                match result {
                    Ok(Ok(0)) => {
                        // EOF - connection closed
                        return Err(anyhow!("LMS CLI connection closed"));
                    }
                    Ok(Ok(_)) => {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            debug!("CLI event: {}", trimmed);
                            handle_cli_event(trimmed, state, bus, rpc).await;
                        }
                    }
                    Ok(Err(e)) => {
                        return Err(anyhow!("CLI read error: {}", e));
                    }
                    Err(_) => {
                        // Timeout - LMS may be unresponsive
                        return Err(anyhow!("CLI read timeout after {:?}", CLI_READ_TIMEOUT));
                    }
                }
            }
        }
    }
}

/// Handle a parsed CLI event
async fn handle_cli_event(
    line: &str,
    state: &Arc<RwLock<LmsState>>,
    bus: &SharedBus,
    rpc: &LmsRpc,
) {
    let event = parse_cli_event(line);

    match event {
        CliEvent::Playlist {
            player_id, command, ..
        } => {
            debug!("Playlist event for {}: {}", player_id, command);

            // Refresh player status on playlist changes
            match rpc.get_player_status(&player_id).await {
                Ok(status) => {
                    let zone_id = PrefixedZoneId::lms(&player_id);

                    // Update cached state
                    {
                        let mut s = state.write().await;
                        if let Some(player) = s.players.get_mut(&player_id) {
                            player.state = status.state.clone();
                            player.mode = status.mode.clone();
                            player.volume = status.volume;
                            player.time = status.time;
                            player.duration = status.duration;
                            player.title = status.title.clone();
                            player.artist = status.artist.clone();
                            player.album = status.album.clone();
                            player.artwork_url = status.artwork_url.clone();
                            player.coverid = status.coverid.clone();
                        }
                    }

                    // Publish bus events
                    bus.publish(BusEvent::LmsPlayerStateChanged {
                        player_id: player_id.clone(),
                        state: status.state.clone(),
                    });

                    if !status.title.is_empty() {
                        bus.publish(BusEvent::NowPlayingChanged {
                            zone_id,
                            title: Some(status.title),
                            artist: Some(status.artist),
                            album: Some(status.album),
                            image_key: status.artwork_url.or(status.coverid),
                        });
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to refresh player status after playlist event: {}",
                        e
                    );
                }
            }
        }
        CliEvent::Mixer {
            player_id,
            param,
            value,
        } => {
            // Only process mixer events when value was successfully parsed
            let Some(value) = value else {
                debug!(
                    "Ignoring mixer event with unparseable value for {}: {}",
                    player_id, param
                );
                return;
            };

            if param == "volume" {
                debug!("Volume change for {}: {}", player_id, value);

                // Update cached state
                {
                    let mut s = state.write().await;
                    if let Some(player) = s.players.get_mut(&player_id) {
                        player.volume = value;
                    }
                }

                // Publish volume changed event
                bus.publish(BusEvent::VolumeChanged {
                    output_id: player_id,
                    value: value as f32,
                    is_muted: false,
                });
            } else if param == "muting" {
                let is_muted = value != 0;
                debug!("Mute change for {}: {}", player_id, is_muted);

                // Get current volume from cache for the event
                let current_volume = {
                    let s = state.read().await;
                    s.players.get(&player_id).map(|p| p.volume).unwrap_or(0)
                };

                // Publish volume changed event with mute state
                bus.publish(BusEvent::VolumeChanged {
                    output_id: player_id,
                    value: current_volume as f32,
                    is_muted,
                });
            }
        }
        CliEvent::Power {
            player_id,
            state: power_state,
        } => {
            debug!("Power change for {}: {}", player_id, power_state);

            // Update cached state
            {
                let mut s = state.write().await;
                if let Some(player) = s.players.get_mut(&player_id) {
                    player.power = power_state;
                }
            }

            // Publish state change
            // When power turns on, we don't know the actual playback state yet
            // When power turns off, playback is effectively stopped
            if !power_state {
                bus.publish(BusEvent::LmsPlayerStateChanged {
                    player_id,
                    state: "stopped".to_string(),
                });
            }
        }
        CliEvent::Client { player_id, action } => {
            debug!("Client event for {}: {}", player_id, action);

            match action.as_str() {
                "new" | "reconnect" => {
                    // Get status and player name before locking state
                    let Ok(status) = rpc.get_player_status(&player_id).await else {
                        return;
                    };

                    // Try to get player name from existing state first (for reconnect)
                    let existing_name = {
                        let s = state.read().await;
                        s.players
                            .get(&player_id)
                            .map(|p| p.name.clone())
                            .filter(|n| !n.is_empty())
                    };

                    // If no existing name, fetch from player list; fall back to player_id
                    let player_name = match existing_name {
                        Some(name) => name,
                        None => match rpc.get_players().await {
                            Ok(players) => players
                                .iter()
                                .find(|p| p.playerid == player_id)
                                .map(|p| p.name.clone())
                                .filter(|n| !n.is_empty())
                                .unwrap_or_else(|| player_id.clone()),
                            Err(_) => player_id.clone(),
                        },
                    };

                    // Now lock and update state
                    let mut s = state.write().await;
                    let is_new = !s.players.contains_key(&player_id);

                    // Create or update player
                    let player = LmsPlayer {
                        playerid: player_id.clone(),
                        name: player_name,
                        connected: true,
                        power: status.power,
                        state: status.state,
                        mode: status.mode,
                        volume: status.volume,
                        title: status.title,
                        artist: status.artist,
                        album: status.album,
                        ..Default::default()
                    };

                    s.players.insert(player_id.clone(), player.clone());
                    drop(s);

                    if is_new {
                        let zone = lms_player_to_zone(&player);
                        bus.publish(BusEvent::ZoneDiscovered { zone });
                    }
                }
                "disconnect" => {
                    // Client disconnected
                    let mut s = state.write().await;
                    if let Some(player) = s.players.get_mut(&player_id) {
                        player.connected = false;
                    }
                }
                _ => {}
            }
        }
        CliEvent::Unknown { raw_line } => {
            // Log unknown events at trace level for debugging
            tracing::trace!("Unknown CLI event: {}", raw_line);
        }
    }
}

// =============================================================================
// AdapterLogic Implementation
// =============================================================================

#[async_trait]
impl AdapterLogic for LmsAdapter {
    fn prefix(&self) -> &'static str {
        "lms"
    }

    async fn run(&self, ctx: AdapterContext) -> Result<()> {
        // Check if configured, if not try auto_discover_and_configure()
        if !self.is_configured().await {
            match self.auto_discover_and_configure().await {
                Ok(true) => {
                    tracing::info!("LMS auto-configured via discovery");
                }
                Ok(false) => {
                    return Err(anyhow!(
                        "LMS not configured and auto-discovery did not find exactly one server. \
                         Configure manually via POST /lms/configure or use GET /lms/discover to see available servers."
                    ));
                }
                Err(e) => {
                    tracing::warn!("LMS auto-discovery failed: {}", e);
                    return Err(anyhow!(
                        "LMS not configured and auto-discovery failed: {}. \
                         Configure manually via POST /lms/configure.",
                        e
                    ));
                }
            }
        }

        // Initial update
        self.update_players().await?;

        // Set state.connected = true and state.running = true
        {
            let mut state = self.state.write().await;
            state.connected = true;
            state.running = true;
        }

        let host = {
            let state = self.state.read().await;
            state.host.clone().unwrap_or_default()
        };

        info!("LMS client connected to {}", host);
        ctx.bus
            .publish(BusEvent::LmsConnected { host: host.clone() });

        // Spawn polling task - starts at FAST interval (2s) since CLI not yet active
        // Will switch to SLOW interval (30s) when CLI connects successfully
        let polling_state = self.state.clone();
        let polling_bus = ctx.bus.clone();
        let polling_rpc = self.rpc.clone();
        let polling_shutdown = ctx.shutdown.clone();
        let polling_task = tokio::spawn(async move {
            run_polling_loop(polling_state, polling_bus, polling_rpc, polling_shutdown).await
        });

        // Attempt CLI subscription - failure is NON-FATAL, polling continues
        // CLI provides real-time events; if unavailable, polling handles everything
        //
        // DESIGN CHOICE: CLI does NOT retry independently within this adapter run.
        // Rationale (see PR #164 for full discussion):
        // - Polling is the primary mechanism and always works
        // - CLI is an optimization, not a requirement
        // - If CLI fails, polling switches to fast interval (2s) and handles updates
        // - CLI gets a fresh attempt on next adapter restart (via AdapterHandle retry)
        // - This avoids duplicate retry logic (AdapterHandle already handles retries)
        // - Simpler code, single source of retry policy
        let cli_task = {
            let cli_state = self.state.clone();
            let cli_bus = ctx.bus.clone();
            let cli_rpc = self.rpc.clone();
            let cli_shutdown = ctx.shutdown.clone();
            let cli_host = host.clone();
            tokio::spawn(async move {
                match run_cli_subscription_once(
                    &cli_host,
                    &cli_state,
                    &cli_bus,
                    &cli_rpc,
                    &cli_shutdown,
                )
                .await
                {
                    Ok(()) => {
                        info!("CLI subscription ended cleanly");
                    }
                    Err(e) => {
                        warn!(
                            "CLI subscription failed: {}. Continuing with polling only.",
                            e
                        );
                    }
                }
                // Always reset cli_subscription_active so polling switches to fast interval
                let mut state = cli_state.write().await;
                state.cli_subscription_active = false;
            })
        };

        // Wait for polling to complete (or fail) - CLI runs independently
        // Only polling failure triggers adapter restart
        let result = tokio::select! {
            _ = ctx.shutdown.cancelled() => {
                info!("LMS adapter received shutdown signal");
                Ok(())
            }
            result = polling_task => {
                // Polling completed or failed
                match result {
                    Ok(r) => r,
                    Err(e) => Err(anyhow!("Polling task panicked: {}", e)),
                }
            }
        };

        // Clean up CLI task
        cli_task.abort();
        let _ = cli_task.await;

        // Clean up state on exit
        {
            let mut state = self.state.write().await;
            state.connected = false;
            state.running = false;
            state.cli_subscription_active = false;
        }

        // Publish LmsDisconnected
        ctx.bus.publish(BusEvent::LmsDisconnected { host });

        result
    }

    async fn handle_command(
        &self,
        zone_id: &str,
        command: AdapterCommand,
    ) -> Result<AdapterCommandResponse> {
        // Extract player_id from zone_id (remove "lms:" prefix)
        let player_id = zone_id.strip_prefix("lms:").unwrap_or(zone_id);

        let result = match command {
            AdapterCommand::Play => self.control(player_id, "play", None).await,
            AdapterCommand::Pause => self.control(player_id, "pause", None).await,
            AdapterCommand::PlayPause => self.control(player_id, "play_pause", None).await,
            AdapterCommand::Stop => self.control(player_id, "stop", None).await,
            AdapterCommand::Next => self.control(player_id, "next", None).await,
            AdapterCommand::Previous => self.control(player_id, "previous", None).await,
            AdapterCommand::VolumeAbsolute(v) => self.control(player_id, "vol_abs", Some(v)).await,
            AdapterCommand::VolumeRelative(v) => self.control(player_id, "vol_rel", Some(v)).await,
            AdapterCommand::Mute(_) => {
                // LMS doesn't have direct mute support via JSON-RPC
                return Ok(AdapterCommandResponse {
                    success: false,
                    error: Some("Mute not supported by LMS adapter".to_string()),
                });
            }
        };

        match result {
            Ok(()) => Ok(AdapterCommandResponse {
                success: true,
                error: None,
            }),
            Err(e) => Ok(AdapterCommandResponse {
                success: false,
                error: Some(e.to_string()),
            }),
        }
    }
}

// Startable trait implementation via macro
crate::impl_startable!(LmsAdapter, "lms", is_configured);

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // CLI Event Parsing Tests (TDD)
    // -------------------------------------------------------------------------

    #[test]
    fn test_parse_cli_event_playlist_newsong() {
        let line = "00%3A04%3A20%3Aaa%3Abb%3Acc playlist newsong Track%20Name 5";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Playlist {
                player_id,
                command,
                track_name,
                index,
            } => {
                assert_eq!(player_id, "00:04:20:aa:bb:cc");
                assert_eq!(command, "newsong");
                assert_eq!(track_name, Some("Track Name".to_string()));
                assert_eq!(index, Some(5));
            }
            _ => panic!("Expected Playlist event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_playlist_play() {
        let line = "00%3A04%3A20%3Axx%3Ayy%3Azz playlist play";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Playlist {
                player_id,
                command,
                track_name,
                index,
            } => {
                assert_eq!(player_id, "00:04:20:xx:yy:zz");
                assert_eq!(command, "play");
                assert_eq!(track_name, None);
                assert_eq!(index, None);
            }
            _ => panic!("Expected Playlist event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_mixer_volume() {
        let line = "00%3A04%3A20%3Aaa%3Abb%3Acc mixer volume 75";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Mixer {
                player_id,
                param,
                value,
            } => {
                assert_eq!(player_id, "00:04:20:aa:bb:cc");
                assert_eq!(param, "volume");
                assert_eq!(value, Some(75));
            }
            _ => panic!("Expected Mixer event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_power_on() {
        let line = "00%3A04%3A20%3Aaa%3Abb%3Acc power 1";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Power { player_id, state } => {
                assert_eq!(player_id, "00:04:20:aa:bb:cc");
                assert!(state);
            }
            _ => panic!("Expected Power event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_power_off() {
        let line = "00%3A04%3A20%3Aaa%3Abb%3Acc power 0";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Power { player_id, state } => {
                assert_eq!(player_id, "00:04:20:aa:bb:cc");
                assert!(!state);
            }
            _ => panic!("Expected Power event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_client_new() {
        let line = "00%3A04%3A20%3Aaa%3Abb%3Acc client new";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Client { player_id, action } => {
                assert_eq!(player_id, "00:04:20:aa:bb:cc");
                assert_eq!(action, "new");
            }
            _ => panic!("Expected Client event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_client_disconnect() {
        let line = "00%3A04%3A20%3Aaa%3Abb%3Acc client disconnect";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Client { player_id, action } => {
                assert_eq!(player_id, "00:04:20:aa:bb:cc");
                assert_eq!(action, "disconnect");
            }
            _ => panic!("Expected Client event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_unknown() {
        let line = "00%3A04%3A20%3Aaa%3Abb%3Acc unknown_command arg1 arg2";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Unknown { raw_line } => {
                assert_eq!(raw_line, line);
            }
            _ => panic!("Expected Unknown event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_empty_line() {
        let event = parse_cli_event("");
        assert!(matches!(event, CliEvent::Unknown { .. }));

        let event = parse_cli_event("   ");
        assert!(matches!(event, CliEvent::Unknown { .. }));
    }

    #[test]
    fn test_parse_cli_event_single_token() {
        let event = parse_cli_event("player_only");
        assert!(matches!(event, CliEvent::Unknown { .. }));
    }

    #[test]
    fn test_parse_cli_event_unencoded_player_id() {
        // Some LMS versions might send unencoded player IDs
        let line = "00:04:20:aa:bb:cc mixer volume 50";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Mixer {
                player_id,
                param,
                value,
            } => {
                assert_eq!(player_id, "00:04:20:aa:bb:cc");
                assert_eq!(param, "volume");
                assert_eq!(value, Some(50));
            }
            _ => panic!("Expected Mixer event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_special_characters_in_track_name() {
        // Track name with special characters
        let line = "00%3A04%3A20%3Aaa%3Abb%3Acc playlist newsong Hello%2C%20World%21 0";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Playlist { track_name, .. } => {
                assert_eq!(track_name, Some("Hello, World!".to_string()));
            }
            _ => panic!("Expected Playlist event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_playlist_pause() {
        let line = "00%3A04%3A20%3Aaa%3Abb%3Acc playlist pause 1";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Playlist {
                player_id, command, ..
            } => {
                assert_eq!(player_id, "00:04:20:aa:bb:cc");
                assert_eq!(command, "pause");
            }
            _ => panic!("Expected Playlist event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_playlist_stop() {
        let line = "00%3A04%3A20%3Aaa%3Abb%3Acc playlist stop";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Playlist { command, .. } => {
                assert_eq!(command, "stop");
            }
            _ => panic!("Expected Playlist event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_mixer_muting() {
        let line = "00%3A04%3A20%3Aaa%3Abb%3Acc mixer muting 1";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Mixer { param, value, .. } => {
                assert_eq!(param, "muting");
                assert_eq!(value, Some(1));
            }
            _ => panic!("Expected Mixer event, got {:?}", event),
        }
    }
}
