//! Integration test for zones_sha in /knob/now_playing responses
//!
//! This test verifies that the bridge emits zones_sha for dynamic zone detection.
//! Issue #148: zones appearing after client starts should be detected.

mod mock_servers;

use serial_test::serial;

use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::{get, post},
    Router,
};
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use mock_servers::lms::MockLmsServer;
use unified_hifi_control::adapters::hqplayer::{HqpInstanceManager, HqpZoneLinkService};
use unified_hifi_control::adapters::lms::LmsAdapter;
use unified_hifi_control::adapters::openhome::OpenHomeAdapter;
use unified_hifi_control::adapters::roon::RoonAdapter;
use unified_hifi_control::adapters::upnp::UPnPAdapter;
use unified_hifi_control::adapters::Startable;
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::api::AppState;
use unified_hifi_control::bus::create_bus;
use unified_hifi_control::coordinator::AdapterCoordinator;
use unified_hifi_control::knobs::{self, KnobStore};

/// Response from /knob/now_playing - must include zones_sha
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct NowPlayingResponse {
    zone_id: String,
    line1: String,
    line2: String,
    is_playing: bool,
    zones: Vec<serde_json::Value>,
    config_sha: Option<String>,
    zones_sha: Option<String>, // This is what we're testing!
}

/// Create test app with LMS adapter connected to mock server
async fn create_test_app_with_lms(mock_addr: std::net::SocketAddr) -> Router {
    let bus = create_bus();
    let coordinator = Arc::new(AdapterCoordinator::new(bus.clone()));

    // Create and start aggregator FIRST so it receives ZoneDiscovered events
    let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
    let agg_clone = aggregator.clone();
    tokio::spawn(async move {
        agg_clone.run().await;
    });

    // Give aggregator time to start its event loop
    tokio::time::sleep(Duration::from_millis(10)).await;

    let roon = Arc::new(RoonAdapter::new_disconnected(bus.clone()));
    let hqp_instances = Arc::new(HqpInstanceManager::new(bus.clone()));
    let hqplayer = hqp_instances.get_default().await;
    let hqp_zone_links = Arc::new(HqpZoneLinkService::new(hqp_instances.clone()));
    let lms = Arc::new(LmsAdapter::new(bus.clone()));
    let openhome = Arc::new(OpenHomeAdapter::new(bus.clone()));
    let upnp = Arc::new(UPnPAdapter::new(bus.clone()));
    let knob_store = KnobStore::new();

    // Configure and start LMS adapter with mock server
    lms.configure(
        mock_addr.ip().to_string(),
        Some(mock_addr.port()),
        None,
        None,
    )
    .await;
    lms.start().await.expect("LMS adapter should start");

    // Wait for adapter to discover players (aggregator will receive events)
    tokio::time::sleep(Duration::from_millis(200)).await;

    let startable_adapters: Vec<Arc<dyn Startable>> =
        vec![roon.clone(), lms.clone(), openhome.clone(), upnp.clone()];

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

    Router::new()
        .route("/knob/zones", get(knobs::knob_zones_handler))
        .route("/knob/now_playing", get(knobs::knob_now_playing_handler))
        .route("/knob/control", post(knobs::knob_control_handler))
        .with_state(state)
}

/// Issue #148: /knob/now_playing MUST include zones_sha for dynamic zone detection
#[tokio::test]
#[serial]
async fn now_playing_includes_zones_sha() {
    // Enable LMS adapter in settings (simulates plugin signal)
    std::env::set_var("LMS_UNIFIEDHIFI_STARTED", "true");

    // Start mock LMS with a player
    let mock = MockLmsServer::start().await;
    mock.add_player("aa:bb:cc:dd:ee:ff", "Test Player").await;
    mock.set_mode("aa:bb:cc:dd:ee:ff", "stop").await;

    // Create app connected to mock
    let app = create_test_app_with_lms(mock.addr()).await;

    // Request now_playing for the LMS zone
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/knob/now_playing?zone_id=lms:aa:bb:cc:dd:ee:ff")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body);

    // Should succeed
    assert_eq!(
        status,
        StatusCode::OK,
        "Expected OK, got {status}. Body: {body_str}"
    );

    // Parse response
    let parsed: NowPlayingResponse =
        serde_json::from_slice(&body).expect(&format!("Failed to parse response: {body_str}"));

    // THE KEY ASSERTION: zones_sha must be present
    assert!(
        parsed.zones_sha.is_some(),
        "zones_sha MUST be present in /knob/now_playing response for dynamic zone detection (issue #148). Response: {body_str}"
    );

    // Verify it's a valid hash (8 hex chars)
    let sha = parsed.zones_sha.unwrap();
    assert_eq!(sha.len(), 8, "zones_sha should be 8 hex chars, got: {sha}");
    assert!(
        sha.chars().all(|c| c.is_ascii_hexdigit()),
        "zones_sha should be hex, got: {sha}"
    );

    mock.stop().await;
}

// Note: A test for "zones_sha changes when zones change" would require
// enabling the LMS adapter in settings, which needs env var configuration.
// The key regression test (zones_sha is present) is covered above.
