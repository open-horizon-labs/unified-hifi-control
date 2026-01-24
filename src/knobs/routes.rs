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

use std::net::SocketAddr;

use axum::{
    body::Body,
    extract::{ConnectInfo, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::Response,
    Json,
};
use serde::{Deserialize, Serialize};

use sha2::{Digest, Sha256};

use crate::api::AppState;
use crate::bus::VolumeControl;
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

/// Format IP address, converting IPv4-mapped IPv6 to plain IPv4
fn format_ip(ip: std::net::IpAddr) -> String {
    match ip {
        std::net::IpAddr::V4(v4) => v4.to_string(),
        std::net::IpAddr::V6(v6) => {
            // Check for IPv4-mapped IPv6 address (::ffff:x.x.x.x)
            if let Some(v4) = v6.to_ipv4_mapped() {
                v4.to_string()
            } else {
                v6.to_string()
            }
        }
    }
}

/// Extract client IP from headers (X-Forwarded-For, X-Real-IP) or socket address
fn extract_client_ip(headers: &HeaderMap, socket_addr: Option<SocketAddr>) -> Option<String> {
    // Check X-Forwarded-For first (when behind a proxy)
    if let Some(forwarded) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        // X-Forwarded-For can be a comma-separated list; take the first one
        return forwarded.split(',').next().map(|s| s.trim().to_string());
    }
    // Check X-Real-IP (nginx style)
    if let Some(real_ip) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
        return Some(real_ip.to_string());
    }
    // Fall back to socket address
    socket_addr.map(|addr| format_ip(addr.ip()))
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
#[derive(Serialize, Clone)]
pub struct ZoneInfo {
    pub zone_id: String,
    pub zone_name: String,
    pub source: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_control: Option<VolumeControl>,
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

/// Helper to aggregate zones from aggregator (respects adapter settings, public for UI module)
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

    // Get all zones from aggregator (already prefixed with source:)
    let all_zones = state.aggregator.get_zones().await;

    // Filter by enabled adapters and convert to ZoneInfo
    all_zones
        .into_iter()
        .filter(|z| {
            // Filter based on adapter settings
            if z.zone_id.starts_with("roon:") {
                adapters.roon
            } else if z.zone_id.starts_with("lms:") {
                adapters.lms
            } else if z.zone_id.starts_with("openhome:") {
                adapters.openhome
            } else if z.zone_id.starts_with("upnp:") {
                adapters.upnp
            } else if z.zone_id.starts_with("hqp:") {
                adapters.hqplayer
            } else {
                true // Unknown prefix, include by default
            }
        })
        .map(|z| ZoneInfo {
            dsp: get_dsp(&z.zone_id),
            zone_id: z.zone_id,
            zone_name: z.zone_name,
            source: z.source,
            state: z.state.to_string(),
            volume_control: z.volume_control,
        })
        .collect()
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
    pub zones_sha: Option<String>,
}

/// Helper to build zone info list for error responses
async fn get_zone_infos(state: &AppState) -> Vec<ZoneInfo> {
    get_all_zones_internal(state).await
}

/// Compute SHA256 hash of zone list (first 8 hex chars)
/// Changes when zones are added/removed, enabling clients to detect zone list updates
fn compute_zones_sha(zones: &[ZoneInfo]) -> String {
    let mut hasher = Sha256::new();
    // Hash zone IDs and names - sorted for deterministic output
    // Use length-prefixing to avoid delimiter collision (e.g., if zone name contains special chars)
    let mut zone_data: Vec<_> = zones
        .iter()
        .map(|z| format!("{}:{}", z.zone_id, z.zone_name))
        .collect();
    zone_data.sort();
    for item in &zone_data {
        let len = item.len() as u32;
        hasher.update(len.to_be_bytes());
        hasher.update(item.as_bytes());
    }
    let result = hasher.finalize();
    hex::encode(&result[..4]) // First 8 hex chars
}

