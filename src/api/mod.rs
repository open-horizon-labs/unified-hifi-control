//! HTTP API handlers

use crate::adapters::hqplayer::{HqpAdapter, HqpInstanceManager, HqpZoneLinkService};
use crate::adapters::lms::LmsAdapter;
use crate::adapters::openhome::OpenHomeAdapter;
use crate::adapters::roon::RoonAdapter;
use crate::adapters::upnp::UPnPAdapter;
use crate::adapters::Startable;
use crate::aggregator::ZoneAggregator;
use crate::bus::SharedBus;
use crate::coordinator::AdapterCoordinator;
use crate::knobs::KnobStore;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse,
    },
    Json,
};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

/// Shared application state
#[derive(Clone)]
pub struct AppState {
    pub roon: Arc<RoonAdapter>,
    pub hqplayer: Arc<HqpAdapter>,
    pub hqp_instances: Arc<HqpInstanceManager>,
    pub hqp_zone_links: Arc<HqpZoneLinkService>,
    pub lms: Arc<LmsAdapter>,
    pub openhome: Arc<OpenHomeAdapter>,
    pub upnp: Arc<UPnPAdapter>,
    pub knobs: KnobStore,
    pub bus: SharedBus,
    pub aggregator: Arc<ZoneAggregator>,
    pub coordinator: Arc<AdapterCoordinator>,
    pub startable_adapters: Arc<Vec<Arc<dyn Startable>>>,
    pub start_time: Instant,
    /// Cancellation token for graceful shutdown (terminates SSE streams)
    pub shutdown: CancellationToken,
    /// Count of active SSE connections (for shutdown diagnostics)
    pub sse_connections: Arc<AtomicUsize>,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        roon: Arc<RoonAdapter>,
        hqplayer: Arc<HqpAdapter>,
        hqp_instances: Arc<HqpInstanceManager>,
        hqp_zone_links: Arc<HqpZoneLinkService>,
        lms: Arc<LmsAdapter>,
        openhome: Arc<OpenHomeAdapter>,
        upnp: Arc<UPnPAdapter>,
        knobs: KnobStore,
        bus: SharedBus,
        aggregator: Arc<ZoneAggregator>,
        coordinator: Arc<AdapterCoordinator>,
        startable_adapters: Vec<Arc<dyn Startable>>,
        start_time: Instant,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            roon,
            hqplayer,
            hqp_instances,
            hqp_zone_links,
            lms,
            openhome,
            upnp,
            knobs,
            bus,
            aggregator,
            coordinator,
            startable_adapters: Arc::new(startable_adapters),
            start_time,
            shutdown,
            sse_connections: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Get the count of active SSE connections
    pub fn active_sse_connections(&self) -> usize {
        self.sse_connections.load(Ordering::Relaxed)
    }

    /// Fetch image from the appropriate adapter based on zone_id prefix
    ///
    /// Routes to the correct backend (Roon, LMS, OpenHome) based on the zone_id
    /// prefix and fetches the image using that adapter's API.
    ///
    /// Note: UPnP zones don't support image retrieval as the protocol doesn't
    /// expose album art URLs in a standardized way that can be proxied.
    ///
    /// If `format` is Some("rgb565"), converts to RGB565 format for ESP32 LCDs.
    pub async fn get_image(
        &self,
        zone_id: &str,
        image_key: &str,
        width: Option<u32>,
        height: Option<u32>,
        format: Option<&str>,
    ) -> anyhow::Result<crate::bus::ImageData> {
        use crate::bus::ImageData;
        use crate::knobs::image::jpeg_to_rgb565;

        // Fetch raw image from appropriate adapter
        let raw_image = if zone_id.starts_with("lms:") {
            let (content_type, data) = self.lms.get_artwork(image_key, width, height).await?;
            ImageData { content_type, data }
        } else if zone_id.starts_with("openhome:") {
            let img = self.openhome.get_image(image_key).await?;
            ImageData {
                content_type: img.content_type,
                data: img.data,
            }
        } else if zone_id.starts_with("upnp:") {
            anyhow::bail!(
                "UPnP zones don't support image retrieval - the protocol doesn't expose album art URLs"
            )
        } else if zone_id.starts_with("roon:") || !zone_id.contains(':') {
            let img = self.roon.get_image(image_key, width, height).await?;
            ImageData {
                content_type: img.content_type,
                data: img.data,
            }
        } else {
            anyhow::bail!("Unknown zone type for image: {}", zone_id)
        };

        // Convert to RGB565 if requested (for ESP32 LCD displays)
        if format == Some("rgb565") {
            // Use square dimensions when only one side specified (matches adapter behavior)
            let (target_w, target_h) = match (width, height) {
                (Some(w), Some(h)) => (w, h),
                (Some(w), None) => (w, w),
                (None, Some(h)) => (h, h),
                (None, None) => (240, 240),
            };

            match jpeg_to_rgb565(&raw_image.data, target_w, target_h) {
                Ok(rgb565) => Ok(ImageData {
                    content_type: "application/octet-stream".to_string(),
                    data: rgb565.data,
                }),
                Err(_) => {
                    // Fall back to original on conversion error
                    Ok(raw_image)
                }
            }
        } else {
            Ok(raw_image)
        }
    }
}

