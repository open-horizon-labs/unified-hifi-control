//! Knobs hardware API routes
//!
//! These endpoints are called by S3 Knob devices:
//! - GET /knob/zones - List available zones
//! - GET /knob/now_playing - Current playback state + album art URL
//! - GET /knob/now_playing/image - Album art (JPEG or RGB565)
//! - POST /knob/control - Playback control commands
//! - GET /knob/config - Get device configuration
//! - POST /knob/config - Update device configuration
//! - GET /knob/devices - List registered knobs (admin)

use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::api::AppState;
use crate::knobs::image::{jpeg_to_rgb565, placeholder_svg, resize_jpeg};
use crate::knobs::store::{KnobConfigUpdate, KnobStatusUpdate, KnobStore};

/// Extract knob ID from headers or query params
fn extract_knob_id(headers: &HeaderMap, query_knob_id: Option<&str>) -> Option<String> {
    headers
        .get("x-knob-id")
        .or_else(|| headers.get("x-device-id"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| query_knob_id.map(|s| s.to_string()))
}

/// Extract knob version from headers
fn extract_knob_version(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-knob-version")
        .or_else(|| headers.get("x-device-version"))
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Zone info for knob response
#[derive(Serialize)]
pub struct ZoneInfo {
    pub zone_id: String,
    pub display_name: String,
    pub state: String,
}

/// GET /knob/zones response
#[derive(Serialize)]
pub struct ZonesResponse {
    pub zones: Vec<ZoneInfo>,
}

/// GET /knob/zones - List all zones
pub async fn knob_zones_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Json<ZonesResponse> {
    let zones = state.roon.get_zones().await;

    let zones: Vec<ZoneInfo> = zones
        .into_iter()
        .map(|z| ZoneInfo {
            zone_id: z.zone_id,
            display_name: z.display_name,
            state: z.state,
        })
        .collect();

    Json(ZonesResponse { zones })
}

/// Query params for now_playing
#[derive(Deserialize)]
pub struct NowPlayingQuery {
    pub zone_id: Option<String>,
    pub knob_id: Option<String>,
    pub battery_level: Option<u8>,
    pub battery_charging: Option<String>,
}

/// Now playing response for knob
#[derive(Serialize)]
pub struct NowPlayingResponse {
    pub zone_id: String,
    pub state: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub image_url: Option<String>,
    pub image_key: Option<String>,
    pub seek_position: Option<i64>,
    pub length: Option<u32>,
    pub is_play_allowed: bool,
    pub is_pause_allowed: bool,
    pub is_next_allowed: bool,
    pub is_previous_allowed: bool,
    pub zones: Vec<ZoneInfo>,
    pub config_sha: Option<String>,
}

/// GET /knob/now_playing - Get current playback state
pub async fn knob_now_playing_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<NowPlayingQuery>,
) -> Result<Json<NowPlayingResponse>, (StatusCode, Json<serde_json::Value>)> {
    let zone_id = params.zone_id.ok_or_else(|| {
        let zones = futures::executor::block_on(state.roon.get_zones());
        let zone_infos: Vec<ZoneInfo> = zones.into_iter().map(|z| ZoneInfo {
            zone_id: z.zone_id,
            display_name: z.display_name,
            state: z.state,
        }).collect();
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "zone_id required",
                "error_code": "MISSING_ZONE_ID",
                "zones": zone_infos
            })),
        )
    })?;

    // Update knob status if knob ID present
    let knob_id = extract_knob_id(&headers, params.knob_id.as_deref());
    let knob_version = extract_knob_version(&headers);
    let mut config_sha = None;

    if let Some(ref id) = knob_id {
        // Get or create knob
        state.knobs.get_or_create(id, knob_version.as_deref()).await;

        // Update status
        let mut status_update = KnobStatusUpdate::default();
        status_update.zone_id = Some(zone_id.clone());

        if let Some(level) = params.battery_level {
            if level <= 100 {
                status_update.battery_level = Some(level);
            }
        }
        if let Some(ref charging) = params.battery_charging {
            status_update.battery_charging = Some(charging == "1" || charging == "true");
        }

        state.knobs.update_status(id, status_update).await;
        config_sha = state.knobs.get_config_sha(id).await;
    }

    // Get zone data
    let zone = state.roon.get_zone(&zone_id).await.ok_or_else(|| {
        let zones = futures::executor::block_on(state.roon.get_zones());
        let zone_infos: Vec<ZoneInfo> = zones.into_iter().map(|z| ZoneInfo {
            zone_id: z.zone_id,
            display_name: z.display_name,
            state: z.state,
        }).collect();
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "zone not found",
                "error_code": "ZONE_NOT_FOUND",
                "zones": zone_infos
            })),
        )
    })?;

    let np = zone.now_playing;
    let image_url = format!("/knob/now_playing/image?zone_id={}", urlencoding::encode(&zone_id));

    let all_zones = state.roon.get_zones().await;
    let zone_infos: Vec<ZoneInfo> = all_zones.into_iter().map(|z| ZoneInfo {
        zone_id: z.zone_id,
        display_name: z.display_name,
        state: z.state,
    }).collect();

    Ok(Json(NowPlayingResponse {
        zone_id: zone.zone_id,
        state: zone.state,
        title: np.as_ref().map(|n| n.title.clone()),
        artist: np.as_ref().map(|n| n.artist.clone()),
        album: np.as_ref().map(|n| n.album.clone()),
        image_url: Some(image_url),
        image_key: np.as_ref().and_then(|n| n.image_key.clone()),
        seek_position: np.as_ref().and_then(|n| n.seek_position),
        length: np.as_ref().and_then(|n| n.length),
        is_play_allowed: zone.is_play_allowed,
        is_pause_allowed: zone.is_pause_allowed,
        is_next_allowed: zone.is_next_allowed,
        is_previous_allowed: zone.is_previous_allowed,
        zones: zone_infos,
        config_sha,
    }))
}

