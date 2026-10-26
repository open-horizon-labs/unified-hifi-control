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
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use sha2::{Digest, Sha256};

use crate::adapters::AdapterCommand;
use crate::api::{
    dispatch_lms_runtime_command, dispatch_openhome_runtime_command, dispatch_roon_runtime_command,
    dispatch_upnp_runtime_command, lms_runtime_command_from_action,
    renderer_runtime_command_from_action, AppState,
};
use crate::bus::runtime::{CommandLane, HqpRuntimeCommand, RuntimeCommand};
use crate::bus::{Command, VolumeControl};
use crate::knobs::image::placeholder_svg;
use crate::knobs::store::{KnobConfigUpdate, KnobStatusUpdate};

/// Keep the existing JSON error shape; never report volatile settings as saved.
fn knob_persistence_error(error: anyhow::Error) -> (StatusCode, Json<serde_json::Value>) {
    // A setting the controller does not have, or out of range, is the caller's mistake.
    if let Some(rejected) = error.downcast_ref::<crate::knobs::store::SettingRejected>() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": rejected.to_string()})),
        );
    }
    tracing::warn!(%error, "Controller configuration request failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(
            serde_json::json!({"error": "controller settings could not be saved or loaded; check bridge logs"}),
        ),
    )
}

/// Extract knob ID from headers or query params
pub(crate) fn extract_knob_id(headers: &HeaderMap, query_knob_id: Option<&str>) -> Option<String> {
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

/// Identity is accepted only from the explicit device header, never inferred from a version.
fn extract_device_type(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("x-device-type")
        .and_then(|value| value.to_str().ok())
        .and_then(crate::firmware_catalog::FirmwareTarget::parse)
        .map(|target| target.slug())
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
    /// Whether `hifi_collections`/`/api/collections` implements this zone's
    /// provider at all (#531). The web UI's `CollectionsBrowser` panel gates
    /// on this instead of a hardcoded `musicassistant:` prefix check, so LMS
    /// and Roon zones light up as their slices land, without a web change.
    pub browse_supported: bool,
    /// Which Library-page tabs this zone's provider serves (#573 defect 6):
    /// a subset of `["browse", "playlists", "favorites", "radio"]`, derived
    /// from the same capability facts `hifi_capabilities` reports (see
    /// `crate::mcp::tools::collections::collections_tabs_for_zone`). The web
    /// Library page hides tabs missing from this list instead of rendering
    /// ones whose every call would be refused.
    pub library_tabs: Vec<&'static str>,
    /// The playback device this zone is linked to (#774, alpha): power, current input and the
    /// device's input catalog. Absent for unlinked zones, so their payload is unchanged. A linked
    /// zone's `volume_control` is already the device's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<crate::devices::LinkedDeviceSummary>,
}

/// GET /knob/zones response
#[derive(Serialize)]
pub struct ZonesResponse {
    pub zones: Vec<ZoneInfo>,
}

/// GET /knob/zones - List all zones from all adapters
pub async fn knob_zones_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ZonesResponse>, (StatusCode, Json<serde_json::Value>)> {
    if let Some(id) = extract_knob_id(&headers, None) {
        state
            .knobs
            .get_or_create_with_device_type(
                &id,
                extract_knob_version(&headers).as_deref(),
                extract_device_type(&headers),
            )
            .await
            .map_err(knob_persistence_error)?;
    }
    let zones = get_zone_infos(&state, extract_knob_id(&headers, None).as_deref()).await;
    Ok(Json(ZonesResponse { zones }))
}

/// Helper to aggregate zones from aggregator (public for UI module).
///
/// Membership and order come from [`crate::zone_list::visible_zones`] — adapter settings, the
/// user's per-zone hide list, and a deterministic sort. This function's remaining job is the
/// `Zone` → [`ZoneInfo`] projection, which attaches HQPlayer DSP links and must preserve the order
/// it is handed.
pub async fn get_all_zones_internal(state: &AppState) -> Vec<ZoneInfo> {
    use std::collections::HashMap;

    // Get HQPlayer zone links for DSP field population
    let hqp_links: HashMap<String, String> = state
        .hqp_zone_links
        .get_links()
        .await
        .into_iter()
        .map(|l| (l.zone_id, l.instance))
        .collect();

    // Helper to create DspInfo if zone is linked to HQPlayer.
    //
    // A direct `hqplayer:` zone never gets one. It already *is* an HQPlayer control path, so a `dsp`
    // block pointing at an instance would give the client two routes to one daemon with no rule for
    // which wins (#328). `HqpZoneLinkService::link_zone` refuses such a link at the source, and this
    // is the second half of that guard: links persisted by an older build must not resurface here.
    let get_dsp = |zone_id: &str| -> Option<DspInfo> {
        if zone_id.starts_with("hqplayer:") {
            return None;
        }
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

    // Filtered (adapter settings + hide list) and sorted by the shared policy.
    let zones = crate::zone_list::visible_zones(state).await;
    let mut devices = HashMap::new();
    for zone in &zones {
        if let Some(device) = state.aggregator.linked_device(&zone.zone_id).await {
            devices.insert(zone.zone_id.clone(), device);
        }
    }
    zones
        .into_iter()
        .map(|z| ZoneInfo {
            device: devices.remove(&z.zone_id),
            dsp: get_dsp(&z.zone_id),
            browse_supported: crate::mcp::tools::collections::zone_supports_hifi_collections(
                &z.zone_id,
            ),
            library_tabs: crate::mcp::tools::collections::collections_tabs_for_zone(&z.zone_id),
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
    /// Battery-less firmware reports -1; only 0..=100 reaches stored status.
    pub battery_level: Option<i32>,
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

/// The zone list one controller is offered: the shared list, minus that controller's own hide list.
///
/// A list filter only. The controller's zone is still resolved by ID against the aggregator, so a
/// zone it hides keeps playing when addressed (see `zone_list`).
async fn get_zone_infos(state: &AppState, knob_id: Option<&str>) -> Vec<ZoneInfo> {
    let zones = get_all_zones_internal(state).await;
    let Some(id) = knob_id else {
        return zones;
    };
    match state.knobs.get(id).await {
        Some(knob) => without_hidden(zones, &knob.config.hidden_zone_ids),
        None => zones,
    }
}

/// Remove the zones named in `hidden`, preserving order.
fn without_hidden(mut zones: Vec<ZoneInfo>, hidden: &[String]) -> Vec<ZoneInfo> {
    if !hidden.is_empty() {
        zones.retain(|z| !hidden.iter().any(|h| h == &z.zone_id));
    }
    zones
}

/// Compute SHA256 hash of zone list (first 8 hex chars)
/// Changes when zones are added/removed, enabling clients to detect zone list updates
fn compute_zones_sha(zones: &[ZoneInfo]) -> String {
    let mut hasher = Sha256::new();
    // Hashed in list order, so a reorder changes the SHA and clients notice.
    //
    // This used to sort first, and that was correct at the time: the list came from
    // `HashMap::values()`, so its order differed between two requests with an unchanged zone set. An
    // order-sensitive hash would have changed on essentially every request and had knobs refetching
    // their zone list constantly, so sorting the order away was the only way to get a stable value.
    //
    // `zone_list::visible_zones` now imposes a deterministic order, which removes the reason for the
    // sort and turns it into a bug: user-visible reordering was invisible to every client that polls
    // this SHA, because reordering is the one change sorting erases.
    //
    // Length-prefixed to avoid delimiter collisions, e.g. a zone named "a:b".
    for zone in zones {
        let item = format!("{}:{}", zone.zone_id, zone.zone_name);
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
            let zone_infos = get_zone_infos(
                &state,
                extract_knob_id(&headers, params.knob_id.as_deref()).as_deref(),
            )
            .await;
            let zones_sha = compute_zones_sha(&zone_infos);
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "zone_id required",
                    "error_code": "MISSING_ZONE_ID",
                    "zones": zone_infos,
                    "zones_sha": zones_sha
                })),
            ));
        }
    };

    // Update knob status if knob ID present
    let knob_id = extract_knob_id(&headers, params.knob_id.as_deref());
    let knob_version = extract_knob_version(&headers);
    let client_ip = extract_client_ip(&headers, connect_info.ok().map(|c| c.0));
    let mut config_sha = None;

    let mut volume_step_override = None;
    if let Some(ref id) = knob_id {
        let knob = state
            .knobs
            .get_or_create_with_device_type(
                id,
                knob_version.as_deref(),
                extract_device_type(&headers),
            )
            .await
            .map_err(knob_persistence_error)?;
        volume_step_override = knob.config.volume_step_override;
        let battery_level = params
            .battery_level
            .filter(|level| (0..=100).contains(level))
            .map(|level| level as u8);
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
    let zone_infos = get_zone_infos(&state, knob_id.as_deref()).await;

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
            let zones_sha = compute_zones_sha(&zone_infos);
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": "zone not found",
                    "error_code": "ZONE_NOT_FOUND",
                    "zones": zone_infos,
                    "zones_sha": zones_sha
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
        let zones_sha = compute_zones_sha(&zone_infos);
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "zone not found",
                "error_code": "ZONE_NOT_FOUND",
                "zones": zone_infos,
                "zones_sha": zones_sha
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
            crate::bus::VolumeScale::Steps => "number".to_string(),
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
        volume_step: volume_step_override
            .map(Some)
            .unwrap_or_else(|| vc.map(|v| v.step as f64))
            .or(Some(1.0)),
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
    pub scale: Option<String>,
    /// Accepted for older firmware; attention fill no longer uses an area budget.
    pub crop_limit: Option<u8>,
}

