//! Manifest endpoint handlers for the knob protocol.
//!
//! GET  /knob/manifest  — Serve manifest (Memex-pushed or default from aggregator)
//! POST /knob/manifest  — Accept manifest push from Memex

use axum::{
    extract::{ConnectInfo, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;

use std::collections::HashMap;

use crate::api::AppState;
use crate::knobs::manifest::*;
use crate::knobs::routes::{extract_knob_id, get_all_zones_internal, ZoneInfo};

// ── Manifest store ──────────────────────────────────────────────────────────

/// Holds per-zone Memex-pushed manifests. When a zone has no entry, the GET
/// handler generates a default manifest from aggregator state.
#[derive(Debug, Clone, Default)]
pub struct ManifestStore {
    /// Manifests pushed by Memex via POST, keyed by zone_id. Missing = use default.
    pushed: Arc<RwLock<HashMap<String, PushedManifest>>>,
}

/// A manifest pushed by Memex, with its screens + nav + interactions.
/// The bridge merges real-time `fast` state from the aggregator at serve time.
#[derive(Debug, Clone)]
struct PushedManifest {
    screens: Vec<Screen>,
    nav: Nav,
    interactions: Option<HashMap<String, String>>,
    sha: String,
}

impl ManifestStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Normalize a zone_id: delegates to the shared `normalize_zone_id` in the parent module.
    fn normalize_zone_id(zone_id: &str) -> String {
        super::normalize_zone_id(zone_id)
    }

    /// Store a Memex-pushed manifest for a specific zone. Bridge will serve
    /// this instead of the default for that zone.
    pub async fn set(&self, zone_id: &str, screens: Vec<Screen>, nav: Nav) {
        self.set_full(zone_id, screens, nav, None).await;
    }

    /// Store a manifest with interactions for a specific zone (used by LLM generation).
    pub async fn set_full(
        &self,
        zone_id: &str,
        screens: Vec<Screen>,
        nav: Nav,
        interactions: Option<HashMap<String, String>>,
    ) {
        let key = Self::normalize_zone_id(zone_id);
        let sha = compute_manifest_sha_full(&screens, &nav, &interactions);
        self.pushed.write().await.insert(
            key,
            PushedManifest {
                screens,
                nav,
                interactions,
                sha,
            },
        );
    }

    /// Clear the pushed manifest for a specific zone, or all zones if `zone_id` is None.
    pub async fn clear(&self, zone_id: Option<&str>) {
        let mut map = self.pushed.write().await;
        match zone_id {
            Some(id) => {
                map.remove(&Self::normalize_zone_id(id));
            }
            None => {
                map.clear();
            }
        }
    }

    /// Get the pushed manifest for a specific zone (if present).
    async fn get(&self, zone_id: &str) -> Option<PushedManifest> {
        let key = Self::normalize_zone_id(zone_id);
        self.pushed.read().await.get(&key).cloned()
    }

    /// Get the SHA of the pushed manifest for a specific zone (if any). Used by UDP fast-path.
    pub async fn get_pushed_sha(&self, zone_id: &str) -> Option<String> {
        let key = Self::normalize_zone_id(zone_id);
        self.pushed.read().await.get(&key).map(|p| p.sha.clone())
    }

    /// Get the current full manifest for a specific zone as JSON (for LLM context).
    /// Returns None if no manifest has been pushed for the zone.
    pub async fn get_current_manifest_json(&self, zone_id: &str) -> Option<serde_json::Value> {
        let key = Self::normalize_zone_id(zone_id);
        let pushed = self.pushed.read().await;
        pushed.get(&key).map(|p| {
            serde_json::json!({
                "screens": p.screens,
                "nav": p.nav,
                "interactions": p.interactions,
            })
        })
    }
}

// ── Query params ────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ManifestQuery {
    pub zone_id: Option<String>,
    /// Client's cached manifest SHA. If matches current, returns 304.
    pub sha: Option<String>,
}

/// POST body from Memex pushing a composed manifest.
#[derive(Deserialize)]
pub struct PushManifestBody {
    pub zone_id: String,
    pub screens: Vec<Screen>,
    pub nav: Nav,
    #[serde(default)]
    pub interactions: Option<HashMap<String, String>>,
}

/// Query params for DELETE /knob/manifest.
#[derive(Deserialize)]
pub struct ClearManifestQuery {
    /// Zone to clear. If absent, clears all zones.
    pub zone_id: Option<String>,
}

