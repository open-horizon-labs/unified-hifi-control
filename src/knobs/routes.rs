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

use crate::adapters::AdapterCommand;
use crate::aggregator::ZoneAggregator;
use crate::api::{
    dispatch_lms_runtime_command, dispatch_openhome_runtime_command, dispatch_roon_runtime_command,
    dispatch_upnp_runtime_command, lms_runtime_command_from_action,
    renderer_runtime_command_from_action, AppState,
};
use crate::bus::runtime::{
    CommandDeadlines, CommandGateway, CommandLane, CommandRequest, CommandStatus,
    HqpRuntimeCommand, RuntimeCommand,
};
use crate::bus::{Command, PrefixedZoneId, VolumeControl};
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
    crate::zone_list::visible_zones(state)
        .await
        .into_iter()
        .map(|z| ZoneInfo {
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
            let zone_infos = get_zone_infos(&state).await;
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
        let knob = state.knobs.get_or_create(id, knob_version.as_deref()).await;
        volume_step_override = knob.config.volume_step_override;
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

/// Control a direct HQPlayer instance (#328).
///
/// **Every refusal below is checked against the zone the aggregator published**, not against a fresh
/// adapter read. That is deliberate and it is the invariant that keeps this function honest: the
/// capability flag a client was handed in `GET /knob/now_playing` is literally the flag its command
/// is judged by, so "advertised" and "permitted" cannot drift apart. It is also required — a surface
/// may not query an adapter for state (`docs/ARCHITECTURE.md`, `tests/architecture_lint.rs`).
///
/// Capability decisions use the last published snapshot. After a successful write, the managed
/// adapter performs a coherent readback and publishes it through the aggregator before this
/// function reports success, so every surface converges on the same post-command state.
/// Transport-neutral outcome of [`dispatch_hqplayer_action`].
///
/// One dispatch function is shared by every surface that lets a caller name an arbitrary zone id
/// and an action string — today this HTTP handler and MCP's `hifi_control` tool
/// (`src/mcp/mod.rs`) — so the capability checks, clamps and command core exist in exactly one
/// place. Each surface converts this into its own wire shape.
pub(crate) enum HqpDispatchError {
    /// The named instance, or the zone the aggregator currently publishes for it, does not exist.
    NotFound(String),
    /// The action, value, or current zone state make this specific request invalid.
    BadRequest { message: String, code: &'static str },
    /// The adapter accepted the request but the native command itself failed.
    Backend(String),
}

impl HqpDispatchError {
    pub(crate) fn message(&self) -> &str {
        match self {
            Self::NotFound(message) | Self::Backend(message) => message,
            Self::BadRequest { message, .. } => message,
        }
    }
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

/// Same aggregator-gated routing as [`dispatch_hqplayer_action`], parameterized directly over the
/// aggregator and reliable command gateway so callers without a full `AppState` - MQTT's inbound
/// command router (#529) - reuse the identical dispatch path HTTP/knob/MCP use.
pub(crate) async fn dispatch_hqplayer_action_via(
    aggregator: &ZoneAggregator,
    gateway: Option<&CommandGateway>,
    zone_id: &str,
    instance: &str,
    action: &str,
    value: Option<f64>,
) -> Result<(), HqpDispatchError> {
    // The aggregator is the only state source consulted here.
    //
    // Its absence is decisive rather than merely inconvenient: the aggregator withdraws the zone when
    // the producer stops, so a zone it does not hold is one no surface will show and none of this
    // function's capability checks can be evaluated against. The transport arms below consulted the
    // zone only for `next`/`previous`, so `play`, `pause` and `stop` were accepted — and answered
    // `{"ok":true}` — for a withdrawn zone whose instance still happened to exist in the manager's
    // map. Found by the review pass, pinned by
    // `transport_is_refused_for_a_zone_the_aggregator_has_withdrawn`.
    let Some(zone) = aggregator.get_zone(zone_id).await else {
        let target = PrefixedZoneId::hqplayer(instance);
        let message = match gateway {
            Some(gateway) if gateway.has_endpoint(&target) => {
                format!("zone {zone_id} is not currently published")
            }
            _ => format!("HQPlayer instance '{instance}' is not configured"),
        };
        return Err(HqpDispatchError::NotFound(message));
    };

    let command = hqp_command_from_published_zone(&zone, action, value)?;
    dispatch_hqplayer_runtime_command_via(
        aggregator,
        gateway,
        instance,
        RuntimeCommand::Control(command),
        CommandLane::Interactive,
        std::time::Duration::from_secs(10),
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

/// Same aggregator-gated routing as [`dispatch_hqplayer_runtime_command`], parameterized directly
/// over the aggregator and reliable command gateway so callers without a full `AppState` - MQTT's
/// inbound command router (#529) - reuse the identical dispatch path HTTP/knob/MCP use.
async fn dispatch_hqplayer_runtime_command_via(
    aggregator: &ZoneAggregator,
    gateway: Option<&CommandGateway>,
    instance: &str,
    command: RuntimeCommand,
    lane: CommandLane,
    confirmation_budget: std::time::Duration,
) -> Result<(), HqpDispatchError> {
    let target = PrefixedZoneId::hqplayer(instance);
    let zone_id = target.to_string();
    if aggregator.get_zone(&zone_id).await.is_none() {
        let message = match gateway {
            Some(gateway) if gateway.has_endpoint(&target) => {
                format!("zone {zone_id} is not currently published")
            }
            _ => format!("HQPlayer instance '{instance}' is not configured"),
        };
        return Err(HqpDispatchError::NotFound(message));
    }
    let Some(gateway) = gateway else {
        return Err(HqpDispatchError::Backend(
            "HQPlayer reliable command runtime is unavailable".to_string(),
        ));
    };
    let now = tokio::time::Instant::now();
    let mut ticket = gateway
        .submit(CommandRequest {
            target,
            command,
            correlation_id: None,
            lane,
            deadlines: CommandDeadlines {
                dispatch_by: now + std::time::Duration::from_secs(3),
                confirm_by: now + confirmation_budget,
            },
        })
        .await
        .map_err(|error| {
            HqpDispatchError::Backend(format!("HQPlayer command admission failed: {error:?}"))
        })?;
    match ticket.wait_for_observable_result().await {
        CommandStatus::Confirmed { .. } => Ok(()),
        CommandStatus::Failed { detail } | CommandStatus::NotDispatched { detail } => {
            Err(HqpDispatchError::Backend(detail))
        }
        CommandStatus::Indeterminate => Err(HqpDispatchError::Backend(
            "HQPlayer accepted the command but did not publish a verified readback in time"
                .to_string(),
        )),
        CommandStatus::Queued | CommandStatus::Dispatched | CommandStatus::AwaitingProjection => {
            Err(HqpDispatchError::Backend(
                "HQPlayer command stopped without a terminal result".to_string(),
            ))
        }
    }
}

/// Translate only a request that the aggregator's published zone says is currently valid.  This
/// mirrors the legacy direct path below, but returns a semantic command for the endpoint worker so
/// no client surface gets its own HQPlayer transport implementation.
fn hqp_command_from_published_zone(
    zone: &crate::bus::Zone,
    action: &str,
    value: Option<f64>,
) -> Result<Command, HqpDispatchError> {
    match action {
        "play" => Ok(Command::Play),
        "pause" => Ok(Command::Pause),
        "stop" => Ok(Command::Stop),
        "next" if zone.is_next_allowed => Ok(Command::Next),
        "previous" | "prev" if zone.is_previous_allowed => Ok(Command::Previous),
        "next" | "previous" | "prev" => Err(HqpDispatchError::BadRequest {
            message: format!("{action} is not available in the zone's current state"),
            code: "ACTION_NOT_ALLOWED",
        }),
        // Resolve at the serialized native endpoint. Concurrent requests can otherwise all observe
        // one published state and collapse several toggles into the same Play or Pause command.
        "play_pause" | "playpause" => Ok(Command::PlayPause),
        "seek" => {
            if !zone.is_seekable {
                return Err(HqpDispatchError::BadRequest {
                    message: "this zone is not seekable in its current state".to_string(),
                    code: "ACTION_NOT_ALLOWED",
                });
            }
            let Some(position) = value else {
                return Err(HqpDispatchError::BadRequest {
                    message: "seek requires a numeric position in seconds".to_string(),
                    code: "INVALID_VALUE",
                });
            };
            if !position.is_finite() || position < 0.0 {
                return Err(HqpDispatchError::BadRequest {
                    message: "seek position must be a non-negative number of seconds".to_string(),
                    code: "INVALID_VALUE",
                });
            }
            let ceiling = zone
                .now_playing
                .as_ref()
                .and_then(|np| np.duration)
                .filter(|duration| *duration > 0.0);
            Ok(Command::Seek {
                position: ceiling.map_or(position, |duration| position.min(duration)),
            })
        }
        "vol_up" | "volume_up" | "vol_down" | "volume_down" => {
            let vc = require_volume_control(Some(zone))?;
            let step = value
                .map(f64::abs)
                .filter(|step| step.is_finite() && *step > 0.0)
                .unwrap_or(f64::from(vc.step));
            let delta = if action.contains("down") { -step } else { step };
            Ok(Command::VolumeRelative {
                // The endpoint resolves this delta against native State after it reaches the head
                // of the serialized queue. Resolving it here against `vc.value` collapses concurrent
                // knob turns that all observed the same pre-command projection into one step.
                delta: quantise_db(delta) as f32,
                output_id: None,
            })
        }
        "vol_abs" | "volume" => {
            let vc = require_volume_control(Some(zone))?;
            let Some(value) = value.filter(|value| value.is_finite()) else {
                return Err(HqpDispatchError::BadRequest {
                    message: "an absolute volume requires a finite numeric level in dB".to_string(),
                    code: "INVALID_VALUE",
                });
            };
            Ok(Command::VolumeAbsolute {
                value: quantise_db(value.clamp(f64::from(vc.min), f64::from(vc.max))) as f32,
                output_id: None,
            })
        }
        "mute" => {
            require_volume_control(Some(zone))?;
            Ok(Command::Mute {
                muted: true,
                output_id: None,
            })
        }
        _ => Err(HqpDispatchError::BadRequest {
            message: format!("Unknown action: {action}"),
            code: "UNKNOWN_ACTION",
        }),
    }
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

/// Round a computed dB level to a hundredth of a decibel before it reaches the wire.
///
/// The published zone carries `value`, `min`, `max` and `step` as `f32` (`crate::bus::VolumeControl`),
/// so widening them back to `f64` to do arithmetic reintroduces the representation error as trailing
/// digits. A daemon reporting a 0.1 dB step turned `-23.5 + 0.1` into `-23.399999998509884`, and
/// `set_volume_db` formats with `{}` — so that is what would have gone out on the wire, where the
/// reference client sends `-23.4`.
///
/// A hundredth of a dB is far below audibility and below the finest step any observed daemon reports,
/// so this cannot quantise away a real distinction; it only removes float noise. It is deliberately
/// **not** a clamp and does not decide any bound — the clamp against the zone's observed range has
/// already happened by the time this is called.
fn quantise_db(db: f64) -> f64 {
    (db * 100.0).round() / 100.0
}

/// The zone's published volume control, or a refusal.
///
/// A zone with no volume control is a zone whose daemon answers `Volume` with a bare
/// `result="Error"`. Sending one anyway is a request with no safe interpretation on a
/// hardware-attenuated setup, so it is refused before it reaches the wire.
///
/// The bounds are re-checked here even though the projection already refuses an unorderable range
/// (`HqpAdapter::volume_range_is_usable`). Both `.clamp` calls below panic on `min > max` or a NaN
/// bound, and a panic in an HTTP or MCP handler is a worse failure than a 400: this function is the
/// only gate in front of them, so it owns the precondition rather than trusting its caller's caller.
fn require_volume_control(
    zone: Option<&crate::bus::Zone>,
) -> Result<crate::bus::VolumeControl, HqpDispatchError> {
    let refuse = |message: &str| HqpDispatchError::BadRequest {
        message: message.to_string(),
        code: "VOLUME_NOT_AVAILABLE",
    };
    let control = zone
        .and_then(|z| z.volume_control.clone())
        .ok_or_else(|| refuse("this zone has no volume control"))?;
    if !(control.min.is_finite() && control.max.is_finite() && control.min < control.max) {
        return Err(refuse(
            "this zone's published volume range cannot bound a level",
        ));
    }
    Ok(control)
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

/// DELETE /knob/devices/{knob_id} - Remove a registered knob (admin)
///
/// Callers that publish Home Assistant discovery for knobs (#523) observe
/// this via the knob no longer appearing in the store and are responsible
/// for retracting any retained discovery configs they previously published.
pub async fn knob_remove_handler(
    State(state): State<AppState>,
    axum::extract::Path(knob_id): axum::extract::Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if state.knobs.remove(&knob_id).await {
        Ok(Json(serde_json::json!({ "ok": true })))
    } else {
        Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "knob not found"})),
        ))
    }
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
            browse_supported: false,
            library_tabs: Vec::new(),
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