// Image conversion is now handled by state.get_image()

use crate::knobs::image::svg_to_rgb565;

/// GET /knob/now_playing/image - Get album artwork
#[allow(clippy::unwrap_used)] // Response::builder().body().unwrap() cannot fail with valid inputs
pub async fn knob_image_handler(
    State(state): State<AppState>,
    Query(params): Query<ImageQuery>,
    request_headers: axum::http::HeaderMap,
) -> Response {
    let target_width = params.width.unwrap_or(240);
    let target_height = params.height.unwrap_or(240);
    let format = params.format.as_deref();
    let rgb565_requested = matches!(format, Some("rgb565" | "rgb565-smart"));
    let eink_requested = format == Some("eink_acep6");
    if !crate::knobs::image::valid_artwork_dimensions(
        target_width,
        target_height,
        if eink_requested { 1_536_000 } else { 8_388_608 },
    ) {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("invalid artwork dimensions"))
            .unwrap();
    }
    let resize_policy = Some(match params.scale.as_deref() {
        Some("fit") => crate::knobs::image::Rgb565ResizePolicy::Fit,
        _ => crate::knobs::image::Rgb565ResizePolicy::AttentionCover,
    });

    // Helper to return placeholder image in appropriate format
    let placeholder_body = || -> Response {
        let svg = placeholder_svg(target_width, target_height);
        if eink_requested {
            let bytes_per_row = target_width.div_ceil(2) as usize;
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .header("X-Image-Format", "eink_acep6")
                .header("X-Image-Width", target_width.to_string())
                .header("X-Image-Height", target_height.to_string())
                .body(Body::from(vec![
                    0x11;
                    bytes_per_row * target_height as usize
                ]))
                .unwrap()
        } else if rgb565_requested {
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

    // Placeholders deliberately carry no ETag and `no-store`: an error
    // placeholder cached or revalidated as "unchanged" would hide recovery.
    let placeholder_response = || -> Response {
        let mut response = placeholder_body();
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-store"),
        );
        response
    };

    // Handle legacy zone_id without prefix (assume Roon)
    let zone_id = if !params.zone_id.contains(':') {
        format!("roon:{}", params.zone_id)
    } else {
        params.zone_id.clone()
    };

    // Get zone from aggregator to find image_key
    // Every placeholder is logged with its reason: clients fetch once per
    // image_key and never retry, so a placeholder sticks for the whole track
    // and the log line is the only evidence of why it was served.
    let zone = match state.aggregator.get_zone(&zone_id).await {
        Some(z) => z,
        None => {
            tracing::warn!(zone_id = %zone_id, format = ?format, "Artwork placeholder served: zone not found");
            return placeholder_response();
        }
    };

    // Get image_key from now_playing
    let image_key = match zone.now_playing.and_then(|np| np.image_key) {
        Some(key) => key,
        None => {
            tracing::warn!(zone_id = %zone_id, format = ?format, "Artwork placeholder served: zone has no now-playing image_key");
            return placeholder_response();
        }
    };

    // Fetch image through unified interface (handles format conversion)
    match state
        .get_image_tagged(
            &zone_id,
            &image_key,
            Some(target_width),
            Some(target_height),
            format,
            resize_policy,
        )
        .await
    {
        Ok(artwork) => {
            let source_headers = artwork.artwork_headers();
            let crate::api::ArtworkResponse {
                image: image_data,
                etag,
                ..
            } = artwork;
            // If RGB565 was requested but conversion failed (content_type != octet-stream),
            // return the placeholder instead of misleading headers
            if (rgb565_requested || eink_requested)
                && image_data.content_type != "application/octet-stream"
            {
                tracing::warn!(
                    zone_id = %zone_id,
                    image_key = %image_key,
                    format = ?format,
                    source_content_type = %image_data.content_type,
                    "Artwork placeholder served: display conversion failed"
                );
                return placeholder_response();
            }

            // Add RGB565 metadata headers for ESP32 clients
            let mut extra: Vec<(&'static str, String)> = Vec::new();
            if eink_requested || rgb565_requested {
                extra.push((
                    "X-Image-Format",
                    if eink_requested {
                        "eink_acep6"
                    } else {
                        "rgb565"
                    }
                    .to_string(),
                ));
                extra.push(("X-Image-Width", target_width.to_string()));
                extra.push(("X-Image-Height", target_height.to_string()));
            }
            extra.extend(source_headers);

            match etag {
                Some(etag) => crate::api::artwork_http_response(
                    &request_headers,
                    image_data,
                    &etag,
                    crate::api::ARTWORK_CACHE_CONTROL_NOW_PLAYING,
                    &extra,
                ),
                // A conversion-failure fallback is not revalidatable.
                None => {
                    let mut response = Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, &image_data.content_type);
                    for (name, value) in &extra {
                        response = response.header(*name, value);
                    }
                    response.body(Body::from(image_data.data)).unwrap()
                }
            }
        }
        Err(error) => {
            tracing::warn!(
                zone_id = %zone_id,
                image_key = %image_key,
                format = ?format,
                "Artwork placeholder served: artwork fetch failed: {error:#}"
            );
            placeholder_response()
        }
    }
}