/// Error response
#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

/// Generic zones response wrapper - clients expect {zones: [...]}
#[derive(Serialize)]
pub struct ZonesWrapper<T: Serialize> {
    pub zones: Vec<T>,
}

/// HQPlayer instances response wrapper - clients expect {instances: [...]}
#[derive(Serialize)]
pub struct InstancesWrapper<T: Serialize> {
    pub instances: Vec<T>,
}

/// LMS players response wrapper - clients expect {players: [...]}
#[derive(Serialize)]
pub struct PlayersWrapper<T: Serialize> {
    pub players: Vec<T>,
}

/// General status response
#[derive(Serialize)]
pub struct StatusResponse {
    pub service: &'static str,
    pub version: &'static str,
    pub git_sha: &'static str,
    pub uptime_secs: u64,
    pub roon_connected: bool,
    pub hqplayer_connected: bool,
    pub lms_connected: bool,
    pub openhome_devices: usize,
    pub upnp_devices: usize,
    pub bus_subscribers: usize,
}

/// GET /status - Service health check
pub async fn status_handler(State(state): State<AppState>) -> Json<StatusResponse> {
    let roon_status = state.roon.get_status().await;
    let hqp_status = state.hqplayer.get_status().await;
    let lms_status = state.lms.get_status().await;
    let openhome_status = state.openhome.get_status().await;
    let upnp_status = state.upnp.get_status().await;

    Json(StatusResponse {
        service: "unified-hifi-control",
        version: env!("UHC_VERSION"),
        git_sha: env!("UHC_GIT_SHA"),
        uptime_secs: state.start_time.elapsed().as_secs(),
        roon_connected: roon_status.connected,
        hqplayer_connected: hqp_status.connected,
        lms_connected: lms_status.connected,
        openhome_devices: openhome_status.device_count,
        upnp_devices: upnp_status.renderer_count,
        bus_subscribers: state.bus.subscriber_count(),
    })
}

// =============================================================================
// Roon handlers
// =============================================================================

/// GET /roon/status - Roon connection status
pub async fn roon_status_handler(
    State(state): State<AppState>,
) -> Json<crate::adapters::roon::RoonStatus> {
    Json(state.roon.get_status().await)
}

/// GET /roon/zones - List all Roon zones
pub async fn roon_zones_handler(
    State(state): State<AppState>,
) -> Json<ZonesWrapper<crate::adapters::roon::Zone>> {
    Json(ZonesWrapper {
        zones: state.roon.get_zones().await,
    })
}

/// GET /roon/zone/:zone_id - Get specific zone
pub async fn roon_zone_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
) -> impl IntoResponse {
    match state.roon.get_zone(&zone_id).await {
        Some(zone) => (StatusCode::OK, Json(zone)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Zone not found: {}", zone_id),
            }),
        )
            .into_response(),
    }
}

/// Control request body
#[derive(Deserialize)]
pub struct ControlRequest {
    pub zone_id: String,
    pub action: String,
}

