//! HQPlayer Native Protocol Client + HTTP/Web Client for Profiles
//!
//! Implements the TCP/XML control protocol on port 4321 for pipeline control.
//! Also implements HTTP/Digest auth for web UI profile loading (port 8088).
//! Based on Jussi Laako's hqp-control reference implementation.

use anyhow::{anyhow, Result};
use quick_xml::events::{BytesStart, Event};
use quick_xml::Writer;
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Cursor;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{Mutex, RwLock};
use tokio::time::timeout;

use crate::bus::{
    BusEvent, NowPlaying as BusNowPlaying, PlaybackState, SharedBus, TrackMetadata,
    VolumeControl as BusVolumeControl, VolumeScale, Zone as BusZone,
};
use crate::config::{get_config_file_path, read_config_file};

const HQP_CONFIG_FILE: &str = "hqp-config.json";

/// Saved config for persistence (single instance format)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedHqpConfig {
    host: String,
    port: u16,
    web_port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<String>,
}

/// Named instance config (for multi-instance support)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HqpInstanceConfig {
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_web_port")]
    pub web_port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_web_port() -> u16 {
    DEFAULT_WEB_PORT
}

fn hqp_config_path() -> PathBuf {
    get_config_file_path(HQP_CONFIG_FILE)
}

/// Load HQP config from disk (supports both single-object and array formats)
/// Issue #76: Uses read_config_file for backwards-compatible fallback
pub fn load_hqp_configs() -> Vec<HqpInstanceConfig> {
    // read_config_file checks subdir first, falls back to root for legacy files
    let content = match read_config_file(HQP_CONFIG_FILE) {
        Some(c) => c,
        None => return Vec::new(),
    };

    // Try parsing as array first
    if let Ok(configs) = serde_json::from_str::<Vec<HqpInstanceConfig>>(&content) {
        return configs;
    }

    // Fall back to single-object format (legacy)
    if let Ok(single) = serde_json::from_str::<SavedHqpConfig>(&content) {
        return vec![HqpInstanceConfig {
            name: "default".to_string(),
            host: single.host,
            port: single.port,
            web_port: single.web_port,
            username: single.username,
            password: single.password,
        }];
    }

    tracing::warn!("Failed to parse HQP config file");
    Vec::new()
}

/// Save HQP configs to disk (always saves as array)
pub fn save_hqp_configs(configs: &[HqpInstanceConfig]) -> bool {
    let path = hqp_config_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    match serde_json::to_string_pretty(configs) {
        Ok(json) => match std::fs::write(&path, json) {
            Ok(()) => {
                tracing::info!("Saved HQP config ({} instances)", configs.len());
                true
            }
            Err(e) => {
                tracing::error!("Failed to save HQP config: {}", e);
                false
            }
        },
        Err(e) => {
            tracing::error!("Failed to serialize HQP config: {}", e);
            false
        }
    }
}

const DEFAULT_PORT: u16 = 4321;
const DEFAULT_WEB_PORT: u16 = 8088;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(3);
const PROFILE_PATH: &str = "/config/profile/load";
/// Maximum reconnection attempts before giving up
const MAX_RECONNECT_ATTEMPTS: u32 = 3;
/// Delay between reconnection attempts
const RECONNECT_DELAY: Duration = Duration::from_millis(200);

/// HQPlayer state information
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HqpState {
    pub state: u8, // 0=stopped, 1=paused, 2=playing
    pub mode: u8,  // PCM=0, SDM=1
    pub filter: u32,
    pub filter1x: Option<u32>,
    pub filter_nx: Option<u32>,
    pub shaper: u32,
    pub rate: u32,
    pub volume: i32,
    pub active_mode: u8,
    pub active_rate: u32,
    pub invert: bool,
    pub convolution: bool,
    pub repeat: u8, // 0=off, 1=track, 2=all
    pub random: bool,
    pub adaptive: bool,
    pub filter_20k: bool,
    pub matrix_profile: String,
}

/// HQPlayer info
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HqpInfo {
    pub name: String,
    pub product: String,
    pub version: String,
    pub platform: String,
    pub engine: String,
}

/// HQPlayer playback status
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HqpStatus {
    pub state: u8,
    pub track: u32,
    pub track_id: String,
    pub position: u32,
    pub length: u32,
    pub volume: i32,
    pub active_mode: String,
    pub active_filter: String,
    pub active_shaper: String,
    pub active_rate: u32,
    pub active_bits: u32,
    pub active_channels: u32,
    pub samplerate: u32,
    pub bitrate: u32,
}

/// Volume range info
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VolumeRange {
    pub min: i32,
    pub max: i32,
    pub step: i32,
    pub enabled: bool,
    pub adaptive: bool,
}

/// Mode/Filter/Shaper item
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListItem {
    pub index: u32,
    pub name: String,
    pub value: i32, // Can be negative (e.g., -1 for PCM mode)
}

/// Rate item
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateItem {
    pub index: u32,
    pub rate: u32,
}

/// Filter item with arg
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterItem {
    pub index: u32,
    pub name: String,
    pub value: i32, // Filter values can be negative
    pub arg: u32,
}