/// Device control for a linked zone, or `None` when the provider should handle the action.
///
/// Power and input actions on an unlinked zone are refused here rather than offered to a provider
/// that has no such concept (and, for LMS, a *different* power concept — #304).
async fn control_linked_device(
    state: &AppState,
    req: &KnobControlRequest,
) -> Option<Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)>> {
    crate::zone_control::dispatch_linked_device(
        &state.devices,
        &req.zone_id,
        &req.action,
        req.value.as_ref(),
    )
    .await
    .map(|result| match result {
        Ok(report) => Ok(Json(serde_json::json!({"ok": true, "result": report}))),
        Err(error) => Err(crate::api::devices::device_error_response(error)),
    })
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
    headers: HeaderMap,
    Json(req): Json<KnobControlRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if let Some(id) = extract_knob_id(&headers, None) {
        state
            .knobs
            .get_or_create_with_device_type(
                &id,
                extract_knob_version(&headers).as_deref(),
                extract_device_type(&headers),
            )
            .await
            .map_err(knob_persistence_error)?;
    }
    // A zone linked to an amplifier (#774, alpha): its volume, mute, power and input belong to the
    // device, so they are resolved here, before prefix routing could hand them to the provider.
    // Transport actions fall through to the provider below.
    if let Some(response) = control_linked_device(&state, &req).await {
        return response;
    }

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
    } else if req.zone_id.starts_with("hqplayer:") {
        // Direct HQPlayer instance control (#328). Before this arm existed, a `hqplayer:` zone id
        // fell through to the Roon branch below and every HQPlayer command was executed against
        // Roon with the prefix stripped.
        let instance = req.zone_id.trim_start_matches("hqplayer:");
        return control_hqplayer(
            &state,
            &req.zone_id,
            instance,
            &req.action,
            req.value.as_ref(),
        )
        .await;
    }

    // Roon zone, or a legacy zone_id with no prefix at all.
    //
    // A *prefixed* id that reached here names a backend this server does not route unless it is
    // registered in the `AdapterRegistry` (streaming providers), and offering it to Roon is how a
    // command for one zone silently became a command for another (or for none). Only the legacy
    // unprefixed shape — which the Node.js server and older knob firmware still send — may mean
    // Roon implicitly.
    if !req.zone_id.starts_with("roon:") && req.zone_id.contains(':') {
        let prefix = req.zone_id.split(':').next().unwrap_or_default();
        if state.adapter_registry.has_adapter(prefix).await {
            return control_registry_provider(
                &state,
                prefix,
                &req.zone_id,
                &req.action,
                req.value.as_ref(),
                prefix,
            )
            .await;
        }
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Unsupported zone source: {prefix}"),
                "error_code": "UNSUPPORTED_ZONE_SOURCE",
            })),
        ));
    }

    let roon_zone_id = if req.zone_id.starts_with("roon:") {
        req.zone_id.trim_start_matches("roon:").to_string()
    } else {
        req.zone_id.clone()
    };

    control_roon(&state, &roon_zone_id, &req.action, req.value.as_ref()).await
}

