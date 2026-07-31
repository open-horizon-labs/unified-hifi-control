//! Roon protocol integration tests (issue #408).
//!
//! These are the first tests in this repo to assert a Roon **success** path. They
//! drive the real `RoonAdapter` event loop — the same code production runs,
//! including the `pending_browses` / `pending_loads` correlation maps — against
//! `tests/mock_servers/roon_core.rs`, a WebSocket server that speaks Roon's MOO
//! protocol. No Roon Core, no network, no multicast.
//!
//! Read `tests/mock_servers/README.md` before adding to this file. In particular:
//! **green here means "unchanged", not "correct".** The fake's semantics were
//! derived from the pinned `roon_api` fork and from this repo's own adapter, so
//! these tests cannot prove the adapter matches a real Roon Core. What they can and
//! do prove is that a change to the wire mapping is visible.
//!
//! Two tests deliberately pin *defects* rather than desired behaviour, and say so
//! at the assertion: `browse_error_is_delivered_but_the_adapter_drops_it` and
//! `concurrent_browses_one_rejected_leaves_the_rejected_caller_hanging`. Both are
//! #405's subject. When #405 lands, they flip from "hangs" to "returns a typed,
//! recoverable error promptly" — the instrument is here so that flip is provable.

mod mock_servers;

use std::sync::Arc;
use std::time::{Duration, Instant};

use mock_servers::roon_core::{
    album, FakeItem, FakeLibrary, FakeRoonCore, Hint, ItemKeyScope,
};
use roon_api::browse::{BrowseOpts, LoadOpts};
use unified_hifi_control::adapters::roon::{PlayAction, RoonAdapter, SearchSource};
use unified_hifi_control::bus::create_bus;
use unified_hifi_control::knobs::KnobStore;

// =============================================================================
// Harness
// =============================================================================

/// Point the adapter's state-file writes at a throwaway directory, and return it.
///
/// `run_roon_loop` persists Roon pairing state via `get_config_file_path`, which
/// honours `UHC_CONFIG_DIR`. Without this, registering against the fake would write
/// `roon_state.json` into the operator's real config directory.
///
/// The `set_var` happens *inside* the `OnceLock` initializer, so it runs exactly
/// once and every later read of the variable is ordered after it by the `OnceLock`'s
/// own acquire/release. Calling `set_var` per test would race with concurrent
/// `getenv` from the other test threads.
fn isolate_config_dir() -> &'static std::path::Path {
    use std::sync::OnceLock;
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("UHC_CONFIG_DIR", dir.path());
        dir
    })
    .path()
}

/// Start a fake Core and a `RoonAdapter` connected to it, and wait until the
/// adapter reports Browse as available.
async fn connected(core: &FakeRoonCore) -> Arc<RoonAdapter> {
    let _ = isolate_config_dir();

    let bus = create_bus();
    let adapter = Arc::new(RoonAdapter::new_configured(
        bus,
        "http://test.invalid:8088".to_string(),
        KnobStore::new(),
    ));

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
    panic!(
        "adapter never reported Browse connected; requests seen by the fake: {:?}",
        core.requests()
            .await
            .iter()
            .map(|r| r.name.clone())
            .collect::<Vec<_>>()
    );
}

fn browse_at(session: &str, item_key: Option<&str>) -> BrowseOpts {
    BrowseOpts {
        multi_session_key: Some(session.to_string()),
        item_key: item_key.map(str::to_string),
        ..Default::default()
    }
}

fn browse_root(session: &str) -> BrowseOpts {
    BrowseOpts {
        multi_session_key: Some(session.to_string()),
        pop_all: true,
        ..Default::default()
    }
}

fn load_all(session: &str) -> LoadOpts {
    LoadOpts {
        multi_session_key: Some(session.to_string()),
        count: Some(100),
        ..Default::default()
    }
}

// =============================================================================
// The fake is validated by the real client library, not by a hand-written client
// =============================================================================

#[tokio::test]
async fn roon_core_completes_handshake() {
    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;

    let status = adapter.get_status().await;
    assert!(status.connected);
    assert_eq!(status.core_name.as_deref(), Some("Fake Roon Core"));
    assert_eq!(status.core_version.as_deref(), Some("2.0.408"));

    // The handshake order is the fork's, not ours: info (request id 0) then
    // register. If the fake's framing ever breaks, this test fails first.
    let names: Vec<String> = core.requests().await.iter().map(|r| r.name.clone()).collect();
    assert_eq!(names[0], "com.roonlabs.registry:1/info");
    assert_eq!(core.requests().await[0].req_id, 0);
    assert!(names.contains(&"com.roonlabs.registry:1/register".to_string()));

    // Registering makes the adapter persist pairing state. Prove it landed in the
    // throwaway directory: if this ever fails, the suite is writing into the
    // operator's real config directory.
    let state_file = isolate_config_dir()
        .join("unified-hifi")
        .join("roon_state.json");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !state_file.exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        state_file.exists(),
        "expected pairing state at {state_file:?}"
    );
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state_file).unwrap()).unwrap();
    assert_eq!(
        saved["roonstate"]["tokens"]["fake-core-408"],
        "fake-token-408",
        "the token from the fake's Registered reply should be persisted: {saved}"
    );

    core.assert_no_unhandled_requests().await;
    core.stop().await;
}

