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
    response::Response,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::api::AppState;
use crate::knobs::image::placeholder_svg;
use crate::knobs::store::{KnobConfigUpdate, KnobStatusUpdate};

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

/// DSP info for zones linked to HQPlayer (iOS compatible)
#[derive(Serialize, Clone)]
pub struct DspInfo {
    pub r#type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pipeline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profiles: Option<String>,
}

/// Zone info for knob response - matches Node.js bus adapter format
#[derive(Serialize)]
pub struct ZoneInfo {
    pub zone_id: String,
    pub zone_name: String,
    pub source: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dsp: Option<DspInfo>,
}

/// GET /knob/zones response
#[derive(Serialize)]
pub struct ZonesResponse {
    pub zones: Vec<ZoneInfo>,
}

/// GET /knob/zones - List all zones from all adapters
pub async fn knob_zones_handler(
    State(state): State<AppState>,
    _headers: HeaderMap,
) -> Json<ZonesResponse> {
    let zones = get_all_zones_internal(&state).await;
    Json(ZonesResponse { zones })
}

/// Helper to aggregate zones from all adapters (respects adapter settings, public for UI module)
pub async fn get_all_zones_internal(state: &AppState) -> Vec<ZoneInfo> {
    use crate::api::load_app_settings;
    use std::collections::HashMap;

    let settings = load_app_settings();
    let adapters = settings.adapters;

    // Get HQPlayer zone links for DSP field population
    let hqp_links: HashMap<String, String> = state
        .hqp_zone_links
        .get_links()
        .await
        .into_iter()
        .map(|l| (l.zone_id, l.instance))
        .collect();

    // Helper to create DspInfo if zone is linked to HQPlayer
    let get_dsp = |zone_id: &str| -> Option<DspInfo> {
        hqp_links.get(zone_id).map(|instance| DspInfo {
            r#type: "hqplayer".to_string(),
            instance: Some(instance.clone()),
            pipeline: Some(format!(
                "/hqp/pipeline?zone_id={}",
                urlencoding::encode(zone_id)
            )),
            profiles: Some("/hqp/profiles".to_string()),
        })
    };

    let mut zones = Vec::new();

    // Roon zones (prefixed with roon: for routing)
    if adapters.roon {
        for z in state.roon.get_zones().await {
            let zone_id = format!("roon:{}", z.zone_id);
            zones.push(ZoneInfo {
                dsp: get_dsp(&zone_id),
                zone_id,
                zone_name: z.display_name,
                source: "roon".to_string(),
                state: z.state,
            });
        }
    }

    // LMS players (prefixed with lms:)
    if adapters.lms {
        for p in state.lms.get_cached_players().await {
            let zone_id = format!("lms:{}", p.playerid);
            zones.push(ZoneInfo {
                dsp: get_dsp(&zone_id),
                zone_id,
                zone_name: p.name,
                source: "lms".to_string(),
                state: if p.mode == "play" {
                    "playing".to_string()
                } else if p.mode == "pause" {
                    "paused".to_string()
                } else {
                    "stopped".to_string()
                },
            });
        }
    }

    // OpenHome zones (prefixed with openhome:)
    if adapters.openhome {
        for z in state.openhome.get_zones().await {
            let zone_id = format!("openhome:{}", z.zone_id);
            zones.push(ZoneInfo {
                dsp: get_dsp(&zone_id),
                zone_id,
                zone_name: z.zone_name,
                source: "openhome".to_string(),
                state: z.state.clone(),
            });
        }
    }

    // UPnP zones (prefixed with upnp:)
    if adapters.upnp {
        for z in state.upnp.get_zones().await {
            let zone_id = format!("upnp:{}", z.zone_id);
            zones.push(ZoneInfo {
                dsp: get_dsp(&zone_id),
                zone_id,
                zone_name: z.zone_name,
                source: "upnp".to_string(),
                state: z.state.clone(),
            });
        }
    }

    zones
}

