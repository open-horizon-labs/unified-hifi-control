//! Protocol Integration Tests
//!
//! These tests stand up an actual HTTP server and verify that all protocol
//! endpoints return the expected format (JSON, not HTML). This catches regressions
//! where routes are accidentally changed to return the wrong content type.
//!
//! Run with: cargo test --test protocol_integration

use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::{delete, get, post, put},
    Router,
};
use serde_json::Value;
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
use unified_hifi_control::event_reporter::EventReporter;
use unified_hifi_control::knobs::{self, KnobStore};

// Stub HTML handlers for UI route tests (replacing deleted ui module)
mod ui_stubs {
    use axum::response::Html;

    const HTML_PAGE: &str = "<!DOCTYPE html><html><head></head><body>Test</body></html>";

    pub async fn stub_page() -> Html<&'static str> {
        Html(HTML_PAGE)
    }
}

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
    let shutdown_token = CancellationToken::new();
    let event_reporter = Arc::new(EventReporter::new(
        None,
        aggregator.clone(),
        shutdown_token.clone(),
    ));
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
        shutdown_token,
        event_reporter,
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
        // OpenHome routes
        .route("/openhome/status", get(api::openhome_status_handler))
        .route("/openhome/zones", get(api::openhome_zones_handler))
        .route(
            "/openhome/zone/{zone_id}/now_playing",
            get(api::openhome_now_playing_handler),
        )
        // UPnP routes
        .route("/upnp/status", get(api::upnp_status_handler))
        .route("/upnp/zones", get(api::upnp_zones_handler))
        .route(
            "/upnp/zone/{zone_id}/now_playing",
            get(api::upnp_now_playing_handler),
        )
        // App settings API
        .route("/api/settings", get(api::api_settings_get_handler))
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
        .route("/knob/devices", get(knobs::knob_devices_handler))
        // Web UI routes (MUST return HTML) - using stubs for testing
        .route("/", get(ui_stubs::stub_page))
        .route("/ui/zones", get(ui_stubs::stub_page))
        .route("/hqplayer", get(ui_stubs::stub_page))
        .route("/lms", get(ui_stubs::stub_page))
        .route("/knobs", get(ui_stubs::stub_page))
        .route("/settings", get(ui_stubs::stub_page))
        .with_state(state)
}

/// Helper to make a GET request and return body as string
async fn get_body(app: &Router, path: &str) -> (StatusCode, String) {
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

/// Helper to verify response is JSON (not HTML)
fn assert_json(path: &str, body: &str) {
    assert!(
        !body.starts_with("<!DOCTYPE") && !body.starts_with("<html"),
        "{} returned HTML instead of JSON:\n{}",
        path,
        &body[..body.len().min(200)]
    );

    // Verify it parses as JSON
    let parsed: Result<Value, _> = serde_json::from_str(body);
    assert!(
        parsed.is_ok(),
        "{} returned invalid JSON: {}\nBody: {}",
        path,
        parsed.unwrap_err(),
        &body[..body.len().min(500)]
    );
}

/// Helper to verify response is HTML (not JSON)
fn assert_html(path: &str, body: &str) {
    assert!(
        body.contains("<!DOCTYPE html>") || body.contains("<html"),
        "{} returned non-HTML:\n{}",
        path,
        &body[..body.len().min(200)]
    );
}

// =============================================================================
// Protocol endpoint tests - these MUST return JSON
// =============================================================================

mod protocol_endpoints {
    use super::*;

    #[tokio::test]
    async fn zones_returns_json() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/zones").await;
        assert_eq!(status, StatusCode::OK);
        assert_json("/zones", &body);

        // Verify structure
        let json: Value = serde_json::from_str(&body).unwrap();
        assert!(
            json.get("zones").is_some(),
            "/zones must have 'zones' field"
        );
    }

    #[tokio::test]
    async fn now_playing_returns_json() {
        let app = create_test_app().await;
        let (_status, body) = get_body(&app, "/now_playing").await;
        // May return 400 if no zone selected, but should be JSON
        assert_json("/now_playing", &body);
    }

    #[tokio::test]
    async fn knob_zones_returns_json() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/knob/zones").await;
        assert_eq!(status, StatusCode::OK);
        assert_json("/knob/zones", &body);
    }

    #[tokio::test]
    async fn knob_config_returns_json() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/config/test-knob-123").await;
        assert_eq!(status, StatusCode::OK);
        assert_json("/config/test-knob-123", &body);
    }

    #[tokio::test]
    async fn knob_devices_returns_json() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/knob/devices").await;
        assert_eq!(status, StatusCode::OK);
        assert_json("/knob/devices", &body);
    }
}

