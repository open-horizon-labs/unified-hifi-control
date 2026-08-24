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

// =============================================================================
// Zone management types (`/api/zones/visibility`, `/api/zones/order`)
// =============================================================================

/// One row of the zone management list. Unlike [`Zone`], this includes zones the user has hidden —
/// the management view is the one place that must show them, or unhiding would be impossible.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ManagedZone {
    pub zone_id: String,
    /// The effective display name: the user's override when set, otherwise the provider's.
    pub zone_name: String,
    /// What Roon/LMS/etc. calls this zone, so a renamed zone can still be identified.
    #[serde(default)]
    pub provider_name: String,
    #[serde(default)]
    pub renamed: bool,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub hidden: bool,
}

impl ManagedZone {
    /// Name plus provider, for accessible labels. Zone names are not unique — the same room name on
    /// two providers is common in this product — so a bare name would produce two controls with
    /// identical accessible names and no way to tell them apart by ear.
    pub fn qualified_label(&self) -> String {
        format!("{} ({})", self.zone_name, source_label(&self.source))
    }
}

/// Provider ids as people know them, matching the Features table's spelling.
pub fn source_label(source: &str) -> &str {
    match source {
        "roon" => "Roon",
        "lms" => "LMS",
        "hqplayer" => "HQPlayer",
        "openhome" => "OpenHome",
        "upnp" => "UPnP",
        other => other,
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ManagedZonesResponse {
    pub zones: Vec<ManagedZone>,
}

/// `POST /api/zones/visibility`.
///
/// The zone request types are defined once and used by both sides: `src/app` serialises them and
/// `src/api` deserialises them in its handlers. `src/app` is not feature-gated, so the server
/// compiles this module too. Two parallel definitions would let a field name or an enum spelling
/// drift into a 422 that the UI has no way to explain.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ZoneVisibilityRequest {
    pub zone_id: String,
    pub hidden: bool,
}

/// `POST /api/zones/order`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ZoneOrderRequest {
    pub zone_id: String,
    /// Step one place — the up/down buttons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<MoveDirection>,
    /// Take the slot this zone currently occupies — a drag-and-drop drop.
    ///
    /// A target *zone* rather than an index: the client's row indices can go stale between render
    /// and drop if a zone appears or disappears, and a stale index would land the zone somewhere
    /// the user did not point at, silently. A stale zone id simply resolves to nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_zone_id: Option<String>,
}

impl ZoneOrderRequest {
    pub fn step(zone_id: String, direction: MoveDirection) -> Self {
        Self {
            zone_id,
            direction: Some(direction),
            target_zone_id: None,
        }
    }

    pub fn drop_onto(zone_id: String, target_zone_id: String) -> Self {
        Self {
            zone_id,
            direction: None,
            target_zone_id: Some(target_zone_id),
        }
    }
}

/// Which way a step-reorder moves a zone. Re-exported by `crate::zone_list` for the server side.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MoveDirection {
    Up,
    Down,
}

/// `POST /api/zones/name`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ZoneNameRequest {
    pub zone_id: String,
    /// `None`, empty, or whitespace-only clears the override and restores the provider's name — so
    /// there is always a way back without a separate "reset" endpoint.
    #[serde(default)]
    pub name: Option<String>,
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
    #[serde(default)]
    pub volume_min: Option<f32>,
    #[serde(default)]
    pub volume_max: Option<f32>,
    /// Volume step size (e.g., 0.5 for Roon, 2.5 for LMS)
    pub volume_step: Option<f32>,
    pub is_previous_allowed: bool,
    pub is_next_allowed: bool,
    #[serde(default)]
    pub seek_position: Option<i64>,
    #[serde(default)]
    pub length: Option<u32>,
    #[serde(default)]
    pub is_play_allowed: bool,
    #[serde(default)]
    pub is_pause_allowed: bool,
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
    pub mode: Option<String>,
    pub active_mode: Option<String>,
    pub active_filter: Option<String>,
    pub active_shaper: Option<String>,
    pub active_rate: Option<u64>,
    pub convolution: Option<bool>,
    pub invert: Option<bool>,
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
    #[serde(rename = "shaperLabel")]
    pub shaper_label: Option<String>,
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
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpProfile {
    pub name: Option<String>,
    pub title: Option<String>,
    pub value: Option<String>,
    #[serde(default)]
    pub active: bool,
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
    pub current: Option<HqpMatrixProfile>,
    #[serde(default)]
    pub junk_filters: Vec<HqpNativeChoice>,
    pub junk_filter: Option<u32>,
    pub convolution: Option<bool>,
    pub adaptive_volume: Option<bool>,
    pub repeat: Option<u8>,
    pub random: Option<bool>,
    pub native_state: Option<HqpNativeState>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpNativeChoice {
    pub index: u32,
    pub name: String,
    pub value: i32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct HqpNativeState {
    pub state: u8,
    pub mode: u8,
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
    pub repeat: u8,
    pub random: bool,
    pub adaptive: bool,
    pub filter_20k: bool,
    pub matrix_profile: String,
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
    /// Volume step override (None/0 = use zone default)
    pub volume_step_override: Option<f64>,
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

    if !resp.ok() {
        return Err(response_error(resp).await);
    }

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

    if !resp.ok() {
        return Err(response_error(resp).await);
    }

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
    if !resp.ok() {
        return Err(response_error(resp).await);
    }

    Ok(())
}

#[cfg(target_arch = "wasm32")]
async fn response_error(resp: web_sys::Response) -> String {
    use wasm_bindgen_futures::JsFuture;

    let status = resp.status();
    let body = match resp.text() {
        Ok(text) => JsFuture::from(text)
            .await
            .ok()
            .and_then(|value| value.as_string())
            .unwrap_or_default(),
        Err(_) => String::new(),
    };
    let detail = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|json| {
            json.get("error")
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned)
        })
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| {
            if body.is_empty() {
                "Request failed".to_string()
            } else {
                body
            }
        });
    format!("HTTP {status}: {detail}")
}

/// SSR stub - returns error (should not be called during SSR)
#[cfg(not(target_arch = "wasm32"))]
pub async fn post_json_no_response<T: Serialize>(_url: &str, _body: &T) -> Result<(), String> {
    Err("post_json_no_response is only available in browser".to_string())
}