/// Query params for now_playing
#[derive(Deserialize)]
pub struct NowPlayingQuery {
    pub zone_id: Option<String>,
    pub knob_id: Option<String>,
    pub battery_level: Option<u8>,
    pub battery_charging: Option<String>,
}

/// Now playing response for knob - matches Node.js format
/// Node.js uses line1/line2/line3/is_playing (see src/roon/client.js:200-203)
#[derive(Serialize)]
pub struct NowPlayingResponse {
    pub zone_id: String,
    pub line1: String,
    pub line2: String,
    pub line3: Option<String>,
    pub is_playing: bool,
    pub volume: Option<f64>,
    pub volume_type: Option<String>,
    pub volume_min: Option<f64>,
    pub volume_max: Option<f64>,
    pub volume_step: Option<f64>,
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

/// Helper to build zone info list for error responses
async fn get_zone_infos(state: &AppState) -> Vec<ZoneInfo> {
    get_all_zones_internal(state).await
}

/// GET /knob/now_playing - Get current playback state (routes by zone_id prefix)
pub async fn knob_now_playing_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<NowPlayingQuery>,
) -> Result<Json<NowPlayingResponse>, (StatusCode, Json<serde_json::Value>)> {
    // Check zone_id first
    let zone_id = match params.zone_id {
        Some(id) => id,
        None => {
            let zone_infos = get_zone_infos(&state).await;
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "zone_id required",
                    "error_code": "MISSING_ZONE_ID",
                    "zones": zone_infos
                })),
            ));
        }
    };

    // Update knob status if knob ID present
    let knob_id = extract_knob_id(&headers, params.knob_id.as_deref());
    let knob_version = extract_knob_version(&headers);
    let mut config_sha = None;

    if let Some(ref id) = knob_id {
        state.knobs.get_or_create(id, knob_version.as_deref()).await;
        let battery_level = params.battery_level.filter(|&level| level <= 100);
        let battery_charging = params
            .battery_charging
            .as_ref()
            .map(|c| c == "1" || c == "true");
        let status_update = KnobStatusUpdate {
            zone_id: Some(zone_id.clone()),
            battery_level,
            battery_charging,
            ..Default::default()
        };
        state.knobs.update_status(id, status_update).await;
        config_sha = state.knobs.get_config_sha(id).await;
    }

    let image_url = format!(
        "/knob/now_playing/image?zone_id={}",
        urlencoding::encode(&zone_id)
    );
    let zone_infos = get_zone_infos(&state).await;

    // Route based on zone_id prefix
    if zone_id.starts_with("lms:") {
        // LMS player
        let player_id = zone_id.trim_start_matches("lms:");
        let player = match state.lms.get_cached_player(player_id).await {
            Some(p) => p,
            None => {
                return Err((
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({
                        "error": "zone not found",
                        "error_code": "ZONE_NOT_FOUND",
                        "zones": zone_infos
                    })),
                ));
            }
        };

        let state_str = if player.mode == "play" {
            "playing"
        } else if player.mode == "pause" {
            "paused"
        } else {
            "stopped"
        };

        // Node.js format: line1/line2/line3/is_playing with volume info
        let is_playing = state_str == "playing";

        Ok(Json(NowPlayingResponse {
            zone_id: zone_id.clone(),
            line1: if player.title.is_empty() {
                "Idle".to_string()
            } else {
                player.title
            },
            line2: player.artist,
            line3: if player.album.is_empty() {
                None
            } else {
                Some(player.album)
            },
            is_playing,
            volume: Some(player.volume as f64),
            volume_type: Some("number".to_string()),
            volume_min: Some(0.0),
            volume_max: Some(100.0),
            volume_step: Some(1.0),
            image_url: Some(image_url),
            image_key: player
                .artwork_url
                .or(player.coverid)
                .or(player.artwork_track_id),
            seek_position: Some(player.time as i64),
            length: Some(player.duration as u32),
            is_play_allowed: !is_playing,
            is_pause_allowed: is_playing,
            is_next_allowed: true,
            is_previous_allowed: true,
            zones: zone_infos,
            config_sha,
        }))
    } else if zone_id.starts_with("openhome:") {
        // OpenHome zone - zone_id is the UUID
        let uuid = zone_id.trim_start_matches("openhome:");
        let device = match state.openhome.get_zone(uuid).await {
            Some(d) => d,
            None => {
                return Err((
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({
                        "error": "zone not found",
                        "error_code": "ZONE_NOT_FOUND",
                        "zones": zone_infos
                    })),
                ));
            }
        };

        let np = state.openhome.get_now_playing(uuid).await;
        let is_playing = device.state == "playing";

        Ok(Json(NowPlayingResponse {
            zone_id: zone_id.clone(),
            line1: np
                .as_ref()
                .map(|n| n.line1.clone())
                .unwrap_or_else(|| "Idle".to_string()),
            line2: np.as_ref().map(|n| n.line2.clone()).unwrap_or_default(),
            line3: np.as_ref().and_then(|n| {
                if n.line3.is_empty() {
                    None
                } else {
                    Some(n.line3.clone())
                }
            }),
            is_playing,
            volume: np.as_ref().and_then(|n| n.volume.map(|v| v as f64)),
            volume_type: Some("number".to_string()),
            volume_min: np.as_ref().map(|n| n.volume_min as f64),
            volume_max: np.as_ref().map(|n| n.volume_max as f64),
            volume_step: Some(1.0),
            image_url: Some(image_url),
            image_key: np.as_ref().and_then(|n| n.image_key.clone()),
            seek_position: np.as_ref().and_then(|n| n.seek_position),
            length: np.as_ref().and_then(|n| n.length),
            is_play_allowed: !is_playing,
            is_pause_allowed: is_playing,
            is_next_allowed: true,
            is_previous_allowed: true,
            zones: zone_infos,
            config_sha,
        }))
    } else if zone_id.starts_with("upnp:") {
        // UPnP zone - zone_id_part is the zone_id from UPnPZone
        let zone_id_part = zone_id.trim_start_matches("upnp:");
        let zones = state.upnp.get_zones().await;
        let zone = match zones.into_iter().find(|z| z.zone_id == zone_id_part) {
            Some(z) => z,
            None => {
                return Err((
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({
                        "error": "zone not found",
                        "error_code": "ZONE_NOT_FOUND",
                        "zones": zone_infos
                    })),
                ));
            }
        };

        let np = state.upnp.get_now_playing(zone_id_part).await;
        let is_playing = zone.state == "playing";

        Ok(Json(NowPlayingResponse {
            zone_id: zone_id.clone(),
            line1: np
                .as_ref()
                .map(|n| n.line1.clone())
                .unwrap_or_else(|| "Idle".to_string()),
            line2: np.as_ref().map(|n| n.line2.clone()).unwrap_or_default(),
            line3: np.as_ref().and_then(|n| {
                if n.line3.is_empty() {
                    None
                } else {
                    Some(n.line3.clone())
                }
            }),
            is_playing,
            volume: np.as_ref().and_then(|n| n.volume.map(|v| v as f64)),
            volume_type: Some("number".to_string()),
            volume_min: np.as_ref().map(|n| n.volume_min as f64),
            volume_max: np.as_ref().map(|n| n.volume_max as f64),
            volume_step: Some(1.0),
            image_url: Some(image_url),
            image_key: np.as_ref().and_then(|n| n.image_key.clone()),
            seek_position: np.as_ref().and_then(|n| n.seek_position),
            length: np.as_ref().and_then(|n| n.length),
            is_play_allowed: !is_playing,
            is_pause_allowed: is_playing,
            is_next_allowed: true,
            is_previous_allowed: true,
            zones: zone_infos,
            config_sha,
        }))
    } else {
        // Roon zone (or legacy zone_id without prefix)
        let roon_zone_id = if zone_id.starts_with("roon:") {
            zone_id.trim_start_matches("roon:").to_string()
        } else {
            zone_id.clone()
        };

        let zone = match state.roon.get_zone(&roon_zone_id).await {
            Some(z) => z,
            None => {
                return Err((
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({
                        "error": "zone not found",
                        "error_code": "ZONE_NOT_FOUND",
                        "zones": zone_infos
                    })),
                ));
            }
        };

        let np = zone.now_playing;
        let is_playing = zone.state == "playing";

        // Node.js format: line1 = title, line2 = artist, line3 = album
        let line1 = np
            .as_ref()
            .map(|n| n.title.clone())
            .unwrap_or_else(|| "Idle".to_string());
        let line2 = np.as_ref().map(|n| n.artist.clone()).unwrap_or_default();
        let line3 = np.as_ref().and_then(|n| {
            if n.album.is_empty() {
                None
            } else {
                Some(n.album.clone())
            }
        });

        // Extract volume from first output (Roon zones have volume per-output)
        let vol = zone.outputs.first().and_then(|o| o.volume.as_ref());

        Ok(Json(NowPlayingResponse {
            zone_id: zone.zone_id,
            line1,
            line2,
            line3,
            is_playing,
            volume: vol.and_then(|v| v.value.map(|x| x as f64)),
            volume_type: vol.map(|_| "db".to_string()),
            volume_min: vol.and_then(|v| v.min.map(|x| x as f64)),
            volume_max: vol.and_then(|v| v.max.map(|x| x as f64)),
            volume_step: Some(1.0),
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
}

/// Query params for image endpoint
#[derive(Deserialize)]
pub struct ImageQuery {
    pub zone_id: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub format: Option<String>,
}

use crate::knobs::image::jpeg_to_rgb565;

/// GET /knob/now_playing/image - Get album artwork
pub async fn knob_image_handler(
    State(state): State<AppState>,
    Query(params): Query<ImageQuery>,
) -> Response {
    let target_width = params.width.unwrap_or(240);
    let target_height = params.height.unwrap_or(240);
    let format = params.format.as_deref().unwrap_or("jpeg");

    // Helper to convert to RGB565 if requested
    let maybe_convert = |content_type: String, body: Vec<u8>| -> Response {
        if format == "rgb565" {
            // Convert JPEG to RGB565
            match jpeg_to_rgb565(&body, target_width, target_height) {
                Ok(rgb565) => Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .header("X-Image-Format", "rgb565")
                    .header("X-Image-Width", rgb565.width.to_string())
                    .header("X-Image-Height", rgb565.height.to_string())
                    .body(Body::from(rgb565.data))
                    .unwrap(),
                Err(_) => {
                    // Fall back to original image on conversion error
                    Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, content_type)
                        .body(Body::from(body))
                        .unwrap()
                }
            }
        } else {
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, content_type)
                .body(Body::from(body))
                .unwrap()
        }
    };

    // Route based on zone_id prefix
    if params.zone_id.starts_with("lms:") {
        // LMS zone
        let player_id = params.zone_id.trim_start_matches("lms:");
        let player = match state.lms.get_cached_player(player_id).await {
            Some(p) => p,
            None => {
                let svg = placeholder_svg(target_width, target_height);
                return Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "image/svg+xml")
                    .body(Body::from(svg))
                    .unwrap();
            }
        };

        // Get image key - prefer artwork_url for streaming services
        let image_key = player
            .artwork_url
            .or(player.coverid)
            .or(player.artwork_track_id);

        let image_key = match image_key {
            Some(key) => key,
            None => {
                let svg = placeholder_svg(target_width, target_height);
                return Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "image/svg+xml")
                    .body(Body::from(svg))
                    .unwrap();
            }
        };

        // Fetch artwork from LMS
        match state
            .lms
            .get_artwork(&image_key, Some(target_width), Some(target_height))
            .await
        {
            Ok((content_type, body)) => maybe_convert(content_type, body),
            Err(_) => {
                let svg = placeholder_svg(target_width, target_height);
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "image/svg+xml")
                    .body(Body::from(svg))
                    .unwrap()
            }
        }
    } else if params.zone_id.starts_with("roon:") || !params.zone_id.contains(':') {
        // Roon zone (or legacy zone_id without prefix)
        let zone_id = if params.zone_id.starts_with("roon:") {
            params.zone_id.trim_start_matches("roon:").to_string()
        } else {
            params.zone_id.clone()
        };

        let zone = match state.roon.get_zone(&zone_id).await {
            Some(z) => z,
            None => {
                let svg = placeholder_svg(target_width, target_height);
                return Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "image/svg+xml")
                    .body(Body::from(svg))
                    .unwrap();
            }
        };

        let image_key = match zone.now_playing.and_then(|np| np.image_key) {
            Some(key) => key,
            None => {
                let svg = placeholder_svg(target_width, target_height);
                return Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "image/svg+xml")
                    .body(Body::from(svg))
                    .unwrap();
            }
        };

        // Fetch from Roon image service
        match state
            .roon
            .get_image(&image_key, Some(target_width), Some(target_height))
            .await
        {
            Ok(image_data) => maybe_convert(image_data.content_type, image_data.data),
            Err(_) => {
                let svg = placeholder_svg(target_width, target_height);
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "image/svg+xml")
                    .body(Body::from(svg))
                    .unwrap()
            }
        }
    } else {
        // Unknown zone type - return placeholder
        let svg = placeholder_svg(target_width, target_height);
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "image/svg+xml")
            .body(Body::from(svg))
            .unwrap()
    }
}