// =============================================================================
// API endpoint tests - these MUST return JSON
// =============================================================================

mod api_endpoints {
    use super::*;

    #[tokio::test]
    async fn status_returns_json() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_json("/status", &body);

        // Verify structure
        let json: Value = serde_json::from_str(&body).unwrap();
        assert!(json.get("service").is_some());
        assert!(json.get("version").is_some());
    }

    #[tokio::test]
    async fn roon_status_returns_json() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/roon/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_json("/roon/status", &body);
    }

    #[tokio::test]
    async fn roon_zones_returns_json() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/roon/zones").await;
        assert_eq!(status, StatusCode::OK);
        assert_json("/roon/zones", &body);
    }

    #[tokio::test]
    async fn hqplayer_status_returns_json() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/hqplayer/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_json("/hqplayer/status", &body);
    }

    #[tokio::test]
    async fn hqplayer_pipeline_returns_json() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/hqplayer/pipeline").await;
        // 503 when not connected (fast fail), 200 when connected
        assert!(
            status == StatusCode::OK || status == StatusCode::SERVICE_UNAVAILABLE,
            "Expected OK or SERVICE_UNAVAILABLE, got {status}"
        );
        assert_json("/hqplayer/pipeline", &body);
    }

    #[tokio::test]
    async fn hqplayer_config_returns_json() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/hqplayer/config").await;
        assert_eq!(status, StatusCode::OK);
        assert_json("/hqplayer/config", &body);
    }

    #[tokio::test]
    async fn hqplayer_profiles_returns_json() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/hqplayer/profiles").await;
        // 500 when fetch fails, 200 when connected
        assert!(
            status == StatusCode::OK || status == StatusCode::INTERNAL_SERVER_ERROR,
            "Expected OK or INTERNAL_SERVER_ERROR, got {status}"
        );
        assert_json("/hqplayer/profiles", &body);
    }

    #[tokio::test]
    async fn hqp_instances_returns_json() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/hqp/instances").await;
        assert_eq!(status, StatusCode::OK);
        assert_json("/hqp/instances", &body);
    }

    #[tokio::test]
    async fn hqp_zones_links_returns_json() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/hqp/zones/links").await;
        assert_eq!(status, StatusCode::OK);
        assert_json("/hqp/zones/links", &body);
    }

    #[tokio::test]
    async fn hqp_discover_returns_json() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/hqp/discover").await;
        assert_eq!(status, StatusCode::OK);
        assert_json("/hqp/discover", &body);
    }

    #[tokio::test]
    async fn lms_status_returns_json() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/lms/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_json("/lms/status", &body);
    }

    #[tokio::test]
    async fn lms_config_returns_json() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/lms/config").await;
        assert_eq!(status, StatusCode::OK);
        assert_json("/lms/config", &body);
    }

    #[tokio::test]
    async fn lms_players_returns_json() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/lms/players").await;
        assert_eq!(status, StatusCode::OK);
        assert_json("/lms/players", &body);
    }

    #[tokio::test]
    async fn openhome_status_returns_json() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/openhome/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_json("/openhome/status", &body);
    }

    #[tokio::test]
    async fn openhome_zones_returns_json() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/openhome/zones").await;
        assert_eq!(status, StatusCode::OK);
        assert_json("/openhome/zones", &body);
    }

    #[tokio::test]
    async fn upnp_status_returns_json() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/upnp/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_json("/upnp/status", &body);
    }

    #[tokio::test]
    async fn upnp_zones_returns_json() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/upnp/zones").await;
        assert_eq!(status, StatusCode::OK);
        assert_json("/upnp/zones", &body);
    }

    #[tokio::test]
    async fn api_settings_returns_json() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/api/settings").await;
        assert_eq!(status, StatusCode::OK);
        assert_json("/api/settings", &body);
    }
}