/// POST /roon/control - Control playback
pub async fn roon_control_handler(
    State(state): State<AppState>,
    Json(req): Json<ControlRequest>,
) -> impl IntoResponse {
    match state.roon.control(&req.zone_id, &req.action).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// Volume request body
#[derive(Deserialize)]
pub struct VolumeRequest {
    pub output_id: String,
    pub value: i32,
    #[serde(default)]
    pub relative: bool,
}

/// POST /roon/volume - Change volume
pub async fn roon_volume_handler(
    State(state): State<AppState>,
    Json(req): Json<VolumeRequest>,
) -> impl IntoResponse {
    match state
        .roon
        .change_volume(&req.output_id, req.value, req.relative)
        .await
    {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// Query params for image request
#[derive(Deserialize)]
pub struct ImageQuery {
    pub image_key: String,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

/// GET /roon/image - fetch album art
pub async fn roon_image_handler(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<ImageQuery>,
) -> impl IntoResponse {
    match state
        .roon
        .get_image(&params.image_key, params.width, params.height)
        .await
    {
        Ok(image_data) => {
            let headers = [(
                axum::http::header::CONTENT_TYPE,
                image_data
                    .content_type
                    .parse()
                    .unwrap_or(axum::http::HeaderValue::from_static("image/jpeg")),
            )];
            (StatusCode::OK, headers, image_data.data).into_response()
        }
        Err(e) => {
            tracing::warn!("Image fetch failed: {}", e);
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
                .into_response()
        }
    }
}

// =============================================================================
// HQPlayer handlers
// =============================================================================

/// GET /hqplayer/status - HQPlayer connection status
pub async fn hqp_status_handler(
    State(state): State<AppState>,
) -> Json<crate::adapters::hqplayer::HqpConnectionStatus> {
    Json(state.hqplayer.get_status().await)
}

/// GET /hqplayer/pipeline - HQPlayer pipeline status
pub async fn hqp_pipeline_handler(State(state): State<AppState>) -> impl IntoResponse {
    // Quick check - if not connected, return error immediately (don't block on timeout)
    let status = state.hqplayer.get_status().await;
    if !status.connected {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "HQPlayer not connected".to_string(),
            }),
        )
            .into_response();
    }

    match state.hqplayer.get_pipeline_status().await {
        Ok(pipeline) => (StatusCode::OK, Json(pipeline)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// HQPlayer control request
#[derive(Deserialize)]
pub struct HqpControlRequest {
    pub action: String,
}

/// POST /hqplayer/control - Control HQPlayer playback
pub async fn hqp_control_handler(
    State(state): State<AppState>,
    Json(req): Json<HqpControlRequest>,
) -> impl IntoResponse {
    match state.hqplayer.control(&req.action).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// HQPlayer volume request
#[derive(Deserialize)]
pub struct HqpVolumeRequest {
    pub value: i32,
}

/// POST /hqplayer/volume - Change HQPlayer volume
pub async fn hqp_volume_handler(
    State(state): State<AppState>,
    Json(req): Json<HqpVolumeRequest>,
) -> impl IntoResponse {
    match state.hqplayer.set_volume(req.value).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// HQPlayer setting request (legacy - uses name/value with u32)
#[derive(Deserialize)]
pub struct HqpSettingRequest {
    pub name: String,
    pub value: u32,
}

/// POST /hqplayer/setting - Change HQPlayer pipeline setting (legacy endpoint)
pub async fn hqp_setting_handler(
    State(state): State<AppState>,
    Json(req): Json<HqpSettingRequest>,
) -> impl IntoResponse {
    let result = match req.name.as_str() {
        "mode" => state.hqplayer.set_mode(req.value).await,
        "filter" => state.hqplayer.set_filter(req.value, Some(req.value)).await, // Sets both 1x and Nx
        "filter1x" => state.hqplayer.set_filter_1x(req.value).await, // Sets only 1x, preserves Nx
        "filterNx" | "filternx" => state.hqplayer.set_filter_nx(req.value).await, // Sets only Nx, preserves 1x
        "shaper" => state.hqplayer.set_shaper(req.value).await,
        "samplerate" | "rate" => state.hqplayer.set_rate(req.value).await,
        _ => Err(anyhow::anyhow!("Unknown setting: {}", req.name)),
    };

    match result {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// HQPlayer pipeline setting request - iOS/Node.js compatible format
#[derive(Deserialize)]
pub struct HqpPipelineRequest {
    pub setting: String,
    pub value: serde_json::Value, // Can be string or number
}

/// POST /hqp/pipeline - Change HQPlayer pipeline setting (iOS compatible)
pub async fn hqp_pipeline_update_handler(
    State(state): State<AppState>,
    Json(req): Json<HqpPipelineRequest>,
) -> impl IntoResponse {
    // Convert value to u32 - accept both numeric and string representations
    // Note: HQPlayer mode values can be negative (e.g., -1 for PCM), so we parse as i64 first
    // and cast to u32 to preserve the bit pattern
    let value: u32 = match &req.value {
        serde_json::Value::Number(n) => n.as_i64().unwrap_or(0) as u32,
        serde_json::Value::String(s) => s.parse::<i64>().unwrap_or(0) as u32,
        _ => 0,
    };

    let valid_settings = [
        "mode",
        "samplerate",
        "filter1x",
        "filterNx",
        "shaper",
        "dither",
    ];
    if !valid_settings.contains(&req.setting.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("Invalid setting. Valid: {}", valid_settings.join(", ")),
            }),
        )
            .into_response();
    }

    let result = match req.setting.as_str() {
        "mode" => state.hqplayer.set_mode(value).await,
        "filter1x" => state.hqplayer.set_filter_1x(value).await,
        "filterNx" | "filternx" => state.hqplayer.set_filter_nx(value).await,
        "shaper" => state.hqplayer.set_shaper(value).await,
        "samplerate" => state.hqplayer.set_rate(value).await,
        "dither" => state.hqplayer.set_shaper(value).await, // dither uses same API
        _ => Err(anyhow::anyhow!("Unknown setting: {}", req.setting)),
    };

    match result {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// GET /hqplayer/profiles - Get available profiles
pub async fn hqp_profiles_handler(State(state): State<AppState>) -> impl IntoResponse {
    match state.hqplayer.fetch_profiles().await {
        Ok(profiles) => (StatusCode::OK, Json(profiles)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// HQPlayer profile request
#[derive(Deserialize)]
pub struct HqpProfileRequest {
    pub profile: String,
}

/// POST /hqplayer/profile - Load a profile
pub async fn hqp_load_profile_handler(
    State(state): State<AppState>,
    Json(req): Json<HqpProfileRequest>,
) -> impl IntoResponse {
    match state.hqplayer.load_profile(&req.profile).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// GET /hqplayer/matrix/profiles - Get matrix profiles and current selection
pub async fn hqp_matrix_profiles_handler(State(state): State<AppState>) -> impl IntoResponse {
    // Quick check - if not connected, return empty immediately (don't block on timeout)
    let status = state.hqplayer.get_status().await;
    if !status.connected {
        return (
            StatusCode::OK,
            Json(serde_json::json!({
                "profiles": [],
                "current": null
            })),
        )
            .into_response();
    }

    let profiles = state.hqplayer.get_matrix_profiles().await;
    let current = state.hqplayer.get_matrix_profile().await;

    match (profiles, current) {
        (Ok(profiles), Ok(current)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "profiles": profiles,
                "current": current
            })),
        )
            .into_response(),
        (Err(e), _) | (_, Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// Matrix profile request
#[derive(Deserialize)]
pub struct HqpMatrixProfileRequest {
    pub profile: u32,
}

/// POST /hqplayer/matrix/profile - Set matrix profile
pub async fn hqp_set_matrix_profile_handler(
    State(state): State<AppState>,
    Json(req): Json<HqpMatrixProfileRequest>,
) -> impl IntoResponse {
    match state.hqplayer.set_matrix_profile(req.profile).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

// =============================================================================
// LMS handlers
// =============================================================================

/// GET /lms/status - LMS connection status
pub async fn lms_status_handler(
    State(state): State<AppState>,
) -> Json<crate::adapters::lms::LmsStatus> {
    Json(state.lms.get_status().await)
}

/// GET /lms/players - Get all players
pub async fn lms_players_handler(
    State(state): State<AppState>,
) -> Json<PlayersWrapper<crate::adapters::lms::LmsPlayer>> {
    Json(PlayersWrapper {
        players: state.lms.get_cached_players().await,
    })
}

/// GET /lms/player/:player_id - Get specific player
pub async fn lms_player_handler(
    State(state): State<AppState>,
    Path(player_id): Path<String>,
) -> impl IntoResponse {
    match state.lms.get_cached_player(&player_id).await {
        Some(player) => (StatusCode::OK, Json(player)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Player not found: {}", player_id),
            }),
        )
            .into_response(),
    }
}

/// LMS control request
#[derive(Deserialize)]
pub struct LmsControlRequest {
    pub player_id: String,
    pub action: String,
    #[serde(default)]
    pub value: Option<i32>,
}

/// POST /lms/control - Control LMS player
pub async fn lms_control_handler(
    State(state): State<AppState>,
    Json(req): Json<LmsControlRequest>,
) -> impl IntoResponse {
    match state
        .lms
        .control(&req.player_id, &req.action, req.value)
        .await
    {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// LMS volume request
#[derive(Deserialize)]
pub struct LmsVolumeRequest {
    pub player_id: String,
    pub value: i32,
    #[serde(default)]
    pub relative: bool,
}

/// POST /lms/volume - Change LMS player volume
pub async fn lms_volume_handler(
    State(state): State<AppState>,
    Json(req): Json<LmsVolumeRequest>,
) -> impl IntoResponse {
    match state
        .lms
        .change_volume(&req.player_id, req.value, req.relative)
        .await
    {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// LMS discovery request query params
#[derive(Deserialize)]
pub struct LmsDiscoverRequest {
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// GET /lms/discover - Discover LMS servers on the local network via UDP broadcast
pub async fn lms_discover_handler(Query(params): Query<LmsDiscoverRequest>) -> impl IntoResponse {
    use crate::adapters::discover_lms_servers;

    match discover_lms_servers(params.timeout_ms).await {
        Ok(servers) => (
            StatusCode::OK,
            Json(serde_json::json!({ "discovered": servers })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Discovery failed: {}", e),
            }),
        )
            .into_response(),
    }
}

// =============================================================================
// SSE Events
// =============================================================================

/// GET /events - Server-Sent Events stream
/// Guard that decrements SSE connection count on drop
struct SseConnectionGuard {
    counter: Arc<AtomicUsize>,
}

impl Drop for SseConnectionGuard {
    fn drop(&mut self) {
        let prev = self.counter.fetch_sub(1, Ordering::Relaxed);
        tracing::debug!("SSE connection closed ({} remaining)", prev - 1);
    }
}

pub async fn events_handler(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Track this connection
    let count = state.sse_connections.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::debug!("SSE connection opened ({} active)", count);

    let guard = SseConnectionGuard {
        counter: state.sse_connections.clone(),
    };
    let shutdown = state.shutdown.clone();
    let rx = state.bus.subscribe();

    // Create stream that terminates on shutdown
    // Use futures::StreamExt::take_until via UFCS (tokio_stream doesn't have it)
    let base_stream = BroadcastStream::new(rx);
    let with_shutdown =
        futures::StreamExt::take_until(base_stream, async move { shutdown.cancelled().await });

    let stream = with_shutdown
        .filter_map(|result| match result {
            Ok(event) => {
                // Serialize event to JSON
                match serde_json::to_string(&event) {
                    Ok(json) => Some(Ok(Event::default().data(json))),
                    Err(_) => None,
                }
            }
            Err(_) => None, // Skip lagged messages
        })
        // Use map + flatten to attach guard lifetime to stream
        // When stream ends, guard is dropped (decrementing counter)
        .map(move |item| {
            let _ = &guard; // Keep guard alive while stream produces items
            item
        });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}

// =============================================================================
// OpenHome handlers
// =============================================================================

/// GET /openhome/status - OpenHome discovery status
pub async fn openhome_status_handler(
    State(state): State<AppState>,
) -> Json<crate::adapters::openhome::OpenHomeStatus> {
    Json(state.openhome.get_status().await)
}

/// GET /openhome/zones - List all discovered OpenHome devices
pub async fn openhome_zones_handler(
    State(state): State<AppState>,
) -> Json<ZonesWrapper<crate::adapters::openhome::OpenHomeZone>> {
    Json(ZonesWrapper {
        zones: state.openhome.get_zones().await,
    })
}

/// GET /openhome/zone/:zone_id/now_playing - Get now playing for zone
pub async fn openhome_now_playing_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
) -> impl IntoResponse {
    match state.openhome.get_now_playing(&zone_id).await {
        Some(np) => (StatusCode::OK, Json(np)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Zone not found: {}", zone_id),
            }),
        )
            .into_response(),
    }
}

/// OpenHome control request
#[derive(Deserialize)]
pub struct OpenHomeControlRequest {
    pub zone_id: String,
    pub action: String,
    #[serde(default)]
    pub value: Option<i32>,
}

/// POST /openhome/control - Control OpenHome device
pub async fn openhome_control_handler(
    State(state): State<AppState>,
    Json(req): Json<OpenHomeControlRequest>,
) -> impl IntoResponse {
    match state
        .openhome
        .control(&req.zone_id, &req.action, req.value)
        .await
    {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

// =============================================================================
// UPnP handlers
// =============================================================================

/// GET /upnp/status - UPnP discovery status
pub async fn upnp_status_handler(
    State(state): State<AppState>,
) -> Json<crate::adapters::upnp::UPnPStatus> {
    Json(state.upnp.get_status().await)
}

/// GET /upnp/zones - List all discovered UPnP renderers
pub async fn upnp_zones_handler(
    State(state): State<AppState>,
) -> Json<ZonesWrapper<crate::adapters::upnp::UPnPZone>> {
    Json(ZonesWrapper {
        zones: state.upnp.get_zones().await,
    })
}

/// GET /upnp/zone/:zone_id/now_playing - Get now playing for renderer
pub async fn upnp_now_playing_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
) -> impl IntoResponse {
    match state.upnp.get_now_playing(&zone_id).await {
        Some(np) => (StatusCode::OK, Json(np)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Renderer not found: {}", zone_id),
            }),
        )
            .into_response(),
    }
}

/// UPnP control request
#[derive(Deserialize)]
pub struct UPnPControlRequest {
    pub zone_id: String,
    pub action: String,
    #[serde(default)]
    pub value: Option<i32>,
}

/// POST /upnp/control - Control UPnP renderer
pub async fn upnp_control_handler(
    State(state): State<AppState>,
    Json(req): Json<UPnPControlRequest>,
) -> impl IntoResponse {
    match state
        .upnp
        .control(&req.zone_id, &req.action, req.value)
        .await
    {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

// =============================================================================
// Configuration handlers
// =============================================================================

/// LMS configuration request
#[derive(Deserialize)]
pub struct LmsConfigRequest {
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// POST /lms/configure - Configure LMS connection
pub async fn lms_configure_handler(
    State(state): State<AppState>,
    Json(req): Json<LmsConfigRequest>,
) -> impl IntoResponse {
    // Stop existing connection if any
    state.lms.stop().await;

    // Configure new connection
    state
        .lms
        .configure(req.host.clone(), req.port, req.username, req.password)
        .await;

    // Start the adapter
    match state.lms.start().await {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "ok": true,
                "host": req.host,
                "port": req.port.unwrap_or(9000)
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// HQPlayer configuration request
#[derive(Deserialize)]
pub struct HqpConfigRequest {
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub web_port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// POST /hqplayer/configure - Configure HQPlayer connection
pub async fn hqp_configure_handler(
    State(state): State<AppState>,
    Json(req): Json<HqpConfigRequest>,
) -> impl IntoResponse {
    // Configure the adapter
    state
        .hqplayer
        .configure(
            req.host.clone(),
            req.port,
            req.web_port,
            req.username,
            req.password,
        )
        .await;

    // Save to instance manager for persistence
    state.hqp_instances.save_to_config().await;

    // Test connection by attempting to get pipeline status (this establishes connection)
    let connected = match state.hqplayer.get_pipeline_status().await {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("HQPlayer connection test failed: {}", e);
            false
        }
    };

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "host": req.host,
            "port": req.port.unwrap_or(4321),
            "web_port": req.web_port.unwrap_or(8088),
            "connected": connected
        })),
    )
        .into_response()
}

/// GET /lms/config - Get current LMS configuration
pub async fn lms_config_handler(State(state): State<AppState>) -> impl IntoResponse {
    let status = state.lms.get_status().await;
    Json(serde_json::json!({
        "configured": status.host.is_some(),
        "connected": status.connected,
        "host": status.host,
        "port": status.port
    }))
}

/// GET /hqplayer/config - Get current HQPlayer configuration
pub async fn hqp_config_handler(State(state): State<AppState>) -> impl IntoResponse {
    let status = state.hqplayer.get_status().await;
    let has_web_creds = state.hqplayer.has_web_credentials().await;
    Json(serde_json::json!({
        "configured": status.host.is_some(),
        "connected": status.connected,
        "host": status.host,
        "port": status.port,
        "web_port": status.web_port,
        "has_web_credentials": has_web_creds
    }))
}

/// HQPlayer detect request body
#[derive(Deserialize)]
pub struct HqpDetectRequest {
    pub host: String,
    #[serde(default = "default_hqp_port")]
    pub port: u16,
}

fn default_hqp_port() -> u16 {
    4321
}

/// POST /hqp/detect - Detect HQPlayer at a given host
pub async fn hqp_detect_handler(Json(req): Json<HqpDetectRequest>) -> impl IntoResponse {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::time::{timeout, Duration};

    // Try to connect to HQPlayer's native protocol port
    let addr = format!("{}:{}", req.host, req.port);

    let stream = match timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(_)) | Err(_) => {
            return Json(serde_json::json!({
                "reachable": false,
                "error": "Cannot connect to HQPlayer at this address"
            }));
        }
    };

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // Read initial greeting
    let mut greeting = String::new();
    if timeout(Duration::from_secs(2), reader.read_line(&mut greeting))
        .await
        .is_err()
    {
        return Json(serde_json::json!({
            "reachable": false,
            "error": "No response from HQPlayer"
        }));
    }

    // Send INFO command
    if write_half
        .write_all(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><info/>\n")
        .await
        .is_err()
    {
        return Json(serde_json::json!({
            "reachable": false,
            "error": "Failed to send command to HQPlayer"
        }));
    }

    // Read INFO response
    let mut response = String::new();
    if timeout(Duration::from_secs(2), reader.read_line(&mut response))
        .await
        .is_err()
    {
        return Json(serde_json::json!({
            "reachable": false,
            "error": "No INFO response from HQPlayer"
        }));
    }

    // Parse XML response for product/version
    let product = extract_xml_attr(&response, "product");
    let version = extract_xml_attr(&response, "version");
    let is_embedded = product
        .as_ref()
        .map(|p| p.to_lowercase().contains("embedded"))
        .unwrap_or(false);

    Json(serde_json::json!({
        "reachable": true,
        "product": product,
        "version": version,
        "isEmbedded": is_embedded
    }))
}

/// Extract attribute value from XML string
fn extract_xml_attr(xml: &str, attr: &str) -> Option<String> {
    let pattern = format!("{}=\"", attr);
    if let Some(start) = xml.find(&pattern) {
        let value_start = start + pattern.len();
        if let Some(end) = xml[value_start..].find('"') {
            return Some(xml[value_start..value_start + end].to_string());
        }
    }
    None
}

// =============================================================================
// HQPlayer multi-instance handlers
// =============================================================================

/// GET /hqp/instances - List all HQPlayer instances
pub async fn hqp_instances_handler(State(state): State<AppState>) -> impl IntoResponse {
    let instances = state.hqp_instances.list_instances().await;
    Json(InstancesWrapper { instances })
}

/// HQPlayer add instance request
#[derive(Deserialize)]
pub struct HqpAddInstanceRequest {
    pub name: String,
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub web_port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// POST /hqp/instances - Add or update an HQPlayer instance
pub async fn hqp_add_instance_handler(
    State(state): State<AppState>,
    Json(req): Json<HqpAddInstanceRequest>,
) -> impl IntoResponse {
    if req.name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Instance name is required".to_string(),
            }),
        )
            .into_response();
    }

    if req.host.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Host is required".to_string(),
            }),
        )
            .into_response();
    }

    let _adapter = state
        .hqp_instances
        .add_instance(
            req.name.clone(),
            req.host.clone(),
            req.port,
            req.web_port,
            req.username,
            req.password,
        )
        .await;

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "name": req.name,
            "host": req.host,
            "port": req.port.unwrap_or(4321)
        })),
    )
        .into_response()
}