/// Control request body
#[derive(Deserialize)]
pub struct KnobControlRequest {
    pub zone_id: String,
    pub action: String,
    pub value: Option<serde_json::Value>,
}

/// POST /knob/control - Send control command (routes by zone_id prefix)
pub async fn knob_control_handler(
    State(state): State<AppState>,
    _headers: HeaderMap,
    Json(req): Json<KnobControlRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // Route based on zone_id prefix
    if req.zone_id.starts_with("lms:") {
        // LMS player control
        let player_id = req.zone_id.trim_start_matches("lms:");
        return control_lms(&state, player_id, &req.action, req.value.as_ref()).await;
    } else if req.zone_id.starts_with("openhome:") {
        // OpenHome zone control
        let udn = req.zone_id.trim_start_matches("openhome:");
        return control_openhome(&state, udn, &req.action).await;
    } else if req.zone_id.starts_with("upnp:") {
        // UPnP zone control
        let udn = req.zone_id.trim_start_matches("upnp:");
        return control_upnp(&state, udn, &req.action).await;
    }

    // Roon zone (or legacy zone_id without prefix)
    let roon_zone_id = if req.zone_id.starts_with("roon:") {
        req.zone_id.trim_start_matches("roon:").to_string()
    } else {
        req.zone_id.clone()
    };

    control_roon(&state, &roon_zone_id, &req.action, req.value.as_ref()).await
}