/// GET /knob/now_playing - Get current playback state (routes by zone_id prefix)
pub async fn knob_now_playing_handler(
    State(state): State<AppState>,
    connect_info: Result<ConnectInfo<SocketAddr>, axum::extract::rejection::ExtensionRejection>,
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
    let client_ip = extract_client_ip(&headers, connect_info.ok().map(|c| c.0));
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
            ip: client_ip,
        };
        state.knobs.update_status(id, status_update).await;
        config_sha = state.knobs.get_config_sha(id).await;
    }

    let image_url = format!(
        "/knob/now_playing/image?zone_id={}",
        urlencoding::encode(&zone_id)
    );
    let zone_infos = get_zone_infos(&state).await;

    // Handle legacy zone_id without prefix (assume Roon)
    let prefixed_zone_id = if !zone_id.contains(':') {
        format!("roon:{}", zone_id)
    } else {
        zone_id.clone()
    };

    // Get zone from aggregator (single source of truth)
    let zone = match state.aggregator.get_zone(&prefixed_zone_id).await {
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

    // Check if zone's adapter is enabled
    use crate::api::load_app_settings;
    let settings = load_app_settings();
    let adapter_enabled = match zone.source.as_str() {
        "roon" => settings.adapters.roon,
        "lms" => settings.adapters.lms,
        "openhome" => settings.adapters.openhome,
        "upnp" => settings.adapters.upnp,
        "hqplayer" => settings.adapters.hqplayer,
        _ => true,
    };

    if !adapter_enabled {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "zone not found",
                "error_code": "ZONE_NOT_FOUND",
                "zones": zone_infos
            })),
        ));
    }

    // Extract now_playing info (title/artist/album -> line1/line2/line3)
    let np = zone.now_playing.as_ref();
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

    // Determine playback state
    let is_playing = zone.state == crate::bus::PlaybackState::Playing;

    // Extract volume info from zone's volume_control
    let vc = zone.volume_control.as_ref();
    let volume_type = match vc {
        Some(v) => match v.scale {
            crate::bus::VolumeScale::Decibel => "db".to_string(),
            crate::bus::VolumeScale::Percentage => "number".to_string(),
            crate::bus::VolumeScale::Linear => "number".to_string(),
            crate::bus::VolumeScale::Unknown => "fixed".to_string(),
        },
        None => "fixed".to_string(),
    };

    Ok(Json(NowPlayingResponse {
        zone_id: zone.zone_id,
        line1,
        line2,
        line3,
        is_playing,
        volume: vc.map(|v| v.value as f64),
        volume_type: Some(volume_type),
        volume_min: vc.map(|v| v.min as f64).or(Some(0.0)),
        volume_max: vc.map(|v| v.max as f64).or(Some(0.0)),
        volume_step: vc.map(|v| v.step as f64).or(Some(1.0)),
        image_url: Some(image_url),
        image_key: np.and_then(|n| n.image_key.clone()),
        seek_position: np.and_then(|n| n.seek_position.map(|p| p as i64)),
        length: np.and_then(|n| n.duration.map(|d| d as u32)),
        is_play_allowed: zone.is_play_allowed,
        is_pause_allowed: zone.is_pause_allowed,
        is_next_allowed: zone.is_next_allowed,
        is_previous_allowed: zone.is_previous_allowed,
        zones: zone_infos.clone(),
        config_sha,
        zones_sha: Some(compute_zones_sha(&zone_infos)),
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

// Image conversion is now handled by state.get_image()

use crate::knobs::image::svg_to_rgb565;

/// GET /knob/now_playing/image - Get album artwork
#[allow(clippy::unwrap_used)] // Response::builder().body().unwrap() cannot fail with valid inputs
pub async fn knob_image_handler(
    State(state): State<AppState>,
    Query(params): Query<ImageQuery>,
) -> Response {
    let target_width = params.width.unwrap_or(240);
    let target_height = params.height.unwrap_or(240);
    let format = params.format.as_deref();

    // Helper to return placeholder image in appropriate format
    let placeholder_response = || -> Response {
        let svg = placeholder_svg(target_width, target_height);
        if format == Some("rgb565") {
            // Convert SVG placeholder to RGB565
            match svg_to_rgb565(svg.as_bytes(), target_width, target_height) {
                Ok(rgb565) => Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .header("X-Image-Format", "rgb565")
                    .header("X-Image-Width", rgb565.width.to_string())
                    .header("X-Image-Height", rgb565.height.to_string())
                    .body(Body::from(rgb565.data))
                    .unwrap(),
                Err(_) => Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "image/svg+xml")
                    .body(Body::from(svg))
                    .unwrap(),
            }
        } else {
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "image/svg+xml")
                .body(Body::from(svg))
                .unwrap()
        }
    };

    // Handle legacy zone_id without prefix (assume Roon)
    let zone_id = if !params.zone_id.contains(':') {
        format!("roon:{}", params.zone_id)
    } else {
        params.zone_id.clone()
    };

    // Get zone from aggregator to find image_key
    let zone = match state.aggregator.get_zone(&zone_id).await {
        Some(z) => z,
        None => return placeholder_response(),
    };

    // Get image_key from now_playing
    let image_key = match zone.now_playing.and_then(|np| np.image_key) {
        Some(key) => key,
        None => return placeholder_response(),
    };

    // Fetch image through unified interface (handles format conversion)
    match state
        .get_image(
            &zone_id,
            &image_key,
            Some(target_width),
            Some(target_height),
            format,
        )
        .await
    {
        Ok(image_data) => {
            // If RGB565 was requested but conversion failed (content_type != octet-stream),
            // return the placeholder instead of misleading headers
            if format == Some("rgb565") && image_data.content_type != "application/octet-stream" {
                return placeholder_response();
            }

            let mut response = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, &image_data.content_type);

            // Add RGB565 metadata headers for ESP32 clients
            if format == Some("rgb565") {
                response = response
                    .header("X-Image-Format", "rgb565")
                    .header("X-Image-Width", target_width.to_string())
                    .header("X-Image-Height", target_height.to_string());
            }

            response.body(Body::from(image_data.data)).unwrap()
        }
        Err(_) => placeholder_response(),
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
            // Use provided value, or look up zone's actual step from aggregator
            let step = match value.and_then(|v| v.as_f64()) {
                Some(v) => v as f32,
                None => get_zone_step(state, &format!("roon:{}", zone_id)).await,
            };
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
            // Use provided value, or look up zone's actual step from aggregator
            let step = match value.and_then(|v| v.as_f64()) {
                Some(v) => v as f32,
                None => get_zone_step(state, &format!("roon:{}", zone_id)).await,
            };
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
            let vol = value.and_then(|v| v.as_f64()).unwrap_or(50.0) as f32;
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
            // Use provided value, or look up zone's actual step from aggregator
            let step = match value.and_then(|v| v.as_f64()) {
                Some(v) => v as f32,
                None => get_zone_step(state, &format!("lms:{}", player_id)).await,
            };
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
            // Use provided value, or look up zone's actual step from aggregator
            let step = match value.and_then(|v| v.as_f64()) {
                Some(v) => v as f32,
                None => get_zone_step(state, &format!("lms:{}", player_id)).await,
            };
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
            let vol = value.and_then(|v| v.as_f64()).unwrap_or(50.0) as f32;
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

/// Helper to get zone's volume step from aggregator (returns 1.0 if not found)
async fn get_zone_step(state: &AppState, zone_id: &str) -> f32 {
    state
        .aggregator
        .get_zone(zone_id)
        .await
        .and_then(|z| z.volume_control)
        .map(|vc| vc.step)
        .unwrap_or(1.0)
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

    // Build config response with name included in config object (matches frontend expected format)
    let mut config = serde_json::to_value(&knob.config).unwrap_or_default();
    if let serde_json::Value::Object(ref mut obj) = config {
        obj.insert("knob_id".to_string(), serde_json::json!(knob_id.clone()));
        obj.insert("name".to_string(), serde_json::json!(knob.name));
    }

    Ok(Json(serde_json::json!({
        "knob_id": knob_id,
        "config": config,
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
#[allow(clippy::unwrap_used)] // Response::builder().body().unwrap() cannot fail with valid inputs
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
#[allow(clippy::unwrap_used)] // Response::builder().body().unwrap() cannot fail with valid inputs
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
#[allow(clippy::unwrap_used)] // Response::builder().body().unwrap() cannot fail with valid inputs
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_zone(id: &str, name: &str) -> ZoneInfo {
        ZoneInfo {
            zone_id: id.to_string(),
            zone_name: name.to_string(),
            source: "test".to_string(),
            state: "stopped".to_string(),
            volume_control: None,
            dsp: None,
        }
    }

    #[test]
    fn zones_sha_deterministic() {
        // Same input should always produce same output
        let zones = vec![
            make_zone("zone-1", "Living Room"),
            make_zone("zone-2", "Kitchen"),
        ];

        let sha1 = compute_zones_sha(&zones);
        let sha2 = compute_zones_sha(&zones);
        let sha3 = compute_zones_sha(&zones);

        assert_eq!(sha1, sha2);
        assert_eq!(sha2, sha3);
        assert_eq!(sha1.len(), 8, "SHA should be 8 hex chars");
    }

    #[test]
    fn zones_sha_order_insensitive() {
        // Same zones in different order should produce same SHA
        let zones_a = vec![
            make_zone("zone-1", "Living Room"),
            make_zone("zone-2", "Kitchen"),
        ];

        let zones_b = vec![
            make_zone("zone-2", "Kitchen"),
            make_zone("zone-1", "Living Room"),
        ];

        let sha_a = compute_zones_sha(&zones_a);
        let sha_b = compute_zones_sha(&zones_b);

        assert_eq!(sha_a, sha_b, "Order should not affect SHA");
    }

    #[test]
    fn zones_sha_changes_on_add() {
        let zones_before = vec![make_zone("zone-1", "Living Room")];

        let zones_after = vec![
            make_zone("zone-1", "Living Room"),
            make_zone("zone-2", "Kitchen"),
        ];

        let sha_before = compute_zones_sha(&zones_before);
        let sha_after = compute_zones_sha(&zones_after);

        assert_ne!(sha_before, sha_after, "SHA should change when zone added");
    }

    #[test]
    fn zones_sha_changes_on_remove() {
        let zones_before = vec![
            make_zone("zone-1", "Living Room"),
            make_zone("zone-2", "Kitchen"),
        ];

        let zones_after = vec![make_zone("zone-1", "Living Room")];

        let sha_before = compute_zones_sha(&zones_before);
        let sha_after = compute_zones_sha(&zones_after);

        assert_ne!(sha_before, sha_after, "SHA should change when zone removed");
    }

    #[test]
    fn zones_sha_changes_on_rename() {
        let zones_before = vec![make_zone("zone-1", "Living Room")];

        let zones_after = vec![make_zone("zone-1", "Lounge")];

        let sha_before = compute_zones_sha(&zones_before);
        let sha_after = compute_zones_sha(&zones_after);

        assert_ne!(sha_before, sha_after, "SHA should change when zone renamed");
    }

    #[test]
    fn zones_sha_empty_list() {
        // Empty list should produce a valid SHA
        let zones: Vec<ZoneInfo> = vec![];
        let sha = compute_zones_sha(&zones);

        assert_eq!(sha.len(), 8, "Empty list should still produce 8-char SHA");
        assert!(
            sha.chars().all(|c| c.is_ascii_hexdigit()),
            "SHA should be hex"
        );
    }

    #[test]
    fn zones_sha_special_chars_no_collision() {
        // Zone names with special chars should not cause collisions
        // These would collide with comma-joining: "a,b" vs ["a", "b"]
        let zones_a = vec![make_zone("z1", "Room A,B")];

        let zones_b = vec![make_zone("z1", "Room A"), make_zone("z2", "B")];

        let sha_a = compute_zones_sha(&zones_a);
        let sha_b = compute_zones_sha(&zones_b);

        assert_ne!(
            sha_a, sha_b,
            "Special chars in names should not cause collision"
        );
    }
}
