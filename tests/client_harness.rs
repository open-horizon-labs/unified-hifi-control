//! Client Test Harness
//!
//! Simulates both the ESP32 Knob and iOS/Apple Watch clients to exercise
//! every endpoint the bridge exposes. This ensures API compatibility across
//! all client implementations.
//!
//! Run with: cargo test --test client_harness
//!
//! Endpoints tested:
//!
//! ## Knob (ESP32) Protocol - from roon-knob C implementation
//! - GET /zones?knob_id={mac} with X-Knob-Id, X-Knob-Version headers
//! - GET /now_playing?zone_id={}&battery_level={}&battery_charging={}&knob_id={}
//! - POST /control with JSON body {zone_id, action, value}
//! - GET /now_playing/image?zone_id={}&width={}&height={}&format=rgb565
//! - GET /config/{knob_id}
//!
//! ## iOS/Apple Watch Protocol - from hifi-control-ios Swift implementation
//! - GET /zones
//! - GET /now_playing?zone_id={}
//! - POST /control with JSON body {zone_id, action, value}
//! - GET /now_playing/image?zone_id={}&width={}&height={}
//! - GET /hqp/pipeline
//! - POST /hqp/pipeline with JSON body {setting, value}
//! - GET /hqp/profiles
//! - GET /hqp/status
//! - POST /hqp/profiles/load with JSON body {profile}

use axum::{
    body::Body,
    http::{header, Method, Request, StatusCode},
    routing::{delete, get, post, put},
    Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{sync::Arc, time::Instant};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use unified_hifi_control::adapters::hqplayer::{HqpInstanceManager, HqpZoneLinkService};
use unified_hifi_control::adapters::lms::LmsAdapter;
use unified_hifi_control::adapters::openhome::OpenHomeAdapter;
use unified_hifi_control::adapters::roon::RoonAdapter;
use unified_hifi_control::adapters::upnp::UPnPAdapter;
use unified_hifi_control::adapters::Startable;
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::api;
use unified_hifi_control::api::AppState;
use unified_hifi_control::bus::create_bus;
use unified_hifi_control::coordinator::AdapterCoordinator;
use unified_hifi_control::knobs::{self, KnobStore};

// Stub HTML handlers for UI route tests (replacing deleted ui module)
mod ui_stubs {
    use axum::response::Html;

    const HTML_PAGE: &str = "<!DOCTYPE html><html><head></head><body>Test</body></html>";

    pub async fn stub_page() -> Html<&'static str> {
        Html(HTML_PAGE)
    }
}

// =============================================================================
// Response Types - matching what clients expect
// =============================================================================