/// Control Roon zone
async fn control_roon(
    state: &AppState,
    zone_id: &str,
    action: &str,
    value: Option<&serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let roon_action = match action {
        "play" => "play",
        "pause" => "pause",
        "play_pause" | "playpause" => "play_pause",
        "next" => "next",
        "previous" | "prev" => "previous",
        "stop" => "stop",
        "vol_up" | "volume_up" => {
            let output = get_first_output_id(state, zone_id).await.ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "no outputs in zone"})),
                )
            })?;
            // Use as_f64() which handles both JSON integers and floats
            let step = value.and_then(|v| v.as_f64()).unwrap_or(1.0) as i32;
            state
                .roon
                .change_volume(&output, step, true)
                .await
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error": e.to_string()})),
                    )
                })?;
            return Ok(Json(serde_json::json!({"ok": true})));
        }
        "vol_down" | "volume_down" => {
            let output = get_first_output_id(state, zone_id).await.ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "no outputs in zone"})),
                )
            })?;
            // Use as_f64() which handles both JSON integers and floats
            let step = value.and_then(|v| v.as_f64()).unwrap_or(1.0) as i32;
            state
                .roon
                .change_volume(&output, -step, true)
                .await
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error": e.to_string()})),
                    )
                })?;
            return Ok(Json(serde_json::json!({"ok": true})));
        }
        "vol_abs" | "volume" => {
            let output = get_first_output_id(state, zone_id).await.ok_or_else(|| {
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"error": "no outputs in zone"})),
                )
            })?;
            // Log raw value for debugging - knob sends floats like 75.0
            tracing::debug!("vol_abs raw value: {:?}", value);
            // Use as_f64() which handles both JSON integers and floats
            // (as_i64() returns None for floats like 75.0, causing fallback to 50)
            let vol = value.and_then(|v| v.as_f64()).unwrap_or(50.0) as i32;
            state
                .roon
                .change_volume(&output, vol, false)
                .await
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error": e.to_string()})),
                    )
                })?;
            return Ok(Json(serde_json::json!({"ok": true})));
        }
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Unknown action: {}", action)})),
            ));
        }
    };

    match state.roon.control(zone_id, roon_action).await {
        Ok(()) => Ok(Json(serde_json::json!({"ok": true}))),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )),
    }
}