#[tokio::test]
async fn roon_core_publishes_zones() {
    // A missing required field in the zone JSON makes the fork's deserializer
    // error and the client library swallow the zone silently, so this test is
    // what keeps `default_zone()` honest.
    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;

    let deadline = Instant::now() + Duration::from_secs(5);
    let zones = loop {
        let zones = adapter.get_zones().await;
        if !zones.is_empty() || Instant::now() > deadline {
            break zones;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    assert_eq!(zones.len(), 1, "fake zone was not accepted by roon_api");
    assert_eq!(zones[0].zone_id, "zone_fake_1");
    assert_eq!(zones[0].display_name, "Fake Living Room");
    assert_eq!(zones[0].state, "stopped");
    let volume = zones[0].outputs[0]
        .volume
        .as_ref()
        .expect("output volume should survive deserialization");
    assert_eq!(volume.value, Some(50.0));
    assert_eq!(volume.max, Some(100.0));

    core.stop().await;
}

// =============================================================================
// THE REGRESSION TEST THIS ISSUE EXISTS FOR (#408 acceptance criterion)
// =============================================================================

/// #394's gate: "if the module split had swapped `title` and `subtitle` in Roon's
/// search mapping, none of its 48 contract tests would have noticed."
///
/// This is that test. Every value below is *asymmetric* — the title is never a
/// plausible subtitle and vice versa — so any swap, at any layer between the wire
/// and the adapter's return value, changes an asserted string.
///
/// Verified to bite: swapping the two fields in `SearchResultItem::from`
/// (`src/api/mod.rs:402`) fails `search_results_survive_the_http_boundary`, and
/// swapping them in the adapter fails this test. See the PR body.
#[tokio::test]
async fn search_maps_title_and_subtitle_without_swapping_them() {
    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;

    let results = adapter
        .search("kind of blue", None, Some(10), SearchSource::Library)
        .await
        .expect("search against the fake Core should succeed");

    assert_eq!(results.len(), 1, "expected exactly one match");
    let hit = &results[0];

    // The album name is the title. The artist is the subtitle. Not the reverse.
    assert_eq!(
        hit.title, "Kind of Blue",
        "title must carry the item's title, not its subtitle"
    );
    assert_eq!(
        hit.subtitle.as_deref(),
        Some("Miles Davis"),
        "subtitle must carry the item's subtitle, not its title"
    );

    // The two are distinguishable by shape as well as by value, so a swap cannot
    // pass by coincidence.
    assert_ne!(hit.title, "Miles Davis");
    assert_ne!(hit.subtitle.as_deref(), Some("Kind of Blue"));

    // An item_key came back, and it is opaque: nothing in it is derivable from the
    // title. #396 will wrap this in a ref; today it is what /roon/play_item takes.
    let key = hit.item_key.as_deref().expect("search hit should carry a key");
    assert!(!key.is_empty());
    assert!(!key.to_lowercase().contains("kind of blue"));

    core.assert_no_unhandled_requests().await;
    core.stop().await;
}

/// The same anti-swap assertion one layer up, over HTTP, so the mapping in
/// `SearchResultItem::from` (`src/api/mod.rs:402-421`) is covered too.
///
/// The MCP projection in `src/mcp/` is a third, structurally identical `.map()`
/// (`item.title -> title`, `item.subtitle -> subtitle`). It is deliberately NOT
/// covered here: #394 (PR #404) is rewriting that whole module and owns its
/// harness. With this fake in place that harness can add the same assertion in a
/// few lines. Stated as a gap rather than papered over.
#[tokio::test]
async fn search_results_survive_the_http_boundary() {
    use axum::{body::Body, http::Request, routing::get, Router};
    use tower::ServiceExt;

    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;
    let state = app_state_with(adapter);

    let app = Router::new()
        .route(
            "/roon/search",
            get(unified_hifi_control::api::roon_search_handler),
        )
        .with_state(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/roon/search?q=kind%20of%20blue")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let results = json["results"].as_array().expect("results array");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["title"], "Kind of Blue");
    assert_eq!(results[0]["subtitle"], "Miles Davis");
    assert_eq!(results[0]["hint"], "list");
    assert!(results[0]["item_key"].is_string());

    core.stop().await;
}

/// The third projection of the same two fields:
/// `roon_browse_handler`'s inline `BrowseItemResponse` map (`src/api/mod.rs:672-687`),
/// which is what the web UI reads. Also the end-to-end proof that
/// `POST /roon/browse` works against something that answers like Roon — the route
/// #399 builds its MCP browse contract on top of.
#[tokio::test]
async fn browse_items_survive_the_http_boundary() {
    use axum::{body::Body, http::Request, routing::post, Router};
    use tower::ServiceExt;

    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;
    let state = app_state_with(adapter);

    let app = Router::new()
        .route(
            "/roon/browse",
            post(unified_hifi_control::api::roon_browse_handler),
        )
        .with_state(state);

    // Root, then into Library > Albums, over HTTP with a caller-supplied session
    // key — the flow the web UI uses.
    let post_browse = |app: Router, body: serde_json::Value| async move {
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/roon/browse")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
    };

    let root = post_browse(
        app.clone(),
        serde_json::json!({ "session_key": "http_browse", "pop_all": true }),
    )
    .await;
    assert_eq!(root["action"], "list");
    assert_eq!(root["session_key"], "http_browse");
    assert_eq!(root["list"]["level"], 0);
    let library_key = root["items"][0]["item_key"].as_str().unwrap().to_string();
    assert_eq!(root["items"][0]["title"], "Library");

    let library = post_browse(
        app.clone(),
        serde_json::json!({ "session_key": "http_browse", "item_key": library_key }),
    )
    .await;
    let albums_key = library["items"][2]["item_key"].as_str().unwrap().to_string();

    let albums = post_browse(
        app,
        serde_json::json!({ "session_key": "http_browse", "item_key": albums_key }),
    )
    .await;
    assert_eq!(albums["list"]["title"], "Albums");
    assert_eq!(albums["list"]["level"], 2);
    // The anti-swap assertion, again with asymmetric values.
    assert_eq!(albums["items"][0]["title"], "Sketches of Spain");
    assert_eq!(albums["items"][0]["subtitle"], "Miles Davis");
    assert_eq!(albums["items"][0]["hint"], "list");

    core.stop().await;
}

// =============================================================================
// Search: the multi_session_key mint/consume sequence
// =============================================================================

/// The epic needs this pinned: `search()` mints one `search_{nanos}` session key,
/// uses it for all six requests, and drops it — leaving the returned `item_key`s
/// outliving the only thing that scopes them (#396's third acceptance criterion).
#[tokio::test]
async fn search_mints_one_session_key_and_uses_it_throughout() {
    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;

    adapter
        .search("kind of blue", None, Some(10), SearchSource::Library)
        .await
        .expect("search should succeed");

    let sessions = core.session_keys().await;
    assert_eq!(sessions.len(), 1, "search should mint exactly one session key");
    assert!(
        sessions[0].starts_with("search_"),
        "session key should be search-scoped, got {:?}",
        sessions[0]
    );

    // Six requests, alternating browse and load, all in that one session.
    let browses = core.browse_requests().await;
    let loads = core.load_requests().await;
    assert_eq!(browses.len(), 3, "root, into source, search with input");
    assert_eq!(loads.len(), 3, "one load after each browse");
    for req in browses.iter().chain(loads.iter()) {
        assert_eq!(req.session_key(), Some(sessions[0].as_str()));
    }

    // Request 1: reset to the root. No item key, pop_all set.
    assert!(browses[0].pop_all(), "first browse must pop to the root");
    assert_eq!(browses[0].item_key(), None);
    // Request 2: enter the source by the key the Core minted for it.
    assert_eq!(browses[1].item_key(), core.key_for_title("Library").await.as_deref());
    assert!(!browses[1].pop_all());
    // Request 3: the query is carried as `input` on the Search item, not as a
    // path or a filter.
    assert_eq!(browses[2].input(), Some("kind of blue"));
    assert!(browses[2].item_key().is_some());

    // The fork injects hierarchy=browse on every browse/load (browse.rs:124).
    for req in browses.iter().chain(loads.iter()) {
        assert_eq!(req.body["hierarchy"], "browse");
    }

    core.assert_no_unhandled_requests().await;
    core.stop().await;
}

#[tokio::test]
async fn search_source_selects_a_different_branch() {
    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;

    let tidal = adapter
        .search("blue", None, Some(10), SearchSource::Tidal)
        .await
        .expect("TIDAL search should succeed");
    assert_eq!(tidal.len(), 1);
    assert_eq!(tidal[0].title, "Blue Note Reimagined");

    let library = adapter
        .search("blue", None, Some(10), SearchSource::Library)
        .await
        .expect("library search should succeed");
    assert_eq!(library.len(), 2, "Kind of Blue and Blue Train");

    // The source is chosen by entering the matching root row, so the second
    // browse of each search carries a different item key.
    let browses = core.browse_requests().await;
    assert_eq!(
        browses[1].item_key(),
        core.key_for_title("TIDAL").await.as_deref()
    );
    assert_eq!(
        browses[4].item_key(),
        core.key_for_title("Library").await.as_deref()
    );

    core.stop().await;
}

#[tokio::test]
async fn search_with_no_matches_returns_empty_not_an_error() {
    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;

    let results = adapter
        .search("no such album anywhere", None, Some(10), SearchSource::Library)
        .await
        .expect("an empty result set is not an error");
    assert!(results.is_empty());

    // list.count == 0 short-circuits before the final load, so only three loads
    // happen for a hit and two for a miss.
    assert_eq!(core.load_requests().await.len(), 2);

    core.stop().await;
}

// =============================================================================
// Browse and load across levels, including pop_all
// =============================================================================

#[tokio::test]
async fn browse_walks_two_levels_and_pop_all_returns_to_the_root() {
    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;
    let session = "browse_test_session";

    // Level 0: the browse root.
    let root = adapter.browse(browse_root(session)).await.expect("root browse");
    let root_list = root.list.expect("root browse should carry a list");
    assert_eq!(root_list.level, 0);
    assert_eq!(root_list.count, 4);

    let root_items = adapter.load(load_all(session)).await.expect("root load");
    let titles: Vec<&str> = root_items.items.iter().map(|i| i.title.as_str()).collect();
    assert_eq!(titles, vec!["Library", "TIDAL", "Qobuz", "Settings"]);
    // A row a real Core presents as non-navigable has no key, and the adapter
    // must cope with that rather than assume every row is enterable.
    assert!(root_items.items[3].item_key.is_none());

    let library_key = root_items.items[0].item_key.clone().expect("Library key");

    // Level 1: inside Library.
    let library = adapter
        .browse(browse_at(session, Some(&library_key)))
        .await
        .expect("browse into Library");
    let library_list = library.list.expect("list");
    assert_eq!(library_list.title, "Library");
    assert_eq!(library_list.level, 1);

    let library_items = adapter.load(load_all(session)).await.expect("library load");
    let titles: Vec<&str> = library_items
        .items
        .iter()
        .map(|i| i.title.as_str())
        .collect();
    assert_eq!(titles, vec!["Search", "Artists", "Albums"]);
    // The Search row advertises that it takes input; that is how a client knows
    // to send `input` rather than just entering it.
    assert_eq!(
        library_items.items[0]
            .input_prompt
            .as_ref()
            .map(|p| p.prompt.as_str()),
        Some("Search")
    );

    // Level 2: inside Library > Albums.
    let albums_key = library_items.items[2].item_key.clone().expect("Albums key");
    let albums = adapter
        .browse(browse_at(session, Some(&albums_key)))
        .await
        .expect("browse into Albums");
    assert_eq!(albums.list.expect("list").level, 2);
    assert_eq!(
        core.session_levels(session).await,
        vec!["Explore", "Library", "Albums"]
    );

    // pop_all discards every level but the root — the thing #399's back/pop
    // contract rests on.
    let back = adapter.browse(browse_root(session)).await.expect("pop_all");
    assert_eq!(back.list.expect("list").level, 0);
    assert_eq!(core.session_levels(session).await, vec!["Explore"]);

    core.assert_no_unhandled_requests().await;
    core.stop().await;
}

#[tokio::test]
async fn browse_sessions_are_independent() {
    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;

    adapter.browse(browse_root("session_a")).await.unwrap();
    adapter.browse(browse_root("session_b")).await.unwrap();
    let root = adapter.load(load_all("session_a")).await.unwrap();
    let library_key = root.items[0].item_key.clone().unwrap();

    adapter
        .browse(browse_at("session_a", Some(&library_key)))
        .await
        .unwrap();

    // Descending in one session must not move the other; this is the collision
    // the adapter's `multi_session_key` requirement exists to prevent.
    assert_eq!(
        core.session_levels("session_a").await,
        vec!["Explore", "Library"]
    );
    assert_eq!(core.session_levels("session_b").await, vec!["Explore"]);

    core.stop().await;
}

#[tokio::test]
async fn load_pages_within_a_level_and_reports_the_total() {
    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;
    let session = "paging_session";

    adapter.browse(browse_root(session)).await.unwrap();
    let root = adapter.load(load_all(session)).await.unwrap();
    let library_key = root.items[0].item_key.clone().unwrap();
    adapter
        .browse(browse_at(session, Some(&library_key)))
        .await
        .unwrap();
    let library = adapter.load(load_all(session)).await.unwrap();
    let artists_key = library.items[1].item_key.clone().unwrap();
    adapter
        .browse(browse_at(session, Some(&artists_key)))
        .await
        .unwrap();

    // 25 artists, requested ten at a time.
    let first = adapter
        .load(LoadOpts {
            multi_session_key: Some(session.to_string()),
            count: Some(10),
            offset: 0,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(first.items.len(), 10);
    assert_eq!(first.offset, 0);
    assert_eq!(first.list.count, 25, "total count, not page size");
    assert_eq!(first.items[0].title, "Artist 01");

    let third = adapter
        .load(LoadOpts {
            multi_session_key: Some(session.to_string()),
            count: Some(10),
            offset: 20,
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(third.items.len(), 5, "last page is short, not padded");
    assert_eq!(third.offset, 20);
    assert_eq!(third.items[0].title, "Artist 21");
    assert_eq!(third.list.count, 25);

    // Off the end is empty, not an error and not a wrap-around.
    let past_end = adapter
        .load(LoadOpts {
            multi_session_key: Some(session.to_string()),
            count: Some(10),
            offset: 99,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(past_end.items.is_empty());

    core.stop().await;
}

#[tokio::test]
async fn load_without_a_browse_is_refused_not_answered_with_stale_items() {
    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;

    // The fake answers InvalidLevels. Today the adapter's catch-all drops that,
    // so the caller waits out BROWSE_TIMEOUT; #405 covers the routing fix. What
    // this test pins is that the *Core* refused, which is visible in its
    // response log even though the adapter cannot see it yet.
    let pending = tokio::time::timeout(
        Duration::from_millis(750),
        adapter.load(load_all("never_browsed")),
    )
    .await;
    assert!(
        pending.is_err(),
        "expected the load to still be pending: today's adapter drops Parsed::Error"
    );
    assert!(
        core.response_names().await.contains(&"InvalidLevels".to_string()),
        "the fake Core must have refused: {:?}",
        core.response_names().await
    );

    core.stop().await;
}

// =============================================================================
// play_item and search_and_play resolve to a playable action
// =============================================================================

#[tokio::test]
async fn play_item_resolves_an_item_key_to_a_play_action() {
    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;

    // A client's real flow: search, hold the key, play it.
    let results = adapter
        .search("kind of blue", None, Some(10), SearchSource::Library)
        .await
        .unwrap();
    let key = results[0].item_key.clone().unwrap();

    let message = adapter
        .play_item(&key, "roon:zone_fake_1", PlayAction::Play)
        .await
        .expect("play_item should resolve to a playable action");

    // Recorded behaviour, not desired behaviour: the message names the action
    // container ("Play Album") rather than the album, because `play_item` takes
    // its title from the playable item it lands on
    // (src/adapters/roon.rs:1140-1147). Cosmetic, and filed separately rather
    // than fixed here — this issue builds the instrument.
    assert_eq!(message, "Play Now: Play Album");

    // The load-bearing assertion: the Core was actually asked to invoke Play Now,
    // reached by entering the album and then its action list.
    let walked = core.browsed_titles().await;
    assert_eq!(
        walked,
        vec!["Library", "Search", "Kind of Blue", "Play Album", "Play Now"],
        "play_item should enter the item, find its action list, and invoke Play Now"
    );

    // The zone id reached the Core with the "roon:" prefix stripped.
    let browses = core.browse_requests().await;
    let zone_ids: Vec<Option<String>> = browses
        .iter()
        .map(|r| {
            r.body
                .get("zone_or_output_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .collect();
    assert!(zone_ids.contains(&Some("zone_fake_1".to_string())));
    assert!(!zone_ids.contains(&Some("roon:zone_fake_1".to_string())));

    core.assert_no_unhandled_requests().await;
    core.stop().await;
}

#[tokio::test]
async fn play_item_mints_a_session_key_unrelated_to_the_one_that_minted_the_key() {
    // This is the empirical unknown #405 must settle against the operator's rig:
    // `play_item` browses a caller's item_key inside a *fresh* session, so the
    // repo already assumes keys are portable across `multi_session_key`s. The
    // fake defaults to ItemKeyScope::Global, i.e. it assumes the same thing —
    // so this test documents the assumption, it does not verify it.
    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;

    let results = adapter
        .search("kind of blue", None, Some(10), SearchSource::Library)
        .await
        .unwrap();
    let key = results[0].item_key.clone().unwrap();

    adapter
        .play_item(&key, "roon:zone_fake_1", PlayAction::Play)
        .await
        .unwrap();

    let sessions = core.session_keys().await;
    assert_eq!(sessions.len(), 2, "search and play_item use separate sessions");
    assert!(sessions[0].starts_with("search_"));
    assert!(sessions[1].starts_with("play_item_"));
}

#[tokio::test]
async fn a_foreign_item_key_is_rejected_when_keys_are_session_scoped() {
    // Flip the unverified assumption and the failure is instant and specific:
    // the Core answers InvalidItemKey. If the operator's rig says Roon keys are
    // session-scoped, `/roon/play_item` is broken and this is the shape of the
    // break.
    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;

    let results = adapter
        .search("kind of blue", None, Some(10), SearchSource::Library)
        .await
        .unwrap();
    let key = results[0].item_key.clone().unwrap();

    core.set_item_key_scope(ItemKeyScope::PerSession).await;

    let outcome = tokio::time::timeout(
        Duration::from_millis(750),
        adapter.play_item(&key, "roon:zone_fake_1", PlayAction::Play),
    )
    .await;
    assert!(
        outcome.is_err(),
        "today a rejected key hangs until BROWSE_TIMEOUT rather than failing fast"
    );
    assert!(
        core.response_names()
            .await
            .contains(&"InvalidItemKey".to_string()),
        "the Core should have rejected the foreign key"
    );

    core.stop().await;
}

#[tokio::test]
async fn search_and_play_navigates_into_a_result_to_find_a_playable_action() {
    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;

    let message = adapter
        .search_and_play(
            "kind of blue",
            "roon:zone_fake_1",
            SearchSource::Library,
            PlayAction::Play,
        )
        .await
        .expect("search_and_play should find a playable action");
    assert_eq!(message, "Play Now: Kind of Blue");

    assert_eq!(
        core.browsed_titles().await,
        vec!["Library", "Search", "Kind of Blue", "Play Album", "Play Now"],
        "search_and_play should navigate into the hit before invoking an action"
    );

    let sessions = core.session_keys().await;
    assert_eq!(sessions.len(), 1);
    assert!(sessions[0].starts_with("play_"));

    core.assert_no_unhandled_requests().await;
    core.stop().await;
}

#[tokio::test]
async fn queue_and_radio_invoke_different_actions_than_play() {
    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;

    let queued = adapter
        .search_and_play(
            "kind of blue",
            "roon:zone_fake_1",
            SearchSource::Library,
            PlayAction::Queue,
        )
        .await
        .unwrap();
    assert_eq!(queued, "Queue: Kind of Blue");

    let invoked = core.browsed_titles().await;
    assert!(invoked.contains(&"Queue".to_string()), "got {invoked:?}");
    assert!(
        !invoked.contains(&"Play Now".to_string()),
        "action='queue' must not invoke Play Now: {invoked:?}"
    );

    core.stop().await;
}

#[tokio::test]
async fn an_action_the_core_does_not_offer_is_reported_with_what_is_available() {
    // A library whose album page offers only Play Now: asking to queue must fail
    // with a message that lists the real options, because that text is what a
    // caller (and an AI client) reads to recover.
    let mut library = FakeLibrary::standard();
    library.search_results.insert(
        "Library".to_string(),
        vec![FakeItem::list("Solo Album")
            .with_subtitle("Some Artist")
            .with_children(vec![FakeItem::action_list("Play Album")
                .with_children(vec![FakeItem::action("Play Now")])])],
    );

    let core = FakeRoonCore::start_with(library).await;
    let adapter = connected(&core).await;

    let error = adapter
        .search_and_play(
            "solo",
            "roon:zone_fake_1",
            SearchSource::Library,
            PlayAction::Radio,
        )
        .await
        .expect_err("Start Radio is not offered");
    let text = error.to_string();
    assert!(text.contains("Start Radio"), "got {text}");
    assert!(text.contains("Play Now"), "should list what is available: {text}");

    core.stop().await;
}

// =============================================================================
// On-demand Parsed::Error(BrowseInvalidItemKey) — what #405 needs
// =============================================================================

/// The fake emits `InvalidItemKey` on demand and the fork turns it into
/// `Parsed::Error(RoonApiError::BrowseInvalidItemKey((req_id, session_key)))`.
///
/// **This test pins a defect.** `src/adapters/roon.rs`'s event-loop catch-all
/// (`_ => {}`) discards `Parsed::Error`, so the pending oneshot is never resolved
/// and the caller waits out the 10s `BROWSE_TIMEOUT`. The assertions below say
/// "still pending after 750ms" and "the Core did reply" — together they prove the
/// error was delivered and dropped, which no test could show before.
///
/// When #405 lands, invert this: the browse should return a typed, recoverable
/// error promptly. Do not delete the test; change the assertion.
#[tokio::test]
async fn browse_error_is_delivered_but_the_adapter_drops_it() {
    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;

    let session = "error_session";
    adapter.browse(browse_root(session)).await.unwrap();
    let root = adapter.load(load_all(session)).await.unwrap();
    let library_key = root.items[0].item_key.clone().unwrap();

    core.reject_item_key(&library_key).await;

    let started = Instant::now();
    let pending = tokio::time::timeout(
        Duration::from_millis(750),
        adapter.browse(browse_at(session, Some(&library_key))),
    )
    .await;

    assert!(
        pending.is_err(),
        "TODAY: a Core-rejected item key hangs until BROWSE_TIMEOUT (10s). \
         When #405 routes Parsed::Error, flip this to assert a prompt typed error."
    );
    assert!(started.elapsed() < Duration::from_secs(2));

    // The Core answered immediately, and named the error. So the 10s the caller
    // waits is entirely UHC's, not Roon's.
    assert!(
        core.response_names()
            .await
            .contains(&"InvalidItemKey".to_string()),
        "the fake must have emitted InvalidItemKey: {:?}",
        core.response_names().await
    );

    core.stop().await;
}

/// #405's acceptance criterion: "a test with several in-flight browse/load
/// requests, where one is rejected, proves the right waiter is resolved and the
/// others complete normally."
///
/// The instrument for that is here. The rejected waiter's *current* fate is a
/// hang, and that is what is asserted — but the two healthy requests are proven
/// to complete, in their own sessions, while the rejected one is outstanding.
/// The pending maps are keyed by `req_id` and scanned by session key
/// (`src/adapters/roon.rs`), so a correlation bug in #405's error arm would show
/// up here as a healthy browse resolving with the wrong list, or not at all.
#[tokio::test]
async fn concurrent_browses_one_rejected_leaves_the_rejected_caller_hanging() {
    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;

    // Prime three independent sessions and learn two distinct keys.
    for session in ["c_a", "c_b", "c_bad"] {
        adapter.browse(browse_root(session)).await.unwrap();
    }
    let root = adapter.load(load_all("c_a")).await.unwrap();
    let library_key = root.items[0].item_key.clone().unwrap();
    let tidal_key = root.items[1].item_key.clone().unwrap();

    core.reject_item_key(&tidal_key).await;
    // Hold every response back so all three browses really are in flight, and
    // make the rejected one answer *first* so the error cannot be mistaken for
    // "whatever arrived last".
    core.set_delay(Duration::from_millis(300)).await;
    core.set_delay_for_item_key(&tidal_key, Duration::from_millis(50))
        .await;

    let a = adapter.browse(browse_at("c_a", Some(&library_key)));
    let b = adapter.browse(browse_at("c_b", Some(&library_key)));
    let bad = tokio::time::timeout(
        Duration::from_millis(1_500),
        adapter.browse(browse_at("c_bad", Some(&tidal_key))),
    );

    let (a, b, bad) = tokio::join!(a, b, bad);

    // The healthy requests resolved, each with its own level, unaffected by the
    // rejection that landed between them.
    assert_eq!(a.expect("session c_a").list.expect("list").title, "Library");
    assert_eq!(b.expect("session c_b").list.expect("list").title, "Library");
    assert_eq!(core.session_levels("c_a").await, vec!["Explore", "Library"]);
    assert_eq!(core.session_levels("c_b").await, vec!["Explore", "Library"]);
    assert_eq!(
        core.session_levels("c_bad").await,
        vec!["Explore"],
        "the rejected browse must not have advanced its session"
    );

    assert!(
        bad.is_err(),
        "TODAY: the rejected browse hangs. #405 must make this a prompt typed \
         error while leaving the other two exactly as asserted above."
    );

    let names = core.response_names().await;
    assert_eq!(
        names.iter().filter(|n| *n == "InvalidItemKey").count(),
        1,
        "exactly one request should have been rejected: {names:?}"
    );

    core.stop().await;
}

// =============================================================================
// Drift guard
// =============================================================================

/// The fake answers `InvalidRequest` to anything it does not model and records it,
/// so an adapter that grows a new Roon call fails a test instead of hanging for
/// ten seconds. This is the `MockHqpServer` lesson from #394: a mock that quietly
/// mismatches its adapter reads as coverage.
#[tokio::test]
async fn the_fake_models_everything_the_adapter_sends() {
    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;

    adapter
        .search("kind of blue", None, Some(10), SearchSource::Library)
        .await
        .unwrap();
    adapter
        .search_and_play(
            "blue train",
            "roon:zone_fake_1",
            SearchSource::Library,
            PlayAction::Play,
        )
        .await
        .unwrap();
    let session = "drift";
    adapter.browse(browse_root(session)).await.unwrap();
    adapter.load(load_all(session)).await.unwrap();

    core.assert_no_unhandled_requests().await;

    // And the set of request names is closed: exactly these, nothing else.
    let mut names: Vec<String> = core
        .requests()
        .await
        .iter()
        .map(|r| r.name.clone())
        .collect();
    names.sort();
    names.dedup();
    assert_eq!(
        names,
        vec![
            "com.roonlabs.browse:1/browse",
            "com.roonlabs.browse:1/load",
            "com.roonlabs.registry:1/info",
            "com.roonlabs.registry:1/register",
            "com.roonlabs.transport:2/subscribe_zones",
        ],
        "the adapter's Roon protocol surface changed"
    );

    core.stop().await;
}

/// Sanity: `album()` and the `FakeItem` builders compose a tree the adapter can
/// walk, so a test needing a different library shape can build one without
/// touching the fake's protocol code.
#[tokio::test]
async fn a_custom_library_is_browsable() {
    let mut library = FakeLibrary::standard();
    library.root_items = vec![FakeItem::list("Library").with_children(vec![
        FakeItem::list("Search").searchable("Search"),
        FakeItem::list("Genres").with_children(vec![
            FakeItem::list("Jazz").with_hint(Hint::List),
            FakeItem::list("Ambient"),
        ]),
    ])];
    library
        .search_results
        .insert("Library".to_string(), vec![album("Music for Airports", "Brian Eno", &["1/1"])]);

    let core = FakeRoonCore::start_with(library).await;
    let adapter = connected(&core).await;

    let session = "custom";
    adapter.browse(browse_root(session)).await.unwrap();
    let root = adapter.load(load_all(session)).await.unwrap();
    assert_eq!(root.items.len(), 1);
    let library_key = root.items[0].item_key.clone().unwrap();
    adapter
        .browse(browse_at(session, Some(&library_key)))
        .await
        .unwrap();
    let inner = adapter.load(load_all(session)).await.unwrap();
    assert_eq!(
        inner
            .items
            .iter()
            .map(|i| i.title.as_str())
            .collect::<Vec<_>>(),
        vec!["Search", "Genres"]
    );

    let hits = adapter
        .search("airports", None, Some(10), SearchSource::Library)
        .await
        .unwrap();
    assert_eq!(hits[0].title, "Music for Airports");
    assert_eq!(hits[0].subtitle.as_deref(), Some("Brian Eno"));

    core.stop().await;
}

// =============================================================================
// AppState for the HTTP-boundary test
// =============================================================================

fn app_state_with(roon: Arc<RoonAdapter>) -> unified_hifi_control::api::AppState {
    use unified_hifi_control::adapters::hqplayer::{HqpInstanceManager, HqpZoneLinkService};
    use unified_hifi_control::adapters::lms::LmsAdapter;
    use unified_hifi_control::adapters::openhome::OpenHomeAdapter;
    use unified_hifi_control::adapters::upnp::UPnPAdapter;
    use unified_hifi_control::adapters::Startable;
    use unified_hifi_control::aggregator::ZoneAggregator;
    use unified_hifi_control::api::AppState;
    use unified_hifi_control::coordinator::AdapterCoordinator;

    let bus = create_bus();
    let hqp_instances = Arc::new(HqpInstanceManager::new(bus.clone()));
    let hqplayer = futures::executor::block_on(hqp_instances.get_default());
    let hqp_zone_links = Arc::new(HqpZoneLinkService::new(hqp_instances.clone()));
    let lms = Arc::new(LmsAdapter::new(bus.clone()));
    let openhome = Arc::new(OpenHomeAdapter::new(bus.clone()));
    let upnp = Arc::new(UPnPAdapter::new(bus.clone()));
    let startable: Vec<Arc<dyn Startable>> = vec![roon.clone()];

    AppState::new(
        roon,
        hqplayer,
        hqp_instances,
        hqp_zone_links,
        lms,
        openhome,
        upnp,
        KnobStore::new(),
        bus.clone(),
        Arc::new(ZoneAggregator::new(bus.clone())),
        Arc::new(AdapterCoordinator::new(bus)),
        startable,
        Instant::now(),
        tokio_util::sync::CancellationToken::new(),
    )
}