/// Query params for image endpoint
#[derive(Deserialize)]
pub struct ImageQuery {
    pub zone_id: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub format: Option<String>,
}

/// GET /knob/now_playing/image - Get album artwork
pub async fn knob_image_handler(
    State(state): State<AppState>,
    Query(params): Query<ImageQuery>,
) -> Response {
    let target_width = params.width.unwrap_or(240);
    let target_height = params.height.unwrap_or(240);
    let format = params.format.as_deref().unwrap_or("jpeg");

    // Get zone to find image key
    let zone = match state.roon.get_zone(&params.zone_id).await {
        Some(z) => z,
        None => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"error":"zone not found"}"#))
                .unwrap();
        }
    };

    let image_key = match zone.now_playing.and_then(|np| np.image_key) {
        Some(key) => key,
        None => {
            // Return placeholder SVG
            let svg = placeholder_svg(target_width, target_height);
            return Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "image/svg+xml")
                .body(Body::from(svg))
                .unwrap();
        }
    };

    // TODO: Fetch actual image from Roon API
    // For now, return placeholder since we need to implement image fetching
    // from rust-roon-api's image service

    // Placeholder: return SVG for now
    // In production, this would:
    // 1. Fetch image from Roon: state.roon.get_image(&image_key, opts).await
    // 2. If format == "rgb565", convert with jpeg_to_rgb565()
    // 3. Otherwise, resize and return JPEG

    let svg = placeholder_svg(target_width, target_height);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/svg+xml")
        .body(Body::from(svg))
        .unwrap()
}

/// Control request body
#[derive(Deserialize)]
pub struct KnobControlRequest {
    pub zone_id: String,
    pub action: String,
    pub value: Option<serde_json::Value>,
}

/// POST /knob/control - Send control command
pub async fn knob_control_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<KnobControlRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // Map knob actions to Roon transport actions
    let roon_action = match req.action.as_str() {
        "play" => "play",
        "pause" => "pause",
        "play_pause" | "playpause" => "play_pause",
        "next" => "next",
        "previous" | "prev" => "previous",
        "stop" => "stop",
        "vol_up" | "volume_up" => {
            // Handle relative volume
            if let Some(output) = get_first_output_id(&state, &req.zone_id).await {
                let step = req.value.as_ref()
                    .and_then(|v| v.as_i64())
                    .unwrap_or(1) as i32;
                let _ = state.roon.change_volume(&output, step, true).await;
            }
            return Ok(Json(serde_json::json!({"ok": true})));
        }
        "vol_down" | "volume_down" => {
            if let Some(output) = get_first_output_id(&state, &req.zone_id).await {
                let step = req.value.as_ref()
                    .and_then(|v| v.as_i64())
                    .unwrap_or(1) as i32;
                let _ = state.roon.change_volume(&output, -step, true).await;
            }
            return Ok(Json(serde_json::json!({"ok": true})));
        }
        "vol_abs" | "volume" => {
            if let Some(output) = get_first_output_id(&state, &req.zone_id).await {
                let value = req.value.as_ref()
                    .and_then(|v| v.as_i64())
                    .unwrap_or(50) as i32;
                let _ = state.roon.change_volume(&output, value, false).await;
            }
            return Ok(Json(serde_json::json!({"ok": true})));
        }
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Unknown action: {}", req.action)})),
            ));
        }
    };

    match state.roon.control(&req.zone_id, roon_action).await {
        Ok(()) => Ok(Json(serde_json::json!({"ok": true}))),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )),
    }
}

/// Helper to get first output ID for a zone (for volume control)
async fn get_first_output_id(state: &AppState, zone_id: &str) -> Option<String> {
    let zone = state.roon.get_zone(zone_id).await?;
    zone.outputs.first().map(|o| o.output_id.clone())
}

/// GET /knob/config - Get knob configuration
pub async fn knob_config_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<KnobIdQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let knob_id = extract_knob_id(&headers, params.knob_id.as_deref())
        .ok_or_else(|| (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "knob_id required"})),
        ))?;

    let knob = state.knobs.get(&knob_id).await
        .ok_or_else(|| (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "knob not found"})),
        ))?;

    Ok(Json(serde_json::json!({
        "knob_id": knob_id,
        "name": knob.name,
        "config": knob.config,
        "config_sha": knob.config_sha,
    })))
}

#[derive(Deserialize)]
pub struct KnobIdQuery {
    pub knob_id: Option<String>,
}

/// POST /knob/config - Update knob configuration
pub async fn knob_config_update_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<KnobIdQuery>,
    Json(updates): Json<KnobConfigUpdate>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let knob_id = extract_knob_id(&headers, params.knob_id.as_deref())
        .ok_or_else(|| (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "knob_id required"})),
        ))?;

    let knob = state.knobs.update_config(&knob_id, updates).await
        .ok_or_else(|| (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "knob not found"})),
        ))?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "config_sha": knob.config_sha,
    })))
}

/// GET /knob/devices - List all registered knobs (admin)
pub async fn knob_devices_handler(
    State(state): State<AppState>,
) -> Json<serde_json::Value> {
    let knobs = state.knobs.list().await;
    Json(serde_json::json!({ "knobs": knobs }))
}
