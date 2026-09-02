//! Configuring a provider at runtime must make it visible, not merely connected.
//!
//! `POST /lms/configure` is the documented way to point UHC at a Lyrion server from
//! the web UI, and it used to leave the install in a state that looked healthy from
//! every angle except the one that matters: `/lms/config` reported connected, the
//! poll loop published complete player snapshots into the aggregator, collection
//! browsing worked — and no zone appeared in any zone list, because every list
//! filters on `adapters.lms` re-read from disk and configuring never set it. Only a
//! later `POST /api/settings` (or a restart with the toggle already on) repaired it.
//!
//! So this test asserts the *visible* list, not aggregator membership: aggregator
//! membership was never the broken half. It writes no settings of its own — a
//! settings write is the workaround this regression is about.
//!
//! The same hole existed on `/hqplayer/configure`, which started the managed
//! lifecycle only when the toggle happened to be on already, so the second test
//! covers the shared rule rather than the one provider it was first reported on.

mod mock_servers;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::Json;
use tokio_util::sync::CancellationToken;

use unified_hifi_control::adapters::hqplayer::{HqpInstanceManager, HqpZoneLinkService};
use unified_hifi_control::adapters::lms;
use unified_hifi_control::adapters::openhome::OpenHomeAdapter;
use unified_hifi_control::adapters::roon::RoonAdapter;
use unified_hifi_control::adapters::upnp::UPnPAdapter;
use unified_hifi_control::adapters::Startable;
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::api::{AppState, LmsConfigRequest};
use unified_hifi_control::bus::create_bus;
use unified_hifi_control::coordinator::AdapterCoordinator;
use unified_hifi_control::knobs::KnobStore;

/// Compose the real production wiring: aggregator, reliable projection runtime, the
/// LMS poller and its CLI companion sharing one bridge, and a coordinator whose
/// enabled state comes from the settings on disk.
async fn app_state_with_live_lms_projection() -> (AppState, Arc<ZoneAggregator>) {
    let bus = create_bus();
    let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
    let runtime = unified_hifi_control::bus::runtime::build_runtime(aggregator.clone(), 16, 64);
    let bridge = Arc::new(lms::LmsRuntimeBridge::new(
        runtime.projection_ingress.clone(),
        runtime.commands.clone(),
    ));
    let (lms_adapter, lms_cli) = lms::create_lms_adapters_with_runtime(bus.clone(), Some(bridge));

    let coordinator = Arc::new(AdapterCoordinator::new(bus.clone()));
    let settings = unified_hifi_control::api::load_app_settings();
    coordinator.register_from_settings(&settings.adapters).await;
    coordinator.register_companion("lms", lms_cli.clone()).await;

    // Same ordering guarantee as `main`: the aggregator is subscribed and the
    // projection actor is running before any producer can publish.
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let agg_task = aggregator.clone();
    tokio::spawn(async move { agg_task.run_with_ready(ready_tx).await });
    ready_rx.await.expect("aggregator ready");
    tokio::spawn(runtime.projection_actor.run());

    let hqp_instances = Arc::new(HqpInstanceManager::new(bus.clone()));
    let hqplayer = hqp_instances.get_default().await;
    let hqp_zone_links = Arc::new(HqpZoneLinkService::new(hqp_instances.clone()));
    let startables: Vec<Arc<dyn Startable>> = vec![lms_adapter.clone(), lms_cli.clone()];

    let state = AppState::new(
        Arc::new(RoonAdapter::new_disconnected(bus.clone())),
        hqplayer,
        hqp_instances,
        hqp_zone_links,
        lms_adapter,
        Arc::new(OpenHomeAdapter::new(bus.clone())),
        Arc::new(UPnPAdapter::new(bus.clone())),
        KnobStore::new(),
        bus,
        aggregator.clone(),
        coordinator,
        startables,
        Instant::now(),
        CancellationToken::new(),
    );
    (state, aggregator)
}

