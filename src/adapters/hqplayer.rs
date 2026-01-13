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
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, RwLock};
use tokio::time::timeout;

use crate::bus::{BusEvent, SharedBus};

const DEFAULT_PORT: u16 = 4321;
const DEFAULT_WEB_PORT: u16 = 8088;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const PROFILE_PATH: &str = "/config/profile/load";
/// Maximum reconnection attempts before giving up
const MAX_RECONNECT_ATTEMPTS: u32 = 3;
/// Delay between reconnection attempts
const RECONNECT_DELAY: Duration = Duration::from_millis(500);

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
    pub value: u32,
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
    pub value: u32,
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
pub struct PipelineSettings {
    pub mode: PipelineSetting,
    pub filter1x: PipelineSetting,
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
struct HqpAdapterState {
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
        Self {
            state: Arc::new(RwLock::new(HqpAdapterState::default())),
            connection: Arc::new(Mutex::new(None)),
            http_client: Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("Failed to create HTTP client"),
            bus,
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
            let mut conn = self.connection.lock().await;
            *conn = None;
        }
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

        Ok(())
    }

    /// Disconnect
    pub async fn disconnect(&self) {
        let host = {
            let mut state = self.state.write().await;
            state.connected = false;
            state.host.clone()
        };

        {
            let mut conn = self.connection.lock().await;
            *conn = None;
        }

        if let Some(host) = host {
            self.bus.publish(BusEvent::HqpDisconnected { host });
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
        let host = {
            let mut state = self.state.write().await;
            state.connected = false;
            state.host.clone()
        };

        {
            let mut conn = self.connection.lock().await;
            *conn = None;
        }

        if let Some(ref host) = host {
            tracing::warn!("HQPlayer connection lost to {}", host);
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

        // Read response (single line)
        let mut response = String::new();
        timeout(RESPONSE_TIMEOUT, conn.stream.read_line(&mut response))
            .await
            .map_err(|_| anyhow!("Response timeout"))?
            .map_err(|e| anyhow!("Read error: {}", e))?;

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
            value: Self::parse_attr_u32(item, "value"),
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
                value: Self::parse_attr_u32(item, "value"),
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
                value: Self::parse_attr_u32(item, "value"),
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
            value: Self::parse_attr_u32(item, "value"),
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
                value: Self::parse_attr_u32(item, "value"),
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
                value: Self::parse_attr_u32(item, "value"),
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
                .find(|m| m.value == val as u32)
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
            .post(&format!("{}{}", base_url, PROFILE_PATH))
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
                        .post(&format!("{}{}", base_url, PROFILE_PATH))
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
        let name = Self::parse_attr(&response, "value")
            .or_else(|| Self::parse_attr(&response, "name"));

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
}
