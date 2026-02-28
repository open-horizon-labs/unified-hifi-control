//! Manifest endpoint handlers for the knob protocol.
//!
//! GET  /knob/manifest  — Serve manifest (Memex-pushed or default from aggregator)
//! POST /knob/manifest  — Accept manifest push from Memex

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::RwLock;

use std::collections::HashMap;

use crate::api::AppState;
use crate::knobs::manifest::*;
use crate::knobs::routes::{get_all_zones_internal, ZoneInfo};

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
        let sha = compute_manifest_sha_full(&screens, &nav, &interactions);
        self.pushed.write().await.insert(
            zone_id.to_string(),
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
                map.remove(id);
            }
            None => {
                map.clear();
            }
        }
    }

    /// Get the pushed manifest for a specific zone (if present).
    async fn get(&self, zone_id: &str) -> Option<PushedManifest> {
        self.pushed.read().await.get(zone_id).cloned()
    }

    /// Get the SHA of the pushed manifest for a specific zone (if any). Used by UDP fast-path.
    pub async fn get_pushed_sha(&self, zone_id: &str) -> Option<String> {
        self.pushed.read().await.get(zone_id).map(|p| p.sha.clone())
    }

    /// Get the current full manifest for a specific zone as JSON (for LLM context).
    /// Returns None if no manifest has been pushed for the zone.
    pub async fn get_current_manifest_json(&self, zone_id: &str) -> Option<serde_json::Value> {
        let pushed = self.pushed.read().await;
        pushed.get(zone_id).map(|p| {
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

    // Normalize zone_id (legacy without prefix = Roon)
    let prefixed_zone_id = if !zone_id.contains(':') {
        format!("roon:{}", zone_id)
    } else {
        zone_id.clone()
    };

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
        (pushed.screens, pushed.nav, pushed.interactions, pushed.sha)
    } else {
        // Generate default manifest from aggregator state
        let (screens, nav) = build_default_manifest(&state, &zone, &zone_id).await;
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

// ── Default manifest builder ────────────────────────────────────────────────

/// Build the default manifest from aggregator state. This produces screens
/// that are pixel-identical to what the current hardcoded firmware renders.
async fn build_default_manifest(
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

    let media = Screen::Media(MediaScreen {
        id: "now_playing".to_string(),
        image_url: Some(image_url),
        image_key,
        background_color,
        lines,
        controls: None,
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
fn build_zones_screen(zones: &[ZoneInfo], current_zone_id: &str) -> Screen {
    let items = zones
        .iter()
        .map(|z| ListItem {
            id: z.zone_id.clone(),
            label: z.zone_name.clone(),
            sublabel: Some(z.state.clone()),
            selected: z.zone_id == current_zone_id,
            icon: None,
        })
        .collect();

    Screen::List(ListScreen {
        id: "zones".to_string(),
        title: Some("Zones".to_string()),
        items,
    })
}