/// Control LMS player
async fn control_lms(
    state: &AppState,
    player_id: &str,
    action: &str,
    value: Option<&serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let lms_action = match action {
        "play" => "play",
        "pause" => "pause",
        "play_pause" | "playpause" => "pause", // LMS uses pause to toggle
        "next" => "next",
        "previous" | "prev" => "prev",
        "stop" => "stop",
        "vol_up" | "volume_up" => {
            // Use as_f64() which handles both JSON integers and floats
            let step = value.and_then(|v| v.as_f64()).unwrap_or(5.0) as i32;
            state
                .lms
                .change_volume(player_id, step, true)
                .await
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error": e.to_string()})),
                    )
                })?;
            return Ok(Json(serde_json::json!({"ok": true})));
        }
        "vol_down" | "volume_down" => {
            // Use as_f64() which handles both JSON integers and floats
            let step = value.and_then(|v| v.as_f64()).unwrap_or(5.0) as i32;
            state
                .lms
                .change_volume(player_id, -step, true)
                .await
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error": e.to_string()})),
                    )
                })?;
            return Ok(Json(serde_json::json!({"ok": true})));
        }
        "vol_abs" | "volume" => {
            // Use as_f64() which handles both JSON integers and floats
            let vol = value.and_then(|v| v.as_f64()).unwrap_or(50.0) as i32;
            state
                .lms
                .change_volume(player_id, vol, false)
                .await
                .map_err(|e| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error": e.to_string()})),
                    )
                })?;
            return Ok(Json(serde_json::json!({"ok": true})));
        }
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Unknown action: {}", action)})),
            ));
        }
    };

    match state.lms.control(player_id, lms_action, None).await {
        Ok(()) => Ok(Json(serde_json::json!({"ok": true}))),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )),
    }
}