/// Resolve a direct HQPlayer instance and execute one action against it (#328, and MCP's copy of
/// the same routing defect closed by #401).
///
/// **Every refusal below is checked against the zone the aggregator published**, not against a fresh
/// adapter read. That is deliberate and it is the invariant that keeps this function honest: the
/// capability flag a client was handed in `GET /knob/now_playing` (or MCP's `hifi_zones`) is
/// literally the flag its command is judged by, so "advertised" and "permitted" cannot drift apart.
/// It is also required — a surface may not query an adapter for state (`docs/ARCHITECTURE.md`,
/// `tests/architecture_lint.rs`).
///
/// The cost is that the flags can be up to one poll interval (2 s by default) stale. That bound is
/// accepted and recorded in `.oh/hqplayer-direct-zone.md`; closing it would need either a
/// forbidden adapter state read on the command path or daemon-side compare-and-set, which the
/// protocol does not offer.
pub(crate) async fn dispatch_hqplayer_action(
    state: &AppState,
    zone_id: &str,
    instance: &str,
    action: &str,
    value: Option<f64>,
) -> Result<(), HqpDispatchError> {
    dispatch_hqplayer_action_via(
        &state.aggregator,
        state.reliable_commands.as_ref(),
        zone_id,
        instance,
        action,
        value,
    )
    .await
}

/// Submit an HQPlayer pipeline or profile operation through the same exact-instance runtime as
/// transport. Reconfiguration admission is endpoint-scoped, so a slow profile restart blocks only
/// competing commands for that HQPlayer instance.
pub(crate) async fn dispatch_hqplayer_reconfiguration(
    state: &AppState,
    instance: &str,
    command: HqpRuntimeCommand,
) -> Result<(), HqpDispatchError> {
    let confirmation_budget = match &command {
        HqpRuntimeCommand::LoadProfile { .. } => std::time::Duration::from_secs(120),
        HqpRuntimeCommand::Pipeline { .. } | HqpRuntimeCommand::LegacyPipelineIndex { .. } => {
            std::time::Duration::from_secs(15)
        }
        HqpRuntimeCommand::RefreshAdvanced => std::time::Duration::from_secs(15),
        HqpRuntimeCommand::RefreshProfiles => std::time::Duration::from_secs(30),
        HqpRuntimeCommand::Output(_) => {
            return Err(HqpDispatchError::BadRequest {
                message: "HQPlayer output commands use the output command service".to_string(),
                code: "INVALID_COMMAND",
            });
        }
    };
    dispatch_hqplayer_runtime_command(
        state,
        instance,
        RuntimeCommand::Hqplayer(command),
        CommandLane::Reconfiguration,
        confirmation_budget,
    )
    .await
}

pub(crate) async fn dispatch_hqplayer_refresh(
    state: &AppState,
    instance: &str,
    command: HqpRuntimeCommand,
) -> Result<(), HqpDispatchError> {
    let confirmation_budget = match &command {
        HqpRuntimeCommand::RefreshAdvanced => std::time::Duration::from_secs(15),
        HqpRuntimeCommand::RefreshProfiles => std::time::Duration::from_secs(30),
        _ => {
            return Err(HqpDispatchError::BadRequest {
                message: "HQPlayer refresh requires a read command".to_string(),
                code: "INVALID_COMMAND",
            });
        }
    };
    dispatch_hqplayer_runtime_command(
        state,
        instance,
        RuntimeCommand::Hqplayer(command),
        CommandLane::Interactive,
        confirmation_budget,
    )
    .await
}

async fn dispatch_hqplayer_runtime_command(
    state: &AppState,
    instance: &str,
    command: RuntimeCommand,
    lane: CommandLane,
    confirmation_budget: std::time::Duration,
) -> Result<(), HqpDispatchError> {
    dispatch_hqplayer_runtime_command_via(
        &state.aggregator,
        state.reliable_commands.as_ref(),
        instance,
        command,
        lane,
        confirmation_budget,
    )
    .await
}

