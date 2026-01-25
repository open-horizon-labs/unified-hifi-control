//! Client-side API functions for fetching data.
//!
//! These functions use Dioxus server functions to fetch data
//! without causing SSR deadlocks.

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

// =============================================================================
// Status Types
// =============================================================================

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct AppStatus {
    pub version: String,
    #[serde(default)]
    pub git_sha: String,
    pub uptime_secs: u64,
    pub bus_subscribers: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct RoonStatus {
    pub connected: bool,
    pub core_name: Option<String>,
    pub core_version: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpStatus {
    pub connected: bool,
    pub host: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct LmsStatus {
    pub connected: bool,
    pub host: Option<String>,
    pub port: Option<u16>,
}

// =============================================================================
// Settings Types
// =============================================================================

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct AdapterSettings {
    pub roon: bool,
    pub lms: bool,
    pub openhome: bool,
    pub upnp: bool,
    #[serde(default)]
    pub hqplayer: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct AppSettings {
    pub adapters: AdapterSettings,
    #[serde(default)]
    pub hide_knobs_page: bool,
    #[serde(default)]
    pub hide_hqp_page: bool,
    #[serde(default)]
    pub hide_lms_page: bool,
}

// =============================================================================
// Zone Types
// =============================================================================

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Zone {
    pub zone_id: String,
    pub zone_name: String,
    pub source: Option<String>,
    pub dsp: Option<ZoneDsp>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ZoneDsp {
    pub r#type: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ZonesResponse {
    pub zones: Vec<Zone>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct NowPlaying {
    pub line1: Option<String>,
    pub line2: Option<String>,
    pub line3: Option<String>,
    pub image_url: Option<String>,
    /// Image key for cache busting (changes when track changes)
    pub image_key: Option<String>,
    pub is_playing: bool,
    pub volume: Option<f32>,
    pub volume_type: Option<String>,
    /// Volume step size (e.g., 0.5 for Roon, 2.5 for LMS)
    pub volume_step: Option<f32>,
    pub is_previous_allowed: bool,
    pub is_next_allowed: bool,
}

// =============================================================================
// LMS Types
// =============================================================================

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct LmsConfig {
    pub configured: bool,
    pub connected: bool,
    pub host: Option<String>,
    pub port: Option<u16>,
    /// Whether CLI subscription is active (real-time events vs polling-only)
    #[serde(default)]
    pub cli_subscription_active: bool,
    /// Current poll interval in seconds (2s when CLI down, 30s when CLI up)
    #[serde(default)]
    pub poll_interval_secs: u64,
}

/// Wrapper for /lms/players response
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct LmsPlayersResponse {
    pub players: Vec<LmsPlayer>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct LmsPlayer {
    /// Player ID (MAC address) - API returns "playerid" field
    #[serde(alias = "playerid")]
    pub player_id: String,
    pub name: String,
    pub mode: String,
    /// Current track title - API returns "title" field
    #[serde(alias = "title")]
    pub current_title: Option<String>,
    pub artist: Option<String>,
    pub volume: i32,
}

// =============================================================================
// HQPlayer Types
// =============================================================================

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub web_port: Option<u16>,
    #[serde(default)]
    pub has_web_credentials: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpPipeline {
    pub status: Option<HqpPipelineStatus>,
    pub volume: Option<HqpVolume>,
    pub settings: Option<HqpSettings>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpPipelineStatus {
    pub state: Option<String>,
    pub active_mode: Option<String>,
    pub active_filter: Option<String>,
    pub active_shaper: Option<String>,
    pub active_rate: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpVolume {
    pub value: Option<i32>,
    pub min: Option<i32>,
    pub max: Option<i32>,
    pub is_fixed: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpSettings {
    pub mode: Option<HqpSettingOptions>,
    pub samplerate: Option<HqpSettingOptions>,
    pub filter1x: Option<HqpSettingOptions>,
    #[serde(rename = "filterNx")]
    pub filter_nx: Option<HqpSettingOptions>,
    pub shaper: Option<HqpSettingOptions>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpSettingOptions {
    pub options: Vec<HqpOption>,
    pub selected: Option<HqpOption>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpOption {
    pub value: String,
    pub label: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpProfile {
    pub name: Option<String>,
    pub title: Option<String>,
    pub value: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpProfilesResponse {
    pub profiles: Vec<HqpProfile>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpMatrixProfile {
    pub index: u32,
    pub name: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpMatrixProfilesResponse {
    pub profiles: Vec<HqpMatrixProfile>,
    pub current: Option<u32>,
}

// =============================================================================
// Knob Types
// =============================================================================

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct KnobDevicesResponse {
    pub knobs: Vec<KnobDevice>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct KnobDevice {
    pub knob_id: String,
    pub name: Option<String>,
    pub version: Option<String>,
    pub last_seen: Option<String>,
    pub status: Option<KnobStatus>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct KnobStatus {
    pub battery_level: Option<i32>,
    pub battery_charging: Option<bool>,
    pub zone_id: Option<String>,
    pub ip: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct KnobConfigResponse {
    pub config: Option<KnobConfig>,
}

/// Power mode configuration for knob timeout-based state transitions
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PowerModeConfig {
    pub enabled: bool,
    pub timeout_sec: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct KnobConfig {
    pub name: Option<String>,
    pub rotation_charging: Option<i32>,
    pub rotation_not_charging: Option<i32>,
    // Power modes when charging
    pub art_mode_charging: Option<PowerModeConfig>,
    pub dim_charging: Option<PowerModeConfig>,
    pub sleep_charging: Option<PowerModeConfig>,
    pub deep_sleep_charging: Option<PowerModeConfig>,
    // Power modes when on battery
    pub art_mode_battery: Option<PowerModeConfig>,
    pub dim_battery: Option<PowerModeConfig>,
    pub sleep_battery: Option<PowerModeConfig>,
    pub deep_sleep_battery: Option<PowerModeConfig>,
    // Advanced settings
    pub wifi_power_save_enabled: Option<bool>,
    pub cpu_freq_scaling_enabled: Option<bool>,
    /// Poll interval when playback stopped (seconds)
    pub sleep_poll_stopped_sec: Option<u32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FirmwareVersion {
    pub version: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FetchFirmwareResponse {
    pub version: Option<String>,
    pub error: Option<String>,
}

// =============================================================================
// Client-side fetch helpers (for use in effects/resources)
// =============================================================================

/// Fetch JSON from a URL (client-side only)
#[cfg(target_arch = "wasm32")]
pub async fn fetch_json<T: for<'de> Deserialize<'de>>(url: &str) -> Result<T, String> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{Request, RequestInit, Response};

    let window = web_sys::window().ok_or("No window")?;
    let opts = RequestInit::new();
    opts.set_method("GET");

    let request = Request::new_with_str_and_init(url, &opts).map_err(|e| format!("{:?}", e))?;

    let resp_value = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|e| format!("{:?}", e))?;

    let resp: Response = resp_value.dyn_into().map_err(|_| "Not a Response")?;

    let json = JsFuture::from(resp.json().map_err(|e| format!("{:?}", e))?)
        .await
        .map_err(|e| format!("{:?}", e))?;

    serde_wasm_bindgen::from_value(json).map_err(|e| format!("{:?}", e))
}

/// SSR stub - returns error (should not be called during SSR)
#[cfg(not(target_arch = "wasm32"))]
pub async fn fetch_json<T: for<'de> Deserialize<'de>>(_url: &str) -> Result<T, String> {
    Err("fetch_json is only available in browser".to_string())
}

/// POST JSON to a URL (client-side only)
#[cfg(target_arch = "wasm32")]
pub async fn post_json<T: Serialize, R: for<'de> Deserialize<'de>>(
    url: &str,
    body: &T,
) -> Result<R, String> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{Headers, Request, RequestInit, Response};

    let window = web_sys::window().ok_or("No window")?;

    let headers = Headers::new().map_err(|e| format!("{:?}", e))?;
    headers
        .set("Content-Type", "application/json")
        .map_err(|e| format!("{:?}", e))?;

    let body_str = serde_json::to_string(body).map_err(|e| e.to_string())?;

    let opts = RequestInit::new();
    opts.set_method("POST");
    opts.set_headers(&headers);
    opts.set_body(&wasm_bindgen::JsValue::from_str(&body_str));

    let request = Request::new_with_str_and_init(url, &opts).map_err(|e| format!("{:?}", e))?;

    let resp_value = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|e| format!("{:?}", e))?;

    let resp: Response = resp_value.dyn_into().map_err(|_| "Not a Response")?;

    let json = JsFuture::from(resp.json().map_err(|e| format!("{:?}", e))?)
        .await
        .map_err(|e| format!("{:?}", e))?;

    serde_wasm_bindgen::from_value(json).map_err(|e| format!("{:?}", e))
}

/// SSR stub - returns error (should not be called during SSR)
#[cfg(not(target_arch = "wasm32"))]
pub async fn post_json<T: Serialize, R: for<'de> Deserialize<'de>>(
    _url: &str,
    _body: &T,
) -> Result<R, String> {
    Err("post_json is only available in browser".to_string())
}

/// POST JSON without expecting response body
#[cfg(target_arch = "wasm32")]
pub async fn post_json_no_response<T: Serialize>(url: &str, body: &T) -> Result<(), String> {
    use wasm_bindgen_futures::JsFuture;
    use web_sys::{Headers, Request, RequestInit};

    let window = web_sys::window().ok_or("No window")?;

    let headers = Headers::new().map_err(|e| format!("{:?}", e))?;
    headers
        .set("Content-Type", "application/json")
        .map_err(|e| format!("{:?}", e))?;

    let body_str = serde_json::to_string(body).map_err(|e| e.to_string())?;

    let opts = RequestInit::new();
    opts.set_method("POST");
    opts.set_headers(&headers);
    opts.set_body(&wasm_bindgen::JsValue::from_str(&body_str));

    let request = Request::new_with_str_and_init(url, &opts).map_err(|e| format!("{:?}", e))?;

    JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|e| format!("{:?}", e))?;

    Ok(())
}

/// SSR stub - returns error (should not be called during SSR)
#[cfg(not(target_arch = "wasm32"))]
pub async fn post_json_no_response<T: Serialize>(_url: &str, _body: &T) -> Result<(), String> {
    Err("post_json_no_response is only available in browser".to_string())
}