/// Control OpenHome zone
async fn control_openhome(
    state: &AppState,
    zone_id: &str,
    action: &str,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let oh_action = match action {
        "play" => "play",
        "pause" => "pause",
        "play_pause" | "playpause" => "pause", // OpenHome uses pause to toggle
        "next" => "next",
        "previous" | "prev" => "previous",
        "stop" => "stop",
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Unknown action: {}", action)})),
            ));
        }
    };

    match state.openhome.control(zone_id, oh_action, None).await {
        Ok(()) => Ok(Json(serde_json::json!({"ok": true}))),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )),
    }
}

/// Control UPnP zone
async fn control_upnp(
    state: &AppState,
    zone_id: &str,
    action: &str,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let upnp_action = match action {
        "play" => "play",
        "pause" => "pause",
        "play_pause" | "playpause" => "pause",
        "next" => "next",
        "previous" | "prev" => "previous",
        "stop" => "stop",
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Unknown action: {}", action)})),
            ));
        }
    };

    match state.upnp.control(zone_id, upnp_action, None).await {
        Ok(()) => Ok(Json(serde_json::json!({"ok": true}))),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )),
    }
}

/// Helper to get first output ID for a Roon zone (for volume control)
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
    let knob_id = extract_knob_id(&headers, params.knob_id.as_deref()).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "knob_id required"})),
        )
    })?;

    let knob = state.knobs.get(&knob_id).await.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "knob not found"})),
        )
    })?;

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
    let knob_id = extract_knob_id(&headers, params.knob_id.as_deref()).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "knob_id required"})),
        )
    })?;

    let knob = state
        .knobs
        .update_config(&knob_id, updates)
        .await
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "knob not found"})),
            )
        })?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "config_sha": knob.config_sha,
    })))
}