/// DELETE /hqp/instances/:name - Remove an HQPlayer instance
pub async fn hqp_remove_instance_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    // Remove zone links pointing to this instance first
    let _links_removed = state.hqp_zone_links.remove_links_for_instance(&name).await;

    if state.hqp_instances.remove_instance(&name).await {
        (
            StatusCode::OK,
            Json(serde_json::json!({"ok": true, "removed": name})),
        )
            .into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Instance not found: {}", name),
            }),
        )
            .into_response()
    }
}

/// GET /hqp/instances/:name/profiles - Get profiles for a specific HQPlayer instance
pub async fn hqp_instance_profiles_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let adapter = match state.hqp_instances.get(&name).await {
        Some(a) => a,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: format!("Instance not found: {}", name),
                }),
            )
                .into_response()
        }
    };

    match adapter.fetch_profiles().await {
        Ok(profiles) => (StatusCode::OK, Json(profiles)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// POST /hqp/instances/:name/profile - Load a profile on a specific HQPlayer instance
pub async fn hqp_instance_load_profile_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<HqpProfileRequest>,
) -> impl IntoResponse {
    let adapter = match state.hqp_instances.get(&name).await {
        Some(a) => a,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: format!("Instance not found: {}", name),
                }),
            )
                .into_response()
        }
    };

    match adapter.load_profile(&req.profile).await {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({"ok": true, "instance": name, "profile": req.profile})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// GET /hqp/instances/:name/matrix/profiles - Get matrix profiles for a specific instance
pub async fn hqp_instance_matrix_profiles_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let adapter = match state.hqp_instances.get(&name).await {
        Some(a) => a,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: format!("Instance not found: {}", name),
                }),
            )
                .into_response()
        }
    };

    let profiles = adapter.get_matrix_profiles().await;
    let current = adapter.get_matrix_profile().await;

    match (profiles, current) {
        (Ok(profiles), Ok(current)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "instance": name,
                "profiles": profiles,
                "current": current
            })),
        )
            .into_response(),
        (Err(e), _) | (_, Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// Matrix profile request for instance
#[derive(Deserialize)]
pub struct HqpInstanceMatrixProfileRequest {
    pub value: u32,
}

/// POST /hqp/instances/:name/matrix/profile - Set matrix profile on a specific instance
pub async fn hqp_instance_set_matrix_profile_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<HqpInstanceMatrixProfileRequest>,
) -> impl IntoResponse {
    let adapter = match state.hqp_instances.get(&name).await {
        Some(a) => a,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: format!("Instance not found: {}", name),
                }),
            )
                .into_response()
        }
    };

    match adapter.set_matrix_profile(req.value).await {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({"ok": true, "instance": name, "value": req.value})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

// =============================================================================
// HQPlayer zone linking handlers
// =============================================================================

/// GET /hqp/zones/links - Get all zone links
pub async fn hqp_zone_links_handler(State(state): State<AppState>) -> impl IntoResponse {
    let links = state.hqp_zone_links.get_links().await;
    Json(serde_json::json!({ "links": links }))
}

/// Zone link request
#[derive(Deserialize)]
pub struct ZoneLinkRequest {
    pub zone_id: String,
    pub instance: String,
}

/// POST /hqp/zones/link - Link a zone to an HQPlayer instance
pub async fn hqp_zone_link_handler(
    State(state): State<AppState>,
    Json(req): Json<ZoneLinkRequest>,
) -> impl IntoResponse {
    if req.zone_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "zone_id is required".to_string(),
            }),
        )
            .into_response();
    }

    if req.instance.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "instance is required".to_string(),
            }),
        )
            .into_response();
    }

    match state
        .hqp_zone_links
        .link_zone(req.zone_id.clone(), req.instance.clone())
        .await
    {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "ok": true,
                "zone_id": req.zone_id,
                "instance": req.instance
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// Zone unlink request
#[derive(Deserialize)]
pub struct ZoneUnlinkRequest {
    pub zone_id: String,
}

/// POST /hqp/zones/unlink - Unlink a zone from HQPlayer
pub async fn hqp_zone_unlink_handler(
    State(state): State<AppState>,
    Json(req): Json<ZoneUnlinkRequest>,
) -> impl IntoResponse {
    if req.zone_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "zone_id is required".to_string(),
            }),
        )
            .into_response();
    }

    let was_linked = state.hqp_zone_links.unlink_zone(&req.zone_id).await;

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "zone_id": req.zone_id,
            "was_linked": was_linked
        })),
    )
        .into_response()
}