async fn control_hqplayer(
    state: &AppState,
    zone_id: &str,
    instance: &str,
    action: &str,
    value: Option<&serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let value = value.and_then(|v| v.as_f64());
    match dispatch_hqplayer_action(state, zone_id, instance, action, value).await {
        Ok(()) => Ok(Json(serde_json::json!({"ok": true}))),
        Err(HqpDispatchError::NotFound(message)) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": message, "error_code": "ZONE_NOT_FOUND"})),
        )),
        Err(HqpDispatchError::BadRequest { message, code }) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": message, "error_code": code})),
        )),
        Err(HqpDispatchError::Backend(message)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": message})),
        )),
    }
}

fn adapter_command_for_action(
    provider: &str,
    action: &str,
    value: Option<&serde_json::Value>,
) -> Result<AdapterCommand, String> {
    match action {
        "play" => Ok(AdapterCommand::Play),
        "pause" => Ok(AdapterCommand::Pause),
        "play_pause" | "playpause" => Ok(AdapterCommand::PlayPause),
        "next" => Ok(AdapterCommand::Next),
        "previous" | "prev" => Ok(AdapterCommand::Previous),
        "stop" => Ok(AdapterCommand::Stop),
        "vol_abs" | "volume" => Ok(AdapterCommand::VolumeAbsolute(
            value.and_then(|v| v.as_f64()).unwrap_or(50.0) as i32,
        )),
        "vol_up" | "volume_up" => Ok(AdapterCommand::VolumeRelative(
            value.and_then(|v| v.as_f64()).unwrap_or(1.0).round() as i32,
        )),
        "vol_down" | "volume_down" => Ok(AdapterCommand::VolumeRelative(
            -(value.and_then(|v| v.as_f64()).unwrap_or(1.0).round() as i32),
        )),
        _ => Err(format!("Unknown {provider} action: {action}")),
    }
}

async fn control_registry_provider(
    state: &AppState,
    prefix: &str,
    zone_id: &str,
    action: &str,
    value: Option<&serde_json::Value>,
    provider: &str,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let command = match adapter_command_for_action(provider, action, value) {
        Ok(command) => command,
        Err(error) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": error})),
            ));
        }
    };

    match state
        .adapter_registry
        .command(prefix, zone_id, command)
        .await
    {
        Ok(response) if response.success => Ok(Json(serde_json::json!({"ok": true}))),
        Ok(response) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": response.error.unwrap_or_else(|| format!("{provider} command failed"))
            })),
        )),
        Err(error) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": error.to_string()})),
        )),
    }
}

/// Control Roon zone
async fn control_roon(
    state: &AppState,
    zone_id: &str,
    action: &str,
    value: Option<&serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let command = match action {
        "play" | "pause" | "play_pause" | "playpause" | "next" | "previous" | "prev" | "stop" => {
            renderer_runtime_command_from_action(action, None)
        }
        "vol_up" | "volume_up" => {
            // Use provided value, or look up zone's actual step from aggregator
            let step = match value.and_then(|v| v.as_f64()) {
                Some(v) => v as f32,
                None => get_zone_step(state, &format!("roon:{}", zone_id)).await,
            };
            Ok(Command::VolumeRelative {
                delta: step,
                output_id: None,
            })
        }
        "vol_down" | "volume_down" => {
            // Use provided value, or look up zone's actual step from aggregator
            let step = match value.and_then(|v| v.as_f64()) {
                Some(v) => v as f32,
                None => get_zone_step(state, &format!("roon:{}", zone_id)).await,
            };
            Ok(Command::VolumeRelative {
                delta: -step,
                output_id: None,
            })
        }
        "vol_abs" | "volume" => {
            // Use as_f64() which handles both JSON integers and floats
            // (as_i64() returns None for floats like 75.0, causing fallback to 50)
            let vol = value.and_then(|v| v.as_f64()).unwrap_or(50.0) as f32;
            Ok(Command::VolumeAbsolute {
                value: vol,
                output_id: None,
            })
        }
        // Dial's detail-screen seek-jog (encoder scrubs position, committed
        // after a short idle debounce) - same validation the HQPlayer path
        // above already applies to this action name, just without the
        // duration ceiling: this handler has no pre-fetched zone/now_playing
        // to clamp against, so it relies on the client having already
        // clamped to [0, track length] before sending (roon-knob's
        // common/ui.c does).
        "seek" => match value.and_then(|v| v.as_f64()) {
            Some(position) if position.is_finite() && position >= 0.0 => {
                Ok(Command::Seek { position })
            }
            Some(_) => Err(anyhow::anyhow!(
                "seek position must be a non-negative number of seconds"
            )),
            None => Err(anyhow::anyhow!(
                "seek requires a numeric position in seconds"
            )),
        },
        _ => Err(anyhow::anyhow!("Unknown action: {action}")),
    };

    let command = command.map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": error.to_string()})),
        )
    })?;
    match dispatch_roon_runtime_command(state, zone_id, command).await {
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
    let command = match action {
        "vol_up" | "volume_up" => {
            let delta = value
                .and_then(|v| v.as_f64())
                .map(|value| value as f32)
                .unwrap_or(get_zone_step(state, &format!("lms:{player_id}")).await);
            Command::VolumeRelative {
                delta,
                output_id: None,
            }
        }
        "vol_down" | "volume_down" => {
            let delta = -value
                .and_then(|v| v.as_f64())
                .map(|value| value as f32)
                .unwrap_or(get_zone_step(state, &format!("lms:{player_id}")).await);
            Command::VolumeRelative {
                delta,
                output_id: None,
            }
        }
        _ => lms_runtime_command_from_action(
            action,
            value.and_then(|v| v.as_f64()).map(|v| v as f32),
        )
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Unknown action: {}", action)})),
            )
        })?,
    };

    match dispatch_lms_runtime_command(state, player_id, command).await {
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

    let result = match renderer_runtime_command_from_action(oh_action, None) {
        Ok(command) => dispatch_openhome_runtime_command(state, zone_id, command).await,
        Err(error) => Err(error),
    };
    match result {
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

    let result = match renderer_runtime_command_from_action(upnp_action, None) {
        Ok(command) => dispatch_upnp_runtime_command(state, zone_id, command).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => Ok(Json(serde_json::json!({"ok": true}))),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )),
    }
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

    let knob = if extract_device_type(&headers).is_some() {
        state
            .knobs
            .get_or_create_with_device_type(
                &knob_id,
                extract_knob_version(&headers).as_deref(),
                extract_device_type(&headers),
            )
            .await
            .map_err(knob_persistence_error)?
    } else {
        knob
    };

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

    guard_install_request(&state, &knob_id, &updates).await?;

    let knob = state
        .knobs
        .update_config(&knob_id, updates)
        .await
        .map_err(knob_persistence_error)?
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