// ── GET /knob/manifest ──────────────────────────────────────────────────────

/// Serve the manifest for a knob. If Memex has pushed a manifest, use its screens/nav
/// and merge real-time fast state from the aggregator. Otherwise generate a default
/// manifest that reproduces the current `NowPlayingResponse` semantics.
pub async fn knob_manifest_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    connect_info: Result<ConnectInfo<SocketAddr>, axum::extract::rejection::ExtensionRejection>,
    Query(params): Query<ManifestQuery>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let zone_id = match params.zone_id {
        Some(id) => id,
        None => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "zone_id required" })),
            ));
        }
    };

    // Extract device_id from headers, or fall back to source IP lookup.
    // Firmware doesn't send x-knob-id on /manifest, so IP is the primary resolution path.
    let mut device_id = extract_knob_id(&headers, None);
    if device_id.is_none() {
        if let Ok(ConnectInfo(addr)) = connect_info {
            device_id = state.knobs.find_by_ip(&addr.ip().to_string()).await;
        }
    }
    tracing::debug!(device_id = ?device_id, "Manifest request device resolution");

    // Normalize zone_id (legacy without prefix = Roon)
    let prefixed_zone_id = super::normalize_zone_id(&zone_id);

    // Get zone from aggregator
    let zone = match state.aggregator.get_zone(&prefixed_zone_id).await {
        Some(z) => z,
        None => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "zone not found" })),
            ));
        }
    };

    // Build fast state from aggregator (always real-time)
    let is_playing = zone.state == crate::bus::PlaybackState::Playing;
    let vc = zone.volume_control.as_ref();
    let np = zone.now_playing.as_ref();

    let volume_type = vc.map(|v| match v.scale {
        crate::bus::VolumeScale::Decibel => "db".to_string(),
        crate::bus::VolumeScale::Percentage => "number".to_string(),
        crate::bus::VolumeScale::Linear => "number".to_string(),
        crate::bus::VolumeScale::Unknown => "fixed".to_string(),
    });

    let fast = FastState {
        zone_id: zone.zone_id.clone(),
        is_playing,
        volume: vc.map(|v| v.value as f64),
        volume_min: vc.map(|v| v.min as f64).or(Some(0.0)),
        volume_max: vc.map(|v| v.max as f64).or(Some(0.0)),
        volume_step: vc.map(|v| v.step as f64).or(Some(1.0)),
        volume_type,
        seek_position: np.and_then(|n| n.seek_position.map(|p| p as i64)),
        length: np.and_then(|n| n.duration.map(|d| d as u32)),
        transport: Transport {
            play: zone.is_play_allowed,
            pause: zone.is_pause_allowed,
            next: zone.is_next_allowed,
            prev: zone.is_previous_allowed,
        },
    };

    // Check if Memex has pushed a manifest for this zone
    let pushed = state.manifests.get(&prefixed_zone_id).await;

    let (screens, nav, interactions, sha) = if let Some(pushed) = pushed {
        // Memex push takes priority
        (pushed.screens, pushed.nav, pushed.interactions, pushed.sha)
    } else if let Some(ref dev_id) = device_id {
        // Check layout condition stack for this device
        if let Some(layout) = state.layouts.resolve_layout(dev_id, &zone).await {
            let mut screens = merge_live_state(layout.screens, &zone);
            // Always include a zones list screen for the zone picker
            if !screens.iter().any(|s| matches!(s, Screen::List(_))) {
                let zone_infos = get_all_zones_internal(&state).await;
                screens.push(build_zones_screen(&zone_infos, &prefixed_zone_id));
            }
            let mut nav = layout.nav;
            if !nav.order.contains(&"zones".to_string()) {
                nav.order.push("zones".to_string());
            }
            let sha = compute_manifest_sha_full(&screens, &nav, &layout.interactions);
            (screens, nav, layout.interactions, sha)
        } else {
            // No matching layout — fall through to default
            let (screens, nav) = build_default_manifest(&state, &zone, &prefixed_zone_id).await;
            let sha = compute_manifest_sha(&screens, &nav);
            (screens, nav, None, sha)
        }
    } else {
        // No device_id known — generate default manifest from aggregator state
        let (screens, nav) = build_default_manifest(&state, &zone, &prefixed_zone_id).await;
        let sha = compute_manifest_sha(&screens, &nav);
        (screens, nav, None, sha)
    };

    // SHA match: screens unchanged, return only fast state + sha (smaller payload)
    if let Some(ref client_sha) = params.sha {
        if *client_sha == sha {
            let fast_only = serde_json::json!({
                "version": MANIFEST_VERSION,
                "sha": sha,
                "fast": fast,
            });
            return Ok(Json(fast_only).into_response());
        }
    }

    let manifest = Manifest {
        version: MANIFEST_VERSION,
        sha,
        fast,
        screens,
        nav,
        interactions,
    };

    Ok(Json(manifest).into_response())
}