/// GET /hqp/zones/:zone_id/pipeline - Get HQP pipeline for a linked zone
pub async fn hqp_zone_pipeline_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
) -> impl IntoResponse {
    match state.hqp_zone_links.get_pipeline_for_zone(&zone_id).await {
        Some(pipeline) => (StatusCode::OK, Json(pipeline)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!(
                    "Zone {} not linked to HQPlayer or HQPlayer not configured",
                    zone_id
                ),
            }),
        )
            .into_response(),
    }
}

// =============================================================================
// HQPlayer discovery handler
// =============================================================================

/// HQP discovery request
#[derive(Deserialize)]
pub struct HqpDiscoverRequest {
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// GET /hqp/discover - Discover HQPlayer instances on the network via UDP multicast
pub async fn hqp_discover_handler(Query(params): Query<HqpDiscoverRequest>) -> impl IntoResponse {
    use crate::adapters::hqplayer::discover_hqplayers;

    match discover_hqplayers(params.timeout_ms).await {
        Ok(instances) => (
            StatusCode::OK,
            Json(serde_json::json!({ "discovered": instances })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Discovery failed: {}", e),
            }),
        )
            .into_response(),
    }
}

// =============================================================================
// App settings handlers
// =============================================================================

/// App settings for UI preferences
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    // Support both snake_case (Rust) and camelCase (Node.js) for seamless migration
    #[serde(default, alias = "hideKnobsPage")]
    pub hide_knobs_page: bool,
    #[serde(default, alias = "hideHqpPage")]
    pub hide_hqp_page: bool,
    #[serde(default, alias = "hideLmsPage")]
    pub hide_lms_page: bool,
    #[serde(default)]
    pub adapters: AdapterSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AdapterSettings {
    #[serde(default = "default_true")]
    pub roon: bool,
    #[serde(default)]
    pub upnp: bool,
    #[serde(default)]
    pub openhome: bool,
    #[serde(default)]
    pub lms: bool,
    #[serde(default)]
    pub hqplayer: bool,
}

