//! Now-playing artwork requested while the cover is still being fetched.
//!
//! Production symptom (v4.0.0-beta.5, owner's NAS): after a track change the
//! HALO Touch (RGB565) and the web Zones card got the "No Image" placeholder,
//! while the Core answered UHC's single `get_image` a few hundred ms later.
//! Clients fetch once per `image_key`, so the placeholder stuck for the whole
//! track. These tests drive `AppState`'s artwork lanes -- the same calls
//! `/now_playing/image`, the relay picture lane (`coordinator.rs`) and the
//! Cloud artwork lane make -- against `FakeRoonCore` with the cover held in
//! flight, and require every concurrent request to get the real cover.

mod mock_servers;

use std::sync::Arc;
use std::time::{Duration, Instant};

use mock_servers::roon_core::FakeRoonCore;
use tokio_util::sync::CancellationToken;

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
use unified_hifi_control::knobs::image::Rgb565ResizePolicy;
use unified_hifi_control::knobs::KnobStore;

const ZONE: &str = "roon:zone_1";
const ART_KEY: &str = "cover_after_next";
const ART_IN_FLIGHT: Duration = Duration::from_millis(600);

fn private_state_path() -> std::path::PathBuf {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("roon_state.json");
    std::mem::forget(dir);
    path
}

async fn connected_roon(core: &FakeRoonCore) -> Arc<RoonAdapter> {
    let adapter = Arc::new(
        RoonAdapter::new_configured(
            create_bus(),
            "http://test.invalid:8088".to_string(),
            KnobStore::new(),
        )
        .with_state_path_for_tests(private_state_path()),
    );
    let runner = adapter.clone();
    let ip = core.ip();
    let port = core.port();
    tokio::spawn(async move {
        let _ = runner
            .run_event_loop_against_core_for_tests(ip, &port)
            .await;
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if adapter.is_browse_connected().await {
            return adapter;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("adapter never reported Browse connected");
}

async fn app_state(roon: Arc<RoonAdapter>) -> AppState {
    let bus = create_bus();
    let hqp_instances = Arc::new(HqpInstanceManager::new(bus.clone()));
    let hqplayer = hqp_instances.get_default().await;
    let hqp_zone_links = Arc::new(HqpZoneLinkService::new(hqp_instances.clone()));
    let startable: Vec<Arc<dyn Startable>> = vec![roon.clone()];
    AppState::new(
        roon,
        hqplayer,
        hqp_instances,
        hqp_zone_links,
        Arc::new(LmsAdapter::new(bus.clone())),
        Arc::new(OpenHomeAdapter::new(bus.clone())),
        Arc::new(UPnPAdapter::new(bus.clone())),
        KnobStore::new(),
        bus.clone(),
        Arc::new(ZoneAggregator::new(bus.clone())),
        Arc::new(AdapterCoordinator::new(bus)),
        startable,
        Instant::now(),
        CancellationToken::new(),
    )
}

async fn image_request_sizes(core: &FakeRoonCore) -> Vec<u64> {
    core.requests()
        .await
        .into_iter()
        .filter(|r| r.name == "com.roonlabs.image:1/get_image")
        .map(|r| r.body["width"].as_u64().unwrap_or_default())
        .collect()
}

/// A track change on a zone shown by a knob, the web UI and a relay: the knob
/// asks first, the web card and the relay's byte-bounded picture lane ask
/// while that fetch is still on the wire. Nobody may get an error (the knob
/// handler turns any error into the placeholder).
#[tokio::test]
async fn every_lane_asking_during_the_in_flight_window_gets_the_cover() {
    let core = FakeRoonCore::start().await;
    let state = app_state(connected_roon(&core).await).await;
    core.set_delay_for_item_key(ART_KEY, ART_IN_FLIGHT).await;

    let knob = {
        let state = state.clone();
        tokio::spawn(async move {
            state
                .get_image_tagged(
                    ZONE,
                    ART_KEY,
                    Some(240),
                    Some(240),
                    Some("rgb565"),
                    Some(Rgb565ResizePolicy::Fit),
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let web = {
        let state = state.clone();
        tokio::spawn(async move {
            state
                .get_image_tagged(ZONE, ART_KEY, Some(360), Some(360), None, None)
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let relay = {
        let state = state.clone();
        tokio::spawn(async move { state.get_image_bounded(ZONE, ART_KEY, 1024 * 1024).await })
    };

    let knob = knob.await.expect("knob task");
    let web = web.await.expect("web task");
    let relay = relay.await.expect("relay task");
    // The fake serves a 1x1 PNG, which the RGB565 converter does not take;
    // what matters here is that the fetch itself did not fail mid-flight.
    if let Err(error) = knob {
        panic!("knob lane got an error (=> placeholder): {error}");
    }
    assert!(
        web.is_ok(),
        "web lane got an error (=> placeholder): {:?}",
        web.err()
    );
    assert!(relay.is_ok(), "relay lane got an error: {:?}", relay.err());

    core.stop().await;
}

/// The byte-bounded lanes fetch Roon's small rendition. It must not be cached
/// as the full-size source: a knob asking after the relay must still get the
/// 2048 px source (#805), not the 300 px one for the next five minutes.
#[tokio::test]
async fn the_small_bounded_rendition_is_not_served_as_the_full_size_source() {
    let core = FakeRoonCore::start().await;
    let state = app_state(connected_roon(&core).await).await;

    state
        .get_image_bounded(ZONE, ART_KEY, 1024 * 1024)
        .await
        .expect("relay lane");
    state
        .get_image_tagged(ZONE, ART_KEY, Some(240), Some(240), None, None)
        .await
        .expect("knob lane");
    // And each rendition is cached under its own identity.
    state
        .get_image_bounded(ZONE, ART_KEY, 1024 * 1024)
        .await
        .expect("relay lane, cached");
    state
        .get_image_tagged(ZONE, ART_KEY, Some(480), Some(480), None, None)
        .await
        .expect("knob lane, cached");

    assert_eq!(
        image_request_sizes(&core).await,
        vec![300, 2048],
        "small and full-size renditions are distinct provider fetches, each fetched once"
    );

    core.stop().await;
}