// ── POST /knob/manifest ─────────────────────────────────────────────────────

/// Accept a manifest push from Memex. Replaces the default manifest for the given zone.
pub async fn knob_manifest_push_handler(
    State(state): State<AppState>,
    Json(body): Json<PushManifestBody>,
) -> StatusCode {
    tracing::info!(zone_id = %body.zone_id, screens = body.screens.len(), "Manifest pushed by Memex");
    state
        .manifests
        .set_full(&body.zone_id, body.screens, body.nav, body.interactions)
        .await;
    StatusCode::NO_CONTENT
}

/// DELETE /knob/manifest — Clear pushed manifest (Memex disconnecting).
/// If zone_id query param is provided, clears only that zone. Otherwise clears all.
pub async fn knob_manifest_clear_handler(
    State(state): State<AppState>,
    Query(params): Query<ClearManifestQuery>,
) -> StatusCode {
    match &params.zone_id {
        Some(id) => tracing::info!(zone_id = %id, "Manifest cleared for zone"),
        None => tracing::info!("All manifests cleared (Memex disconnected)"),
    }
    state.manifests.clear(params.zone_id.as_deref()).await;
    StatusCode::NO_CONTENT
}

// ── Live state merge ────────────────────────────────────────────────────────

/// Merge live zone state into layout template screens.
///
/// Layout templates define *structure* (which elements, encoder config).
/// This function fills in *live data*:
/// - Text lines from now_playing (title, artist, album)
/// - Album art URL and image_key
/// - Toggle icon/active state (play/pause, mute/unmute)
pub fn merge_live_state(screens: Vec<Screen>, zone: &crate::bus::Zone) -> Vec<Screen> {
    let is_playing = zone.state == crate::bus::PlaybackState::Playing;
    let is_muted = zone
        .volume_control
        .as_ref()
        .map(|vc| vc.is_muted)
        .unwrap_or(false);
    let np = zone.now_playing.as_ref();

    screens
        .into_iter()
        .map(|screen| match screen {
            Screen::Media(mut media) => {
                // Replace text lines with live now_playing data
                let title = np
                    .map(|n| {
                        if n.title.is_empty() {
                            "Idle".to_string()
                        } else {
                            n.title.clone()
                        }
                    })
                    .unwrap_or_else(|| "Idle".to_string());
                let artist = np.map(|n| n.artist.clone()).unwrap_or_default();
                let mut lines = vec![
                    TextLine {
                        text: title,
                        style: "title".to_string(),
                    },
                    TextLine {
                        text: artist,
                        style: "subtitle".to_string(),
                    },
                ];
                if let Some(album) = np.and_then(|n| {
                    if n.album.is_empty() {
                        None
                    } else {
                        Some(n.album.clone())
                    }
                }) {
                    lines.push(TextLine {
                        text: album,
                        style: "detail".to_string(),
                    });
                }
                media.lines = lines;

                // Set live image URL and key
                media.image_url = Some(format!(
                    "/knob/now_playing/image?zone_id={}",
                    urlencoding::encode(&zone.zone_id)
                ));
                media.image_key = np.and_then(|n| n.image_key.clone());

                // Merge element toggle states
                if let Some(ref mut elements) = media.elements {
                    for elem in elements.iter_mut() {
                        if let Some(ref action) = elem.on_tap {
                            match action.action.as_str() {
                                "toggle_playback" => {
                                    elem.display.icon = Some(
                                        if is_playing { "pause" } else { "play_arrow" }.into(),
                                    );
                                }
                                "toggle_mute" | "mute" => {
                                    elem.display.icon = Some(
                                        if is_muted {
                                            "volume_off"
                                        } else {
                                            "volume_mute"
                                        }
                                        .into(),
                                    );
                                    elem.display.active = Some(is_muted);
                                }
                                "play" => {
                                    elem.display.active = Some(is_playing);
                                }
                                "pause" => {
                                    elem.display.active = Some(!is_playing);
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Screen::Media(media)
            }
            other => other,
        })
        .collect()
}

// ── Default manifest builder ────────────────────────────────────────────────

/// Build context-aware elements for the media screen based on zone state.
///
/// - Omits prev/next elements when the zone doesn't allow those actions.
/// - Uses pause icon when playing, play_arrow when not.
/// - Adds a forward_30 seek element for long content (>600 seconds = podcast/audiobook heuristic).
pub(crate) fn build_default_elements(zone: &crate::bus::Zone) -> Vec<Element> {
    let mut elements = Vec::new();

    // Previous — only if allowed
    if zone.is_previous_allowed {
        elements.push(Element {
            display: Display {
                icon: Some("skip_previous".into()),
                label: None,
                active: None,
            },
            on_tap: Some(Action {
                action: "previous".into(),
                params: None,
            }),
            on_long_press: None,
        });
    }

    // Play/Pause — always present, icon reflects current state
    let is_playing = zone.state == crate::bus::PlaybackState::Playing;
    elements.push(Element {
        display: Display {
            icon: Some(if is_playing { "pause" } else { "play_arrow" }.into()),
            label: None,
            active: None,
        },
        on_tap: Some(Action {
            action: "toggle_playback".into(),
            params: None,
        }),
        on_long_press: Some(Action {
            action: "stop".into(),
            params: None,
        }),
    });

    // Next — only if allowed
    if zone.is_next_allowed {
        elements.push(Element {
            display: Display {
                icon: Some("skip_next".into()),
                label: None,
                active: None,
            },
            on_tap: Some(Action {
                action: "next".into(),
                params: None,
            }),
            on_long_press: None,
        });
    }

    // Podcast/audiobook heuristic: tracks longer than 10 minutes get a seek-forward button
    if let Some(ref np) = zone.now_playing {
        if let Some(length) = np.duration {
            if length > 600.0 {
                elements.push(Element {
                    display: Display {
                        icon: Some("forward_30".into()),
                        label: None,
                        active: None,
                    },
                    on_tap: Some(Action {
                        action: "seek_forward".into(),
                        params: Some(HashMap::from([("seconds".into(), serde_json::json!(30))])),
                    }),
                    on_long_press: None,
                });
            }
        }
    }

    elements
}

/// Build backward-compatible controls list that mirrors the transport-permission-aware elements.
pub(crate) fn build_default_controls(zone: &crate::bus::Zone) -> Vec<String> {
    let mut controls = Vec::new();
    if zone.is_previous_allowed {
        controls.push("prev".into());
    }
    controls.push("play".into()); // always
    if zone.is_next_allowed {
        controls.push("next".into());
    }
    controls
}

/// Build the default manifest from aggregator state. This produces screens
/// that are pixel-identical to what the current hardcoded firmware renders,
/// but now context-aware: transport elements are only included when allowed.
pub(crate) async fn build_default_manifest(
    state: &AppState,
    zone: &crate::bus::Zone,
    zone_id: &str,
) -> (Vec<Screen>, Nav) {
    let np = zone.now_playing.as_ref();

    // Media screen (now_playing)
    let line1 = np
        .map(|n| {
            if n.title.is_empty() {
                "Idle".to_string()
            } else {
                n.title.clone()
            }
        })
        .unwrap_or_else(|| "Idle".to_string());
    let line2 = np.map(|n| n.artist.clone()).unwrap_or_default();
    let line3 = np.and_then(|n| {
        if n.album.is_empty() {
            None
        } else {
            Some(n.album.clone())
        }
    });

    let image_url = format!(
        "/knob/now_playing/image?zone_id={}",
        urlencoding::encode(zone_id)
    );
    let image_key = np.and_then(|n| n.image_key.clone());

    // Look up cached edge color for album art background
    let background_color = if let Some(ref key) = image_key {
        state.art_colors.read().await.get(key).cloned()
    } else {
        None
    };

    let mut lines = vec![
        TextLine {
            text: line1,
            style: "title".to_string(),
        },
        TextLine {
            text: line2,
            style: "subtitle".to_string(),
        },
    ];
    if let Some(album) = line3 {
        lines.push(TextLine {
            text: album,
            style: "detail".to_string(),
        });
    }

    // Context-aware elements and backward-compat controls
    let elements = build_default_elements(zone);
    let controls = build_default_controls(zone);

    // v2 encoder for media screen
    let encoder = Encoder {
        cw: Action {
            action: "volume_up".into(),
            params: None,
        },
        ccw: Action {
            action: "volume_down".into(),
            params: None,
        },
        press: Some(Action {
            action: "toggle_playback".into(),
            params: None,
        }),
        long_press: Some(Action {
            action: "show_zone_picker".into(),
            params: None,
        }),
    };

    let media = Screen::Media(MediaScreen {
        id: "now_playing".to_string(),
        image_url: Some(image_url),
        image_key,
        background_color,
        lines,
        // Backward-compat v1 controls (transport-permission-aware)
        controls: Some(controls),
        elements: Some(elements),
        encoder: Some(encoder),
    });

    // Zones list screen
    let zone_infos = get_all_zones_internal(state).await;
    let zones_screen = build_zones_screen(&zone_infos, zone_id);

    let screens = vec![media, zones_screen];
    let nav = Nav {
        order: vec!["now_playing".to_string(), "zones".to_string()],
        default: "now_playing".to_string(),
    };

    (screens, nav)
}

/// Build the zones list screen from zone info.
/// Each list item includes an on_tap action to select that zone.
pub(crate) fn build_zones_screen(zones: &[ZoneInfo], current_zone_id: &str) -> Screen {
    let items = zones
        .iter()
        .map(|z| ListItem {
            id: z.zone_id.clone(),
            label: z.zone_name.clone(),
            sublabel: Some(z.state.clone()),
            selected: z.zone_id == current_zone_id,
            icon: None,
            on_tap: Some(Action {
                action: "select_zone".into(),
                params: Some(HashMap::from([(
                    "zone_id".into(),
                    serde_json::json!(z.zone_id),
                )])),
            }),
        })
        .collect();

    // v2 encoder for zones list screen
    let zones_encoder = Encoder {
        cw: Action {
            action: "list_scroll_down".into(),
            params: None,
        },
        ccw: Action {
            action: "list_scroll_up".into(),
            params: None,
        },
        press: Some(Action {
            action: "list_select".into(),
            params: None,
        }),
        long_press: None,
    };

    Screen::List(ListScreen {
        id: "zones".to_string(),
        title: Some("Zones".to_string()),
        items,
        encoder: Some(zones_encoder),
    })
}

// ── GET /api/actions ────────────────────────────────────────────────────────

/// Metadata for a single available action.
#[derive(serde::Serialize)]
pub struct ActionInfo {
    pub name: String,
    pub description: String,
    pub category: String,
    pub has_params: bool,
}

/// Return the list of available actions with metadata.
///
/// Used by the Configurator (Phase 4) and LLM generation to know what actions
/// can be wired to elements, encoders, and list items.
pub async fn actions_handler() -> Json<Vec<ActionInfo>> {
    Json(vec![
        ActionInfo {
            name: "toggle_playback".into(),
            description: "Play if paused, pause if playing".into(),
            category: "transport".into(),
            has_params: false,
        },
        ActionInfo {
            name: "play".into(),
            description: "Start playback".into(),
            category: "transport".into(),
            has_params: false,
        },
        ActionInfo {
            name: "pause".into(),
            description: "Pause playback".into(),
            category: "transport".into(),
            has_params: false,
        },
        ActionInfo {
            name: "stop".into(),
            description: "Stop playback".into(),
            category: "transport".into(),
            has_params: false,
        },
        ActionInfo {
            name: "next".into(),
            description: "Skip to next track".into(),
            category: "transport".into(),
            has_params: false,
        },
        ActionInfo {
            name: "previous".into(),
            description: "Go to previous track".into(),
            category: "transport".into(),
            has_params: false,
        },
        ActionInfo {
            name: "seek_forward".into(),
            description: "Seek forward by seconds".into(),
            category: "transport".into(),
            has_params: true,
        },
        ActionInfo {
            name: "seek_backward".into(),
            description: "Seek backward by seconds".into(),
            category: "transport".into(),
            has_params: true,
        },
        ActionInfo {
            name: "volume_up".into(),
            description: "Increase volume by one step".into(),
            category: "volume".into(),
            has_params: false,
        },
        ActionInfo {
            name: "volume_down".into(),
            description: "Decrease volume by one step".into(),
            category: "volume".into(),
            has_params: false,
        },
        ActionInfo {
            name: "toggle_mute".into(),
            description: "Toggle mute on/off".into(),
            category: "volume".into(),
            has_params: false,
        },
        ActionInfo {
            name: "mute".into(),
            description: "Mute the zone".into(),
            category: "volume".into(),
            has_params: false,
        },
        ActionInfo {
            name: "unmute".into(),
            description: "Unmute the zone".into(),
            category: "volume".into(),
            has_params: false,
        },
        ActionInfo {
            name: "show_zone_picker".into(),
            description: "Show the zone selection screen".into(),
            category: "navigation".into(),
            has_params: false,
        },
        ActionInfo {
            name: "select_zone".into(),
            description: "Switch to a specific zone".into(),
            category: "zone".into(),
            has_params: true,
        },
        ActionInfo {
            name: "list_scroll_up".into(),
            description: "Scroll list up by one item".into(),
            category: "navigation".into(),
            has_params: false,
        },
        ActionInfo {
            name: "list_scroll_down".into(),
            description: "Scroll list down by one item".into(),
            category: "navigation".into(),
            has_params: false,
        },
        ActionInfo {
            name: "list_select".into(),
            description: "Select the focused list item".into(),
            category: "navigation".into(),
            has_params: false,
        },
    ])
}

// ── GET /api/icons ─────────────────────────────────────────────────────────

/// Metadata for an available icon.
#[derive(serde::Serialize)]
pub struct IconInfo {
    /// Icon name as used in element display.icon (Material Symbols name)
    pub name: String,
    /// Human-readable label
    pub label: String,
    /// Category for grouping in UI
    pub category: String,
}

/// Return the list of icons the firmware can render.
///
/// Used by the Configurator icon picker and LLM generation prompt
/// to know which icons are available on the device.
pub async fn icons_handler() -> Json<Vec<IconInfo>> {
    Json(vec![
        // Media transport
        IconInfo {
            name: "play_arrow".into(),
            label: "Play".into(),
            category: "transport".into(),
        },
        IconInfo {
            name: "pause".into(),
            label: "Pause".into(),
            category: "transport".into(),
        },
        IconInfo {
            name: "stop".into(),
            label: "Stop".into(),
            category: "transport".into(),
        },
        IconInfo {
            name: "skip_previous".into(),
            label: "Previous".into(),
            category: "transport".into(),
        },
        IconInfo {
            name: "skip_next".into(),
            label: "Next".into(),
            category: "transport".into(),
        },
        IconInfo {
            name: "shuffle".into(),
            label: "Shuffle".into(),
            category: "transport".into(),
        },
        IconInfo {
            name: "repeat".into(),
            label: "Repeat".into(),
            category: "transport".into(),
        },
        IconInfo {
            name: "repeat_one".into(),
            label: "Repeat One".into(),
            category: "transport".into(),
        },
        // Seek / skip
        IconInfo {
            name: "forward_5".into(),
            label: "Forward 5s".into(),
            category: "seek".into(),
        },
        IconInfo {
            name: "forward_10".into(),
            label: "Forward 10s".into(),
            category: "seek".into(),
        },
        IconInfo {
            name: "forward_30".into(),
            label: "Forward 30s".into(),
            category: "seek".into(),
        },
        IconInfo {
            name: "replay_5".into(),
            label: "Replay 5s".into(),
            category: "seek".into(),
        },
        IconInfo {
            name: "replay_10".into(),
            label: "Replay 10s".into(),
            category: "seek".into(),
        },
        IconInfo {
            name: "replay_30".into(),
            label: "Replay 30s".into(),
            category: "seek".into(),
        },
        // Volume
        IconInfo {
            name: "volume_up".into(),
            label: "Volume Up".into(),
            category: "volume".into(),
        },
        IconInfo {
            name: "volume_down".into(),
            label: "Volume Down".into(),
            category: "volume".into(),
        },
        IconInfo {
            name: "volume_mute".into(),
            label: "Volume Mute".into(),
            category: "volume".into(),
        },
        IconInfo {
            name: "volume_off".into(),
            label: "Volume Off".into(),
            category: "volume".into(),
        },
        // Other
        IconInfo {
            name: "music_note".into(),
            label: "Music Note".into(),
            category: "other".into(),
        },
        IconInfo {
            name: "settings".into(),
            label: "Settings".into(),
            category: "other".into(),
        },
    ])
}