/// DELETE /knob/devices/{knob_id} - Remove a registered knob (admin)
///
/// Callers that publish Home Assistant discovery for knobs (#523) observe
/// this via the knob no longer appearing in the store and are responsible
/// for retracting any retained discovery configs they previously published.
pub async fn knob_remove_handler(
    State(state): State<AppState>,
    axum::extract::Path(knob_id): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if state
        .knobs
        .remove(&knob_id)
        .await
        .map_err(knob_persistence_error)?
    {
        Ok(Json(serde_json::json!({ "ok": true })))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "knob not found"})),
        ))
    }
}

/// The version an install request would deliver: the channel's image, only when it is newer than
/// what the controller runs. A controller never changes channel and never goes backwards.
fn install_offer(available: Option<&str>, running: &str) -> Option<String> {
    let available = available?;
    crate::firmware_catalog::firmware_version_is_newer(available, running)
        .then(|| available.to_string())
}

/// Refuse an install request the bridge cannot honour, so the settings page never promises an
/// update that no image backs.
async fn guard_install_request(
    state: &AppState,
    knob_id: &str,
    updates: &KnobConfigUpdate,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    use crate::firmware_catalog::{FirmwareChannel, FirmwareTarget};
    if updates.firmware_update.as_deref() != Some("install") {
        return Ok(());
    }
    let refuse = |status, code: &str, message: &str| {
        (
            status,
            Json(serde_json::json!({"error": message, "error_code": code})),
        )
    };
    let Some(knob) = state.knobs.get(knob_id).await else {
        return Err(refuse(
            StatusCode::NOT_FOUND,
            "KNOB_NOT_FOUND",
            "knob not found",
        ));
    };
    let (Some(target), Some(running)) = (FirmwareTarget::parse(&knob.device_type), knob.version)
    else {
        return Err(refuse(
            StatusCode::CONFLICT,
            "FIRMWARE_UNKNOWN",
            "the controller has not reported its type and firmware version yet",
        ));
    };
    let Some(channel) = FirmwareChannel::for_version(&running) else {
        return Err(refuse(
            StatusCode::CONFLICT,
            "FIRMWARE_CHANNEL_UNKNOWN",
            "this firmware build is outside the published channels and cannot be updated from here",
        ));
    };
    let available = firmware_artifact(target, channel).and_then(|(path, info)| {
        let named = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| target.filename_version(name).map(str::to_string));
        info.version.or(named)
    });
    if install_offer(available.as_deref(), &running).is_none() {
        return Err(refuse(
            StatusCode::CONFLICT,
            "NO_FIRMWARE_UPDATE",
            "no newer firmware is available for this controller on its channel",
        ));
    }
    Ok(())
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
        .get_or_create_with_device_type(&knob_id, version.as_deref(), extract_device_type(&headers))
        .await
        .map_err(knob_persistence_error)?;

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
    guard_install_request(&state, &knob_id, &updates).await?;
    let knob = state
        .knobs
        .update_config(&knob_id, updates)
        .await
        .map_err(knob_persistence_error)?
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

/// Selection is explicit and closed. Conflicting firmware identity never chooses an image.
#[derive(Deserialize, Default)]
pub struct FirmwareQuery {
    pub device_type: Option<String>,
    pub channel: Option<String>,
}

enum FirmwareSelectionError {
    Invalid,
}

