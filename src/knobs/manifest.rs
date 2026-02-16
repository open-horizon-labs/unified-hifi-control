//! Manifest-driven knob protocol
//!
//! The knob is a generic manifest renderer. The bridge generates a default manifest
//! from aggregator state. Memex can push richer manifests with additional screens.
//!
//! Design:
//! - `fast` fields (volume, seek, is_playing) update every poll cycle (~2s)
//! - `screens` change rarely (track change, zone switch, Memex context update)
//! - `sha` covers screens + nav only — knob skips re-parsing screens when SHA matches
//! - `version` enables future schema evolution without breaking old firmware

use serde::{Deserialize, Serialize};

/// Manifest protocol version
pub const MANIFEST_VERSION: u32 = 1;

// ── Top-level manifest ──────────────────────────────────────────────────────

/// The complete manifest response served to knobs.
///
/// Knob polls `GET /knob/manifest?zone_id=...` and receives this.
/// The `sha` covers `screens` + `nav` — if it matches the knob's cached SHA,
/// the knob only reads `fast` and skips re-parsing `screens`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Schema version (currently 1)
    pub version: u32,

    /// SHA256 prefix (8 hex chars) of screens + nav content.
    /// Changes when track changes, zone switches, or Memex pushes new context.
    pub sha: String,

    /// Fast-path fields — read every poll cycle, cheap to parse.
    pub fast: FastState,

    /// Screen definitions — re-parsed only when `sha` changes.
    pub screens: Vec<Screen>,

    /// Navigation order and default screen.
    pub nav: Nav,
}

// ── Fast-path state ─────────────────────────────────────────────────────────

/// Fields that change frequently during playback.
/// Knob reads these every poll without touching screens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FastState {
    pub zone_id: String,
    pub is_playing: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_min: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_max: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_step: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seek_position: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length: Option<u32>,
    /// Transport permissions
    pub transport: Transport,
}

/// What transport controls the zone allows right now.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transport {
    pub play: bool,
    pub pause: bool,
    pub next: bool,
    pub prev: bool,
}

// ── Screens ─────────────────────────────────────────────────────────────────

/// A screen the knob can render. Each screen has a type-specific payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Screen {
    /// Now playing — album art background, text lines, arcs for volume/progress.
    #[serde(rename = "media")]
    Media(MediaScreen),

    /// Scrollable list — zone picker, settings menu, search results.
    #[serde(rename = "list")]
    List(ListScreen),

    /// Text card — Memex context, endeavor status, notifications.
    #[serde(rename = "card")]
    Card(CardScreen),

    /// Full-screen progress bar — OTA updates, long operations.
    #[serde(rename = "progress")]
    Progress(ProgressScreen),

    /// Centered icon + message — network status, errors.
    #[serde(rename = "status")]
    Status(StatusScreen),
}

impl Screen {
    pub fn id(&self) -> &str {
        match self {
            Screen::Media(s) => &s.id,
            Screen::List(s) => &s.id,
            Screen::Card(s) => &s.id,
            Screen::Progress(s) => &s.id,
            Screen::Status(s) => &s.id,
        }
    }
}

// ── Screen payloads ─────────────────────────────────────────────────────────

/// Now playing screen — the primary HiFi display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaScreen {
    pub id: String,
    /// URL for album art (relative to bridge, e.g. "/knob/now_playing/image?zone_id=...")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    /// Opaque key for image change detection (avoids re-fetching same artwork)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_key: Option<String>,
    /// Display lines: title, subtitle, detail (firmware renders in order)
    pub lines: Vec<TextLine>,
}

/// Scrollable list screen.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListScreen {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub items: Vec<ListItem>,
}

/// Text card screen — for Memex context, notes, status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CardScreen {
    pub id: String,
    pub lines: Vec<TextLine>,
}

/// Progress bar screen — OTA, long operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressScreen {
    pub id: String,
    pub label: String,
    /// 0.0 to 1.0
    pub progress: f64,
}

/// Status message screen — centered icon + text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusScreen {
    pub id: String,
    pub message: String,
    /// Material Symbols icon name (e.g. "wifi_off", "error")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

// ── Shared primitives ───────────────────────────────────────────────────────

/// A styled text line within a screen.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextLine {
    pub text: String,
    /// Rendering style hint: "title" (large), "subtitle" (medium), "detail" (small, dimmed)
    #[serde(default = "default_style")]
    pub style: String,
}

fn default_style() -> String {
    "detail".to_string()
}

/// An item in a list screen.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListItem {
    pub id: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sublabel: Option<String>,
    #[serde(default)]
    pub selected: bool,
    /// Optional icon name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

// ── Navigation ──────────────────────────────────────────────────────────────

/// Screen navigation order. Knob swipes or button-navigates through this order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Nav {
    /// Screen IDs in navigation order (swipe left/right cycles through these)
    pub order: Vec<String>,
    /// Which screen to show on startup / after zone switch
    #[serde(default = "default_screen")]
    pub default: String,
}

fn default_screen() -> String {
    "now_playing".to_string()
}

// ── SHA computation ─────────────────────────────────────────────────────────

/// Compute SHA for manifest content (screens + nav).
/// Uses the same 8-hex-char prefix convention as zones_sha.
pub fn compute_manifest_sha(screens: &[Screen], nav: &Nav) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    // Serialize screens + nav deterministically via serde_json
    // This is fine for change detection — exact bytes don't matter,
    // only that the same content produces the same hash.
    if let Ok(json) = serde_json::to_string(&(screens, nav)) {
        hasher.update(json.as_bytes());
    }
    let result = hasher.finalize();
    hex::encode(&result[..4])
}