fn default_true() -> bool {
    true
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            hide_knobs_page: false,
            hide_hqp_page: false,
            hide_lms_page: false,
            adapters: AdapterSettings {
                roon: true,
                upnp: false,
                openhome: false,
                lms: false,
                hqplayer: false,
            },
        }
    }
}

const APP_SETTINGS_FILE: &str = "app-settings.json";

fn settings_path() -> std::path::PathBuf {
    crate::config::get_config_file_path(APP_SETTINGS_FILE)
}

/// Load app settings from disk
/// Issue #76: Uses read_config_file for backwards-compatible fallback
pub fn load_app_settings() -> AppSettings {
    // read_config_file checks subdir first, falls back to root for legacy files
    let mut settings = match crate::config::read_config_file(APP_SETTINGS_FILE) {
        Some(content) => match serde_json::from_str(&content) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Failed to parse app settings: {}", e);
                AppSettings::default()
            }
        },
        None => AppSettings::default(),
    };

    // Auto-enable LMS adapter when started from LMS plugin
    // The LMS plugin sets LMS_UNIFIEDHIFI_STARTED=true when launching the bridge
    if crate::config::is_lms_plugin_started() && !settings.adapters.lms {
        tracing::info!("LMS plugin detected (LMS_UNIFIEDHIFI_STARTED), auto-enabling LMS adapter");
        settings.adapters.lms = true;
    }

    settings
}