impl IntoResponse for FirmwareSelectionError {
    fn into_response(self) -> Response {
        (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error":"Invalid or conflicting firmware selection", "error_code":"INVALID_FIRMWARE_SELECTION"}))).into_response()
    }
}

impl FirmwareQuery {
    fn selection(
        &self,
        headers: &HeaderMap,
    ) -> Result<
        (
            crate::firmware_catalog::FirmwareTarget,
            crate::firmware_catalog::FirmwareChannel,
        ),
        FirmwareSelectionError,
    > {
        use crate::firmware_catalog::{FirmwareChannel, FirmwareTarget};
        let invalid = || FirmwareSelectionError::Invalid;
        let query = self
            .device_type
            .as_deref()
            .map(|value| FirmwareTarget::parse(value).ok_or_else(invalid))
            .transpose()?;
        let header = headers
            .get("X-Device-Type")
            .map(|value| {
                value
                    .to_str()
                    .ok()
                    .and_then(FirmwareTarget::parse)
                    .ok_or_else(invalid)
            })
            .transpose()?;
        if matches!((query, header), (Some(query), Some(header)) if query != header) {
            return Err(invalid());
        }
        let channel = self
            .channel
            .as_deref()
            .map(|value| FirmwareChannel::parse(value).ok_or_else(invalid))
            .transpose()?
            .unwrap_or_default();
        Ok((
            query.or(header).unwrap_or(FirmwareTarget::LegacyKnob),
            channel,
        ))
    }
}

fn firmware_artifact(
    target: crate::firmware_catalog::FirmwareTarget,
    channel: crate::firmware_catalog::FirmwareChannel,
) -> Option<(std::path::PathBuf, FirmwareVersionInfo)> {
    use crate::firmware_catalog::{FirmwareChannel, FirmwareTarget};
    let root = firmware_dir();
    let directory = target.cache_directory(&root, channel);
    if !std::fs::symlink_metadata(&root).ok()?.is_dir() {
        return None;
    }
    if (target != FirmwareTarget::LegacyKnob || channel != FirmwareChannel::Stable)
        && !std::fs::symlink_metadata(root.join(channel.slug()))
            .ok()?
            .is_dir()
    {
        return None;
    }
    if !std::fs::symlink_metadata(&directory).ok()?.is_dir() {
        return None;
    }
    let version_path = directory.join("version.json");
    let version: FirmwareVersionInfo = match std::fs::symlink_metadata(&version_path) {
        Ok(metadata) if metadata.is_file() => {
            serde_json::from_str(&std::fs::read_to_string(version_path).ok()?).ok()?
        }
        Ok(_) => return None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            FirmwareVersionInfo::default()
        }
        Err(_) => return None,
    };
    let default_file = target.application_file();
    let filename = version.file.as_deref().unwrap_or(&default_file);
    if !target.valid_application_filename(filename) {
        return None;
    }
    let named_version = target.filename_version(filename);
    if target != FirmwareTarget::LegacyKnob || channel != FirmwareChannel::Stable {
        version.version.as_deref()?;
    }
    if let Some(selected_version) = version.version.as_deref().or(named_version) {
        if !channel.matches_version(selected_version.trim_start_matches('v')) {
            return None;
        }
    }
    if let (Some(named), Some(metadata)) = (named_version, version.version.as_deref()) {
        if named != metadata.trim_start_matches('v') {
            return None;
        }
    }
    let path = directory.join(filename);
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if !metadata.is_file() || metadata.len() == 0 {
        return None;
    }
    Some((path, version))
}

fn firmware_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({
            "error": "No firmware available", "error_code": "FIRMWARE_NOT_FOUND"
        })),
    )
        .into_response()
}

/// GET /firmware/version - Get available firmware version
pub async fn firmware_version_handler(
    Query(query): Query<FirmwareQuery>,
    headers: HeaderMap,
) -> Response {
    let (target, channel) = match query.selection(&headers) {
        Ok(selection) => selection,
        Err(error) => return error.into_response(),
    };
    let Some((path, version_info)) = firmware_artifact(target, channel) else {
        return firmware_not_found();
    };
    let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
        return firmware_not_found();
    };
    let version = version_info
        .version
        .or_else(|| target.filename_version(filename).map(str::to_string));
    let Some(version) = version else {
        return firmware_not_found();
    };
    let Ok(metadata) = std::fs::metadata(&path) else {
        return firmware_not_found();
    };
    Json(serde_json::json!({"version": version, "size": metadata.len(), "file": filename}))
        .into_response()
}

/// GET /firmware/download - Download only the selected target/channel OTA application
#[allow(clippy::unwrap_used)] // Response::builder().body().unwrap() cannot fail with valid inputs
pub async fn firmware_download_handler(
    Query(query): Query<FirmwareQuery>,
    headers: HeaderMap,
) -> Response {
    let (target, channel) = match query.selection(&headers) {
        Ok(selection) => selection,
        Err(error) => return error.into_response(),
    };
    let Some((path, _)) = firmware_artifact(target, channel) else {
        return firmware_not_found();
    };
    let Ok(data) = std::fs::read(&path) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "Failed to read firmware file"})),
        )
            .into_response();
    };
    let Some(filename) = path.file_name().and_then(|name| name.to_str()) else {
        return firmware_not_found();
    };
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