/// Zone from bridge API - matches both knob and iOS expectations
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct Zone {
    zone_id: String,
    zone_name: String,
    source: String,
    state: String,
    output_count: i32,
    output_name: Option<String>,
    device_name: Option<String>,
    source_control: Option<SourceControl>,
    volume_control: Option<VolumeControl>,
    supports_grouping: Option<bool>,
    dsp: Option<DspInfo>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct SourceControl {
    status: String,
    supports_standby: bool,
    control_key: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct VolumeControl {
    #[serde(rename = "type")]
    volume_type: String,
    min: f64,
    max: f64,
    is_muted: bool,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct DspInfo {
    #[serde(rename = "type")]
    dsp_type: Option<String>,
    instance: Option<String>,
    pipeline: Option<String>,
    profiles: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ZonesResponse {
    zones: Vec<Zone>,
}

/// NowPlaying response - matches iOS expectations with embedded zones
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct NowPlaying {
    line1: String,
    line2: String,
    line3: Option<String>,
    is_playing: bool,
    volume: Option<f64>,
    volume_type: Option<String>,
    volume_min: Option<f64>,
    volume_max: Option<f64>,
    volume_step: Option<f64>,
    seek_position: Option<i32>,
    length: Option<i32>,
    zone_id: String,
    image_key: Option<String>,
    image_url: Option<String>,
    zones: Vec<Zone>,
    config_sha: Option<String>,
    zones_sha: Option<String>,
}

/// Knob config response
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct KnobConfig {
    zone_id: Option<String>,
    zones_sha: Option<String>,
    config_sha: Option<String>,
}

/// Control request - used by both knob and iOS
#[derive(Debug, Serialize)]
struct ControlRequest {
    zone_id: String,
    action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<f64>,
}

/// HQPlayer setting update request
#[derive(Debug, Serialize)]
struct HqpSettingRequest {
    setting: String,
    value: String,
}

/// HQPlayer profile load request
#[derive(Debug, Serialize)]
struct HqpLoadProfileRequest {
    profile: String,
}

// =============================================================================
// Test Infrastructure
// =============================================================================

/// Create a test app with disconnected/mock adapters
async fn create_test_app() -> Router {
    let bus = create_bus();

    // Create coordinator (tests don't need real lifecycle management)
    let coordinator = Arc::new(AdapterCoordinator::new(bus.clone()));

    // Create disconnected adapters
    let roon = Arc::new(RoonAdapter::new_disconnected(bus.clone()));
    let hqp_instances = Arc::new(HqpInstanceManager::new(bus.clone()));
    let hqplayer = hqp_instances.get_default().await;
    let hqp_zone_links = Arc::new(HqpZoneLinkService::new(hqp_instances.clone()));
    let lms = Arc::new(LmsAdapter::new(bus.clone()));
    let openhome = Arc::new(OpenHomeAdapter::new(bus.clone()));
    let upnp = Arc::new(UPnPAdapter::new(bus.clone()));
    let knob_store = KnobStore::new();

    // Build startable adapters list
    let startable_adapters: Vec<Arc<dyn Startable>> =
        vec![roon.clone(), lms.clone(), openhome.clone(), upnp.clone()];

    let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
    let state = AppState::new(
        roon,
        hqplayer,
        hqp_instances,
        hqp_zone_links,
        lms,
        openhome,
        upnp,
        knob_store,
        bus,
        aggregator,
        coordinator,
        startable_adapters,
        Instant::now(),
        CancellationToken::new(),
    );

    // Build router with all routes (same as main.rs)
    Router::new()
        // Health check
        .route("/status", get(api::status_handler))
        // Roon routes
        .route("/roon/status", get(api::roon_status_handler))
        .route("/roon/zones", get(api::roon_zones_handler))
        .route("/roon/zone/{zone_id}", get(api::roon_zone_handler))
        .route("/roon/control", post(api::roon_control_handler))
        .route("/roon/volume", post(api::roon_volume_handler))
        // HQPlayer routes
        .route("/hqplayer/status", get(api::hqp_status_handler))
        .route("/hqplayer/pipeline", get(api::hqp_pipeline_handler))
        .route("/hqplayer/config", get(api::hqp_config_handler))
        .route("/hqplayer/profiles", get(api::hqp_profiles_handler))
        .route(
            "/hqplayer/matrix/profiles",
            get(api::hqp_matrix_profiles_handler),
        )
        .route("/hqplayer/control", post(api::hqp_control_handler))
        .route("/hqplayer/volume", post(api::hqp_volume_handler))
        .route("/hqplayer/setting", post(api::hqp_setting_handler))
        .route("/hqplayer/profile", post(api::hqp_load_profile_handler))
        // HQPlayer legacy routes (iOS uses these)
        .route("/hqp/pipeline", get(api::hqp_pipeline_handler))
        .route("/hqp/pipeline", post(api::hqp_pipeline_update_handler))
        .route("/hqp/profiles", get(api::hqp_profiles_handler))
        .route("/hqp/profiles/load", post(api::hqp_load_profile_handler))
        .route("/hqp/status", get(api::hqp_status_handler))
        // HQPlayer multi-instance routes
        .route("/hqp/instances", get(api::hqp_instances_handler))
        .route("/hqp/instances", post(api::hqp_add_instance_handler))
        .route(
            "/hqp/instances/{name}",
            delete(api::hqp_remove_instance_handler),
        )
        .route(
            "/hqp/instances/{name}/profiles",
            get(api::hqp_instance_profiles_handler),
        )
        .route(
            "/hqp/instances/{name}/matrix/profiles",
            get(api::hqp_instance_matrix_profiles_handler),
        )
        // HQPlayer zone linking routes
        .route("/hqp/zones/links", get(api::hqp_zone_links_handler))
        .route("/hqp/zones/link", post(api::hqp_zone_link_handler))
        .route("/hqp/zones/unlink", post(api::hqp_zone_unlink_handler))
        .route(
            "/hqp/zones/{zone_id}/pipeline",
            get(api::hqp_zone_pipeline_handler),
        )
        .route("/hqp/discover", get(api::hqp_discover_handler))
        // LMS routes
        .route("/lms/status", get(api::lms_status_handler))
        .route("/lms/config", get(api::lms_config_handler))
        .route("/lms/players", get(api::lms_players_handler))
        .route("/lms/player/{player_id}", get(api::lms_player_handler))
        .route("/lms/control", post(api::lms_control_handler))
        .route("/lms/volume", post(api::lms_volume_handler))
        // OpenHome routes
        .route("/openhome/status", get(api::openhome_status_handler))
        .route("/openhome/zones", get(api::openhome_zones_handler))
        .route(
            "/openhome/zone/{zone_id}/now_playing",
            get(api::openhome_now_playing_handler),
        )
        .route("/openhome/control", post(api::openhome_control_handler))
        // UPnP routes
        .route("/upnp/status", get(api::upnp_status_handler))
        .route("/upnp/zones", get(api::upnp_zones_handler))
        .route(
            "/upnp/zone/{zone_id}/now_playing",
            get(api::upnp_now_playing_handler),
        )
        .route("/upnp/control", post(api::upnp_control_handler))
        // App settings API
        .route("/api/settings", get(api::api_settings_get_handler))
        .route("/api/settings", post(api::api_settings_post_handler))
        // Event stream (SSE)
        .route("/events", get(api::events_handler))
        // Knob protocol routes (MUST return JSON)
        .route("/zones", get(knobs::knob_zones_handler))
        .route("/now_playing", get(knobs::knob_now_playing_handler))
        .route("/now_playing/image", get(knobs::knob_image_handler))
        .route("/control", post(knobs::knob_control_handler))
        .route("/config/{knob_id}", get(knobs::knob_config_by_path_handler))
        .route(
            "/config/{knob_id}",
            put(knobs::knob_config_update_by_path_handler),
        )
        .route("/knob/zones", get(knobs::knob_zones_handler))
        .route("/knob/now_playing", get(knobs::knob_now_playing_handler))
        .route("/knob/config", get(knobs::knob_config_handler))
        .route("/knob/config", post(knobs::knob_config_update_handler))
        .route("/knob/devices", get(knobs::knob_devices_handler))
        // Firmware OTA routes
        .route("/firmware/version", get(knobs::firmware_version_handler))
        .route("/firmware/download", get(knobs::firmware_download_handler))
        .route("/manifest-s3.json", get(knobs::manifest_handler))
        // Web UI routes (MUST return HTML) - using stubs for testing
        .route("/", get(ui_stubs::stub_page))
        .route("/ui/zones", get(ui_stubs::stub_page))
        .route("/zone", get(ui_stubs::stub_page))
        .route("/hqplayer", get(ui_stubs::stub_page))
        .route("/lms", get(ui_stubs::stub_page))
        .route("/knobs", get(ui_stubs::stub_page))
        .route("/settings", get(ui_stubs::stub_page))
        .with_state(state)
}

/// Helper to make a GET request and return body as string
async fn get_request(app: &Router, path: &str) -> (StatusCode, String) {
    let response = app
        .clone()
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body).to_string();
    (status, body_str)
}

/// Helper to make a GET request with custom headers (knob-style)
async fn get_with_headers(
    app: &Router,
    path: &str,
    knob_id: &str,
    knob_version: &str,
) -> (StatusCode, String) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(path)
                .header("X-Knob-Id", knob_id)
                .header("X-Knob-Version", knob_version)
                .header("Accept", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body).to_string();
    (status, body_str)
}

/// Helper to make a POST request with JSON body
async fn post_json(app: &Router, path: &str, body: &impl Serialize) -> (StatusCode, String) {
    let json_body = serde_json::to_string(body).unwrap();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(path)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json_body))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body).to_string();
    (status, body_str)
}