/// Pipeline settings for a single setting type
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineSetting {
    pub selected: SelectedOption,
    pub options: Vec<SelectOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectedOption {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
}

/// Full pipeline status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineStatus {
    pub status: PipelineState,
    pub volume: PipelineVolume,
    pub settings: PipelineSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineState {
    pub state: String,
    pub mode: String,
    pub active_mode: String,
    pub active_filter: String,
    pub active_shaper: String,
    pub active_rate: u32,
    pub convolution: bool,
    pub invert: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineVolume {
    pub value: i32,
    pub min: i32,
    pub max: i32,
    pub is_fixed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PipelineSettings {
    pub mode: PipelineSetting,
    pub filter1x: PipelineSetting,
    #[serde(rename = "filterNx")]
    pub filter_nx: PipelineSetting,
    pub shaper: PipelineSetting,
    pub samplerate: PipelineSetting,
}

/// HQPlayer connection status for API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HqpConnectionStatus {
    pub connected: bool,
    pub host: Option<String>,
    pub port: u16,
    pub web_port: u16,
    pub info: Option<HqpInfo>,
}

/// Internal connection state
struct HqpConnection {
    stream: BufReader<tokio::net::tcp::OwnedReadHalf>,
    write_half: tokio::net::tcp::OwnedWriteHalf,
}

/// Profile info from web UI
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HqpProfile {
    pub value: String,
    pub title: String,
}

/// Matrix profile info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixProfile {
    pub index: u32,
    pub name: String,
}

/// Internal adapter state
#[allow(dead_code)]
struct HqpAdapterState {
    instance_name: Option<String>,
    host: Option<String>,
    port: u16,
    web_port: u16,
    web_username: Option<String>,
    web_password: Option<String>,
    connected: bool,
    info: Option<HqpInfo>,
    last_state: Option<HqpState>,
    modes: Vec<ListItem>,
    filters: Vec<FilterItem>,
    shapers: Vec<ListItem>,
    rates: Vec<RateItem>,
    // Web client state for profiles
    profiles: Vec<HqpProfile>,
    hidden_fields: HashMap<String, String>,
    config_title: Option<String>,
    digest_auth: Option<DigestAuth>,
    cookies: HashMap<String, String>,
}

/// Digest authentication state
struct DigestAuth {
    realm: String,
    nonce: String,
    qop: String,
    opaque: String,
    algorithm: String,
    nc: u32,
}

impl Default for HqpAdapterState {
    fn default() -> Self {
        Self {
            instance_name: None,
            host: None,
            port: DEFAULT_PORT,
            web_port: DEFAULT_WEB_PORT,
            web_username: None,
            web_password: None,
            connected: false,
            info: None,
            last_state: None,
            modes: Vec::new(),
            filters: Vec::new(),
            shapers: Vec::new(),
            rates: Vec::new(),
            profiles: Vec::new(),
            hidden_fields: HashMap::new(),
            config_title: None,
            digest_auth: None,
            cookies: HashMap::new(),
        }
    }
}

/// HQPlayer adapter
pub struct HqpAdapter {
    state: Arc<RwLock<HqpAdapterState>>,
    connection: Arc<Mutex<Option<HqpConnection>>>,
    http_client: Client,
    bus: SharedBus,
}

impl HqpAdapter {
    pub fn new(bus: SharedBus) -> Self {
        let adapter = Self {
            state: Arc::new(RwLock::new(HqpAdapterState::default())),
            connection: Arc::new(Mutex::new(None)),
            http_client: Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .expect("Failed to create HTTP client"),
            bus,
        };
        // Load saved config synchronously at startup
        adapter.load_config_sync();
        adapter
    }

    /// Load config from disk (sync, for startup)
    fn load_config_sync(&self) {
        let path = hqp_config_path();
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => match serde_json::from_str::<SavedHqpConfig>(&content) {
                    Ok(saved) => {
                        if let Ok(mut state) = self.state.try_write() {
                            state.host = Some(saved.host.clone());
                            state.port = saved.port;
                            state.web_port = saved.web_port;
                            state.web_username = saved.username;
                            state.web_password = saved.password;
                            tracing::info!(
                                "Loaded HQPlayer config from disk: {}:{}",
                                saved.host,
                                saved.port
                            );
                        }
                    }
                    Err(e) => tracing::warn!("Failed to parse HQPlayer config: {}", e),
                },
                Err(e) => tracing::warn!("Failed to read HQPlayer config: {}", e),
            }
        }
    }

    /// Save config to disk
    async fn save_config(&self) {
        let state = self.state.read().await;
        if let Some(ref host) = state.host {
            let saved = SavedHqpConfig {
                host: host.clone(),
                port: state.port,
                web_port: state.web_port,
                username: state.web_username.clone(),
                password: state.web_password.clone(),
            };
            let path = hqp_config_path();
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match serde_json::to_string_pretty(&saved) {
                Ok(json) => {
                    if let Err(e) = std::fs::write(&path, json) {
                        tracing::error!("Failed to save HQPlayer config: {}", e);
                    } else {
                        tracing::info!("Saved HQPlayer config to disk");
                    }
                }
                Err(e) => tracing::error!("Failed to serialize HQPlayer config: {}", e),
            }
        }
    }

    /// Configure the HQPlayer connection
    pub async fn configure(
        &self,
        host: String,
        port: Option<u16>,
        web_port: Option<u16>,
        web_username: Option<String>,
        web_password: Option<String>,
    ) {
        let changed = {
            let mut state = self.state.write().await;
            let port = port.unwrap_or(DEFAULT_PORT);

            let changed = state.host.as_ref() != Some(&host) || state.port != port;
            state.host = Some(host);
            state.port = port;
            state.web_port = web_port.unwrap_or(DEFAULT_WEB_PORT);
            state.web_username = web_username;
            state.web_password = web_password;

            // Reset auth state when reconfiguring
            state.digest_auth = None;
            state.cookies.clear();

            if changed {
                state.connected = false;
            }
            changed
        };

        if changed {
            let mut conn = self.connection.lock().await;
            *conn = None;
        }

        // Persist to disk
        self.save_config().await;
    }

    /// Check if web credentials are configured
    pub async fn has_web_credentials(&self) -> bool {
        let state = self.state.read().await;
        state.host.is_some() && state.web_username.is_some() && state.web_password.is_some()
    }

    /// Check if configured
    pub async fn is_configured(&self) -> bool {
        self.state.read().await.host.is_some()
    }

    /// Get connection status
    pub async fn get_status(&self) -> HqpConnectionStatus {
        let state = self.state.read().await;
        HqpConnectionStatus {
            connected: state.connected,
            host: state.host.clone(),
            port: state.port,
            web_port: state.web_port,
            info: state.info.clone(),
        }
    }

    /// Connect to HQPlayer
    pub async fn connect(&self) -> Result<()> {
        let (host, port) = {
            let state = self.state.read().await;
            let host = state
                .host
                .clone()
                .ok_or_else(|| anyhow!("HQPlayer host not configured"))?;
            (host, state.port)
        };

        let addr = format!("{}:{}", host, port);
        let stream = timeout(CONNECT_TIMEOUT, TcpStream::connect(&addr))
            .await
            .map_err(|_| anyhow!("Connection timeout"))?
            .map_err(|e| anyhow!("Connection failed: {}", e))?;

        let (read_half, write_half) = stream.into_split();
        let reader = BufReader::new(read_half);

        {
            let mut conn = self.connection.lock().await;
            *conn = Some(HqpConnection {
                stream: reader,
                write_half,
            });
        }

        {
            let mut state = self.state.write().await;
            state.connected = true;
        }

        // Get info and cache lists (use inner methods to avoid reconnection loop)
        let info = self.get_info_inner().await?;
        let modes = self.get_modes_inner().await?;
        let filters = self.get_filters_inner().await?;
        let shapers = self.get_shapers_inner().await?;
        let rates = self.get_rates_inner().await?;

        {
            let mut state = self.state.write().await;
            state.info = Some(info.clone());
            state.modes = modes;
            state.filters = filters;
            state.shapers = shapers;
            state.rates = rates;
        }

        tracing::info!("HQPlayer connected: {} v{}", info.name, info.version);
        self.bus
            .publish(BusEvent::HqpConnected { host: host.clone() });

        // Get status and volume range for ZoneDiscovered (using inner methods to avoid recursion)
        let status = self.get_playback_status_inner().await.unwrap_or_default();
        let vol_range = self.get_volume_range_inner().await.unwrap_or_default();

        // Get instance name for zone ID
        let instance_name = {
            let state = self.state.read().await;
            state.instance_name.clone()
        };

        // Emit ZoneDiscovered for this HQPlayer instance
        let zone =
            Self::hqp_status_to_zone(&host, instance_name.as_deref(), &info, &status, &vol_range);
        self.bus.publish(BusEvent::ZoneDiscovered { zone });

        Ok(())
    }

    /// Disconnect
    pub async fn disconnect(&self) {
        let (host, instance_name) = {
            let mut state = self.state.write().await;
            state.connected = false;
            (state.host.clone(), state.instance_name.clone())
        };

        {
            let mut conn = self.connection.lock().await;
            *conn = None;
        }

        if let Some(ref h) = host {
            // Emit ZoneRemoved for this HQPlayer instance
            let zone_id = format!("hqplayer:{}", instance_name.as_deref().unwrap_or(h));
            self.bus.publish(BusEvent::ZoneRemoved { zone_id });

            self.bus
                .publish(BusEvent::HqpDisconnected { host: h.clone() });
        }
    }

    /// Ensure connection is established, reconnecting if needed
    pub async fn ensure_connected(&self) -> Result<()> {
        // Check if already connected
        {
            let conn = self.connection.lock().await;
            if conn.is_some() {
                return Ok(());
            }
        }

        // Not connected, try to connect
        self.connect().await
    }

    /// Mark connection as broken (called on communication errors)
    async fn mark_disconnected(&self) {
        let (host, instance_name) = {
            let mut state = self.state.write().await;
            state.connected = false;
            (state.host.clone(), state.instance_name.clone())
        };

        {
            let mut conn = self.connection.lock().await;
            *conn = None;
        }

        if let Some(ref h) = host {
            tracing::warn!("HQPlayer connection lost to {}", h);
            // Emit ZoneRemoved for this HQPlayer instance
            let zone_id = format!("hqplayer:{}", instance_name.as_deref().unwrap_or(h));
            self.bus.publish(BusEvent::ZoneRemoved { zone_id });
        }
    }

    /// Send command and get response with auto-reconnection
    async fn send_command(&self, xml: &str) -> Result<String> {
        let mut last_error = None;

        for attempt in 0..MAX_RECONNECT_ATTEMPTS {
            // Ensure we're connected
            if let Err(e) = self.ensure_connected().await {
                last_error = Some(e);
                if attempt < MAX_RECONNECT_ATTEMPTS - 1 {
                    tracing::debug!(
                        "HQPlayer connection attempt {} failed, retrying...",
                        attempt + 1
                    );
                    tokio::time::sleep(RECONNECT_DELAY).await;
                }
                continue;
            }

            // Try to send command
            match self.send_command_inner(xml).await {
                Ok(response) => return Ok(response),
                Err(e) => {
                    // Mark as disconnected so next attempt will reconnect
                    self.mark_disconnected().await;
                    last_error = Some(e);

                    if attempt < MAX_RECONNECT_ATTEMPTS - 1 {
                        tracing::debug!(
                            "HQPlayer command failed, reconnecting (attempt {})...",
                            attempt + 1
                        );
                        tokio::time::sleep(RECONNECT_DELAY).await;
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("Failed to send command after retries")))
    }

    /// Inner send command (without retry logic)
    async fn send_command_inner(&self, xml: &str) -> Result<String> {
        let mut conn_guard = self.connection.lock().await;
        let conn = conn_guard
            .as_mut()
            .ok_or_else(|| anyhow!("Not connected"))?;

        // Send command
        conn.write_half.write_all(xml.as_bytes()).await?;
        conn.write_half.write_all(b"\n").await?;
        conn.write_half.flush().await?;

        // Read response - handle both single-line and multi-line XML
        // HQPlayer sends responses that may span multiple lines for complex data
        let mut response = String::new();
        let mut complete = false;

        while !complete {
            let mut line = String::new();
            let read_result = timeout(RESPONSE_TIMEOUT, conn.stream.read_line(&mut line)).await;

            match read_result {
                Ok(Ok(0)) => break, // EOF
                Ok(Ok(_)) => {
                    response.push_str(&line);
                    // Check if response is complete (has closing XML tag or is self-closing)
                    let trimmed = response.trim();
                    if trimmed.ends_with("/>") || (trimmed.contains("</") && trimmed.ends_with(">"))
                    {
                        complete = true;
                    }
                }
                Ok(Err(e)) => return Err(anyhow!("Read error: {}", e)),
                Err(_) => return Err(anyhow!("Response timeout")),
            }
        }

        Ok(response.trim().to_string())
    }

    // =========================================================================
    // Inner methods (used during connect, no auto-reconnect to avoid recursion)
    // =========================================================================

    /// Get HQPlayer info (no reconnection)
    async fn get_info_inner(&self) -> Result<HqpInfo> {
        let xml = Self::build_request("GetInfo", &[]);
        let response = self.send_command_inner(&xml).await?;

        Ok(HqpInfo {
            name: Self::parse_attr(&response, "name").unwrap_or_default(),
            product: Self::parse_attr(&response, "product").unwrap_or_default(),
            version: Self::parse_attr(&response, "version").unwrap_or_default(),
            platform: Self::parse_attr(&response, "platform").unwrap_or_default(),
            engine: Self::parse_attr(&response, "engine").unwrap_or_default(),
        })
    }

    /// Get available modes (no reconnection)
    async fn get_modes_inner(&self) -> Result<Vec<ListItem>> {
        let xml = Self::build_request("GetModes", &[]);
        let response = self.send_command_inner(&xml).await?;

        Ok(Self::parse_items(&response, "ModesItem", |item| ListItem {
            index: Self::parse_attr_u32(item, "index"),
            name: Self::parse_attr(item, "name").unwrap_or_default(),
            value: Self::parse_attr_i32(item, "value"), // Mode values can be negative (-1 for PCM)
        }))
    }

    /// Get available filters (no reconnection)
    async fn get_filters_inner(&self) -> Result<Vec<FilterItem>> {
        let xml = Self::build_request("GetFilters", &[]);
        let response = self.send_command_inner(&xml).await?;

        Ok(Self::parse_items(&response, "FiltersItem", |item| {
            FilterItem {
                index: Self::parse_attr_u32(item, "index"),
                name: Self::parse_attr(item, "name").unwrap_or_default(),
                value: Self::parse_attr_i32(item, "value"),
                arg: Self::parse_attr_u32(item, "arg"),
            }
        }))
    }

    /// Get available shapers (no reconnection)
    async fn get_shapers_inner(&self) -> Result<Vec<ListItem>> {
        let xml = Self::build_request("GetShapers", &[]);
        let response = self.send_command_inner(&xml).await?;

        Ok(Self::parse_items(&response, "ShapersItem", |item| {
            ListItem {
                index: Self::parse_attr_u32(item, "index"),
                name: Self::parse_attr(item, "name").unwrap_or_default(),
                value: Self::parse_attr_i32(item, "value"),
            }
        }))
    }

    /// Get available sample rates (no reconnection)
    async fn get_rates_inner(&self) -> Result<Vec<RateItem>> {
        let xml = Self::build_request("GetRates", &[]);
        let response = self.send_command_inner(&xml).await?;

        Ok(Self::parse_items(&response, "RatesItem", |item| RateItem {
            index: Self::parse_attr_u32(item, "index"),
            rate: Self::parse_attr_u32(item, "rate"),
        }))
    }

    /// Get playback status (no reconnection)
    async fn get_playback_status_inner(&self) -> Result<HqpStatus> {
        let xml = Self::build_request("Status", &[("subscribe", "0")]);
        let response = self.send_command_inner(&xml).await?;

        Ok(HqpStatus {
            state: Self::parse_attr_u32(&response, "state") as u8,
            track: Self::parse_attr_u32(&response, "track"),
            track_id: Self::parse_attr(&response, "track_id").unwrap_or_default(),
            position: Self::parse_attr_u32(&response, "position"),
            length: Self::parse_attr_u32(&response, "length"),
            volume: Self::parse_attr_i32(&response, "volume"),
            active_mode: Self::parse_attr(&response, "active_mode").unwrap_or_default(),
            active_filter: Self::parse_attr(&response, "active_filter").unwrap_or_default(),
            active_shaper: Self::parse_attr(&response, "active_shaper").unwrap_or_default(),
            active_rate: Self::parse_attr_u32(&response, "active_rate"),
            active_bits: Self::parse_attr_u32(&response, "active_bits"),
            active_channels: Self::parse_attr_u32(&response, "active_channels"),
            samplerate: Self::parse_attr_u32(&response, "samplerate"),
            bitrate: Self::parse_attr_u32(&response, "bitrate"),
        })
    }

    /// Get volume range (no reconnection)
    async fn get_volume_range_inner(&self) -> Result<VolumeRange> {
        let xml = Self::build_request("VolumeRange", &[]);
        let response = self.send_command_inner(&xml).await?;

        Ok(VolumeRange {
            min: Self::parse_attr_i32(&response, "min"),
            max: Self::parse_attr_i32(&response, "max"),
            step: Self::parse_attr_i32(&response, "step").max(1),
            enabled: Self::parse_attr_bool(&response, "enabled"),
            adaptive: Self::parse_attr_bool(&response, "adaptive"),
        })
    }

    /// Build XML request
    fn build_request(element: &str, attrs: &[(&str, &str)]) -> String {
        let mut writer = Writer::new(Cursor::new(Vec::new()));

        let mut elem = BytesStart::new(element);
        for (key, value) in attrs {
            elem.push_attribute((*key, *value));
        }

        writer.write_event(Event::Empty(elem)).unwrap();

        format!(
            "<?xml version=\"1.0\"?>{}",
            String::from_utf8(writer.into_inner().into_inner()).unwrap()
        )
    }

    /// Parse XML attribute
    fn parse_attr(xml: &str, attr: &str) -> Option<String> {
        let pattern = format!("{}=\"", attr);
        if let Some(start) = xml.find(&pattern) {
            let rest = &xml[start + pattern.len()..];
            if let Some(end) = rest.find('"') {
                return Some(rest[..end].to_string());
            }
        }
        None
    }

    fn parse_attr_i32(xml: &str, attr: &str) -> i32 {
        Self::parse_attr(xml, attr)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    fn parse_attr_u32(xml: &str, attr: &str) -> u32 {
        Self::parse_attr(xml, attr)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    fn parse_attr_bool(xml: &str, attr: &str) -> bool {
        Self::parse_attr(xml, attr)
            .map(|s| s == "1")
            .unwrap_or(false)
    }

    /// Get HQPlayer info
    pub async fn get_info(&self) -> Result<HqpInfo> {
        let xml = Self::build_request("GetInfo", &[]);
        let response = self.send_command(&xml).await?;

        Ok(HqpInfo {
            name: Self::parse_attr(&response, "name").unwrap_or_default(),
            product: Self::parse_attr(&response, "product").unwrap_or_default(),
            version: Self::parse_attr(&response, "version").unwrap_or_default(),
            platform: Self::parse_attr(&response, "platform").unwrap_or_default(),
            engine: Self::parse_attr(&response, "engine").unwrap_or_default(),
        })
    }

    /// Get current state
    pub async fn get_state(&self) -> Result<HqpState> {
        let xml = Self::build_request("State", &[]);
        let response = self.send_command(&xml).await?;

        Ok(HqpState {
            state: Self::parse_attr_u32(&response, "state") as u8,
            mode: Self::parse_attr_u32(&response, "mode") as u8,
            filter: Self::parse_attr_u32(&response, "filter"),
            filter1x: Self::parse_attr(&response, "filter1x").and_then(|s| s.parse().ok()),
            filter_nx: Self::parse_attr(&response, "filterNx").and_then(|s| s.parse().ok()),
            shaper: Self::parse_attr_u32(&response, "shaper"),
            rate: Self::parse_attr_u32(&response, "rate"),
            volume: Self::parse_attr_i32(&response, "volume"),
            active_mode: Self::parse_attr_u32(&response, "active_mode") as u8,
            active_rate: Self::parse_attr_u32(&response, "active_rate"),
            invert: Self::parse_attr_bool(&response, "invert"),
            convolution: Self::parse_attr_bool(&response, "convolution"),
            repeat: Self::parse_attr_u32(&response, "repeat") as u8,
            random: Self::parse_attr_bool(&response, "random"),
            adaptive: Self::parse_attr_bool(&response, "adaptive"),
            filter_20k: Self::parse_attr_bool(&response, "filter_20k"),
            matrix_profile: Self::parse_attr(&response, "matrix_profile").unwrap_or_default(),
        })
    }

    /// Get playback status
    pub async fn get_playback_status(&self) -> Result<HqpStatus> {
        let xml = Self::build_request("Status", &[("subscribe", "0")]);
        let response = self.send_command(&xml).await?;

        Ok(HqpStatus {
            state: Self::parse_attr_u32(&response, "state") as u8,
            track: Self::parse_attr_u32(&response, "track"),
            track_id: Self::parse_attr(&response, "track_id").unwrap_or_default(),
            position: Self::parse_attr_u32(&response, "position"),
            length: Self::parse_attr_u32(&response, "length"),
            volume: Self::parse_attr_i32(&response, "volume"),
            active_mode: Self::parse_attr(&response, "active_mode").unwrap_or_default(),
            active_filter: Self::parse_attr(&response, "active_filter").unwrap_or_default(),
            active_shaper: Self::parse_attr(&response, "active_shaper").unwrap_or_default(),
            active_rate: Self::parse_attr_u32(&response, "active_rate"),
            active_bits: Self::parse_attr_u32(&response, "active_bits"),
            active_channels: Self::parse_attr_u32(&response, "active_channels"),
            samplerate: Self::parse_attr_u32(&response, "samplerate"),
            bitrate: Self::parse_attr_u32(&response, "bitrate"),
        })
    }

    /// Get volume range
    pub async fn get_volume_range(&self) -> Result<VolumeRange> {
        let xml = Self::build_request("VolumeRange", &[]);
        let response = self.send_command(&xml).await?;

        Ok(VolumeRange {
            min: Self::parse_attr_i32(&response, "min"),
            max: Self::parse_attr_i32(&response, "max"),
            step: Self::parse_attr_i32(&response, "step").max(1),
            enabled: Self::parse_attr_bool(&response, "enabled"),
            adaptive: Self::parse_attr_bool(&response, "adaptive"),
        })
    }

    /// Parse multi-item response
    fn parse_items<F, T>(response: &str, item_tag: &str, parser: F) -> Vec<T>
    where
        F: Fn(&str) -> T,
    {
        let mut items = Vec::new();
        let pattern = format!("<{}", item_tag);

        for part in response.split(&pattern).skip(1) {
            if let Some(end) = part.find("/>") {
                let item_xml = format!("<{}{}", item_tag, &part[..end + 2]);
                items.push(parser(&item_xml));
            }
        }

        items
    }

    /// Get available modes
    pub async fn get_modes(&self) -> Result<Vec<ListItem>> {
        let xml = Self::build_request("GetModes", &[]);
        let response = self.send_command(&xml).await?;

        Ok(Self::parse_items(&response, "ModesItem", |item| ListItem {
            index: Self::parse_attr_u32(item, "index"),
            name: Self::parse_attr(item, "name").unwrap_or_default(),
            value: Self::parse_attr_i32(item, "value"), // Mode values can be negative (-1 for PCM)
        }))
    }

    /// Get available filters
    pub async fn get_filters(&self) -> Result<Vec<FilterItem>> {
        let xml = Self::build_request("GetFilters", &[]);
        let response = self.send_command(&xml).await?;

        Ok(Self::parse_items(&response, "FiltersItem", |item| {
            FilterItem {
                index: Self::parse_attr_u32(item, "index"),
                name: Self::parse_attr(item, "name").unwrap_or_default(),
                value: Self::parse_attr_i32(item, "value"),
                arg: Self::parse_attr_u32(item, "arg"),
            }
        }))
    }

    /// Get available shapers
    pub async fn get_shapers(&self) -> Result<Vec<ListItem>> {
        let xml = Self::build_request("GetShapers", &[]);
        let response = self.send_command(&xml).await?;

        Ok(Self::parse_items(&response, "ShapersItem", |item| {
            ListItem {
                index: Self::parse_attr_u32(item, "index"),
                name: Self::parse_attr(item, "name").unwrap_or_default(),
                value: Self::parse_attr_i32(item, "value"),
            }
        }))
    }

    /// Get available sample rates
    pub async fn get_rates(&self) -> Result<Vec<RateItem>> {
        let xml = Self::build_request("GetRates", &[]);
        let response = self.send_command(&xml).await?;

        Ok(Self::parse_items(&response, "RatesItem", |item| RateItem {
            index: Self::parse_attr_u32(item, "index"),
            rate: Self::parse_attr_u32(item, "rate"),
        }))
    }

    /// Set mode
    pub async fn set_mode(&self, value: u32) -> Result<()> {
        let xml = Self::build_request("SetMode", &[("value", &value.to_string())]);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Set filter (low-level)
    /// - value: sets the Nx (non-1x) filter
    /// - value1x: if provided, also sets the 1x filter
    pub async fn set_filter(&self, value: u32, value1x: Option<u32>) -> Result<()> {
        let value_str = value.to_string();
        let mut attrs = vec![("value", value_str.as_str())];
        let value1x_str;
        if let Some(v1x) = value1x {
            value1x_str = v1x.to_string();
            attrs.push(("value1x", value1x_str.as_str()));
        }
        let xml = Self::build_request("SetFilter", &attrs);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Set only the 1x filter, preserving current Nx filter value
    pub async fn set_filter_1x(&self, value: u32) -> Result<()> {
        // Get current state to preserve Nx filter
        let state = self.get_state().await?;
        let current_nx = state.filter_nx.unwrap_or(state.filter);
        self.set_filter(current_nx, Some(value)).await
    }

    /// Set only the Nx filter, preserving current 1x filter value
    pub async fn set_filter_nx(&self, value: u32) -> Result<()> {
        // Get current state to preserve 1x filter
        let state = self.get_state().await?;
        let current_1x = state.filter1x.unwrap_or(state.filter);
        self.set_filter(value, Some(current_1x)).await
    }

    /// Set shaper
    pub async fn set_shaper(&self, value: u32) -> Result<()> {
        let xml = Self::build_request("SetShaping", &[("value", &value.to_string())]);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Set sample rate
    pub async fn set_rate(&self, value: u32) -> Result<()> {
        let xml = Self::build_request("SetRate", &[("value", &value.to_string())]);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Set volume
    pub async fn set_volume(&self, value: i32) -> Result<()> {
        let xml = Self::build_request("Volume", &[("value", &value.to_string())]);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Volume up
    pub async fn volume_up(&self) -> Result<()> {
        let xml = Self::build_request("VolumeUp", &[]);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Volume down
    pub async fn volume_down(&self) -> Result<()> {
        let xml = Self::build_request("VolumeDown", &[]);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Mute toggle
    pub async fn volume_mute(&self) -> Result<()> {
        let xml = Self::build_request("VolumeMute", &[]);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Play
    pub async fn play(&self) -> Result<()> {
        let xml = Self::build_request("Play", &[("last", "0")]);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Pause
    pub async fn pause(&self) -> Result<()> {
        let xml = Self::build_request("Pause", &[]);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Stop
    pub async fn stop(&self) -> Result<()> {
        let xml = Self::build_request("Stop", &[]);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Previous track
    pub async fn previous(&self) -> Result<()> {
        let xml = Self::build_request("Previous", &[]);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Next track
    pub async fn next(&self) -> Result<()> {
        let xml = Self::build_request("Next", &[]);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Seek to position
    pub async fn seek(&self, position: u32) -> Result<()> {
        let xml = Self::build_request("Seek", &[("position", &position.to_string())]);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Control playback
    pub async fn control(&self, action: &str) -> Result<()> {
        match action {
            "play" => self.play().await,
            "pause" => self.pause().await,
            "stop" => self.stop().await,
            "previous" => self.previous().await,
            "next" => self.next().await,
            _ => Err(anyhow!("Unknown action: {}", action)),
        }
    }

    /// Get full pipeline status
    pub async fn get_pipeline_status(&self) -> Result<PipelineStatus> {
        let state = self.get_state().await?;
        let vol_range = self.get_volume_range().await?;

        let cached = self.state.read().await;
        let modes = &cached.modes;
        let filters = &cached.filters;
        let shapers = &cached.shapers;
        let rates = &cached.rates;

        // State returns array indices, look up to get actual objects
        let filter1x_idx = state.filter1x.unwrap_or(state.filter) as usize;
        let filter_nx_idx = state.filter_nx.unwrap_or(state.filter) as usize;
        let shaper_idx = state.shaper as usize;

        let filter1x_obj = filters.get(filter1x_idx);
        let filter_nx_obj = filters.get(filter_nx_idx);
        let shaper_obj = shapers.get(shaper_idx);

        let get_mode_by_index = |idx: u8| -> String {
            modes
                .iter()
                .find(|m| m.index == idx as u32)
                .map(|m| m.name.clone())
                .unwrap_or_default()
        };
        let get_mode_by_value = |val: u8| -> String {
            modes
                .iter()
                .find(|m| m.value == val as i32)
                .map(|m| m.name.clone())
                .unwrap_or_default()
        };

        let state_str = match state.state {
            0 => "Stopped",
            1 => "Paused",
            2 => "Playing",
            _ => "Unknown",
        };

        Ok(PipelineStatus {
            status: PipelineState {
                state: state_str.to_string(),
                mode: get_mode_by_index(state.mode),
                active_mode: get_mode_by_value(state.active_mode),
                active_filter: filter1x_obj.map(|f| f.name.clone()).unwrap_or_default(),
                active_shaper: shaper_obj.map(|s| s.name.clone()).unwrap_or_default(),
                active_rate: state.active_rate,
                convolution: state.convolution,
                invert: state.invert,
            },
            volume: PipelineVolume {
                value: state.volume,
                min: vol_range.min,
                max: vol_range.max,
                is_fixed: !vol_range.enabled,
            },
            settings: PipelineSettings {
                mode: PipelineSetting {
                    selected: SelectedOption {
                        value: modes
                            .iter()
                            .find(|m| m.index == state.mode as u32)
                            .map(|m| m.value.to_string())
                            .unwrap_or_else(|| state.mode.to_string()),
                        label: get_mode_by_index(state.mode),
                    },
                    options: modes
                        .iter()
                        .map(|m| SelectOption {
                            value: m.value.to_string(),
                            label: m.name.clone(),
                        })
                        .collect(),
                },
                filter1x: PipelineSetting {
                    selected: SelectedOption {
                        value: filter1x_obj
                            .map(|f| f.value.to_string())
                            .unwrap_or_else(|| filter1x_idx.to_string()),
                        label: filter1x_obj.map(|f| f.name.clone()).unwrap_or_default(),
                    },
                    options: filters
                        .iter()
                        .map(|f| SelectOption {
                            value: f.value.to_string(),
                            label: f.name.clone(),
                        })
                        .collect(),
                },
                filter_nx: PipelineSetting {
                    selected: SelectedOption {
                        value: filter_nx_obj
                            .map(|f| f.value.to_string())
                            .unwrap_or_else(|| filter_nx_idx.to_string()),
                        label: filter_nx_obj.map(|f| f.name.clone()).unwrap_or_default(),
                    },
                    options: filters
                        .iter()
                        .map(|f| SelectOption {
                            value: f.value.to_string(),
                            label: f.name.clone(),
                        })
                        .collect(),
                },
                shaper: PipelineSetting {
                    selected: SelectedOption {
                        value: shaper_obj
                            .map(|s| s.value.to_string())
                            .unwrap_or_else(|| shaper_idx.to_string()),
                        label: shaper_obj.map(|s| s.name.clone()).unwrap_or_default(),
                    },
                    options: shapers
                        .iter()
                        .map(|s| SelectOption {
                            value: s.value.to_string(),
                            label: s.name.clone(),
                        })
                        .collect(),
                },
                samplerate: PipelineSetting {
                    selected: SelectedOption {
                        value: state.rate.to_string(),
                        label: if state.rate == 0 {
                            "Auto".to_string()
                        } else {
                            rates
                                .iter()
                                .find(|r| r.index == state.rate)
                                .map(|r| r.rate.to_string())
                                .unwrap_or_else(|| "Auto".to_string())
                        },
                    },
                    options: rates
                        .iter()
                        .map(|r| SelectOption {
                            value: r.index.to_string(),
                            label: if r.index == 0 {
                                "Auto".to_string()
                            } else {
                                r.rate.to_string()
                            },
                        })
                        .collect(),
                },
            },
        })
    }

    // =========================================================================
    // Web UI methods for profile loading (HTTP with Digest Auth)
    // =========================================================================

    /// Get web base URL
    async fn web_base_url(&self) -> Result<String> {
        let state = self.state.read().await;
        let host = state
            .host
            .as_ref()
            .ok_or_else(|| anyhow!("HQPlayer host not configured"))?;
        Ok(format!("http://{}:{}", host, state.web_port))
    }

    /// MD5 hash helper
    fn md5_hash(input: &str) -> String {
        format!("{:x}", md5::compute(input.as_bytes()))
    }

    /// Build digest auth header
    async fn build_digest_header(&self, method: &str, uri: &str) -> Option<String> {
        let mut state = self.state.write().await;

        // Extract all values first to avoid borrow conflicts
        let username = state.web_username.clone()?;
        let password = state.web_password.clone()?;

        let digest = state.digest_auth.as_mut()?;

        digest.nc += 1;
        let nc = format!("{:08x}", digest.nc);
        let cnonce = format!("{:016x}", rand::random::<u64>());

        // Clone digest fields we need
        let realm = digest.realm.clone();
        let nonce = digest.nonce.clone();
        let qop = digest.qop.clone();
        let opaque = digest.opaque.clone();
        let algorithm = digest.algorithm.clone();

        let ha1 = if algorithm.to_uppercase() == "MD5-SESS" {
            let initial = Self::md5_hash(&format!("{}:{}:{}", username, realm, password));
            Self::md5_hash(&format!("{}:{}:{}", initial, nonce, cnonce))
        } else {
            Self::md5_hash(&format!("{}:{}:{}", username, realm, password))
        };

        let ha2 = Self::md5_hash(&format!("{}:{}", method, uri));

        let response = if !qop.is_empty() {
            let qop_value = qop.split(',').next().unwrap_or("auth").trim();
            Self::md5_hash(&format!(
                "{}:{}:{}:{}:{}:{}",
                ha1, nonce, nc, cnonce, qop_value, ha2
            ))
        } else {
            Self::md5_hash(&format!("{}:{}:{}", ha1, nonce, ha2))
        };

        let mut parts = vec![
            format!("Digest username=\"{}\"", username),
            format!("realm=\"{}\"", realm),
            format!("nonce=\"{}\"", nonce),
            format!("uri=\"{}\"", uri),
            format!("algorithm={}", algorithm),
            format!("response=\"{}\"", response),
        ];

        if !qop.is_empty() {
            let qop_value = qop.split(',').next().unwrap_or("auth").trim();
            parts.push(format!("qop={}", qop_value));
            parts.push(format!("nc={}", nc));
            parts.push(format!("cnonce=\"{}\"", cnonce));
        }

        if !opaque.is_empty() {
            parts.push(format!("opaque=\"{}\"", opaque));
        }

        Some(parts.join(", "))
    }

    /// Parse WWW-Authenticate header for digest auth
    async fn parse_digest_challenge(&self, header: &str) {
        let mut state = self.state.write().await;

        let challenge = header
            .trim_start_matches("Digest ")
            .trim_start_matches("digest ");
        let mut realm = String::new();
        let mut nonce = String::new();
        let mut qop = String::new();
        let mut opaque = String::new();
        let mut algorithm = "MD5".to_string();

        for part in challenge.split(',') {
            let part = part.trim();
            if let Some(eq_pos) = part.find('=') {
                let key = part[..eq_pos].trim();
                let value = part[eq_pos + 1..].trim().trim_matches('"');

                match key {
                    "realm" => realm = value.to_string(),
                    "nonce" => nonce = value.to_string(),
                    "qop" => qop = value.to_string(),
                    "opaque" => opaque = value.to_string(),
                    "algorithm" => algorithm = value.to_uppercase(),
                    _ => {}
                }
            }
        }

        state.digest_auth = Some(DigestAuth {
            realm,
            nonce,
            qop,
            opaque,
            algorithm,
            nc: 0,
        });
    }

    /// Make authenticated web request
    async fn web_request(&self, path: &str, method: &str, body: Option<&str>) -> Result<String> {
        let base_url = self.web_base_url().await?;
        let url = format!("{}{}", base_url, path);

        // First attempt
        let mut request = match method {
            "POST" => self.http_client.post(&url),
            _ => self.http_client.get(&url),
        };

        if let Some(auth_header) = self.build_digest_header(method, path).await {
            request = request.header("Authorization", auth_header);
        }

        if let Some(b) = body {
            request = request
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(b.to_string());
        }

        let response = request.send().await?;

        // Handle 401 - parse challenge and retry
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            if let Some(auth_header) = response.headers().get("www-authenticate") {
                if let Ok(header_str) = auth_header.to_str() {
                    if header_str.to_lowercase().starts_with("digest") {
                        self.parse_digest_challenge(header_str).await;

                        // Retry with auth
                        let mut request = match method {
                            "POST" => self.http_client.post(&url),
                            _ => self.http_client.get(&url),
                        };

                        if let Some(auth_header) = self.build_digest_header(method, path).await {
                            request = request.header("Authorization", auth_header);
                        }

                        if let Some(b) = body {
                            request = request
                                .header("Content-Type", "application/x-www-form-urlencoded")
                                .body(b.to_string());
                        }

                        let response = request.send().await?;
                        if !response.status().is_success() {
                            return Err(anyhow!("Request failed: {}", response.status()));
                        }
                        return Ok(response.text().await?);
                    }
                }
            }
            return Err(anyhow!("Authentication failed"));
        }

        if !response.status().is_success() {
            return Err(anyhow!("Request failed: {}", response.status()));
        }

        Ok(response.text().await?)
    }

    /// Parse hidden form inputs from HTML
    fn parse_hidden_inputs(html: &str) -> HashMap<String, String> {
        let mut fields = HashMap::new();

        let input_re = Regex::new(r#"<input[^>]*name\s*=\s*["']([^"'>\s]+)["'][^>]*>"#).unwrap();
        let value_re = Regex::new(r#"value\s*=\s*["']([^"']*)["']"#).unwrap();
        let type_re = Regex::new(r#"type\s*=\s*["']([^"']*)["']"#).unwrap();

        for cap in input_re.captures_iter(html) {
            let tag = &cap[0];
            let name = &cap[1];

            let input_type = type_re
                .captures(tag)
                .map(|c| c[1].to_lowercase())
                .unwrap_or_default();

            if input_type == "hidden" || name == "_xsrf" {
                let value = value_re
                    .captures(tag)
                    .map(|c| c[1].to_string())
                    .unwrap_or_default();
                fields.insert(name.to_string(), value);
            }
        }

        fields
    }

    /// Parse profiles from HTML select
    fn parse_profiles_from_html(html: &str) -> Vec<HqpProfile> {
        let mut profiles = Vec::new();

        let select_re =
            Regex::new(r#"<select[^>]*name\s*=\s*["']profile["'][^>]*>([\s\S]*?)</select>"#)
                .unwrap();
        let option_re = Regex::new(r#"<option([^>]*)>([\s\S]*?)</option>"#).unwrap();
        let value_re = Regex::new(r#"value\s*=\s*["']([^"']*)["']"#).unwrap();

        if let Some(select_cap) = select_re.captures(html) {
            let content = &select_cap[1];

            for opt_cap in option_re.captures_iter(content) {
                let attrs = &opt_cap[1];
                let text = opt_cap[2].trim();

                let value = value_re
                    .captures(attrs)
                    .map(|c| c[1].to_string())
                    .unwrap_or_else(|| text.to_string());

                // Skip default/empty profiles
                let slug: String = value
                    .to_lowercase()
                    .chars()
                    .filter(|c| c.is_alphanumeric())
                    .collect();
                if !value.is_empty() && !slug.is_empty() && slug != "default" {
                    profiles.push(HqpProfile {
                        value: value.trim().to_string(),
                        title: if text.is_empty() {
                            value.clone()
                        } else {
                            text.to_string()
                        },
                    });
                }
            }
        }

        profiles
    }

    /// Fetch available profiles from web UI
    pub async fn fetch_profiles(&self) -> Result<Vec<HqpProfile>> {
        if !self.has_web_credentials().await {
            return Err(anyhow!("Web credentials not configured"));
        }

        let html = self.web_request(PROFILE_PATH, "GET", None).await?;

        let hidden_fields = Self::parse_hidden_inputs(&html);
        let profiles = Self::parse_profiles_from_html(&html);

        // Cache for later use
        {
            let mut state = self.state.write().await;
            state.hidden_fields = hidden_fields;
            state.profiles = profiles.clone();
        }

        Ok(profiles)
    }

    /// Get cached profiles
    pub async fn get_cached_profiles(&self) -> Vec<HqpProfile> {
        self.state.read().await.profiles.clone()
    }

    /// Load a profile via web UI form submission
    pub async fn load_profile(&self, profile_value: &str) -> Result<()> {
        if profile_value.is_empty() || profile_value.to_lowercase() == "default" {
            return Err(anyhow!("Profile value is required"));
        }

        if !self.has_web_credentials().await {
            return Err(anyhow!("Web credentials not configured"));
        }

        // Ensure we have hidden fields
        {
            let state = self.state.read().await;
            if state.hidden_fields.is_empty() || state.profiles.is_empty() {
                drop(state);
                self.fetch_profiles().await?;
            }
        }

        // Build form body
        let body = {
            let state = self.state.read().await;
            let mut params: Vec<(String, String)> = state
                .hidden_fields
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            params.push(("profile".to_string(), profile_value.to_string()));

            params
                .iter()
                .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
                .collect::<Vec<_>>()
                .join("&")
        };

        let base_url = self.web_base_url().await?;

        // POST with proper headers
        let mut request = self
            .http_client
            .post(format!("{}{}", base_url, PROFILE_PATH))
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Origin", &base_url)
            .header("Referer", &format!("{}{}", base_url, PROFILE_PATH));

        if let Some(auth_header) = self.build_digest_header("POST", PROFILE_PATH).await {
            request = request.header("Authorization", auth_header);
        }

        let response = request.body(body.clone()).send().await?;

        // Handle 401 retry
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            if let Some(auth_header) = response.headers().get("www-authenticate") {
                if let Ok(header_str) = auth_header.to_str() {
                    self.parse_digest_challenge(header_str).await;

                    let mut request = self
                        .http_client
                        .post(format!("{}{}", base_url, PROFILE_PATH))
                        .header("Content-Type", "application/x-www-form-urlencoded")
                        .header("Origin", &base_url)
                        .header("Referer", &format!("{}{}", base_url, PROFILE_PATH));

                    if let Some(auth_header) = self.build_digest_header("POST", PROFILE_PATH).await
                    {
                        request = request.header("Authorization", auth_header);
                    }

                    let response = request.body(body).send().await?;
                    if response.status().is_client_error() || response.status().is_server_error() {
                        return Err(anyhow!("Profile load failed: {}", response.status()));
                    }
                    return Ok(());
                }
            }
            return Err(anyhow!("Authentication failed"));
        }

        if response.status().is_client_error() || response.status().is_server_error() {
            return Err(anyhow!("Profile load failed: {}", response.status()));
        }

        Ok(())
    }

    /// Check if this is HQPlayer Embedded (supports profiles)
    pub async fn is_embedded(&self) -> bool {
        let state = self.state.read().await;
        state
            .info
            .as_ref()
            .map(|i| i.product.to_lowercase().contains("embedded"))
            .unwrap_or(false)
    }

    /// Check if profiles are supported (Embedded + web creds)
    pub async fn supports_profiles(&self) -> bool {
        self.is_embedded().await && self.has_web_credentials().await
    }

    // =========================================================================
    // Matrix profile methods (native TCP protocol)
    // =========================================================================

    /// Get available matrix profiles
    pub async fn get_matrix_profiles(&self) -> Result<Vec<MatrixProfile>> {
        let xml = Self::build_request("MatrixListProfiles", &[]);
        let response = self.send_command(&xml).await?;

        Ok(Self::parse_items(&response, "MatrixProfile", |item| {
            MatrixProfile {
                index: Self::parse_attr_u32(item, "index"),
                name: Self::parse_attr(item, "name").unwrap_or_default(),
            }
        }))
    }

    /// Get current matrix profile
    pub async fn get_matrix_profile(&self) -> Result<Option<MatrixProfile>> {
        let xml = Self::build_request("MatrixGetProfile", &[]);
        let response = self.send_command(&xml).await?;

        // HQPlayer returns current profile - try both 'value' (as per Node.js reference)
        // and 'name' attribute for compatibility
        let index = Self::parse_attr_u32(&response, "index");
        let name =
            Self::parse_attr(&response, "value").or_else(|| Self::parse_attr(&response, "name"));

        match name {
            Some(n) if !n.is_empty() => Ok(Some(MatrixProfile { index, name: n })),
            _ => Ok(None),
        }
    }

    /// Set matrix profile by index
    pub async fn set_matrix_profile(&self, value: u32) -> Result<()> {
        let xml = Self::build_request("MatrixSetProfile", &[("value", &value.to_string())]);
        self.send_command(&xml).await?;
        Ok(())
    }

    /// Get name of this instance (if set)
    pub async fn get_instance_name(&self) -> Option<String> {
        let state = self.state.read().await;
        state.instance_name.clone()
    }

    /// Set name of this instance
    pub async fn set_instance_name(&self, name: String) {
        let mut state = self.state.write().await;
        state.instance_name = Some(name);
    }

    /// Convert HQPlayer status to a unified bus Zone
    fn hqp_status_to_zone(
        host: &str,
        instance_name: Option<&str>,
        info: &HqpInfo,
        status: &HqpStatus,
        vol_range: &VolumeRange,
    ) -> BusZone {
        use std::time::{SystemTime, UNIX_EPOCH};

        let zone_id = format!("hqplayer:{}", instance_name.unwrap_or(host));
        let zone_name = if info.name.is_empty() {
            format!("HQPlayer @ {}", host)
        } else {
            info.name.clone()
        };

        let state = match status.state {
            0 => PlaybackState::Stopped,
            1 => PlaybackState::Paused,
            2 => PlaybackState::Playing,
            _ => PlaybackState::Unknown,
        };

        let volume_control = if vol_range.enabled {
            Some(BusVolumeControl {
                value: status.volume as f32,
                min: vol_range.min as f32,
                max: vol_range.max as f32,
                step: vol_range.step as f32,
                is_muted: false, // HQPlayer doesn't report mute separately
                scale: VolumeScale::Decibel,
                output_id: Some(zone_id.clone()),
            })
        } else {
            None
        };

        // Build now_playing if we have track info
        let now_playing = if !status.track_id.is_empty() || status.length > 0 {
            Some(BusNowPlaying {
                title: String::new(), // HQPlayer status doesn't include title
                artist: String::new(),
                album: String::new(),
                image_key: None,
                seek_position: Some(status.position as f64),
                duration: Some(status.length as f64),
                metadata: Some(TrackMetadata {
                    format: Some(status.active_mode.clone()),
                    sample_rate: Some(status.samplerate),
                    bit_depth: Some(status.active_bits as u8),
                    bitrate: Some(status.bitrate),
                    genre: None,
                    composer: None,
                    track_number: Some(status.track),
                    disc_number: None,
                }),
            })
        } else {
            None
        };

        let last_updated = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        BusZone {
            zone_id,
            zone_name,
            state,
            volume_control,
            now_playing,
            source: "hqplayer".to_string(),
            is_controllable: true,
            is_seekable: true,
            last_updated,
        }
    }
}

// =============================================================================
// Multi-instance manager
// =============================================================================

/// Instance info for API responses
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HqpInstanceInfo {
    pub name: String,
    pub host: Option<String>,
    pub port: u16,
    pub connected: bool,
    pub info: Option<HqpInfo>,
}

/// Manager for multiple HQPlayer instances
pub struct HqpInstanceManager {
    instances: Arc<RwLock<HashMap<String, Arc<HqpAdapter>>>>,
    bus: SharedBus,
}

impl HqpInstanceManager {
    /// Create a new instance manager
    pub fn new(bus: SharedBus) -> Self {
        Self {
            instances: Arc::new(RwLock::new(HashMap::new())),
            bus,
        }
    }

    /// Load instances from config file
    pub async fn load_from_config(&self) {
        let configs = load_hqp_configs();
        for config in configs {
            let adapter = Arc::new(HqpAdapter::new(self.bus.clone()));
            adapter.set_instance_name(config.name.clone()).await;
            adapter
                .configure(
                    config.host,
                    Some(config.port),
                    Some(config.web_port),
                    config.username,
                    config.password,
                )
                .await;

            let mut instances = self.instances.write().await;
            instances.insert(config.name, adapter);
        }
    }

    /// Save all instances to config file
    pub async fn save_to_config(&self) {
        let instances = self.instances.read().await;
        let mut configs = Vec::new();

        for (name, adapter) in instances.iter() {
            let status = adapter.get_status().await;
            if let Some(host) = status.host {
                let state = adapter.state.read().await;
                configs.push(HqpInstanceConfig {
                    name: name.clone(),
                    host,
                    port: status.port,
                    web_port: state.web_port,
                    username: state.web_username.clone(),
                    password: state.web_password.clone(),
                });
            }
        }

        save_hqp_configs(&configs);
    }

    /// Get or create an instance by name
    pub async fn get_or_create(&self, name: &str) -> Arc<HqpAdapter> {
        {
            let instances = self.instances.read().await;
            if let Some(adapter) = instances.get(name) {
                return adapter.clone();
            }
        }

        // Create new instance
        let adapter = Arc::new(HqpAdapter::new(self.bus.clone()));
        adapter.set_instance_name(name.to_string()).await;

        let mut instances = self.instances.write().await;
        instances.insert(name.to_string(), adapter.clone());
        adapter
    }

    /// Get an instance by name (if it exists)
    pub async fn get(&self, name: &str) -> Option<Arc<HqpAdapter>> {
        let instances = self.instances.read().await;
        instances.get(name).cloned()
    }

    /// Get the default instance (creates if not exists)
    pub async fn get_default(&self) -> Arc<HqpAdapter> {
        self.get_or_create("default").await
    }

    /// List all configured instances
    pub async fn list_instances(&self) -> Vec<HqpInstanceInfo> {
        let instances = self.instances.read().await;
        let mut result = Vec::new();

        for (name, adapter) in instances.iter() {
            let status = adapter.get_status().await;
            result.push(HqpInstanceInfo {
                name: name.clone(),
                host: status.host,
                port: status.port,
                connected: status.connected,
                info: status.info,
            });
        }

        result.sort_by(|a, b| a.name.cmp(&b.name));
        result
    }

    /// Add or update an instance
    pub async fn add_instance(
        &self,
        name: String,
        host: String,
        port: Option<u16>,
        web_port: Option<u16>,
        username: Option<String>,
        password: Option<String>,
    ) -> Arc<HqpAdapter> {
        let adapter = self.get_or_create(&name).await;
        adapter
            .configure(host, port, web_port, username, password)
            .await;
        self.save_to_config().await;
        adapter
    }

    /// Remove an instance by name
    pub async fn remove_instance(&self, name: &str) -> bool {
        let mut instances = self.instances.write().await;
        let removed = instances.remove(name).is_some();
        if removed {
            drop(instances);
            self.save_to_config().await;
        }
        removed
    }

    /// Check if any instance is configured
    pub async fn has_instances(&self) -> bool {
        let instances = self.instances.read().await;
        !instances.is_empty()
    }

    /// Get instance count
    pub async fn instance_count(&self) -> usize {
        let instances = self.instances.read().await;
        instances.len()
    }
}

// =============================================================================
// Zone linking service
// =============================================================================

const ZONE_LINKS_FILE: &str = "hqp-zone-links.json";

fn zone_links_path() -> PathBuf {
    get_config_file_path(ZONE_LINKS_FILE)
}

/// Zone link info for API responses
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoneLink {
    pub zone_id: String,
    pub instance: String,
}

/// Service for managing zone-to-HQPlayer-instance links
pub struct HqpZoneLinkService {
    links: Arc<RwLock<HashMap<String, String>>>, // zone_id -> instance_name
    instances: Arc<HqpInstanceManager>,
}

impl HqpZoneLinkService {
    /// Create a new zone link service
    pub fn new(instances: Arc<HqpInstanceManager>) -> Self {
        let service = Self {
            links: Arc::new(RwLock::new(HashMap::new())),
            instances,
        };
        service.load_links_sync();
        service
    }

    /// Load links from disk synchronously (at startup)
    /// Issue #76: Uses read_config_file for backwards-compatible fallback
    fn load_links_sync(&self) {
        // read_config_file checks subdir first, falls back to root for legacy files
        if let Some(content) = read_config_file(ZONE_LINKS_FILE) {
            match serde_json::from_str::<HashMap<String, String>>(&content) {
                Ok(saved_links) => {
                    if let Ok(mut links) = self.links.try_write() {
                        *links = saved_links;
                        tracing::info!("Loaded {} HQP zone links from disk", links.len());
                    }
                }
                Err(e) => tracing::warn!("Failed to parse zone links: {}", e),
            }
        }
    }

    /// Save links to disk
    async fn save_links(&self) {
        let links = self.links.read().await;
        let path = zone_links_path();

        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        match serde_json::to_string_pretty(&*links) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    tracing::error!("Failed to save zone links: {}", e);
                } else {
                    tracing::debug!("Saved {} zone links to disk", links.len());
                }
            }
            Err(e) => tracing::error!("Failed to serialize zone links: {}", e),
        }
    }

    /// Link a zone to an HQP instance
    pub async fn link_zone(&self, zone_id: String, instance_name: String) -> Result<()> {
        // Verify instance exists
        if self.instances.get(&instance_name).await.is_none() {
            return Err(anyhow!("Unknown HQP instance: {}", instance_name));
        }

        {
            let mut links = self.links.write().await;
            links.insert(zone_id.clone(), instance_name.clone());
        }

        self.save_links().await;
        tracing::info!("Zone {} linked to HQP instance {}", zone_id, instance_name);
        Ok(())
    }

    /// Unlink a zone from HQP
    pub async fn unlink_zone(&self, zone_id: &str) -> bool {
        let was_linked = {
            let mut links = self.links.write().await;
            links.remove(zone_id).is_some()
        };

        if was_linked {
            self.save_links().await;
            tracing::info!("Zone {} unlinked from HQP", zone_id);
        }

        was_linked
    }

    /// Get the HQP instance name for a zone
    pub async fn get_instance_for_zone(&self, zone_id: &str) -> Option<String> {
        let links = self.links.read().await;
        links.get(zone_id).cloned()
    }

    /// Get all zone links
    pub async fn get_links(&self) -> Vec<ZoneLink> {
        let links = self.links.read().await;
        links
            .iter()
            .map(|(zone_id, instance)| ZoneLink {
                zone_id: zone_id.clone(),
                instance: instance.clone(),
            })
            .collect()
    }

    /// Get HQP pipeline data for a linked zone
    pub async fn get_pipeline_for_zone(&self, zone_id: &str) -> Option<PipelineStatus> {
        let instance_name = self.get_instance_for_zone(zone_id).await?;

        let adapter = self.instances.get(&instance_name).await?;
        if !adapter.is_configured().await {
            return None;
        }

        match adapter.get_pipeline_status().await {
            Ok(pipeline) => Some(pipeline),
            Err(e) => {
                tracing::error!("Failed to fetch HQP pipeline for zone {}: {}", zone_id, e);
                None
            }
        }
    }

    /// Remove all links pointing to a specific instance
    pub async fn remove_links_for_instance(&self, instance_name: &str) -> usize {
        let mut links = self.links.write().await;
        let zones_to_remove: Vec<String> = links
            .iter()
            .filter(|(_, inst)| *inst == instance_name)
            .map(|(zone_id, _)| zone_id.clone())
            .collect();

        let count = zones_to_remove.len();
        for zone_id in zones_to_remove {
            links.remove(&zone_id);
        }

        drop(links);

        if count > 0 {
            self.save_links().await;
            tracing::info!(
                "Removed {} zone links for deleted instance {}",
                count,
                instance_name
            );
        }

        count
    }

    /// Auto-correct links when instances are renamed (called after loading)
    pub async fn auto_correct_links(&self) -> bool {
        let instances = self.instances.list_instances().await;
        if instances.len() != 1 {
            return false; // Can only auto-correct with single instance
        }

        let single_instance = &instances[0].name;
        let mut corrected = false;

        {
            let mut links = self.links.write().await;
            let instance_names: Vec<String> = instances.iter().map(|i| i.name.clone()).collect();

            for (zone_id, instance_name) in links.iter_mut() {
                if !instance_names.contains(instance_name) {
                    tracing::warn!(
                        "Auto-correcting zone link {} from {} to {}",
                        zone_id,
                        instance_name,
                        single_instance
                    );
                    *instance_name = single_instance.clone();
                    corrected = true;
                }
            }
        }

        if corrected {
            self.save_links().await;
        }

        corrected
    }
}

// =============================================================================
// HQPlayer UDP multicast discovery
// =============================================================================

const HQP_MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(239, 192, 0, 199);
const HQP_DISCOVERY_PORT: u16 = 4321;
const HQP_DISCOVERY_TIMEOUT_MS: u64 = 3000;

/// Discovered HQPlayer instance
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredHqp {
    pub host: String,
    pub port: u16,
    pub name: String,
    pub version: String,
    pub product: Option<String>,
}

/// Discover HQPlayer instances on the network via UDP multicast
pub async fn discover_hqplayers(timeout_ms: Option<u64>) -> Result<Vec<DiscoveredHqp>> {
    let timeout_duration = Duration::from_millis(timeout_ms.unwrap_or(HQP_DISCOVERY_TIMEOUT_MS));
    let mut discovered: HashMap<String, DiscoveredHqp> = HashMap::new();

    // Create UDP socket
    let socket = UdpSocket::bind("0.0.0.0:0").await?;

    // Join multicast group
    socket.set_broadcast(true)?;

    // Send discovery message
    let message = b"<?xml version=\"1.0\"?><discover>hqplayer</discover>";
    let dest = SocketAddrV4::new(HQP_MULTICAST_ADDR, HQP_DISCOVERY_PORT);
    socket.send_to(message, dest).await?;

    tracing::debug!(
        "Sent HQPlayer discovery multicast to {}:{}",
        HQP_MULTICAST_ADDR,
        HQP_DISCOVERY_PORT
    );

    // Receive responses with timeout
    let mut buf = [0u8; 2048];
    let deadline = tokio::time::Instant::now() + timeout_duration;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        match timeout(remaining, socket.recv_from(&mut buf)).await {
            Ok(Ok((len, addr))) => {
                let response = String::from_utf8_lossy(&buf[..len]);
                tracing::debug!("HQP discovery response from {}: {}", addr, response);

                // Parse XML response
                if let Some(hqp) = parse_discovery_response(&response, addr.ip().to_string()) {
                    discovered.insert(hqp.host.clone(), hqp);
                }
            }
            Ok(Err(e)) => {
                tracing::warn!("HQP discovery recv error: {}", e);
                break;
            }
            Err(_) => {
                // Timeout - done receiving
                break;
            }
        }
    }

    let result: Vec<DiscoveredHqp> = discovered.into_values().collect();
    tracing::info!("HQPlayer discovery found {} instance(s)", result.len());
    Ok(result)
}

/// Parse HQPlayer discovery XML response
fn parse_discovery_response(xml: &str, host: String) -> Option<DiscoveredHqp> {
    // Look for <discover result="OK" .../>
    if !xml.contains("result=\"OK\"") && !xml.contains("result='OK'") {
        return None;
    }

    let name = extract_xml_attr(xml, "name").unwrap_or_else(|| "HQPlayer".to_string());
    let version = extract_xml_attr(xml, "version").unwrap_or_else(|| "unknown".to_string());
    let product = extract_xml_attr(xml, "product");

    Some(DiscoveredHqp {
        host,
        port: HQP_DISCOVERY_PORT,
        name,
        version,
        product,
    })
}

/// Extract attribute value from XML string
fn extract_xml_attr(xml: &str, attr: &str) -> Option<String> {
    // Try double quotes
    let pattern = format!("{}=\"", attr);
    if let Some(start) = xml.find(&pattern) {
        let value_start = start + pattern.len();
        if let Some(end) = xml[value_start..].find('"') {
            return Some(xml[value_start..value_start + end].to_string());
        }
    }

    // Try single quotes
    let pattern = format!("{}='", attr);
    if let Some(start) = xml.find(&pattern) {
        let value_start = start + pattern.len();
        if let Some(end) = xml[value_start..].find('\'') {
            return Some(xml[value_start..value_start + end].to_string());
        }
    }

    None
}