fn save_app_settings(settings: &AppSettings) -> bool {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match serde_json::to_string_pretty(settings) {
        Ok(json) => match std::fs::write(&path, json) {
            Ok(()) => {
                tracing::info!("Saved app settings");
                true
            }
            Err(e) => {
                tracing::error!("Failed to save app settings: {}", e);
                false
            }
        },
        Err(e) => {
            tracing::error!("Failed to serialize app settings: {}", e);
            false
        }
    }
}

/// GET /api/settings - Get app settings
pub async fn api_settings_get_handler() -> impl IntoResponse {
    Json(load_app_settings())
}

/// POST /api/settings - Update app settings with dynamic adapter enable/disable
pub async fn api_settings_post_handler(
    State(state): State<AppState>,
    Json(new_settings): Json<AppSettings>,
) -> impl IntoResponse {
    // Load current settings to compare
    let old_settings = load_app_settings();

    // Save the new settings
    if !save_app_settings(&new_settings) {
        return Json(serde_json::json!({"ok": false, "error": "Failed to save settings"}));
    }

    // Compare adapter enabled states and start/stop as needed
    let old_adapters = &old_settings.adapters;
    let new_adapters = &new_settings.adapters;

    // Helper to process adapter state changes
    let adapters_list = state.startable_adapters.clone();
    let coord = state.coordinator.clone();

    // Check each adapter for state changes
    let adapter_changes: Vec<(&str, bool)> = vec![
        ("roon", old_adapters.roon != new_adapters.roon),
        ("lms", old_adapters.lms != new_adapters.lms),
        ("openhome", old_adapters.openhome != new_adapters.openhome),
        ("upnp", old_adapters.upnp != new_adapters.upnp),
        ("hqplayer", old_adapters.hqplayer != new_adapters.hqplayer),
    ];

    for (name, changed) in adapter_changes {
        if !changed {
            continue;
        }

        // Get the new enabled state
        let now_enabled = match name {
            "roon" => new_adapters.roon,
            "lms" => new_adapters.lms,
            "openhome" => new_adapters.openhome,
            "upnp" => new_adapters.upnp,
            "hqplayer" => new_adapters.hqplayer,
            _ => continue,
        };

        // Update coordinator state
        coord.set_enabled(name, now_enabled).await;

        // Find the adapter and start/stop it
        if let Some(adapter) = adapters_list.iter().find(|a| a.name() == name) {
            if now_enabled {
                tracing::info!("Dynamically enabling adapter: {}", name);
                if adapter.can_start().await {
                    if let Err(e) = adapter.start().await {
                        tracing::warn!("Failed to start adapter {}: {}", name, e);
                    }
                }
            } else {
                tracing::info!("Dynamically disabling adapter: {}", name);
                adapter.stop().await;
            }
        }
    }

    Json(serde_json::json!({"ok": true}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::env;

    #[test]
    #[serial]
    fn test_lms_auto_enabled_when_plugin_started() {
        // When started from LMS plugin, adapters.lms should be auto-enabled
        env::set_var("LMS_UNIFIEDHIFI_STARTED", "true");
        env::set_var("UHC_CONFIG_DIR", "/tmp/uhc-test-nonexistent-api");

        let settings = load_app_settings();

        env::remove_var("LMS_UNIFIEDHIFI_STARTED");
        env::remove_var("UHC_CONFIG_DIR");

        assert!(
            settings.adapters.lms,
            "adapters.lms should be true when LMS_UNIFIEDHIFI_STARTED=true"
        );
    }

    #[test]
    #[serial]
    fn test_lms_not_enabled_without_plugin_signal() {
        // Without LMS_UNIFIEDHIFI_STARTED, LMS should default to disabled
        env::remove_var("LMS_UNIFIEDHIFI_STARTED");
        env::set_var("UHC_CONFIG_DIR", "/tmp/uhc-test-nonexistent-api2");

        let settings = load_app_settings();

        env::remove_var("UHC_CONFIG_DIR");

        assert!(
            !settings.adapters.lms,
            "adapters.lms should be false without LMS_UNIFIEDHIFI_STARTED"
        );
    }
}