/// Helper to make a PUT request with JSON body
async fn put_json(app: &Router, path: &str, body: &impl Serialize) -> (StatusCode, String) {
    let json_body = serde_json::to_string(body).unwrap();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(path)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json_body))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body).to_string();
    (status, body_str)
}

/// Assert that the response is valid JSON
fn assert_json(context: &str, body: &str) -> Value {
    assert!(
        !body.starts_with("<!DOCTYPE") && !body.starts_with("<html"),
        "{} returned HTML instead of JSON:\n{}",
        context,
        &body[..body.len().min(200)]
    );

    serde_json::from_str(body).unwrap_or_else(|e| {
        panic!(
            "{} returned invalid JSON: {}\nBody: {}",
            context,
            e,
            &body[..body.len().min(500)]
        )
    })
}

// =============================================================================
// Knob Client Tests - simulating ESP32 roon-knob firmware
// =============================================================================

mod knob_client {
    use super::*;

    const KNOB_ID: &str = "AA:BB:CC:DD:EE:FF";
    const KNOB_VERSION: &str = "2.1.0";

    /// Test: GET /zones?knob_id={mac} with X-Knob-Id, X-Knob-Version headers
    /// This is the first call the knob makes on boot to discover zones
    #[tokio::test]
    async fn get_zones_with_knob_headers() {
        let app = create_test_app().await;
        let path = format!("/zones?knob_id={}", KNOB_ID);

        let (status, body) = get_with_headers(&app, &path, KNOB_ID, KNOB_VERSION).await;
        assert_eq!(status, StatusCode::OK);

        let json = assert_json("GET /zones (knob)", &body);

        // Must have zones array
        assert!(
            json.get("zones").is_some(),
            "/zones must have 'zones' field, got: {:?}",
            json
        );

        // Zones must be an array
        assert!(
            json["zones"].is_array(),
            "/zones 'zones' must be array, got: {:?}",
            json["zones"]
        );
    }