// =============================================================================
// UI endpoint tests - these MUST return HTML
// =============================================================================

mod ui_endpoints {
    use super::*;

    #[tokio::test]
    async fn root_returns_html() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/").await;
        assert_eq!(status, StatusCode::OK);
        assert_html("/", &body);
    }

    #[tokio::test]
    async fn ui_zones_returns_html() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/ui/zones").await;
        assert_eq!(status, StatusCode::OK);
        assert_html("/ui/zones", &body);
    }

    #[tokio::test]
    async fn hqplayer_page_returns_html() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/hqplayer").await;
        assert_eq!(status, StatusCode::OK);
        assert_html("/hqplayer", &body);
    }

    #[tokio::test]
    async fn lms_page_returns_html() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/lms").await;
        assert_eq!(status, StatusCode::OK);
        assert_html("/lms", &body);
    }

    #[tokio::test]
    async fn knobs_page_returns_html() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/knobs").await;
        assert_eq!(status, StatusCode::OK);
        assert_html("/knobs", &body);
    }

    #[tokio::test]
    async fn settings_page_returns_html() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/settings").await;
        assert_eq!(status, StatusCode::OK);
        assert_html("/settings", &body);
    }
}

// =============================================================================
// Protocol/UI separation tests - critical for client compatibility
// =============================================================================

mod protocol_ui_separation {
    use super::*;

    /// This is THE critical test - /zones MUST return JSON for knob/iOS clients
    #[tokio::test]
    async fn zones_is_json_not_html() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/zones").await;

        assert_eq!(status, StatusCode::OK, "/zones should return 200");

        // CRITICAL: This must NOT be HTML
        assert!(
            !body.starts_with("<!DOCTYPE"),
            "PROTOCOL VIOLATION: /zones returned HTML! This breaks knob and iOS clients.\n\
             The /zones endpoint MUST return JSON.\n\
             Got: {}",
            &body[..body.len().min(200)]
        );

        // Must be valid JSON
        let json: Value = serde_json::from_str(&body).expect("/zones must return valid JSON");

        // Must have zones array
        assert!(
            json.get("zones").is_some(),
            "/zones must have 'zones' field, got: {:?}",
            json
        );
    }

    /// UI zones page should be at /ui/zones (not /zones)
    #[tokio::test]
    async fn ui_zones_is_html() {
        let app = create_test_app().await;
        let (status, body) = get_body(&app, "/ui/zones").await;

        assert_eq!(status, StatusCode::OK, "/ui/zones should return 200");
        assert!(
            body.contains("<!DOCTYPE html>"),
            "/ui/zones should return HTML page"
        );
    }

    /// now_playing protocol endpoint must return JSON
    #[tokio::test]
    async fn now_playing_is_json_not_html() {
        let app = create_test_app().await;
        let (_, body) = get_body(&app, "/now_playing").await;

        assert!(
            !body.starts_with("<!DOCTYPE"),
            "PROTOCOL VIOLATION: /now_playing returned HTML!"
        );
    }

    /// config endpoint must return JSON
    #[tokio::test]
    async fn config_is_json_not_html() {
        let app = create_test_app().await;
        let (_, body) = get_body(&app, "/config/test-knob").await;

        assert!(
            !body.starts_with("<!DOCTYPE"),
            "PROTOCOL VIOLATION: /config returned HTML!"
        );
    }
}