/// GET /manifest-s3.json - ESP Web Tools clean installation
pub async fn manifest_handler() -> Response {
    // /firmware/download serves an OTA application, not a merged installation image.
    // Advertising it at offset zero overwrites the bootloader. Browser installation is
    // excluded from this OTA scope; fail closed instead of offering a bad flash.
    firmware_not_found()
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
                    "message": "Firmware caches updated"
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

    #[test]
    fn device_header_uses_catalog_identity_and_ignores_invalid_values() {
        let mut headers = HeaderMap::new();
        assert_eq!(extract_device_type(&headers), None);
        for (header, expected) in [
            ("hiphi-frame", Some("frame")),
            ("hiphi-dial-beta", Some("m5dial")),
            ("roon-knob", Some("knob")),
            ("../frame", None),
            ("frmae", None),
            ("", None),
        ] {
            headers.insert("x-device-type", header.parse().unwrap());
            assert_eq!(extract_device_type(&headers), expected);
        }
        headers.insert(
            "x-device-type",
            axum::http::HeaderValue::from_bytes(b"\xff").unwrap(),
        );
        assert_eq!(extract_device_type(&headers), None);
    }

    #[test]
    fn an_install_is_offered_only_when_the_channel_holds_a_newer_image() {
        // A controller follows its own channel: an alpha build is offered alphas only.
        assert_eq!(
            install_offer(Some("2.8.0-alpha.6"), "2.8.0-alpha.5"),
            Some("2.8.0-alpha.6".to_string())
        );
        assert_eq!(install_offer(Some("2.8.0-alpha.5"), "2.8.0-alpha.5"), None);
        assert_eq!(install_offer(Some("2.8.0-alpha.4"), "2.8.0-alpha.5"), None);
        assert_eq!(install_offer(None, "2.8.0-alpha.5"), None);
        // A local build outside the published grammar is never offered a downgrade or sidegrade.
        assert_eq!(install_offer(Some("2.8.0"), "2.8.0-dev"), None);
    }

    #[test]
    fn rejected_settings_are_the_callers_error_not_a_server_fault() {
        let error = anyhow::Error::new(crate::knobs::store::SettingRejected(
            "Frame has no \"brightness\" setting".into(),
        ));
        let (status, body) = knob_persistence_error(error);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body.0["error"].as_str().unwrap().contains("brightness"));
        let (status, _) = knob_persistence_error(anyhow::anyhow!("disk full"));
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    fn make_zone(id: &str, name: &str) -> ZoneInfo {
        ZoneInfo {
            zone_id: id.to_string(),
            zone_name: name.to_string(),
            source: "test".to_string(),
            state: "stopped".to_string(),
            volume_control: None,
            dsp: None,
            browse_supported: false,
            device: None,
            library_tabs: Vec::new(),
        }
    }

    #[test]
    fn controller_hide_list_filters_only_its_own_zones_and_keeps_order() {
        let zones = vec![
            make_zone("roon:a", "A"),
            make_zone("roon:b", "B"),
            make_zone("lms:c", "C"),
        ];
        let kept = without_hidden(zones.clone(), &["roon:b".to_string()]);
        let ids: Vec<_> = kept.iter().map(|z| z.zone_id.as_str()).collect();
        assert_eq!(ids, ["roon:a", "lms:c"]);

        // An empty list, or ids naming no listed zone, changes nothing.
        assert_eq!(without_hidden(zones.clone(), &[]).len(), 3);
        assert_eq!(without_hidden(zones, &["roon:gone".to_string()]).len(), 3);
    }

    #[test]
    fn controller_hide_list_changes_the_zones_sha_the_controller_polls() {
        let zones = vec![make_zone("roon:a", "A"), make_zone("roon:b", "B")];
        let shown = compute_zones_sha(&zones);
        let hidden = compute_zones_sha(&without_hidden(zones, &["roon:b".to_string()]));
        assert_ne!(shown, hidden);
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

    /// Reordering must change the SHA, or clients never learn about it.
    ///
    /// This test replaces `zones_sha_order_insensitive`, which asserted the opposite. That assertion
    /// was right for its time: the zone list came from `HashMap::values()` and had no stable order,
    /// so hashing order would have churned the SHA on every request. `zone_list::visible_zones` now
    /// guarantees a deterministic order, so hashing it is safe — and necessary, because reordering
    /// changes neither the set of zones nor their names, and is therefore the one user-visible
    /// change a sorted hash cannot see.
    #[test]
    fn zones_sha_changes_on_reorder() {
        let zones_a = vec![
            make_zone("zone-1", "Living Room"),
            make_zone("zone-2", "Kitchen"),
        ];

        let zones_b = vec![
            make_zone("zone-2", "Kitchen"),
            make_zone("zone-1", "Living Room"),
        ];

        assert_ne!(
            compute_zones_sha(&zones_a),
            compute_zones_sha(&zones_b),
            "a knob polling this SHA must be able to detect that the zone order changed"
        );
    }

    /// The same list still hashes the same. Determinism is what makes the SHA usable at all; the
    /// change above is that it is now determined by the order too, not that it became unstable.
    #[test]
    fn zones_sha_is_stable_for_an_unchanged_list() {
        let zones = vec![
            make_zone("zone-1", "Living Room"),
            make_zone("zone-2", "Kitchen"),
        ];

        assert_eq!(compute_zones_sha(&zones), compute_zones_sha(&zones));
    }

    /// Hiding a zone changes the SHA, because a hidden zone is absent from the list this hashes.
    /// (Renaming is already covered by `zones_sha_changes_on_rename` below.)
    #[test]
    fn zones_sha_changes_when_a_zone_is_hidden() {
        let shown = vec![
            make_zone("zone-1", "Living Room"),
            make_zone("zone-2", "Kitchen"),
        ];
        let hidden = vec![make_zone("zone-1", "Living Room")];

        assert_ne!(compute_zones_sha(&shown), compute_zones_sha(&hidden));
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

    #[test]
    fn apple_music_web_controls_use_adapter_commands() {
        assert!(matches!(
            adapter_command_for_action("Apple Music", "pause", None),
            Ok(AdapterCommand::Pause)
        ));
        assert!(matches!(
            adapter_command_for_action("Apple Music", "next", None),
            Ok(AdapterCommand::Next)
        ));
    }
}

pub(crate) use crate::provider_control::dispatch_hqplayer_action_via;

pub(crate) use crate::provider_control::dispatch_hqplayer_runtime_command_via;

pub(crate) use crate::provider_control::HqpDispatchError;