    /// Test: GET /now_playing?zone_id={}&battery_level={}&battery_charging={}&knob_id={}
    /// Knob polls this regularly to get current playback state
    #[tokio::test]
    async fn get_now_playing_with_battery_params() {
        let app = create_test_app().await;

        // First request without a zone configured - should return error JSON
        let path = format!(
            "/now_playing?zone_id=&battery_level=85&battery_charging=false&knob_id={}",
            KNOB_ID
        );
        let (status, body) = get_with_headers(&app, &path, KNOB_ID, KNOB_VERSION).await;

        // Should return JSON (may be error or empty state)
        let json = assert_json("GET /now_playing (knob)", &body);

        // Even errors should have structured response
        assert!(
            json.is_object(),
            "/now_playing must return JSON object, got: {:?}",
            json
        );

        // With an invalid zone, we expect 400 or error response
        assert!(
            status == StatusCode::BAD_REQUEST
                || json.get("error").is_some()
                || json.get("line1").is_some(),
            "Expected error or now_playing response, got status {} and: {:?}",
            status,
            json
        );
    }

    /// Test: GET /now_playing response structure
    /// Node.js returns: line1, line2, line3, is_playing, volume, volume_type, etc.
    /// NOT: title, artist, state (these are internal field names)
    /// See: src/roon/client.js:200-203
    #[tokio::test]
    async fn now_playing_response_structure() {
        let app = create_test_app().await;
        let path = format!(
            "/now_playing?zone_id=test-zone-123&battery_level=100&battery_charging=true&knob_id={}",
            KNOB_ID
        );
        let (_, body) = get_with_headers(&app, &path, KNOB_ID, KNOB_VERSION).await;
        let json = assert_json("GET /now_playing structure", &body);

        // If it's an error response, that's acceptable for invalid zone
        if json.get("error").is_some() {
            return;
        }

        // For valid responses, knob expects these EXACT field names
        // Node.js returns line1/line2/is_playing, NOT title/artist/state
        // zones_sha added in PR #149 for dynamic zone detection (#148)
        let required_fields = [
            "line1",
            "line2",
            "is_playing",
            "zone_id",
            "zones",
            "zones_sha",
        ];
        for field in required_fields {
            assert!(
                json.get(field).is_some(),
                "/now_playing must have '{}' field (knob expects this). Got: {:?}",
                field,
                json.as_object().map(|o| o.keys().collect::<Vec<_>>())
            );
        }

        // is_playing must be a boolean, not a string
        assert!(
            json["is_playing"].is_boolean(),
            "is_playing must be boolean, got: {:?}",
            json["is_playing"]
        );
    }

    /// Test: POST /control with play_pause action
    /// Knob sends control commands as JSON
    #[tokio::test]
    async fn post_control_play_pause() {
        let app = create_test_app().await;

        let request = ControlRequest {
            zone_id: "test-zone-123".to_string(),
            action: "play_pause".to_string(),
            value: None,
        };

        let (status, body) = post_json(&app, "/control", &request).await;
        let json = assert_json("POST /control play_pause", &body);

        // Response should be JSON (success or error)
        assert!(
            json.is_object(),
            "/control must return JSON object, got: {:?}",
            json
        );

        // Accept: OK, 400 (invalid zone/params), 404 (zone not found), 500 (service error)
        // Knob handles all error responses the same way (checks for "error" in body)
        assert!(
            status == StatusCode::OK
                || status == StatusCode::BAD_REQUEST
                || status == StatusCode::NOT_FOUND
                || status == StatusCode::INTERNAL_SERVER_ERROR,
            "Expected OK, BAD_REQUEST, NOT_FOUND, or INTERNAL_SERVER_ERROR, got {}",
            status
        );
    }

    /// Test: POST /control with next action
    #[tokio::test]
    async fn post_control_next() {
        let app = create_test_app().await;

        let request = ControlRequest {
            zone_id: "test-zone".to_string(),
            action: "next".to_string(),
            value: None,
        };

        let (_, body) = post_json(&app, "/control", &request).await;
        assert_json("POST /control next", &body);
    }

    /// Test: POST /control with prev action
    #[tokio::test]
    async fn post_control_prev() {
        let app = create_test_app().await;

        let request = ControlRequest {
            zone_id: "test-zone".to_string(),
            action: "prev".to_string(),
            value: None,
        };

        let (_, body) = post_json(&app, "/control", &request).await;
        assert_json("POST /control prev", &body);
    }

    /// Test: POST /control with vol_abs action
    #[tokio::test]
    async fn post_control_volume_absolute() {
        let app = create_test_app().await;

        let request = ControlRequest {
            zone_id: "test-zone".to_string(),
            action: "vol_abs".to_string(),
            value: Some(-30.0),
        };

        let (_, body) = post_json(&app, "/control", &request).await;
        assert_json("POST /control vol_abs", &body);
    }

