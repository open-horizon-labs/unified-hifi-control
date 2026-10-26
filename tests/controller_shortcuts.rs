//! Controller shortcut presses, end to end against a real `LmsAdapter` and the mock LMS server.
//!
//! Each test names the tempting wrong implementation it would catch. They run in a private config
//! directory so they cannot touch a developer's real controller or shortcut settings.

mod mock_servers;

use std::sync::{Arc, Once};
use std::time::Instant;

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue};
use axum::Json;
use mock_servers::roon_core::{FakeRoonCore, ItemKeyScope};
use mock_servers::MockLmsServer;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use unified_hifi_control::adapters::hqplayer::{HqpInstanceManager, HqpZoneLinkService};
use unified_hifi_control::adapters::lms::LmsAdapter;
use unified_hifi_control::adapters::openhome::OpenHomeAdapter;
use unified_hifi_control::adapters::roon::RoonAdapter;
use unified_hifi_control::adapters::upnp::UPnPAdapter;
use unified_hifi_control::adapters::Startable;
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::api::shortcuts::{input_handler, InputRequest};
use unified_hifi_control::api::AppState;
use unified_hifi_control::bus::{create_bus, BusEvent, PlaybackState, Zone};
use unified_hifi_control::coordinator::AdapterCoordinator;
use unified_hifi_control::knobs::store::{KnobConfigUpdate, KnobStatusUpdate};
use unified_hifi_control::knobs::KnobStore;
use unified_hifi_control::mcp::tools::collections::{handle_collections, HifiCollectionsTool};
use unified_hifi_control::mcp::tools::library::{handle_search, HifiSearchTool};
use unified_hifi_control::shortcuts::resolve::{press, PressCode};
use unified_hifi_control::shortcuts::{ShortcutStore, ShortcutTarget};

const LMS_PLAYER: &str = "aa:bb:cc:dd:ee:ff";
const OTHER_PLAYER: &str = "11:22:33:44:55:66";
const GONE_PLAYER: &str = "de:ad:be:ef:00:01";
/// One controller id per test. Controller settings (assigned zone, hide list) persist in the
/// config directory every test in this process shares, so a fixed id would let one test's
/// assignment decide another's outcome depending on run order.
fn controller() -> &'static str {
    thread_local! {
        static ID: &'static str = Box::leak(
            format!("dial-{:?}", std::thread::current().id())
                .replace(['(', ')'], "-")
                .into_boxed_str(),
        );
    }
    ID.with(|id| *id)
}
const EVENT: &str = "preset_1:press";

static CONFIG: Once = Once::new();
static CONFIG_DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();

fn isolate_config() {
    CONFIG.call_once(|| {
        let dir = CONFIG_DIR.get_or_init(|| tempfile::tempdir().unwrap());
        std::env::set_var("UHC_CONFIG_DIR", dir.path());
    });
}

fn zone(zone_id: &str, name: &str) -> Zone {
    Zone {
        zone_id: zone_id.to_string(),
        zone_name: name.to_string(),
        state: PlaybackState::Stopped,
        volume_control: None,
        now_playing: None,
        source: zone_id.split(':').next().unwrap_or("lms").to_string(),
        is_controllable: true,
        is_seekable: false,
        last_updated: 0,
        is_play_allowed: true,
        is_pause_allowed: true,
        is_next_allowed: true,
        is_previous_allowed: true,
    }
}

struct Harness {
    state: AppState,
    mock: MockLmsServer,
    _dir: tempfile::TempDir,
}