/// Poll the user-visible zone list until `predicate` holds, or fail with what the
/// aggregator held at the deadline — the aggregator side is the diagnostic that
/// distinguishes "never published" from "published but filtered out".
async fn await_visible_zone(state: &AppState, aggregator: &ZoneAggregator, zone_id: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let visible = unified_hifi_control::zone_list::visible_zones(state).await;
        if visible.iter().any(|zone| zone.zone_id == zone_id) {
            return;
        }
        if std::time::Instant::now() >= deadline {
            let held = aggregator.get_zones().await;
            panic!(
                "{zone_id} never reached the zone list. visible={:?}, aggregator={:?}",
                visible.iter().map(|z| &z.zone_id).collect::<Vec<_>>(),
                held.iter().map(|z| &z.zone_id).collect::<Vec<_>>(),
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
#[serial_test::serial(lms_config)]
async fn configuring_lms_at_runtime_yields_a_zone_without_a_settings_write() {
    let dir = std::env::temp_dir().join("uhc-test-lms-runtime-configure");
    let _ = std::fs::remove_dir_all(&dir);
    std::env::set_var("UHC_CONFIG_DIR", &dir);

    // The reported starting point: an install that has never enabled LMS, so
    // `adapters.lms` is the default `false` and no LMS config exists on disk.
    let before = unified_hifi_control::api::load_app_settings();
    assert!(
        !before.adapters.lms,
        "this regression starts from an install where LMS was never enabled"
    );

    let mock = mock_servers::lms::MockLmsServer::start().await;
    mock.add_player("aa:bb:cc:dd:ee:ff", "Kitchen").await;
    let addr = mock.addr();

    let (state, aggregator) = app_state_with_live_lms_projection().await;

    let response = unified_hifi_control::api::lms_configure_handler(
        State(state.clone()),
        Json(LmsConfigRequest {
            host: addr.ip().to_string(),
            port: Some(addr.port()),
            username: None,
            password: None,
        }),
    )
    .await;
    let status = axum::response::IntoResponse::into_response(response).status();
    assert_eq!(
        status,
        axum::http::StatusCode::OK,
        "configure should succeed"
    );

    await_visible_zone(&state, &aggregator, "lms:aa:bb:cc:dd:ee:ff").await;

    // Configuring is the enable gesture, and it has to survive a restart: the
    // process reloads `adapters.lms` from disk to decide whether to start LMS at
    // all, so an in-memory-only enable would put the install straight back into
    // the reported state on the next boot.
    let after = unified_hifi_control::api::load_app_settings();
    assert!(
        after.adapters.lms,
        "configuring LMS must persist adapters.lms so the next start keeps it"
    );
    assert!(state.coordinator.is_enabled("lms").await);
    assert!(state.coordinator.is_running("lms").await);

    let _ = std::fs::remove_dir_all(&dir);
}

/// `/hqplayer/configure` is the same gesture through a different provider, and had
/// the same two-part failure: the managed lifecycle was skipped while the toggle was
/// off, and `hqplayer:` zones are filtered by that toggle even once it runs.
///
/// This stops at the enable half deliberately. Publishing an HQPlayer zone needs a
/// wire-protocol daemon and a full managed-instance rig, which `hqplayer_direct_zone`
/// already owns; what was broken here, and all this needs to hold, is that
/// configuring turns the provider on and keeps it on across a restart.
#[tokio::test]
#[serial_test::serial(lms_config)]
async fn configuring_hqplayer_at_runtime_enables_it() {
    let dir = std::env::temp_dir().join("uhc-test-hqp-runtime-configure");
    let _ = std::fs::remove_dir_all(&dir);
    std::env::set_var("UHC_CONFIG_DIR", &dir);

    assert!(
        !unified_hifi_control::api::load_app_settings()
            .adapters
            .hqplayer,
        "this regression starts from an install where HQPlayer was never enabled"
    );

    let (state, _aggregator) = app_state_with_live_lms_projection().await;

    // No daemon is listening; the connection test in the handler is expected to fail.
    // Enabling the provider is a decision about the user's intent, not about whether
    // the host happened to answer, so it must hold either way.
    let response = unified_hifi_control::api::hqp_configure_handler(
        State(state.clone()),
        Json(unified_hifi_control::api::HqpConfigRequest {
            host: "127.0.0.1".to_string(),
            port: Some(4321),
            web_port: Some(8088),
            username: None,
            password: None,
        }),
    )
    .await;
    let status = axum::response::IntoResponse::into_response(response).status();
    assert_eq!(status, axum::http::StatusCode::OK);

    assert!(
        unified_hifi_control::api::load_app_settings()
            .adapters
            .hqplayer,
        "configuring HQPlayer must persist adapters.hqplayer so its zones are listed"
    );
    assert!(state.coordinator.is_enabled("hqplayer").await);

    let _ = std::fs::remove_dir_all(&dir);
}

/// The whole point of persisting the toggle is that the zones become visible, so a
/// configure that could not persist it must not answer `200 ok`. Reporting success
/// while the toggle stayed off puts the user back in the reported bug with no signal
/// at all — a connected provider whose zones no list shows. A read-only config
/// directory is the realistic way to reach this.
#[tokio::test]
#[serial_test::serial(lms_config)]
async fn a_configure_that_cannot_persist_the_toggle_reports_failure() {
    let dir = std::env::temp_dir().join("uhc-test-lms-unwritable-settings");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("unified-hifi")).expect("create config dir");
    std::env::set_var("UHC_CONFIG_DIR", &dir);

    let mock = mock_servers::lms::MockLmsServer::start().await;
    mock.add_player("aa:bb:cc:dd:ee:ff", "Kitchen").await;
    let addr = mock.addr();

    let (state, _aggregator) = app_state_with_live_lms_projection().await;

    // Take the settings directory away from the process after the state is built, so
    // only the settings write fails and everything before it behaves normally.
    let mut permissions = std::fs::metadata(dir.join("unified-hifi"))
        .expect("config dir metadata")
        .permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(dir.join("unified-hifi"), permissions).expect("make dir read-only");

    let response = unified_hifi_control::api::lms_configure_handler(
        State(state.clone()),
        Json(LmsConfigRequest {
            host: addr.ip().to_string(),
            port: Some(addr.port()),
            username: None,
            password: None,
        }),
    )
    .await;
    let status = axum::response::IntoResponse::into_response(response).status();

    let mut permissions = std::fs::metadata(dir.join("unified-hifi"))
        .expect("config dir metadata")
        .permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    let _ = std::fs::set_permissions(dir.join("unified-hifi"), permissions);

    assert_eq!(
        status,
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        "a configure whose enable toggle could not be saved must not report success"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