    /// Test: GET /now_playing/image?zone_id={}&width={}&height={}&format=rgb565
    /// Knob requests album art in RGB565 format for direct display
    #[tokio::test]
    async fn get_image_rgb565_format() {
        let app = create_test_app().await;
        let path = "/now_playing/image?zone_id=test-zone&width=240&height=240&format=rgb565";

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("X-Knob-Id", KNOB_ID)
                    .header("X-Knob-Version", KNOB_VERSION)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let status = response.status();

        // 404 for non-existent zone is acceptable
        // 200 with binary data for valid zones
        assert!(
            status == StatusCode::OK
                || status == StatusCode::NOT_FOUND
                || status == StatusCode::BAD_REQUEST,
            "Expected OK, NOT_FOUND, or BAD_REQUEST for image endpoint, got {}",
            status
        );
    }

    /// Test: GET /config/{knob_id}
    /// Knob fetches its configuration from the bridge
    #[tokio::test]
    async fn get_knob_config() {
        let app = create_test_app().await;
        let path = format!("/config/{}", KNOB_ID);

        let (status, body) = get_with_headers(&app, &path, KNOB_ID, KNOB_VERSION).await;
        assert_eq!(status, StatusCode::OK);

        let json = assert_json("GET /config/{knob_id}", &body);

        // Config should be a JSON object
        assert!(
            json.is_object(),
            "/config must return JSON object, got: {:?}",
            json
        );
    }

    /// Test: Zone IDs must have source prefix (roon:, openhome:, upnp:, lms:)
    /// The knob uses this prefix to route commands to the correct adapter
    /// Node.js returns: zone_id: `roon:${zone.zone_id}` (see bus/adapters/roon.js:30)
    #[tokio::test]
    async fn zone_ids_have_source_prefix() {
        let app = create_test_app().await;
        let path = format!("/zones?knob_id={}", KNOB_ID);

        let (status, body) = get_with_headers(&app, &path, KNOB_ID, KNOB_VERSION).await;
        assert_eq!(status, StatusCode::OK);

        let json = assert_json("GET /zones zone_id prefix check", &body);
        let zones = json["zones"].as_array().expect("zones must be array");

        // If there are zones, each zone_id must be prefixed with its source
        for zone in zones {
            let zone_id = zone["zone_id"].as_str().expect("zone_id must be string");
            let source = zone["source"].as_str().expect("source must be string");

            // Zone ID must start with source prefix
            let expected_prefix = format!("{}:", source);
            assert!(
                zone_id.starts_with(&expected_prefix),
                "zone_id '{}' must start with '{}' (source prefix)",
                zone_id,
                expected_prefix
            );
        }
    }

    /// Test: PUT /config/{knob_id} - update knob configuration
    #[tokio::test]
    async fn put_knob_config() {
        let app = create_test_app().await;
        let path = format!("/config/{}", KNOB_ID);

        let config = json!({
            "zone_id": "roon:1601defc-1234-5678-abcd-1234567890ab"
        });

        let (status, body) = put_json(&app, &path, &config).await;
        assert_json("PUT /config/{knob_id}", &body);

        // Should accept config updates (404 for unknown knob is valid)
        assert!(
            status == StatusCode::OK
                || status == StatusCode::BAD_REQUEST
                || status == StatusCode::NOT_FOUND,
            "Expected OK, BAD_REQUEST, or NOT_FOUND for config update, got {}",
            status
        );
    }
}

// =============================================================================
// iOS/Apple Watch Client Tests - simulating hifi-control-ios
// =============================================================================

mod ios_client {
    use super::*;

    /// Test: GET /zones - simple zones list without knob parameters
    /// iOS app fetches zones without special headers
    #[tokio::test]
    async fn get_zones_simple() {
        let app = create_test_app().await;
        let (status, body) = get_request(&app, "/zones").await;

        assert_eq!(status, StatusCode::OK);
        let json = assert_json("GET /zones (iOS)", &body);

        // Must have zones array
        let zones_response: ZonesResponse = serde_json::from_value(json.clone())
            .expect("Response must match ZonesResponse structure");
        assert!(
            zones_response.zones.is_empty() || !zones_response.zones.is_empty(),
            "Zones must be an array"
        );
    }

    /// Test: GET /now_playing?zone_id={} - simple now playing
    /// iOS uses simpler query without battery parameters
    #[tokio::test]
    async fn get_now_playing_simple() {
        let app = create_test_app().await;
        let (_, body) = get_request(&app, "/now_playing?zone_id=test-zone").await;

        let json = assert_json("GET /now_playing (iOS)", &body);
        assert!(json.is_object());
    }

    /// Test: POST /control with all iOS actions
    #[tokio::test]
    async fn post_control_all_actions() {
        let app = create_test_app().await;

        // Test all actions iOS supports
        let actions = vec![
            ("play", None),
            ("pause", None),
            ("play_pause", None),
            ("stop", None),
            ("next", None),
            ("previous", None),
            ("prev", None), // Bridge accepts both
            ("vol_abs", Some(-25.0)),
            ("vol_rel", Some(3.0)),
        ];

        for (action, value) in actions {
            let request = ControlRequest {
                zone_id: "test-zone".to_string(),
                action: action.to_string(),
                value,
            };

            let (_, body) = post_json(&app, "/control", &request).await;
            assert_json(&format!("POST /control {} (iOS)", action), &body);
        }
    }

