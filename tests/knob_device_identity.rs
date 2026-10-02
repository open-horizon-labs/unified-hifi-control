//! Device identity survives legacy requests and restarts without guessing from firmware versions.
use unified_hifi_control::knobs::store::{Knob, KnobStore};

use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::get,
    Router,
};
use serde_json::Value;
use std::{sync::Arc, time::Instant};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use unified_hifi_control::{
    adapters::{
        hqplayer::{HqpInstanceManager, HqpZoneLinkService},
        lms::LmsAdapter,
        openhome::OpenHomeAdapter,
        roon::RoonAdapter,
        upnp::UPnPAdapter,
        Startable,
    },
    aggregator::ZoneAggregator,
    api::AppState,
    bus::{create_bus, runtime::build_runtime},
    coordinator::AdapterCoordinator,
    knobs,
};

// This integration-test executable has exactly one test. Its temporary config directory
// covers both store checks and real HTTP requests, including a fresh router/store restart.
async fn identity_router() -> Router {
    let bus = create_bus();

    // Create coordinator (tests don't need real lifecycle management)
    let coordinator = Arc::new(AdapterCoordinator::new(bus.clone()));

    // Create disconnected adapters
    let roon = Arc::new(RoonAdapter::new_disconnected(bus.clone()));
    let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
    let runtime = build_runtime(aggregator.clone(), 16, 32);
    let bridge = Arc::new(
        unified_hifi_control::adapters::hqplayer::HqpRuntimeBridge::new(
            runtime.projection_ingress.clone(),
            runtime.commands.clone(),
        ),
    );
    let reliable_commands = runtime.commands.clone();
    tokio::spawn(runtime.projection_actor.run());
    let hqp_instances = Arc::new(HqpInstanceManager::new_with_runtime(bus.clone(), bridge));
    let hqplayer = hqp_instances.get_default().await;
    let hqp_zone_links = Arc::new(HqpZoneLinkService::new(hqp_instances.clone()));
    let lms = Arc::new(LmsAdapter::new(bus.clone()));
    let openhome = Arc::new(OpenHomeAdapter::new(bus.clone()));
    let upnp = Arc::new(UPnPAdapter::new(bus.clone()));
    let knob_store = KnobStore::new();

    // Build startable adapters list
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
    )
    .with_reliable_commands(reliable_commands);

    Router::new()
        .route("/zones", get(knobs::knob_zones_handler))
        .route("/knob/devices", get(knobs::knob_devices_handler))
        .with_state(state)
}

#[tokio::test]
async fn controller_identity_is_canonical_persistent_and_never_erased_by_legacy_requests() {
    let temporary = tempfile::tempdir().unwrap();
    let previous = std::env::var_os("UHC_CONFIG_DIR");
    std::env::set_var("UHC_CONFIG_DIR", temporary.path());
    let store = KnobStore::new();
    let legacy = store.get_or_create("legacy", Some("2.5.2")).await;
    assert_eq!(
        serde_json::to_value(&legacy).unwrap()["device_type"],
        "knob"
    );
    let frame = store
        .get_or_create_with_device_type("frame", Some("2.7.0-alpha.5"), Some("hiphi-frame"))
        .await;
    let sha = frame.config_sha.clone();
    assert_eq!(
        serde_json::to_value(&frame).unwrap()["device_type"],
        "frame"
    );
    for identity in [None, Some("frmae"), Some("../dial"), Some("")] {
        let preserved = store
            .get_or_create_with_device_type("frame", None, identity)
            .await;
        assert_eq!(
            serde_json::to_value(&preserved).unwrap()["device_type"],
            "frame"
        );
        assert_eq!(
            preserved.config_sha, sha,
            "identity must not reset power settings or config hash"
        );
    }
    let m5 = store
        .get_or_create_with_device_type("m5", None, Some("hiphi-dial-beta"))
        .await;
    assert_eq!(serde_json::to_value(&m5).unwrap()["device_type"], "m5dial");
    let restored = KnobStore::new();
    let summaries = serde_json::to_value(restored.list().await).unwrap();
    assert_eq!(
        summaries
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["knob_id"] == "frame")
            .unwrap()["device_type"],
        "frame"
    );
    let mut old_record = serde_json::to_value(&legacy).unwrap();
    old_record.as_object_mut().unwrap().remove("device_type");
    let migrated: Knob = serde_json::from_value(old_record).unwrap();
    assert_eq!(
        serde_json::to_value(migrated).unwrap()["device_type"],
        "knob"
    );
    let app = identity_router().await;
    let id = "identity-frame-http";
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/zones")
                .header("X-Knob-Id", id)
                .header("X-Device-Type", "hiphi-frame")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    for identity in [None, Some("frmae")] {
        let mut request = Request::builder().uri("/zones").header("X-Knob-Id", id);
        if let Some(identity) = identity {
            request = request.header("X-Device-Type", identity);
        }
        assert_eq!(
            app.clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }
    for router in [app, identity_router().await] {
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/knob/devices")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let summary: Value = serde_json::from_slice(&bytes).unwrap();
        let device = summary["knobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["knob_id"] == id)
            .expect("controller header must register a device");
        assert_eq!(device["device_type"], "frame");
        for legacy_key in ["knob_id", "name", "last_seen", "version", "status"] {
            assert!(device.get(legacy_key).is_some());
        }
    }

    if let Some(value) = previous {
        std::env::set_var("UHC_CONFIG_DIR", value);
    } else {
        std::env::remove_var("UHC_CONFIG_DIR");
    }
}