/// GET /knob/devices - List all registered knobs (admin)
pub async fn knob_devices_handler(State(state): State<AppState>) -> Json<serde_json::Value> {
    let knobs = state.knobs.list().await;
    Json(serde_json::json!({ "knobs": knobs }))
}

/// GET /config/{knob_id} - Get knob configuration (path parameter format)
pub async fn knob_config_by_path_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(knob_id): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let version = extract_knob_version(&headers);

    // Get or create knob (ensures it exists for newly connected devices)
    let knob = state
        .knobs
        .get_or_create(&knob_id, version.as_deref())
        .await;

    // Build config response matching Node.js format
    let mut config = serde_json::to_value(&knob.config).unwrap_or_default();
    if let serde_json::Value::Object(ref mut obj) = config {
        obj.insert("knob_id".to_string(), serde_json::json!(knob_id));
        obj.insert("name".to_string(), serde_json::json!(knob.name));
    }

    Ok(Json(serde_json::json!({
        "config": config,
        "config_sha": knob.config_sha,
    })))
}

/// PUT /config/{knob_id} - Update knob configuration (path parameter format)
pub async fn knob_config_update_by_path_handler(
    State(state): State<AppState>,
    axum::extract::Path(knob_id): axum::extract::Path<String>,
    Json(updates): Json<KnobConfigUpdate>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let knob = state
        .knobs
        .update_config(&knob_id, updates)
        .await
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "knob not found"})),
            )
        })?;

    // Build config response matching Node.js format
    let mut config = serde_json::to_value(&knob.config).unwrap_or_default();
    if let serde_json::Value::Object(ref mut obj) = config {
        obj.insert("knob_id".to_string(), serde_json::json!(knob_id));
        obj.insert("name".to_string(), serde_json::json!(knob.name));
    }

    Ok(Json(serde_json::json!({
        "config": config,
        "config_sha": knob.config_sha,
    })))
}

// ========== Firmware endpoints ==========

use crate::config::get_config_dir;

/// Get firmware directory path
fn firmware_dir() -> std::path::PathBuf {
    get_config_dir().join("firmware")
}

/// Version info from version.json
#[derive(Deserialize, Default)]
struct FirmwareVersionInfo {
    version: Option<String>,
    file: Option<String>,
}

/// GET /firmware/version - Get available firmware version
pub async fn firmware_version_handler() -> Response {
    let fw_dir = firmware_dir();

    if !fw_dir.exists() {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"error":"No firmware available","error_code":"FIRMWARE_NOT_FOUND"}"#,
            ))
            .unwrap();
    }

    // Look for .bin files
    let bin_files: Vec<_> = std::fs::read_dir(&fw_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "bin")
                .unwrap_or(false)
        })
        .collect();

    if bin_files.is_empty() {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"error":"No firmware available","error_code":"FIRMWARE_NOT_FOUND"}"#,
            ))
            .unwrap();
    }

    // Try to read version.json
    let version_path = fw_dir.join("version.json");
    let version_info: FirmwareVersionInfo = if version_path.exists() {
        std::fs::read_to_string(&version_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    } else {
        FirmwareVersionInfo::default()
    };

    let firmware_file = version_info
        .file
        .unwrap_or_else(|| "roon_knob.bin".to_string());
    let version = version_info.version.or_else(|| {
        // Try to extract version from filename
        let re = regex::Regex::new(r"roon_knob[_-]?v?(\d+\.\d+\.\d+)\.bin").ok()?;
        re.captures(&firmware_file)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
    });

    let version = match version {
        Some(v) => v,
        None => {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"error":"No firmware version available","error_code":"FIRMWARE_NOT_FOUND"}"#))
                .unwrap();
        }
    };

    let firmware_path = fw_dir.join(&firmware_file);
    let size = std::fs::metadata(&firmware_path)
        .map(|m| m.len())
        .unwrap_or(0);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({
                "version": version,
                "size": size,
                "file": firmware_file
            })
            .to_string(),
        ))
        .unwrap()
}