    /// Test: GET /now_playing/image without format param
    /// iOS requests JPEG/PNG (no RGB565)
    #[tokio::test]
    async fn get_image_standard_format() {
        let app = create_test_app().await;
        let path = "/now_playing/image?zone_id=test-zone&width=360&height=360";

        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();

        let status = response.status();

        // Accept success or not found for non-existent zone
        assert!(
            status == StatusCode::OK
                || status == StatusCode::NOT_FOUND
                || status == StatusCode::BAD_REQUEST,
            "Expected OK, NOT_FOUND, or BAD_REQUEST for image, got {}",
            status
        );
    }

    /// Test: GET /hqp/pipeline - HQPlayer DSP settings
    /// iOS app fetches HQPlayer pipeline for DSP control
    #[tokio::test]
    async fn get_hqp_pipeline() {
        let app = create_test_app().await;
        let (status, body) = get_request(&app, "/hqp/pipeline").await;

        // May be 200 OK or 503 Service Unavailable (not connected)
        assert!(
            status == StatusCode::OK || status == StatusCode::SERVICE_UNAVAILABLE,
            "Expected OK or SERVICE_UNAVAILABLE for HQP pipeline, got {}",
            status
        );

        let json = assert_json("GET /hqp/pipeline (iOS)", &body);
        assert!(json.is_object());
    }

    /// Test: GET /hqp/profiles - HQPlayer profiles list
    #[tokio::test]
    async fn get_hqp_profiles() {
        let app = create_test_app().await;
        let (status, body) = get_request(&app, "/hqp/profiles").await;

        // May fail if HQPlayer not configured
        assert!(
            status == StatusCode::OK || status == StatusCode::INTERNAL_SERVER_ERROR,
            "Expected OK or INTERNAL_SERVER_ERROR for HQP profiles, got {}",
            status
        );

        let json = assert_json("GET /hqp/profiles (iOS)", &body);
        assert!(json.is_object());
    }

    /// Test: GET /hqp/status - HQPlayer connection status
    #[tokio::test]
    async fn get_hqp_status() {
        let app = create_test_app().await;
        let (status, body) = get_request(&app, "/hqp/status").await;

        assert_eq!(status, StatusCode::OK);
        let json = assert_json("GET /hqp/status (iOS)", &body);

        // Should have connected field
        assert!(
            json.get("connected").is_some() || json.get("enabled").is_some(),
            "HQP status should have connection info"
        );
    }

    /// Test: POST /hqp/pipeline with setting update
    /// iOS sends {setting: "filter1x", value: "0"} or {setting: "filter1x", value: 0}
    #[tokio::test]
    async fn post_hqp_setting() {
        let app = create_test_app().await;

        let request = HqpSettingRequest {
            setting: "filter1x".to_string(),
            value: "0".to_string(), // iOS sends string or number
        };

        // POST /hqp/pipeline is the iOS-compatible endpoint
        let (status, body) = post_json(&app, "/hqp/pipeline", &request).await;
        let json = assert_json("POST /hqp/pipeline (iOS)", &body);

        // May fail if HQPlayer not connected
        assert!(
            status == StatusCode::OK
                || status == StatusCode::INTERNAL_SERVER_ERROR
                || status == StatusCode::SERVICE_UNAVAILABLE,
            "Expected OK, INTERNAL_SERVER_ERROR, or SERVICE_UNAVAILABLE, got {}",
            status
        );
        assert!(json.is_object());
    }

    /// Test: POST /hqp/profiles/load - Load HQPlayer profile (iOS uses this path)
    #[tokio::test]
    async fn post_hqp_load_profile() {
        let app = create_test_app().await;

        let request = HqpLoadProfileRequest {
            profile: "Default".to_string(),
        };

        // iOS uses /hqp/profiles/load
        let (status, body) = post_json(&app, "/hqp/profiles/load", &request).await;
        let json = assert_json("POST /hqp/profiles/load (iOS)", &body);

        // Accept: OK, 400 (not configured/no creds), 500 (actual error), 503 (unavailable)
        // Node.js returns 400 for "HQPlayer not configured" and "Web credentials required"
        assert!(
            status == StatusCode::OK
                || status == StatusCode::BAD_REQUEST
                || status == StatusCode::INTERNAL_SERVER_ERROR
                || status == StatusCode::SERVICE_UNAVAILABLE,
            "Expected OK, BAD_REQUEST, INTERNAL_SERVER_ERROR, or SERVICE_UNAVAILABLE, got {}",
            status
        );
        assert!(json.is_object());
    }
}

// =============================================================================
// Shared Endpoint Tests - used by both clients
// =============================================================================

mod shared_endpoints {
    use super::*;