async fn harness(zones: &[(&str, &str)]) -> Harness {
    isolate_config();
    let mock = MockLmsServer::start().await;
    mock.add_player(LMS_PLAYER, "Living Room").await;
    mock.add_player(OTHER_PLAYER, "Kitchen").await;
    mock.set_library_albums(vec![(101, "Kind of Blue", "Miles Davis")])
        .await;

    let bus = create_bus();
    let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
    let runtime = unified_hifi_control::bus::runtime::build_runtime(aggregator.clone(), 16, 64);
    let gateway = runtime.commands.clone();
    let bridge = Arc::new(unified_hifi_control::adapters::lms::LmsRuntimeBridge::new(
        runtime.projection_ingress.clone(),
        gateway.clone(),
    ));
    tokio::spawn(runtime.projection_actor.run());
    let (lms, _) = unified_hifi_control::adapters::lms::create_lms_adapters_with_runtime(
        bus.clone(),
        Some(bridge),
    );
    lms.configure(
        mock.addr().ip().to_string(),
        Some(mock.addr().port()),
        None,
        None,
    )
    .await;
    lms.start().await.expect("LMS adapter must start");

    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    {
        let aggregator = aggregator.clone();
        tokio::spawn(async move { aggregator.run_with_ready(ready_tx).await });
    }
    ready_rx.await.unwrap();

    let roon = Arc::new(RoonAdapter::new_disconnected(bus.clone()));
    let hqp_instances = Arc::new(HqpInstanceManager::new(bus.clone()));
    let hqplayer = hqp_instances.get_default().await;
    let hqp_zone_links = Arc::new(HqpZoneLinkService::new(hqp_instances.clone()));
    let openhome = Arc::new(OpenHomeAdapter::new(bus.clone()));
    let upnp = Arc::new(UPnPAdapter::new(bus.clone()));
    let startable: Vec<Arc<dyn Startable>> = vec![lms.clone()];

    let mut state = AppState::new(
        roon,
        hqplayer,
        hqp_instances,
        hqp_zone_links,
        lms,
        openhome,
        upnp,
        KnobStore::new(),
        bus.clone(),
        aggregator.clone(),
        Arc::new(AdapterCoordinator::new(bus.clone())),
        startable,
        Instant::now(),
        CancellationToken::new(),
    )
    .with_reliable_commands(gateway.clone());
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    while !gateway.has_endpoint(&unified_hifi_control::bus::PrefixedZoneId::lms(LMS_PLAYER)) {
        assert!(Instant::now() < deadline, "LMS endpoint never registered");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    // The resolver plays through the registry, as main.rs wires it.
    state
        .adapter_registry
        .register_library("lms", state.lms.clone())
        .await;
    let dir = tempfile::tempdir().unwrap();
    state.shortcuts = ShortcutStore::open(dir.path().join("shortcuts.json"));

    for (id, name) in zones {
        bus.publish(BusEvent::ZoneDiscovered {
            zone: zone(id, name),
        });
    }
    for (id, _) in zones {
        for _ in 0..200 {
            if aggregator.get_zone(id).await.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            aggregator.get_zone(id).await.is_some(),
            "{id} never appeared"
        );
    }

    state
        .knobs
        .get_or_create(controller(), Some("test"))
        .await
        .unwrap();
    Harness {
        state,
        mock,
        _dir: dir,
    }
}

impl Harness {
    /// Pin "Kind of Blue" the way the Library does: search mints a ref, the bridge converts it.
    async fn pin_kind_of_blue(&self) -> String {
        let search = handle_search(
            &self.state,
            HifiSearchTool {
                query: "kind of blue".to_string(),
                zone_id: Some(format!("lms:{LMS_PLAYER}")),
                source: None,
            },
        )
        .await
        .unwrap();
        let token = search.structured_content.unwrap()["data"][0]["ref"]
            .as_str()
            .expect("an LMS library album must carry a ref")
            .to_string();
        let ref_target = self.state.mcp_refs.resolve(&token).await.unwrap();
        let target = ShortcutTarget::from_ref(&ref_target).expect("an LMS album is pinnable");
        let (shortcut, _) = self
            .state
            .shortcuts
            .pin(target, ref_target.title().to_string(), None)
            .await
            .unwrap();
        shortcut.id
    }

    async fn bind(&self, shortcut_id: &str) {
        let mut map = std::collections::BTreeMap::new();
        map.insert(EVENT.to_string(), shortcut_id.to_string());
        self.state
            .shortcuts
            .set_bindings(controller(), map)
            .await
            .unwrap();
    }

    async fn controller_reports_zone(&self, zone_id: &str) {
        self.state
            .knobs
            .update_status(
                controller(),
                KnobStatusUpdate {
                    zone_id: Some(zone_id.to_string()),
                    ..Default::default()
                },
            )
            .await;
    }

    async fn playlist_commands(&self, player: &str) -> Vec<Vec<String>> {
        self.mock
            .write_commands(player)
            .await
            .into_iter()
            .filter(|c| c.first().map(String::as_str) == Some("playlistcontrol"))
            .collect()
    }

    async fn stop(self) {
        self.mock.stop().await;
        self.state.lms.stop().await;
    }
}

#[tokio::test]
async fn a_press_plays_the_pinned_item_on_the_controllers_own_zone() {
    let h = harness(&[
        (&format!("lms:{LMS_PLAYER}"), "Living Room"),
        (&format!("lms:{OTHER_PLAYER}"), "Kitchen"),
    ])
    .await;
    let id = h.pin_kind_of_blue().await;
    h.bind(&id).await;
    h.controller_reports_zone(&format!("lms:{OTHER_PLAYER}"))
        .await;
    h.mock.clear_commands().await;

    let outcome = press(&h.state, controller(), EVENT).await;
    assert_eq!(outcome.code, PressCode::Accepted, "{}", outcome.message);

    assert!(
        h.playlist_commands(OTHER_PLAYER)
            .await
            .iter()
            .any(|c| c.contains(&"album_id:101".to_string())),
        "the controller's zone must play it"
    );
    // The tempting wrong patch: play on the zone the Library page happened to have armed (here the
    // first one pinned from). The other player must not have been touched.
    assert!(h.playlist_commands(LMS_PLAYER).await.is_empty());
    h.stop().await;
}

#[tokio::test]
async fn an_assigned_zone_wins_over_the_zone_the_controller_last_reported() {
    let h = harness(&[
        (&format!("lms:{LMS_PLAYER}"), "Living Room"),
        (&format!("lms:{OTHER_PLAYER}"), "Kitchen"),
    ])
    .await;
    let id = h.pin_kind_of_blue().await;
    h.bind(&id).await;
    h.controller_reports_zone(&format!("lms:{OTHER_PLAYER}"))
        .await;
    h.state
        .knobs
        .update_config(
            controller(),
            KnobConfigUpdate {
                assigned_zone_id: Some(format!("lms:{LMS_PLAYER}")),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    h.mock.clear_commands().await;

    assert_eq!(
        press(&h.state, controller(), EVENT).await.code,
        PressCode::Accepted
    );
    assert!(!h.playlist_commands(LMS_PLAYER).await.is_empty());
    assert!(h.playlist_commands(OTHER_PLAYER).await.is_empty());
    h.stop().await;
}

/// Hiding is a list filter. A resolver that validated the zone against the controller's own zone
/// list (the tempting way to "respect hiding") would refuse here.
#[tokio::test]
async fn a_zone_hidden_on_this_controller_still_plays() {
    let zone_id = format!("lms:{LMS_PLAYER}");
    let h = harness(&[(&zone_id, "Living Room")]).await;
    let id = h.pin_kind_of_blue().await;
    h.bind(&id).await;
    h.controller_reports_zone(&zone_id).await;
    h.state
        .knobs
        .update_config(
            controller(),
            KnobConfigUpdate {
                hidden_zone_ids: Some(vec![zone_id.clone()]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    h.mock.clear_commands().await;

    let outcome = press(&h.state, controller(), EVENT).await;
    assert_eq!(outcome.code, PressCode::Accepted, "{}", outcome.message);
    assert!(!h.playlist_commands(LMS_PLAYER).await.is_empty());
    h.stop().await;
}

/// The controller's zone is gone while another zone is available. Falling back to "the first
/// visible zone" would start music in a room nobody chose.
#[tokio::test]
async fn a_missing_zone_fails_visibly_and_never_falls_back() {
    let h = harness(&[(&format!("lms:{LMS_PLAYER}"), "Living Room")]).await;
    let id = h.pin_kind_of_blue().await;
    h.bind(&id).await;
    // A player the LMS server has never heard of: genuinely absent, with a real zone beside it
    // that a fallback would pick.
    h.controller_reports_zone(&format!("lms:{GONE_PLAYER}"))
        .await;
    h.mock.clear_commands().await;

    let outcome = press(&h.state, controller(), EVENT).await;
    assert_eq!(outcome.code, PressCode::ZoneMissing);
    assert!(!outcome.ok);
    assert!(
        h.playlist_commands(LMS_PLAYER).await.is_empty()
            && h.playlist_commands(OTHER_PLAYER).await.is_empty()
            && h.playlist_commands(GONE_PLAYER).await.is_empty(),
        "nothing may play anywhere"
    );
    let last = h.state.shortcuts.last_press(&id).await.unwrap();
    assert!(!last.ok && last.code == "zone_missing");
    h.stop().await;
}

#[tokio::test]
async fn a_controller_with_no_zone_yet_says_so() {
    let h = harness(&[(&format!("lms:{LMS_PLAYER}"), "Living Room")]).await;
    let id = h.pin_kind_of_blue().await;
    h.bind(&id).await;
    let outcome = press(&h.state, controller(), EVENT).await;
    assert_eq!(outcome.code, PressCode::NoZone);
    h.stop().await;
}

#[tokio::test]
async fn a_shortcut_for_another_provider_is_refused_not_played_anyway() {
    let h = harness(&[
        (&format!("lms:{LMS_PLAYER}"), "Living Room"),
        ("roon:abc", "Office"),
    ])
    .await;
    let id = h.pin_kind_of_blue().await;
    h.bind(&id).await;
    h.controller_reports_zone("roon:abc").await;
    h.mock.clear_commands().await;

    let outcome = press(&h.state, controller(), EVENT).await;
    assert_eq!(outcome.code, PressCode::ProviderMismatch);
    assert!(h.playlist_commands(LMS_PLAYER).await.is_empty());
    h.stop().await;
}

/// The library was rescanned after pinning. The press must fail, report why, and leave the queue
/// alone; and it must say so on the shortcut so the curator sees it without asking the listener.
#[tokio::test]
async fn an_item_that_vanished_fails_visibly_without_touching_the_queue() {
    let zone_id = format!("lms:{LMS_PLAYER}");
    let h = harness(&[(&zone_id, "Living Room")]).await;
    let id = h.pin_kind_of_blue().await;
    h.bind(&id).await;
    h.controller_reports_zone(&zone_id).await;
    h.mock.set_library_albums(vec![]).await;
    h.mock.clear_commands().await;

    let outcome = press(&h.state, controller(), EVENT).await;
    assert!(!outcome.ok, "{}", outcome.message);
    assert_eq!(outcome.code, PressCode::Refused);
    assert!(h.playlist_commands(LMS_PLAYER).await.is_empty());
    assert!(!h.state.shortcuts.last_press(&id).await.unwrap().ok);
    h.stop().await;
}

#[tokio::test]
async fn an_unbound_button_is_reported_as_unmapped_and_plays_nothing() {
    let zone_id = format!("lms:{LMS_PLAYER}");
    let h = harness(&[(&zone_id, "Living Room")]).await;
    h.controller_reports_zone(&zone_id).await;
    h.mock.clear_commands().await;
    let outcome = press(&h.state, controller(), "preset_9:press").await;
    assert_eq!(outcome.code, PressCode::Unmapped);
    assert!(h.playlist_commands(LMS_PLAYER).await.is_empty());
    h.stop().await;
}

fn headers(controller: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("x-knob-id", HeaderValue::from_str(controller).unwrap());
    headers
}

async fn post_input(h: &Harness, controller: &str, key: &str, gesture: &str) -> (u16, Value) {
    match input_handler(
        State(h.state.clone()),
        headers(controller),
        Json(InputRequest {
            key: key.to_string(),
            gesture: gesture.to_string(),
        }),
    )
    .await
    {
        Ok(Json(body)) => (200, body),
        Err((status, Json(body))) => (status.as_u16(), body),
    }
}

#[tokio::test]
async fn the_input_endpoint_plays_unmaps_and_rejects_without_inventing_outcomes() {
    let zone_id = format!("lms:{LMS_PLAYER}");
    let h = harness(&[(&zone_id, "Living Room")]).await;
    let id = h.pin_kind_of_blue().await;
    h.bind(&id).await;
    h.controller_reports_zone(&zone_id).await;

    let (status, body) = post_input(&h, controller(), "preset_1", "press").await;
    assert_eq!(
        (status, body["code"].as_str()),
        (200, Some("accepted")),
        "{body}"
    );

    // A transport key's plain press is the controller's own business: unmapped, never an error.
    let (status, body) = post_input(&h, controller(), "play_pause", "press").await;
    assert_eq!((status, body["code"].as_str()), (200, Some("unmapped")));

    let (status, body) = post_input(&h, controller(), "Bad Key", "press").await;
    assert_eq!(
        (status, body["error_code"].as_str()),
        (400, Some("bad_key"))
    );

    let (status, body) = post_input(&h, "never-registered", "preset_1", "press").await;
    assert_eq!(
        (status, body["error_code"].as_str()),
        (404, Some("unknown_controller"))
    );
    h.stop().await;
}

// =============================================================================
// Roon: a pin made from Browse survives the Core losing its browse sessions
// =============================================================================

const ROON_ZONE: &str = "roon:zone_fake_1";

async fn roon_state(core: &FakeRoonCore) -> (AppState, tempfile::TempDir) {
    isolate_config();
    let bus = create_bus();
    let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
    let runtime = unified_hifi_control::bus::runtime::build_runtime(aggregator.clone(), 16, 64);
    let gateway = runtime.commands.clone();
    let bridge = Arc::new(
        unified_hifi_control::adapters::roon::RoonRuntimeBridge::new(
            runtime.projection_ingress.clone(),
            gateway.clone(),
        ),
    );
    tokio::spawn(runtime.projection_actor.run());
    let roon = Arc::new(RoonAdapter::new_configured_with_runtime(
        bus.clone(),
        "http://test.invalid:8088".to_string(),
        KnobStore::new(),
        Some(bridge),
    ));
    {
        let runner = roon.clone();
        let (ip, port) = (core.ip(), core.port());
        tokio::spawn(async move {
            let _ = runner
                .run_event_loop_against_core_for_tests(ip, &port)
                .await;
        });
    }
    let deadline = Instant::now() + std::time::Duration::from_secs(5);
    while !roon.is_browse_connected().await {
        assert!(Instant::now() < deadline, "Roon adapter never connected");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    {
        let aggregator = aggregator.clone();
        tokio::spawn(async move { aggregator.run_with_ready(ready_tx).await });
    }
    ready_rx.await.unwrap();

    let hqp_instances = Arc::new(HqpInstanceManager::new(bus.clone()));
    let hqplayer = hqp_instances.get_default().await;
    let hqp_zone_links = Arc::new(HqpZoneLinkService::new(hqp_instances.clone()));
    let lms = Arc::new(LmsAdapter::new(bus.clone()));
    let startable: Vec<Arc<dyn Startable>> = vec![roon.clone()];
    let mut state = AppState::new(
        roon.clone(),
        hqplayer,
        hqp_instances,
        hqp_zone_links,
        lms,
        Arc::new(OpenHomeAdapter::new(bus.clone())),
        Arc::new(UPnPAdapter::new(bus.clone())),
        KnobStore::new(),
        bus.clone(),
        aggregator.clone(),
        Arc::new(AdapterCoordinator::new(bus.clone())),
        startable,
        Instant::now(),
        CancellationToken::new(),
    )
    .with_reliable_commands(gateway);
    state.adapter_registry.register_library("roon", roon).await;
    let dir = tempfile::tempdir().unwrap();
    state.shortcuts = ShortcutStore::open(dir.path().join("shortcuts.json"));

    bus.publish(BusEvent::ZoneDiscovered {
        zone: zone(ROON_ZONE, "Fake Zone"),
    });
    for _ in 0..200 {
        if aggregator.get_zone(ROON_ZONE).await.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    state
        .knobs
        .get_or_create(controller(), Some("test"))
        .await
        .unwrap();
    state
        .knobs
        .update_status(
            controller(),
            KnobStatusUpdate {
                zone_id: Some(ROON_ZONE.to_string()),
                ..Default::default()
            },
        )
        .await;
    (state, dir)
}

async fn browse(state: &AppState, location: Option<&str>) -> Value {
    let result = handle_collections(
        state,
        HifiCollectionsTool {
            zone_id: ROON_ZONE.to_string(),
            action: "browse".to_string(),
            path: None,
            location: location.map(str::to_string),
            media_type: None,
            limit: Some(50),
            offset: Some(0),
        },
    )
    .await
    .unwrap();
    result.structured_content.unwrap()["data"].clone()
}

fn row<'a>(page: &'a Value, title: &str) -> &'a Value {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["title"] == title)
        .unwrap_or_else(|| panic!("no {title:?} in {page}"))
}

/// The risk this feature leans on hardest. A pin made while browsing must still play after the
/// Core has forgotten every browse session (extension reconnect, Core restart). Storing the
/// minting session's `item_key` would pass the moment after pinning and fail here.
#[tokio::test]
async fn a_roon_pin_still_plays_after_the_core_forgets_every_browse_session() {
    let core = FakeRoonCore::start().await;
    core.set_item_key_scope(ItemKeyScope::PerSession).await;
    let (state, _dir) = roon_state(&core).await;

    let root = browse(&state, None).await;
    let library = browse(&state, row(&root, "Local Library")["location"].as_str()).await;
    let albums = browse(&state, row(&library, "Albums")["location"].as_str()).await;
    let item_ref = row(&albums, "Sketches of Spain")["ref"]
        .as_str()
        .expect("a browsed album must be playable")
        .to_string();

    let ref_target = state.mcp_refs.resolve(&item_ref).await.unwrap();
    let target = ShortcutTarget::from_ref(&ref_target).expect("a browsed Roon album is pinnable");
    let (shortcut, _) = state
        .shortcuts
        .pin(target, ref_target.title().to_string(), None)
        .await
        .unwrap();
    let mut map = std::collections::BTreeMap::new();
    map.insert(EVENT.to_string(), shortcut.id.clone());
    state
        .shortcuts
        .set_bindings(controller(), map)
        .await
        .unwrap();

    core.drop_all_sessions().await;

    let outcome = press(&state, controller(), EVENT).await;
    assert_eq!(outcome.code, PressCode::Accepted, "{}", outcome.message);
    assert!(
        core.browsed_titles()
            .await
            .iter()
            .rev()
            .take(4)
            .any(|t| t == "Play Now"),
        "the Core must actually have been asked to Play Now: {:?}",
        core.browsed_titles().await
    );
    core.stop().await;
}

/// A search hit's trail is only its own title; pinning it would succeed and then never play.
#[tokio::test]
async fn a_roon_search_hit_cannot_be_pinned() {
    let core = FakeRoonCore::start().await;
    let (state, _dir) = roon_state(&core).await;
    let search = handle_search(
        &state,
        HifiSearchTool {
            query: "kind of blue".to_string(),
            zone_id: Some(ROON_ZONE.to_string()),
            source: None,
        },
    )
    .await
    .unwrap();
    let token = search.structured_content.unwrap()["data"][0]["ref"]
        .as_str()
        .expect("Roon hit carries a ref")
        .to_string();
    let ref_target = state.mcp_refs.resolve(&token).await.unwrap();
    assert!(ShortcutTarget::from_ref(&ref_target).is_err());
    core.stop().await;
}

// =============================================================================
// Roon: two albums with one title are two pins, and a press plays the one that was pinned
// =============================================================================

async fn albums_page(state: &AppState) -> Value {
    let root = browse(state, None).await;
    let library = browse(state, row(&root, "Local Library")["location"].as_str()).await;
    browse(state, row(&library, "Albums")["location"].as_str()).await
}

/// Pin the row at `index` in the Albums list; returns the shortcut id.
async fn pin_album_row(state: &AppState, albums: &Value, index: usize) -> String {
    let item_ref = albums["items"][index]["ref"]
        .as_str()
        .expect("a browsed album must be playable")
        .to_string();
    let ref_target = state.mcp_refs.resolve(&item_ref).await.unwrap();
    let target = ShortcutTarget::from_ref(&ref_target).expect("a browsed Roon album is pinnable");
    let (shortcut, _) = state
        .shortcuts
        .pin(target, ref_target.title().to_string(), None)
        .await
        .unwrap();
    shortcut.id
}

fn greatest_hits() -> Vec<mock_servers::roon_core::FakeItem> {
    vec![
        mock_servers::roon_core::album("Greatest Hits", "Artist A", &["A1"]),
        mock_servers::roon_core::album("Greatest Hits", "Artist B", &["B1"]),
    ]
}

/// Found in review: pins kept only a title trail, so both albums became one pin and a press replayed
/// the first title match. Subtitle and position must survive into the pin.
#[tokio::test]
async fn two_albums_with_one_title_get_distinct_pins() {
    let core = FakeRoonCore::start().await;
    core.set_children_by_title("Albums", greatest_hits()).await;
    let (state, _dir) = roon_state(&core).await;
    let albums = albums_page(&state).await;
    assert_eq!(albums["items"].as_array().unwrap().len(), 2, "{albums}");

    let a = pin_album_row(&state, &albums, 0).await;
    let b = pin_album_row(&state, &albums, 1).await;
    assert_ne!(a, b, "different artists' albums need distinct pins");
    core.stop().await;
}

/// The wrong-recording failure: after the Core forgets every session, pressing the pin made from
/// Artist B's album must open Artist B's, never the first "Greatest Hits" in the list.
#[tokio::test]
async fn a_press_plays_the_album_that_was_pinned_not_the_first_with_that_title() {
    let core = FakeRoonCore::start().await;
    core.set_item_key_scope(ItemKeyScope::PerSession).await;
    core.set_children_by_title("Albums", greatest_hits()).await;
    let (state, _dir) = roon_state(&core).await;
    let albums = albums_page(&state).await;
    let pinned_b = pin_album_row(&state, &albums, 1).await;
    let mut map = std::collections::BTreeMap::new();
    map.insert(EVENT.to_string(), pinned_b);
    state
        .shortcuts
        .set_bindings(controller(), map)
        .await
        .unwrap();

    core.drop_all_sessions().await;
    let before = core.browsed_identities().await.len();

    let outcome = press(&state, controller(), EVENT).await;
    assert_eq!(outcome.code, PressCode::Accepted, "{}", outcome.message);

    let walked = core.browsed_identities().await[before..].to_vec();
    assert!(
        walked.iter().any(|row| row == "Greatest Hits|Artist B"),
        "must open Artist B's album: {walked:?}"
    );
    assert!(
        !walked.iter().any(|row| row == "Greatest Hits|Artist A"),
        "must never open the other artist's: {walked:?}"
    );
    core.stop().await;
}

/// If the pinned album is gone and only a same-titled one by someone else remains, the press must
/// say it cannot find the item and play nothing, not play the lookalike.
#[tokio::test]
async fn a_lookalike_never_stands_in_for_a_vanished_album() {
    let core = FakeRoonCore::start().await;
    core.set_item_key_scope(ItemKeyScope::PerSession).await;
    core.set_children_by_title("Albums", greatest_hits()).await;
    let (state, _dir) = roon_state(&core).await;
    let albums = albums_page(&state).await;
    let pinned_b = pin_album_row(&state, &albums, 1).await;
    let mut map = std::collections::BTreeMap::new();
    map.insert(EVENT.to_string(), pinned_b);
    state
        .shortcuts
        .set_bindings(controller(), map)
        .await
        .unwrap();

    // Artist B's album leaves the library; Artist A's stays.
    core.set_children_by_title(
        "Albums",
        vec![mock_servers::roon_core::album(
            "Greatest Hits",
            "Artist A",
            &["A1"],
        )],
    )
    .await;
    core.drop_all_sessions().await;
    let before = core.browsed_identities().await.len();

    let outcome = press(&state, controller(), EVENT).await;
    assert_eq!(
        outcome.code,
        PressCode::ItemUnavailable,
        "{}",
        outcome.message
    );
    let walked = core.browsed_identities().await[before..].to_vec();
    assert!(
        !walked.iter().any(|row| row == "Greatest Hits|Artist A"),
        "must not open the lookalike: {walked:?}"
    );
    core.stop().await;
}

#[tokio::test]
async fn audit_resolved_pin_must_not_fall_back_to_title_after_key_rejection() {
    let core = FakeRoonCore::start().await;
    core.set_item_key_scope(ItemKeyScope::PerSession).await;
    core.set_children_by_title("Albums", greatest_hits()).await;
    let (state, _dir) = roon_state(&core).await;
    let albums = albums_page(&state).await;
    let pinned = pin_album_row(&state, &albums, 1).await;
    let mut bindings = std::collections::BTreeMap::new();
    bindings.insert(EVENT.to_string(), pinned);
    state
        .shortcuts
        .set_bindings(controller(), bindings)
        .await
        .unwrap();
    core.reject_identity_once("Greatest Hits|Artist B").await;
    let before = core.browsed_identities().await.len();
    let result = press(&state, controller(), EVENT).await;
    let walked = core.browsed_identities().await[before..].to_vec();
    core.stop().await;
    assert!(
        !walked.iter().any(|row| row == "Greatest Hits|Artist A"),
        "Rejected resolved key must not open another artist: {walked:?}; result={result:?}"
    );
    assert_eq!(
        result.code,
        PressCode::Accepted,
        "Retry must recover the same item"
    );
}

#[tokio::test]
async fn audit_disconnected_gateway_must_prevent_shortcut_native_dispatch() {
    use unified_hifi_control::bus::runtime::build_runtime;
    let mut h = harness(&[(&format!("lms:{LMS_PLAYER}"), "Living Room")]).await;
    let pinned = h.pin_kind_of_blue().await;
    h.bind(&pinned).await;
    h.state
        .knobs
        .update_status(
            controller(),
            KnobStatusUpdate {
                zone_id: Some(format!("lms:{LMS_PLAYER}")),
                ..Default::default()
            },
        )
        .await;
    // A gateway with no registered endpoint must refuse admission; keeping a concrete
    // library adapter object must not provide an alternate way to send native commands.
    let runtime = build_runtime(h.state.aggregator.clone(), 1, 1);
    h.state.reliable_commands = Some(runtime.commands);
    h.mock.clear_commands().await;
    let outcome = press(&h.state, controller(), EVENT).await;
    let native = h.playlist_commands(LMS_PLAYER).await;
    eprintln!("Native writes despite missing gateway endpoint: {native:?}");
    h.stop().await;
    assert_ne!(
        outcome.code,
        PressCode::Accepted,
        "Shortcut bypassed the gateway's missing endpoint: {}",
        outcome.message
    );
}

#[tokio::test]
async fn audit_repin_after_reorder_is_idempotent() {
    let core = FakeRoonCore::start().await;
    core.set_children_by_title("Albums", greatest_hits()).await;
    let (state, _dir) = roon_state(&core).await;
    let first_page = albums_page(&state).await;
    let first = pin_album_row(&state, &first_page, 1).await;
    let mut reordered = greatest_hits();
    reordered.reverse();
    core.set_children_by_title("Albums", reordered).await;
    let second_page = albums_page(&state).await;
    let second = pin_album_row(&state, &second_page, 0).await;
    core.stop().await;
    assert_eq!(
        first, second,
        "Reordering an unchanged album must not create another pin"
    );
}

#[tokio::test]
async fn audit_button_status_is_scoped_to_the_controller() {
    use axum::extract::Path;
    use unified_hifi_control::api::shortcuts::controller_get_handler;
    let h = harness(&[(&format!("lms:{LMS_PLAYER}"), "Living Room")]).await;
    let pin = h.pin_kind_of_blue().await;
    h.bind(&pin).await;
    let other = format!("{}-other", controller());
    h.state
        .knobs
        .get_or_create(&other, Some("test"))
        .await
        .unwrap();
    h.state
        .knobs
        .update_status(
            &other,
            KnobStatusUpdate {
                zone_id: Some(format!("lms:{LMS_PLAYER}")),
                ..Default::default()
            },
        )
        .await;
    let mut bindings = std::collections::BTreeMap::new();
    bindings.insert(EVENT.to_string(), pin);
    h.state
        .shortcuts
        .set_bindings(&other, bindings)
        .await
        .unwrap();
    let outcome = press(&h.state, controller(), EVENT).await;
    assert_eq!(outcome.code, PressCode::NoZone);
    let Json(view) = controller_get_handler(State(h.state.clone()), Path(other))
        .await
        .unwrap();
    h.stop().await;
    assert_eq!(
        view["bindings"][0]["status"], "untested",
        "A different controller's failure must not become this controller's history: {view}"
    );
}

#[tokio::test]
async fn audit_legacy_roon_zone_reports_work_for_shortcuts() {
    let core = FakeRoonCore::start().await;
    let (state, _dir) = roon_state(&core).await;
    let albums = albums_page(&state).await;
    let pinned = pin_album_row(&state, &albums, 0).await;
    let mut bindings = std::collections::BTreeMap::new();
    bindings.insert(EVENT.to_string(), pinned);
    state
        .shortcuts
        .set_bindings(controller(), bindings)
        .await
        .unwrap();
    state
        .knobs
        .update_status(
            controller(),
            KnobStatusUpdate {
                zone_id: Some(ROON_ZONE.strip_prefix("roon:").unwrap().into()),
                ..Default::default()
            },
        )
        .await;
    let outcome = press(&state, controller(), EVENT).await;
    core.stop().await;
    assert_eq!(
        outcome.code,
        PressCode::Accepted,
        "Normal knob endpoints accept and store this legacy report: {}",
        outcome.message
    );
}

#[tokio::test]
async fn indistinguishable_roon_recordings_refuse_instead_of_guessing_by_position() {
    let core = FakeRoonCore::start().await;
    core.set_children_by_title(
        "Albums",
        vec![
            mock_servers::roon_core::album("Greatest Hits", "Same Artist", &["Edition one"]),
            mock_servers::roon_core::album("Greatest Hits", "Same Artist", &["Edition two"]),
        ],
    )
    .await;
    let (state, _dir) = roon_state(&core).await;
    let albums = albums_page(&state).await;
    let pin = pin_album_row(&state, &albums, 1).await;
    let mut bindings = std::collections::BTreeMap::new();
    bindings.insert(EVENT.to_string(), pin);
    state
        .shortcuts
        .set_bindings(controller(), bindings)
        .await
        .unwrap();
    let before = core.browsed_titles().await.len();
    let outcome = press(&state, controller(), EVENT).await;
    let walked = core.browsed_titles().await[before..].to_vec();
    core.stop().await;
    assert_eq!(outcome.code, PressCode::ItemUnavailable);
    assert!(
        !walked.iter().any(|title| title == "Play Now"),
        "A list position cannot safely choose between indistinguishable recordings: {walked:?}"
    );
}

#[tokio::test]
async fn stopping_lms_during_item_validation_cannot_start_playback_after_shutdown() {
    let h = harness(&[(&format!("lms:{LMS_PLAYER}"), "Living Room")]).await;
    let pin = h.pin_kind_of_blue().await;
    h.bind(&pin).await;
    h.controller_reports_zone(&format!("lms:{LMS_PLAYER}"))
        .await;
    h.mock.clear_commands().await;
    let gate = h.mock.block_command_once("search").await;
    let state = h.state.clone();
    let controller_id = controller().to_string();
    let press_task = tokio::spawn(async move { press(&state, &controller_id, EVENT).await });
    tokio::time::timeout(std::time::Duration::from_secs(1), gate.0.notified())
        .await
        .unwrap();
    let stopped = tokio::time::timeout(std::time::Duration::from_millis(150), h.state.lms.stop())
        .await
        .is_ok();
    gate.1.notify_one();
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let writes = h.playlist_commands(LMS_PLAYER).await;
    press_task.abort();
    h.stop().await;
    assert!(
        stopped,
        "stopping must cancel the validation future instead of waiting for a provider"
    );
    assert!(
        writes.is_empty(),
        "validation completing after shutdown must not write the queue: {writes:?}"
    );
}