/// GET /firmware/download - Download firmware binary
pub async fn firmware_download_handler() -> Response {
    let fw_dir = firmware_dir();

    if !fw_dir.exists() {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"error":"No firmware available","error_code":"FIRMWARE_NOT_FOUND"}"#,
            ))
            .unwrap();
    }

    // Determine firmware file
    let version_path = fw_dir.join("version.json");
    let firmware_file = if version_path.exists() {
        std::fs::read_to_string(&version_path)
            .ok()
            .and_then(|s| serde_json::from_str::<FirmwareVersionInfo>(&s).ok())
            .and_then(|v| v.file)
            .unwrap_or_else(|| "roon_knob.bin".to_string())
    } else {
        "roon_knob.bin".to_string()
    };

    let firmware_path = fw_dir.join(&firmware_file);

    // Fall back to first .bin file if specified file doesn't exist
    let firmware_path = if firmware_path.exists() {
        firmware_path
    } else {
        let bin_files: Vec<_> = std::fs::read_dir(&fw_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| {
                e.path()
                    .extension()
                    .map(|ext| ext == "bin")
                    .unwrap_or(false)
            })
            .map(|e| e.path())
            .collect();

        if bin_files.is_empty() {
            return Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"error":"Firmware file not found","error_code":"FIRMWARE_NOT_FOUND"}"#,
                ))
                .unwrap();
        }
        bin_files[0].clone()
    };

    // Read file
    let data = match std::fs::read(&firmware_path) {
        Ok(d) => d,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"error":"Failed to read firmware file"}"#))
                .unwrap();
        }
    };

    let filename = firmware_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("firmware.bin");

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, data.len())
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{}\"", filename),
        )
        .body(Body::from(data))
        .unwrap()
}

/// GET /manifest-s3.json - ESP Web Tools manifest
pub async fn manifest_handler() -> Response {
    let fw_dir = firmware_dir();
    let version_path = fw_dir.join("version.json");

    let version = if version_path.exists() {
        std::fs::read_to_string(&version_path)
            .ok()
            .and_then(|s| serde_json::from_str::<FirmwareVersionInfo>(&s).ok())
            .and_then(|v| v.version)
            .unwrap_or_else(|| "latest".to_string())
    } else {
        "latest".to_string()
    };

    let manifest = serde_json::json!({
        "name": "Hi-Fi Control Knob",
        "version": version,
        "new_install_prompt_erase": true,
        "builds": [{
            "chipFamily": "ESP32-S3",
            "parts": [{
                "path": "/firmware/download",
                "offset": 0
            }]
        }]
    });

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(manifest.to_string()))
        .unwrap()
}

/// POST /admin/fetch-firmware - Manually trigger firmware download from GitHub
pub async fn admin_fetch_firmware_handler(
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    use crate::firmware::FirmwareService;

    let service = FirmwareService::new();
    match service.check_for_updates().await {
        Ok(downloaded) => {
            if downloaded {
                let version =
                    FirmwareService::get_current_version().unwrap_or_else(|| "unknown".to_string());
                Ok(Json(serde_json::json!({
                    "ok": true,
                    "version": version,
                    "message": format!("Firmware v{} downloaded", version)
                })))
            } else {
                let version = FirmwareService::get_current_version();
                Ok(Json(serde_json::json!({
                    "ok": true,
                    "version": version,
                    "message": "Firmware is up to date"
                })))
            }
        }
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("Failed to fetch firmware: {}", e)
            })),
        )),
    }
}