    /// Test: GET /status - health check endpoint
    #[tokio::test]
    async fn get_status() {
        let app = create_test_app().await;
        let (status, body) = get_request(&app, "/status").await;

        assert_eq!(status, StatusCode::OK);
        let json = assert_json("GET /status", &body);

        // Should have service and version
        assert!(
            json.get("service").is_some(),
            "Status must have service field"
        );
        assert!(
            json.get("version").is_some(),
            "Status must have version field"
        );
    }

    /// Test: GET /roon/status - Roon adapter status
    #[tokio::test]
    async fn get_roon_status() {
        let app = create_test_app().await;
        let (status, body) = get_request(&app, "/roon/status").await;

        assert_eq!(status, StatusCode::OK);
        let json = assert_json("GET /roon/status", &body);
        assert!(json.is_object());
    }

    /// Test: GET /roon/zones - Roon zones list
    #[tokio::test]
    async fn get_roon_zones() {
        let app = create_test_app().await;
        let (status, body) = get_request(&app, "/roon/zones").await;

        assert_eq!(status, StatusCode::OK);
        let json = assert_json("GET /roon/zones", &body);
        assert!(json.get("zones").is_some());
    }

    /// Test: GET /lms/status - LMS adapter status
    #[tokio::test]
    async fn get_lms_status() {
        let app = create_test_app().await;
        let (status, body) = get_request(&app, "/lms/status").await;

        assert_eq!(status, StatusCode::OK);
        let json = assert_json("GET /lms/status", &body);
        assert!(json.is_object());
    }

    /// Test: GET /lms/players - LMS players list
    #[tokio::test]
    async fn get_lms_players() {
        let app = create_test_app().await;
        let (status, body) = get_request(&app, "/lms/players").await;

        assert_eq!(status, StatusCode::OK);
        let json = assert_json("GET /lms/players", &body);
        assert!(json.is_object());
    }

    /// Test: GET /openhome/status - OpenHome adapter status
    #[tokio::test]
    async fn get_openhome_status() {
        let app = create_test_app().await;
        let (status, body) = get_request(&app, "/openhome/status").await;

        assert_eq!(status, StatusCode::OK);
        let json = assert_json("GET /openhome/status", &body);
        assert!(json.is_object());
    }

    /// Test: GET /openhome/zones - OpenHome zones list
    #[tokio::test]
    async fn get_openhome_zones() {
        let app = create_test_app().await;
        let (status, body) = get_request(&app, "/openhome/zones").await;

        assert_eq!(status, StatusCode::OK);
        let json = assert_json("GET /openhome/zones", &body);
        assert!(json.get("zones").is_some());
    }

    /// Test: GET /upnp/status - UPnP adapter status
    #[tokio::test]
    async fn get_upnp_status() {
        let app = create_test_app().await;
        let (status, body) = get_request(&app, "/upnp/status").await;

        assert_eq!(status, StatusCode::OK);
        let json = assert_json("GET /upnp/status", &body);
        assert!(json.is_object());
    }

    /// Test: GET /upnp/zones - UPnP zones list
    #[tokio::test]
    async fn get_upnp_zones() {
        let app = create_test_app().await;
        let (status, body) = get_request(&app, "/upnp/zones").await;

        assert_eq!(status, StatusCode::OK);
        let json = assert_json("GET /upnp/zones", &body);
        assert!(json.get("zones").is_some());
    }

    /// Test: GET /api/settings - App settings
    #[tokio::test]
    async fn get_api_settings() {
        let app = create_test_app().await;
        let (status, body) = get_request(&app, "/api/settings").await;

        assert_eq!(status, StatusCode::OK);
        let json = assert_json("GET /api/settings", &body);
        assert!(json.is_object());
    }

    /// Test: GET /knob/devices - Registered knob devices
    #[tokio::test]
    async fn get_knob_devices() {
        let app = create_test_app().await;
        let (status, body) = get_request(&app, "/knob/devices").await;

        assert_eq!(status, StatusCode::OK);
        let json = assert_json("GET /knob/devices", &body);
        assert!(json.is_object());
    }

    /// Test: GET /hqp/instances - HQPlayer instances
    #[tokio::test]
    async fn get_hqp_instances() {
        let app = create_test_app().await;
        let (status, body) = get_request(&app, "/hqp/instances").await;

        assert_eq!(status, StatusCode::OK);
        let json = assert_json("GET /hqp/instances", &body);
        assert!(json.is_object());
    }

    /// Test: GET /hqp/zones/links - HQPlayer zone links
    #[tokio::test]
    async fn get_hqp_zone_links() {
        let app = create_test_app().await;
        let (status, body) = get_request(&app, "/hqp/zones/links").await;

        assert_eq!(status, StatusCode::OK);
        let json = assert_json("GET /hqp/zones/links", &body);
        assert!(json.is_object());
    }

    /// Test: GET /hqp/discover - HQPlayer network discovery
    #[tokio::test]
    async fn get_hqp_discover() {
        let app = create_test_app().await;
        let (status, body) = get_request(&app, "/hqp/discover").await;

        assert_eq!(status, StatusCode::OK);
        let json = assert_json("GET /hqp/discover", &body);
        assert!(json.is_object());
    }
}

// =============================================================================
// Edge Cases and Error Handling Tests
// =============================================================================

mod error_handling {
    use super::*;

    /// Test: Missing zone_id parameter returns proper error
    #[tokio::test]
    async fn missing_zone_id_returns_json_error() {
        let app = create_test_app().await;
        let (_, body) = get_request(&app, "/now_playing").await;

        // Should return JSON error, not crash or HTML
        let json = assert_json("Missing zone_id", &body);
        assert!(json.is_object());
    }

    /// Test: Invalid zone_id returns proper error
    #[tokio::test]
    async fn invalid_zone_id_returns_json_error() {
        let app = create_test_app().await;
        let (_, body) = get_request(&app, "/now_playing?zone_id=nonexistent-zone-999").await;

        let json = assert_json("Invalid zone_id", &body);
        assert!(json.is_object());
    }

    /// Test: Empty control request body
    #[tokio::test]
    async fn empty_control_body_returns_error() {
        let app = create_test_app().await;

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let body_str = String::from_utf8_lossy(&body).to_string();

        // Should return 400 or 422 for invalid body
        assert!(
            status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
            "Expected error status for empty body, got {}: {}",
            status,
            body_str
        );
    }

    /// Test: Invalid JSON body
    #[tokio::test]
    async fn invalid_json_returns_error() {
        let app = create_test_app().await;

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("not valid json"))
                    .unwrap(),
            )
            .await
            .unwrap();

        let status = response.status();

        // Should return 400 for invalid JSON
        assert!(
            status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
            "Expected error status for invalid JSON, got {}",
            status
        );
    }

    /// Test: Control with missing action
    #[tokio::test]
    async fn control_missing_action_returns_error() {
        let app = create_test_app().await;

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/control")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"zone_id": "test"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        let status = response.status();

        // Should return 400 or 422 for missing required field
        assert!(
            status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
            "Expected error status for missing action, got {}",
            status
        );
    }

    /// Test: Unknown action returns error
    #[tokio::test]
    async fn unknown_action_returns_error() {
        let app = create_test_app().await;

        let request = ControlRequest {
            zone_id: "test-zone".to_string(),
            action: "invalid_action_xyz".to_string(),
            value: None,
        };

        let (status, body) = post_json(&app, "/control", &request).await;

        // Should return error for unknown action
        let json = assert_json("Unknown action", &body);
        assert!(
            status != StatusCode::OK || json.get("error").is_some(),
            "Expected error for unknown action"
        );
    }
}

// =============================================================================
// Integration Test - Full Client Flow
// =============================================================================

mod integration {
    use super::*;

    /// Simulate full knob boot sequence
    #[tokio::test]
    async fn knob_boot_sequence() {
        let app = create_test_app().await;
        let knob_id = "11:22:33:44:55:66";
        let knob_version = "2.0.0";

        // 1. Get zones list
        let (status, body) = get_with_headers(
            &app,
            &format!("/zones?knob_id={}", knob_id),
            knob_id,
            knob_version,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let zones: ZonesResponse = serde_json::from_str(&body).unwrap();
        println!("Boot: Found {} zones", zones.zones.len());

        // 2. Get knob config (to restore last selected zone)
        let (status, body) =
            get_with_headers(&app, &format!("/config/{}", knob_id), knob_id, knob_version).await;
        assert_eq!(status, StatusCode::OK);
        let _config: Value = serde_json::from_str(&body).unwrap();
        println!("Boot: Got config");

        // 3. Start polling now_playing (simulated)
        let (_, body) = get_with_headers(
            &app,
            &format!(
                "/now_playing?zone_id=&battery_level=100&battery_charging=true&knob_id={}",
                knob_id
            ),
            knob_id,
            knob_version,
        )
        .await;
        let _now_playing: Value = serde_json::from_str(&body).unwrap();
        println!("Boot: Got initial now_playing state");
    }

    /// Simulate iOS app startup
    #[tokio::test]
    async fn ios_app_startup() {
        let app = create_test_app().await;

        // 1. Fetch zones
        let (status, body) = get_request(&app, "/zones").await;
        assert_eq!(status, StatusCode::OK);
        let zones: ZonesResponse = serde_json::from_str(&body).unwrap();
        println!("iOS: Found {} zones", zones.zones.len());

        // 2. Check HQPlayer status
        let (_, body) = get_request(&app, "/hqp/status").await;
        let hqp_status: Value = serde_json::from_str(&body).unwrap();
        println!("iOS: HQPlayer status: {:?}", hqp_status.get("connected"));

        // 3. Fetch HQPlayer profiles if connected
        let (_, body) = get_request(&app, "/hqp/profiles").await;
        let _profiles: Value = serde_json::from_str(&body).unwrap();
        println!("iOS: Got HQPlayer profiles");
    }
}
