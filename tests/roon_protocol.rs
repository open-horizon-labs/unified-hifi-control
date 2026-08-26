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
//!
//! One test has already made that trip. #408 shipped
//! `concurrent_browses_in_one_session_cross_deliver_their_results`, which asserted
//! the cross-delivery defect was *present* and told its successor to invert it. #416
//! did exactly that: see the "Same-session concurrency" section at the end of this
//! file, where three tests now assert correct correlation and one names the hazard
//! correlation cannot remove.

mod mock_servers;

use std::sync::Arc;
use std::time::{Duration, Instant};

use mock_servers::roon_core::{
    album, album_live, artist_live, playlist, playlist_live, radio_station, zone_with_grouping,
    FakeItem, FakeLibrary, FakeRoonCore, Hint, ItemKeyScope,
};
use roon_api::browse::{BrowseOpts, LoadOpts};
use unified_hifi_control::adapters::roon::{PlayAction, RoonAdapter, SearchSource};
use unified_hifi_control::bus::create_bus;
use unified_hifi_control::knobs::KnobStore;

// =============================================================================
// Harness
// =============================================================================

/// Give one test's adapter its own throwaway pairing-state file (issue #554).
///
/// `run_roon_loop` persists Roon pairing state by read-modify-writing a JSON file.
/// Earlier, every `RoonAdapter` in this binary shared one process-global
/// `UHC_CONFIG_DIR`, so they all read-modify-wrote the *same* `roon_state.json`.
/// Two adapters registering concurrently could interleave load->save and
/// permanently drop each other's token — the handshake test polls 2s for its
/// token and that drop is not a slow update, it is a lost one.
///
/// `RoonAdapter::with_state_path_for_tests` now lets each adapter own a private
/// file instead, so this binary's ~70 tests can run fully concurrently without
/// racing over shared state. The `TempDir` is intentionally leaked (not stored)
/// so the file outlives the adapter for the rest of the test binary's run; CI
/// runners and `cargo test`'s tmp dirs are ephemeral regardless.
fn private_state_path() -> std::path::PathBuf {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("roon_state.json");
    std::mem::forget(dir);
    path
}

/// Start a fake Core and a `RoonAdapter` connected to it, and wait until the
/// adapter reports Browse as available.
async fn connected(core: &FakeRoonCore) -> Arc<RoonAdapter> {
    connected_at(core, private_state_path()).await.0
}

/// Like [`connected`], but also returns the pairing-state file path the adapter
/// was configured with, for tests that need to assert on its contents directly.
async fn connected_at(
    core: &FakeRoonCore,
    state_path: std::path::PathBuf,
) -> (Arc<RoonAdapter>, std::path::PathBuf) {
    let bus = create_bus();
    let adapter = Arc::new(
        RoonAdapter::new_configured(
            bus,
            "http://test.invalid:8088".to_string(),
            KnobStore::new(),
        )
        .with_state_path_for_tests(state_path.clone()),
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
            return (adapter, state_path);
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

/// Load a named level of a session's stack. `None` means "wherever the session is",
/// which is what every caller in this repo asks for; naming a level is how #416's
/// load-side correlation test keeps three concurrent loads distinguishable.
fn load_at_level(session: &str, level: Option<u32>) -> LoadOpts {
    LoadOpts {
        multi_session_key: Some(session.to_string()),
        level,
        count: Some(100),
        ..Default::default()
    }
}

/// What the adapter did with a rejection the Core definitely sent.
#[derive(Debug, PartialEq, Eq)]
enum RejectionOutcome {
    /// The rejection was routed to the waiting caller, promptly (#405 / PR #412).
    RoutedToCaller,
    /// The rejection was dropped; the caller is still waiting for its 10s timeout.
    /// This is `v3` before #405.
    DroppedByAdapter,
}

/// Await a browse-backed call that the Core has been told to reject, and classify
/// what the adapter did — enforcing, in both cases, the invariants that hold
/// regardless of whether #405 has landed.
///
/// This is deliberately not "assert it hangs". #405 (PR #412) routes
/// `Parsed::Error` into the pending maps, and this suite has to be correct on
/// `v3` both before and after that merges — a test that pinned the hang would
/// turn red the moment the defect was fixed, which is the wrong signal from a
/// test whose whole job is to be the end-to-end proof of the fix.
///
/// What is enforced either way:
/// * a rejected request **never** resolves as success;
/// * if it does resolve, it resolves *promptly* (well inside `BROWSE_TIMEOUT`),
///   the message does not read as a timeout, and it names the browse session.
///
/// The `session` argument is asserted against the message because #405 carries
/// the `multi_session_key` from the Core's error payload. When #412 is on the
/// base branch, tighten this to `RoonBrowseError::from_error(&e)` and assert
/// `kind == InvalidItemKey` directly; the string checks are the strongest
/// assertions expressible without that type.
async fn classify_rejection<T: std::fmt::Debug>(
    fut: impl std::future::Future<Output = anyhow::Result<T>>,
    session: &str,
) -> RejectionOutcome {
    const BOUND: Duration = Duration::from_millis(750);
    match tokio::time::timeout(BOUND, fut).await {
        Ok(Ok(unexpected)) => {
            panic!("a Core-rejected item key must never resolve as success, got {unexpected:?}")
        }
        Ok(Err(e)) => {
            let msg = e.to_string();
            assert!(
                !msg.to_lowercase().contains("timed out")
                    && !msg.to_lowercase().contains("timeout"),
                "a Core rejection must be distinguishable from an unreachable Core, \
                 got {msg:?}"
            );
            assert!(
                msg.contains(session),
                "the rejection should name the browse session it happened in, got {msg:?}"
            );
            RejectionOutcome::RoutedToCaller
        }
        Err(_) => RejectionOutcome::DroppedByAdapter,
    }
}

// =============================================================================
// The fake is validated by the real client library, not by a hand-written client
// =============================================================================

#[tokio::test]
async fn roon_core_completes_handshake() {
    let core = FakeRoonCore::start().await;
    let (adapter, state_file) = connected_at(&core, private_state_path()).await;

    let status = adapter.get_status().await;
    assert!(status.connected);
    assert_eq!(status.core_name.as_deref(), Some("Fake Roon Core"));
    assert_eq!(status.core_version.as_deref(), Some("2.0.408"));

    // The handshake order is the fork's, not ours: info (request id 0) then
    // register. If the fake's framing ever breaks, this test fails first.
    let names: Vec<String> = core
        .requests()
        .await
        .iter()
        .map(|r| r.name.clone())
        .collect();
    assert_eq!(names[0], "com.roonlabs.registry:1/info");
    assert_eq!(core.requests().await[0].req_id, 0);
    assert!(names.contains(&"com.roonlabs.registry:1/register".to_string()));

    // Registering makes the adapter persist pairing state. Prove it landed in the
    // throwaway, adapter-private file: if this ever fails, the suite is writing
    // into the operator's real config directory.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !state_file.exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        state_file.exists(),
        "expected pairing state at {state_file:?}"
    );
    // This adapter owns its state file exclusively (issue #554), so a partial
    // write can only be this adapter's own in-flight save, not another test's.
    // Retry briefly until it parses and carries this Core's own id.
    let core_id = core.core_id().await;
    let deadline = Instant::now() + Duration::from_secs(2);
    let saved = loop {
        let parsed = std::fs::read_to_string(&state_file)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .filter(|v| v["roonstate"]["tokens"][&core_id].is_string());
        match parsed {
            Some(v) => break v,
            None if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await
            }
            None => panic!("pairing state never carried a token for {core_id}"),
        }
    };
    assert_eq!(saved["roonstate"]["tokens"][&core_id], core.token());

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
    let key = hit
        .item_key
        .as_deref()
        .expect("search hit should carry a key");
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
    let state = app_state_with(adapter).await;

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
    let state = app_state_with(adapter).await;

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
    let albums_key = library["items"][2]["item_key"]
        .as_str()
        .unwrap()
        .to_string();

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
    assert_eq!(
        sessions.len(),
        1,
        "search should mint exactly one session key"
    );
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
    assert_eq!(
        browses[1].item_key(),
        core.key_for_title("Library").await.as_deref()
    );
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
        .search(
            "no such album anywhere",
            None,
            Some(10),
            SearchSource::Library,
        )
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
    let root = adapter
        .browse(browse_root(session))
        .await
        .expect("root browse");
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

    // The fake answers InvalidLevels, correlated to the load's own req_id and
    // session. Whether the caller ever hears about it depends on #405.
    let outcome =
        classify_rejection(adapter.load(load_all("never_browsed")), "never_browsed").await;
    println!("load without a browse: {outcome:?}");

    let errors = core.errors_sent().await;
    assert_eq!(errors.len(), 1, "exactly one refusal: {errors:?}");
    assert_eq!(errors[0].1, "InvalidLevels");
    assert_eq!(errors[0].2, "never_browsed");

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
        vec![
            "Library",
            "Search",
            "Kind of Blue",
            "Play Album",
            "Play Now"
        ],
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
    assert_eq!(
        sessions.len(),
        2,
        "search and play_item use separate sessions"
    );
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

    // play_item mints its own session, so the rejection lands in that one.
    let outcome = classify_rejection(
        adapter.play_item(&key, "roon:zone_fake_1", PlayAction::Play),
        "play_item_",
    )
    .await;
    println!("play_item against a foreign key: {outcome:?}");

    let errors = core.errors_sent().await;
    assert_eq!(errors.len(), 1, "exactly one refusal: {errors:?}");
    assert_eq!(errors[0].1, "InvalidItemKey");
    assert!(
        errors[0].2.starts_with("play_item_"),
        "the refusal should name play_item's own session: {errors:?}"
    );

    core.stop().await;
}

/// The live-observed flavor of session scoping (#593): a foreign key is not
/// rejected at all -- the Core answers *successfully* with the root list.
/// This proves the `PerSessionSilentRootReset` knob actually fires, so the
/// #593 collection pins that run under it are not vacuously green: the fixed
/// adapter never trips it precisely because it stays in-session.
#[tokio::test]
async fn a_foreign_item_key_silently_answers_the_root_when_session_scoping_resets() {
    let core = FakeRoonCore::start().await;
    core.set_item_key_scope(ItemKeyScope::PerSessionSilentRootReset)
        .await;
    let adapter = connected(&core).await;

    // Session A: browse the root and mint a key.
    adapter
        .browse(BrowseOpts {
            multi_session_key: Some("session_a".into()),
            pop_all: true,
            ..Default::default()
        })
        .await
        .unwrap();
    let root = adapter
        .load(LoadOpts {
            multi_session_key: Some("session_a".into()),
            count: Some(10),
            ..Default::default()
        })
        .await
        .unwrap();
    let library_key = root
        .items
        .iter()
        .find(|i| i.title == "Library")
        .and_then(|i| i.item_key.clone())
        .unwrap();

    // Session B browses session A's key: no error, but the answer is the
    // ROOT list -- the exact silent misdirection the operator's Core showed
    // (#593's read-only probe).
    let result = adapter
        .browse(BrowseOpts {
            multi_session_key: Some("session_b".into()),
            item_key: Some(library_key),
            ..Default::default()
        })
        .await
        .expect("the reset is silent: no error surfaces anywhere");
    let list = result.list.expect("the reset answers with a list");
    assert_eq!(
        list.title, "Explore",
        "a foreign key lands the session back at the root, not in Library"
    );
    assert_eq!(list.level, 0, "root level, not one level down");
    assert!(
        core.errors_sent().await.is_empty(),
        "no refusal is ever sent -- that silence is the hazard"
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
        vec![
            "Library",
            "Search",
            "Kind of Blue",
            "Play Album",
            "Play Now"
        ],
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
    assert!(
        text.contains("Play Now"),
        "should list what is available: {text}"
    );

    core.stop().await;
}

// =============================================================================
// On-demand Parsed::Error(BrowseInvalidItemKey) — what #405 needs
// =============================================================================

/// The fake emits `InvalidItemKey` on demand and the fork turns it into
/// `Parsed::Error(RoonApiError::BrowseInvalidItemKey((req_id, session_key)))`.
///
/// This is the end-to-end proof #405 (PR #412) asked for: #412's own correlation
/// tests are unit-scoped precisely because nothing could drive the wire path.
///
/// On `v3` before #405 the event-loop catch-all discards `Parsed::Error`, so the
/// caller waits out the 10s `BROWSE_TIMEOUT`; with #405 it gets a typed rejection
/// promptly. `classify_rejection` enforces what must hold either way — never a
/// silent success, and if it does resolve, promptly and not looking like a timeout —
/// and reports which of the two it saw. So this test is correct on both bases and
/// does not have to be inverted on merge.
///
/// What it pins unconditionally is the wire half: the Core answered, named the
/// error, and correlated it to the rejected request's own `req_id` and session key.
#[tokio::test]
async fn a_rejected_item_key_is_answered_immediately_and_correlated() {
    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;

    let session = "error_session";
    adapter.browse(browse_root(session)).await.unwrap();
    let root = adapter.load(load_all(session)).await.unwrap();
    let library_key = root.items[0].item_key.clone().unwrap();

    core.reject_item_key(&library_key).await;

    let started = Instant::now();
    let outcome = classify_rejection(
        adapter.browse(browse_at(session, Some(&library_key))),
        session,
    )
    .await;
    println!("rejected browse: {outcome:?}");
    assert!(started.elapsed() < Duration::from_secs(2));

    // The Core answered immediately, named the error, and correlated it to the
    // right request id *and* the right session key — which is precisely the
    // `Parsed::Error(BrowseInvalidItemKey((req_id, multi_session_key)))` payload the
    // adapter throws away. So the 10s the caller waits is entirely UHC's.
    let rejected_req_id = core
        .browse_requests()
        .await
        .into_iter()
        .find(|r| r.item_key() == Some(library_key.as_str()) && r.session_key() == Some(session))
        .expect("the rejected browse should be in the request log")
        .req_id;
    assert_eq!(
        core.errors_sent().await,
        vec![(
            rejected_req_id,
            "InvalidItemKey".to_string(),
            session.to_string()
        )],
        "the refusal must carry the rejected request's own id and session key"
    );

    core.stop().await;
}

/// #405's acceptance criterion, end to end: "a test with several in-flight
/// browse/load requests, where one is rejected, proves the right waiter is resolved
/// and the others complete normally."
///
/// Three browses in three sessions, the rejected one answered *first* (via a
/// per-key delay) so its error cannot be mistaken for "whatever arrived last".
/// The two healthy ones must resolve with their own correct lists whether or not
/// #405 has landed — a correlation bug in #412's error arm shows up here as a
/// healthy browse resolving with the wrong list, or not at all.
#[tokio::test]
async fn concurrent_browses_with_one_rejected_do_not_disturb_the_others() {
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
    let bad = classify_rejection(
        adapter.browse(browse_at("c_bad", Some(&tidal_key))),
        "c_bad",
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

    println!("rejected browse among three in flight: {bad:?}");

    // Exactly one refusal, and it named the rejected request's own session — not
    // one of the two healthy ones. That pairing is the whole correlation claim.
    let errors = core.errors_sent().await;
    assert_eq!(errors.len(), 1, "exactly one refusal: {errors:?}");
    assert_eq!(errors[0].1, "InvalidItemKey");
    assert_eq!(errors[0].2, "c_bad");

    core.stop().await;
}

/// **The unverified assumption #405's PR handed to this issue.** The fork matches
/// four literal error names against `msg["name"]`; anything else becomes
/// `Parsed::None`, which the fork drops. So if a real Roon Core spells an error
/// differently, the caller times out exactly as it did before #405 — and every
/// test in this file stays green, because this fake sends the fork's own literals.
///
/// Corroboration, chased rather than assumed: `InvalidItemKey` **is** real, recorded
/// verbatim off Roon Cores as `MOO/1 COMPLETE InvalidItemKey`
/// (`home-assistant/core#137605`). `InvalidLevels`, `UnexpectedError` and
/// `ZoneNotFound` are **not** corroborated anywhere — RoonLabs' published browse API
/// documents errors only as "an error code or false if no error", and none of the
/// three appears in `node-roon-api`.
///
/// So this test pins the *consequence* of being wrong rather than pretending to have
/// checked: with an unrecognised name the Core answers instantly and the caller still
/// waits out `BROWSE_TIMEOUT`. It passes with #405 applied too — routing the error
/// does not help when the fork never recognises it.
///
/// If someone reaches a real Core: confirm the other three names, and if any differs,
/// this is the failure you will be looking at.
#[tokio::test]
async fn an_unrecognised_error_name_degrades_to_an_indistinguishable_timeout() {
    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;

    let session = "unknown_error_name";
    adapter.browse(browse_root(session)).await.unwrap();
    let root = adapter.load(load_all(session)).await.unwrap();
    let library_key = root.items[0].item_key.clone().unwrap();

    core.reject_item_key_with_name(&library_key, "ItemKeyNoLongerValid")
        .await;

    let pending = tokio::time::timeout(
        Duration::from_millis(750),
        adapter.browse(browse_at(session, Some(&library_key))),
    )
    .await;
    assert!(
        pending.is_err(),
        "an error name the fork does not recognise is dropped inside the dependency, \
         so the caller can only time out. This is the cost of the four literals in \
         browse.rs being unverified against a real Core."
    );

    // The Core did answer, instantly, with that name.
    assert_eq!(
        core.errors_sent().await,
        vec![(
            core.browse_requests()
                .await
                .into_iter()
                .find(|r| r.item_key() == Some(library_key.as_str())
                    && r.session_key() == Some(session))
                .expect("the rejected browse should be logged")
                .req_id,
            "ItemKeyNoLongerValid".to_string(),
            session.to_string()
        )]
    );

    core.stop().await;
}

/// The four names are a contract with the pinned fork. If a fork bump renames one,
/// fail here rather than as an unexplained timeout somewhere else.
#[test]
fn the_forks_browse_error_names_are_pinned() {
    assert_eq!(
        FakeRoonCore::FORK_ERROR_NAMES,
        [
            "InvalidItemKey",
            "InvalidLevels",
            "UnexpectedError",
            "ZoneNotFound"
        ],
        "these are the literals roon_api's browse.rs matches on; whether a real Roon \
         Core uses them is UNVERIFIED"
    );
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
    library.search_results.insert(
        "Library".to_string(),
        vec![album("Music for Airports", "Brian Eno", &["1/1"])],
    );

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
// hifi_collections (#531): browse_collection and browse_named_root_node
// =============================================================================

/// `browse_collection` walks two levels: a fresh root session, then resuming
/// it with the returned `(item_key, session_key)` pair -- exactly what
/// `hifi_collections`' `path` round-trips through a minted `RoonBrowse` ref.
/// The second call must never send `pop_all` (see `RoonAdapter::play_ref`'s
/// docs on why that combination hangs a real Core).
#[tokio::test]
async fn roon_collections_browse_walks_two_levels_without_pop_all_on_resume() {
    let mut library = FakeLibrary::standard();
    library.root_items =
        vec![
            FakeItem::list("Library").with_children(vec![FakeItem::list("Genres")
                .with_children(vec![FakeItem::list("Jazz"), FakeItem::list("Ambient")])]),
        ];

    let core = FakeRoonCore::start_with(library).await;
    let adapter = connected(&core).await;

    let (session, root_items, root_count) = adapter
        .browse_collection("zone_1", None, None, 0, 20)
        .await
        .expect("root browse");
    assert_eq!(root_count, 1);
    assert_eq!(root_items[0].title, "Library");
    let library_key = root_items[0]
        .item_key
        .clone()
        .expect("Library must be keyed");

    let (resumed_session, level_two, _count) = adapter
        .browse_collection("zone_1", Some(&library_key), Some(&session), 0, 20)
        .await
        .expect("resumed browse");
    assert_eq!(resumed_session, session, "resuming must reuse the session");
    assert_eq!(
        level_two
            .iter()
            .map(|i| i.title.as_str())
            .collect::<Vec<_>>(),
        vec!["Genres"]
    );

    // The resuming browse must never combine pop_all with item_key.
    let browses = core.browse_requests().await;
    assert_eq!(browses.len(), 2);
    assert!(browses[0].pop_all() && browses[0].item_key().is_none());
    assert!(!browses[1].pop_all());
    assert_eq!(browses[1].item_key(), Some(library_key.as_str()));

    core.stop().await;
}

/// `browse_collection` honors `offset`/`limit` as Roon's own `load` paging,
/// not a client-side slice -- unlike Music Assistant's `music/browse`, Roon's
/// `load` takes `offset`/`count` natively.
#[tokio::test]
async fn roon_collections_browse_pages_with_native_load_offset() {
    let mut library = FakeLibrary::standard();
    library.root_items = vec![
        FakeItem::list("A"),
        FakeItem::list("B"),
        FakeItem::list("C"),
    ];

    let core = FakeRoonCore::start_with(library).await;
    let adapter = connected(&core).await;

    let (_session, page, count) = adapter
        .browse_collection("zone_1", None, None, 1, 1)
        .await
        .expect("paged root browse");
    assert_eq!(count, 3);
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].title, "B");

    core.stop().await;
}

/// `hifi_collections playlists`' whole job: enter a named top-level node in
/// one call and load its contents, without a caller ever seeing the two-hop
/// browse/load plumbing.
#[tokio::test]
async fn roon_collections_playlists_enters_the_named_root_node() {
    let mut library = FakeLibrary::standard();
    library.root_items = vec![
        FakeItem::list("Library"),
        FakeItem::list("Playlists").with_children(vec![
            FakeItem::list("Sunday Morning"),
            FakeItem::list("Focus"),
        ]),
    ];

    let core = FakeRoonCore::start_with(library).await;
    let adapter = connected(&core).await;

    let (_session, items, count) = adapter
        .browse_named_root_node("zone_1", "Playlists", 0, 20)
        .await
        .expect("Playlists node must be found");
    assert_eq!(count, 2);
    assert_eq!(
        items.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(),
        vec!["Sunday Morning", "Focus"]
    );

    core.stop().await;
}

/// A named node the Core's root does not have is an honest, named failure --
/// not a hang, not an empty page pretending to be the real thing.
#[tokio::test]
async fn roon_collections_named_root_node_not_found_is_a_clean_error() {
    let mut library = FakeLibrary::standard();
    library.root_items = vec![FakeItem::list("Library")];

    let core = FakeRoonCore::start_with(library).await;
    let adapter = connected(&core).await;

    let error = adapter
        .browse_named_root_node("zone_1", "Playlists", 0, 20)
        .await
        .expect_err("Playlists does not exist in this root");
    assert!(error.to_string().contains("Playlists"));

    core.stop().await;
}

// =============================================================================
// #545: infinite playlist nesting, missing Play buttons, wrong action matching
//
// `playlist()` (`tests/mock_servers/roon_core.rs`) models the exact shape the
// issue's live repro captured: browsing a playlist's own item_key returns a
// mixed level -- an immediately-invokable "Play Playlist" action sitting
// directly alongside the track list, not wrapped in a further submenu the
// way `album()`'s "Play Album" is.
// =============================================================================

use serde_json::json;
use unified_hifi_control::adapters::traits::LibraryAdapter;

/// `hifi_collections browse` into a playlist must show its tracks and never
/// hand back the playlist's own item_key as one of its own children -- the
/// literal shape of "browse a playlist, see the playlist again as a child of
/// itself".
#[tokio::test]
async fn roon_collections_browse_of_a_playlist_never_contains_itself() {
    let mut library = FakeLibrary::standard();
    library.root_items = vec![FakeItem::list("Playlists").with_children(vec![playlist(
        "An Introduction to Qobuz",
        &["Laundromat (Remastered 2017)", "Second Track"],
    )])];

    let core = FakeRoonCore::start_with(library).await;
    let adapter = connected(&core).await;

    let (session, root_items, _count) = adapter
        .browse_collection("zone_1", None, None, 0, 20)
        .await
        .expect("root browse");
    let playlists_key = root_items[0].item_key.clone().unwrap();
    let (session, level_two, _count) = adapter
        .browse_collection("zone_1", Some(&playlists_key), Some(&session), 0, 20)
        .await
        .expect("Playlists node browse");
    let playlist_key = level_two[0].item_key.clone().unwrap();

    let (_session, children, _count) = adapter
        .browse_collection("zone_1", Some(&playlist_key), Some(&session), 0, 20)
        .await
        .expect("playlist browse");

    assert!(
        children
            .iter()
            .all(|item| item.item_key.as_deref() != Some(playlist_key.as_str())),
        "a level must never contain its own item_key as a child: {:?}",
        children.iter().map(|i| &i.title).collect::<Vec<_>>()
    );
    // The tracks are real content, not the playlist appearing again under
    // its own name.
    let titles: Vec<&str> = children.iter().map(|i| i.title.as_str()).collect();
    assert!(titles.contains(&"Laundromat (Remastered 2017)"));
    assert!(titles.contains(&"Second Track"));
    assert!(
        !titles.contains(&"An Introduction to Qobuz"),
        "the playlist must not appear as its own child: {titles:?}"
    );

    core.stop().await;
}

/// `hifi_collections`' Roon `content()` mapping (`RoonAdapter::content`) is
/// what `handle_roon` (`src/mcp/tools/collections.rs`) reads `navigable`/
/// `playable` from to decide whether to mint a browse path, a play ref, or
/// both. This exercises that mapping directly, end to end through the
/// `LibraryAdapter` trait `hifi_collections`/the web UI both call through.
#[tokio::test]
async fn roon_collections_content_classifies_playlist_and_tracks_correctly() {
    let mut library = FakeLibrary::standard();
    library.root_items = vec![FakeItem::list("Playlists").with_children(vec![playlist(
        "An Introduction to Qobuz",
        &["Laundromat (Remastered 2017)"],
    )])];

    let core = FakeRoonCore::start_with(library).await;
    let adapter = connected(&core).await;

    // List playlists (the root's "Playlists" node is hidden from
    // `collections_browse` since #573 defect 4 -- the dedicated
    // `collections_playlists` operation is the way in), then browse into
    // the playlist itself.
    let playlists_level = adapter
        .content(
            "collections_playlists",
            &json!({ "zone_id": "roon:zone_1", "limit": 20, "offset": 0 }),
        )
        .await
        .expect("Playlists listing");
    let playlist_item = &playlists_level["items"][0];
    assert_eq!(playlist_item["title"], "An Introduction to Qobuz");
    // Missing-Play-button bug (#545): a playlist is both a container (has
    // real tracks to browse into) *and* directly playable (Roon puts "Play
    // Playlist" one level below it) -- it must get both, not one or the
    // other.
    assert_eq!(
        playlist_item["navigable"], true,
        "a playlist must stay navigable: {playlist_item:?}"
    );
    assert_eq!(
        playlist_item["playable"], true,
        "a playlist must also be directly playable: {playlist_item:?}"
    );

    let session_key = playlists_level["session_key"].as_str().unwrap().to_string();
    let playlist_key = playlist_item["item_key"].as_str().unwrap().to_string();

    let tracks_level = adapter
        .content(
            "collections_browse",
            &json!({
                "zone_id": "roon:zone_1",
                "item_key": playlist_key,
                "session_key": session_key,
                "limit": 20,
                "offset": 0,
            }),
        )
        .await
        .expect("playlist browse");
    let items = tracks_level["items"].as_array().unwrap();
    // The "Play Playlist" action row itself must never appear as an
    // ordinary browsable/playable row -- it is implementation detail of
    // play resolution, not addressable content.
    assert!(
        items.iter().all(|item| item["title"] != "Play Playlist"),
        "the action row must be filtered out of the listing: {items:?}"
    );
    let track = items
        .iter()
        .find(|item| item["title"] == "Laundromat (Remastered 2017)")
        .expect("the track must be present");
    // Missing-Play-button bug (#545): a track's only child is its own
    // action list ("Play Track"), so it must be playable...
    assert_eq!(
        track["playable"], true,
        "a track must be playable: {track:?}"
    );
    // ...and, once that action row is filtered out, browsing further would
    // land on nothing -- so it must not also claim to be navigable.
    assert_eq!(
        track["navigable"], false,
        "a leaf track must not also be navigable: {track:?}"
    );

    core.stop().await;
}

/// The literal #545 repro: playing a Roon playlist by ref used to fail with
/// `Action 'Play Now' not available. Available: ["Play Playlist", <track
/// titles>]` -- even though the Core had already started playing. Roon's
/// context verb ("Play Playlist") must be matched, and no error must surface
/// on success.
#[tokio::test]
async fn roon_play_ref_matches_playlist_verb_and_reports_no_error() {
    let mut library = FakeLibrary::standard();
    library.root_items = vec![FakeItem::list("Playlists").with_children(vec![playlist(
        "An Introduction to Qobuz",
        &["Laundromat (Remastered 2017)", "Second Track"],
    )])];

    let core = FakeRoonCore::start_with(library).await;
    let adapter = connected(&core).await;

    let (session, root_items, _count) = adapter
        .browse_collection("zone_1", None, None, 0, 20)
        .await
        .expect("root browse");
    let playlists_key = root_items[0].item_key.clone().unwrap();
    let (session, level_two, _count) = adapter
        .browse_collection("zone_1", Some(&playlists_key), Some(&session), 0, 20)
        .await
        .expect("Playlists node browse");
    let playlist_key = level_two[0].item_key.clone().unwrap();

    let message = adapter
        .play_ref(
            &playlist_key,
            &session,
            "roon:zone_fake_1",
            PlayAction::Play,
            "An Introduction to Qobuz",
        )
        .await
        .expect("playing a playlist ref must succeed, not report a false error");

    assert!(
        message.contains("Play Playlist"),
        "the message should name Roon's own verb, not a literal 'Play Now': {message}"
    );
    assert!(
        message.contains("An Introduction to Qobuz"),
        "the message should name what was played: {message}"
    );

    core.stop().await;
}

/// Requesting Queue/Radio against a playlist that only offers a "Play
/// Playlist" verb must still fail honestly -- #545's broadened Play matching
/// must not silently substitute a different action the caller did not ask
/// for.
#[tokio::test]
async fn roon_play_ref_does_not_silently_substitute_queue_for_an_unavailable_action() {
    let mut library = FakeLibrary::standard();
    library.root_items =
        vec![FakeItem::list("Playlists")
            .with_children(vec![playlist("Solo Playlist", &["Only Track"])])];

    let core = FakeRoonCore::start_with(library).await;
    let adapter = connected(&core).await;

    let (session, root_items, _count) = adapter
        .browse_collection("zone_1", None, None, 0, 20)
        .await
        .expect("root browse");
    let playlists_key = root_items[0].item_key.clone().unwrap();
    let (session, level_two, _count) = adapter
        .browse_collection("zone_1", Some(&playlists_key), Some(&session), 0, 20)
        .await
        .expect("Playlists node browse");
    let playlist_key = level_two[0].item_key.clone().unwrap();

    let error = adapter
        .play_ref(
            &playlist_key,
            &session,
            "roon:zone_fake_1",
            PlayAction::Queue,
            "Solo Playlist",
        )
        .await
        .expect_err("Queue is not offered by this playlist");
    let text = error.to_string();
    assert!(text.contains("Queue"), "got {text}");
    assert!(
        text.contains("Play Playlist"),
        "should list what is available: {text}"
    );

    core.stop().await;
}

/// Category grouping rows (`is_ungrounded_grouping`, unchanged by #545) stay
/// excluded from `hifi_collections browse` results alongside the new
/// Header/Action/ActionList filtering.
#[tokio::test]
async fn roon_collections_browse_still_excludes_category_grouping_rows() {
    let mut library = FakeLibrary::standard();
    library.root_items = vec![FakeItem::list("Search Results").with_children(vec![
        FakeItem::list("Kind of Blue").with_subtitle("Miles Davis"),
        FakeItem::list("Albums").with_subtitle("32 Results"),
    ])];

    let core = FakeRoonCore::start_with(library).await;
    let adapter = connected(&core).await;

    let root = adapter
        .content(
            "collections_browse",
            &json!({ "zone_id": "roon:zone_1", "limit": 20, "offset": 0 }),
        )
        .await
        .unwrap();
    let results_key = root["items"][0]["item_key"].as_str().unwrap().to_string();
    let session_key = root["session_key"].as_str().unwrap().to_string();

    let level = adapter
        .content(
            "collections_browse",
            &json!({
                "zone_id": "roon:zone_1",
                "item_key": results_key,
                "session_key": session_key,
                "limit": 20,
                "offset": 0,
            }),
        )
        .await
        .unwrap();
    let titles: Vec<String> = level["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["title"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(titles, vec!["Kind of Blue"]);

    core.stop().await;
}

/// #566 (live install): Roon's own browse root carries a "Settings" node
/// (extension configuration inside Roon's hierarchy, not music content) that
/// was leaking through `hifi_collections browse`'s root listing as a
/// permanently inert row -- no `item_key`, so it could not be played or
/// browsed into. `FakeLibrary::standard()`'s root already models this
/// (`FakeItem::new("Settings").unkeyed()`), matching the live evidence.
///
/// The filter is scoped to the true collection root only (`item_key: None`)
/// -- real music nodes (`Library`, `TIDAL`, `Qobuz`) at that same root stay,
/// and nothing deeper in the hierarchy is touched.
#[tokio::test]
async fn roon_collections_browse_root_excludes_settings() {
    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;

    let root = adapter
        .content(
            "collections_browse",
            &json!({ "zone_id": "roon:zone_1", "limit": 20, "offset": 0 }),
        )
        .await
        .unwrap();
    let titles: Vec<String> = root["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["title"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        titles,
        vec!["Local Library", "TIDAL", "Qobuz"],
        "the root's own utility node (Settings) must not appear as an inert row, and \
         (#573 defect 4) the Library node is renamed so the page's breadcrumb doesn't \
         read Library / Library"
    );

    core.stop().await;
}

// =============================================================================
// #573: Library UI defect audit -- adapter-level fixes, pinned against the
// live-shaped fixtures (`album_live`/`playlist_live`: track rows are
// `hint: action_list`, exactly what the live crawl captured).
// =============================================================================

/// #573 defect 1 (blocker): an album level lists its tracks -- playable,
/// with no leaked action rows. The #545 filter dropped every
/// Action/ActionList row, and since a live Core marks *track rows*
/// `action_list` too, every album and playlist came back `items: []`.
#[tokio::test]
async fn roon_collections_live_album_level_lists_tracks_with_play_refs() {
    let mut library = FakeLibrary::standard();
    library.root_items = vec![FakeItem::list("Qobuz").with_children(vec![album_live(
        "Kind of Blue",
        "Miles Davis",
        &["So What", "Blue in Green", "Flamenco Sketches"],
    )])];

    let core = FakeRoonCore::start_with(library).await;
    let adapter = connected(&core).await;

    let root = adapter
        .content(
            "collections_browse",
            &json!({ "zone_id": "roon:zone_1", "limit": 20, "offset": 0 }),
        )
        .await
        .expect("root browse");
    let session_key = root["session_key"].as_str().unwrap().to_string();
    let qobuz_key = root["items"][0]["item_key"].as_str().unwrap().to_string();

    // Folder level: the Qobuz node lists its child (the album), navigable.
    let qobuz_level = adapter
        .content(
            "collections_browse",
            &json!({
                "zone_id": "roon:zone_1",
                "item_key": qobuz_key,
                "session_key": session_key,
                "limit": 20,
                "offset": 0,
            }),
        )
        .await
        .expect("Qobuz node browse");
    let album_item = &qobuz_level["items"][0];
    assert_eq!(album_item["title"], "Kind of Blue");
    assert_eq!(
        album_item["navigable"], true,
        "an album is navigable: {album_item:?}"
    );
    assert_eq!(
        album_item["playable"], true,
        "an album is also directly playable: {album_item:?}"
    );

    // Leaf level: the album's tracks, playable, no action rows leaked.
    let session_key = qobuz_level["session_key"].as_str().unwrap().to_string();
    let album_key = album_item["item_key"].as_str().unwrap().to_string();
    let tracks_level = adapter
        .content(
            "collections_browse",
            &json!({
                "zone_id": "roon:zone_1",
                "item_key": album_key,
                "session_key": session_key,
                "limit": 20,
                "offset": 0,
            }),
        )
        .await
        .expect("album browse");
    let items = tracks_level["items"].as_array().unwrap();
    let titles: Vec<&str> = items.iter().map(|i| i["title"].as_str().unwrap()).collect();
    assert_eq!(
        titles,
        vec!["So What", "Blue in Green", "Flamenco Sketches"],
        "the album level must list its tracks (the #573 blocker was items: [])"
    );
    for track in items {
        assert_eq!(
            track["playable"], true,
            "every action_list-hinted track row is a playable leaf: {track:?}"
        );
        assert_eq!(
            track["navigable"], false,
            "a track leaf must not claim navigability: {track:?}"
        );
        assert!(
            track["item_key"].as_str().is_some(),
            "a playable track needs an item_key for its play ref: {track:?}"
        );
    }
    assert!(
        !titles.contains(&"Play Album"),
        "the Play Album verb row must not leak into the listing"
    );
    assert!(
        tracks_level["next_offset"].is_null(),
        "3 tracks in a 20-row page: no further page exists (#573 defect 8)"
    );

    core.stop().await;
}

/// #573 defect 1 (blocker), playlist half: `collections_playlists` +
/// browse into a playlist lists its tracks, with the immediately-invokable
/// "Play Playlist" verb filtered out.
#[tokio::test]
async fn roon_collections_live_playlist_level_lists_tracks() {
    let mut library = FakeLibrary::standard();
    library.root_items = vec![
        FakeItem::list("Library"),
        FakeItem::list("Playlists").with_children(vec![playlist_live(
            "An Introduction to Qobuz",
            &[
                ("Laundromat (Remastered 2017)", "Queen"),
                ("So What", "Miles Davis"),
            ],
        )]),
    ];

    let core = FakeRoonCore::start_with(library).await;
    let adapter = connected(&core).await;

    let playlists = adapter
        .content(
            "collections_playlists",
            &json!({ "zone_id": "roon:zone_1", "limit": 20, "offset": 0 }),
        )
        .await
        .expect("playlists listing");
    let playlist_item = &playlists["items"][0];
    assert_eq!(playlist_item["title"], "An Introduction to Qobuz");
    assert_eq!(playlist_item["navigable"], true);
    assert_eq!(playlist_item["playable"], true);

    let session_key = playlists["session_key"].as_str().unwrap().to_string();
    let playlist_key = playlist_item["item_key"].as_str().unwrap().to_string();
    let tracks_level = adapter
        .content(
            "collections_browse",
            &json!({
                "zone_id": "roon:zone_1",
                "item_key": playlist_key,
                "session_key": session_key,
                "limit": 20,
                "offset": 0,
            }),
        )
        .await
        .expect("playlist browse");
    let items = tracks_level["items"].as_array().unwrap();
    let titles: Vec<&str> = items.iter().map(|i| i["title"].as_str().unwrap()).collect();
    assert_eq!(
        titles,
        vec!["Laundromat (Remastered 2017)", "So What"],
        "the playlist level must list its tracks"
    );
    assert!(
        items.iter().all(|i| i["playable"] == true),
        "playlist tracks are playable leaves: {items:?}"
    );

    core.stop().await;
}

/// #573 defect 3: the Library node's real children (Artists, Albums,
/// Tracks, Genres, ...) share their exact titles with `CATEGORY_NAMES`, so
/// the search-result grouping filter swallowed the whole level down to
/// "Search". Browse levels now rely on the "<N> Results" subtitle signal
/// alone (which those real children never carry).
#[tokio::test]
async fn roon_collections_library_node_lists_its_real_children() {
    let mut library = FakeLibrary::standard();
    library.root_items = vec![FakeItem::list("Library").with_children(vec![
        FakeItem::list("Search").searchable("Search"),
        FakeItem::list("Artists").with_children(vec![FakeItem::list("Miles Davis")]),
        FakeItem::list("Albums").with_children(vec![FakeItem::list("Kind of Blue")]),
        FakeItem::list("Tracks").with_children(vec![FakeItem::list("So What")]),
        FakeItem::list("Genres").with_children(vec![FakeItem::list("Jazz")]),
    ])];

    let core = FakeRoonCore::start_with(library).await;
    let adapter = connected(&core).await;

    let root = adapter
        .content(
            "collections_browse",
            &json!({ "zone_id": "roon:zone_1", "limit": 20, "offset": 0 }),
        )
        .await
        .unwrap();
    let session_key = root["session_key"].as_str().unwrap().to_string();
    let library_key = root["items"][0]["item_key"].as_str().unwrap().to_string();

    let level = adapter
        .content(
            "collections_browse",
            &json!({
                "zone_id": "roon:zone_1",
                "item_key": library_key,
                "session_key": session_key,
                "limit": 20,
                "offset": 0,
            }),
        )
        .await
        .unwrap();
    let titles: Vec<&str> = level["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["title"].as_str().unwrap())
        .collect();
    assert_eq!(
        titles,
        vec!["Search", "Artists", "Albums", "Tracks", "Genres"],
        "the Library node's real children must not be swallowed by the category filter"
    );

    core.stop().await;
}

/// #573 defect 4: the collection root hides the "Playlists" node (the
/// dedicated tab lists the same node) -- `collections_playlists` still
/// reaches it by name.
#[tokio::test]
async fn roon_collections_root_hides_the_playlists_node_but_the_tab_still_works() {
    let mut library = FakeLibrary::standard();
    library.root_items = vec![
        FakeItem::list("Library"),
        FakeItem::list("Playlists")
            .with_children(vec![playlist_live("Focus", &[("Deep Work", "Eno")])]),
        FakeItem::list("My Live Radio"),
    ];

    let core = FakeRoonCore::start_with(library).await;
    let adapter = connected(&core).await;

    let root = adapter
        .content(
            "collections_browse",
            &json!({ "zone_id": "roon:zone_1", "limit": 20, "offset": 0 }),
        )
        .await
        .unwrap();
    let titles: Vec<&str> = root["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["title"].as_str().unwrap())
        .collect();
    assert_eq!(
        titles,
        vec!["Local Library", "My Live Radio"],
        "the root must hide Playlists (dedicated tab) and rename Library"
    );

    let playlists = adapter
        .content(
            "collections_playlists",
            &json!({ "zone_id": "roon:zone_1", "limit": 20, "offset": 0 }),
        )
        .await
        .expect("the Playlists tab still reaches the node by name");
    assert_eq!(playlists["items"][0]["title"], "Focus");

    core.stop().await;
}

/// #573 defect 7: Roon's "No Results" placeholder row (an empty node's only
/// child, which still carries an item_key) must not be minted as playable
/// content.
#[tokio::test]
async fn roon_collections_no_results_placeholder_is_not_listed() {
    let mut library = FakeLibrary::standard();
    library.root_items =
        vec![FakeItem::list("My Live Radio").with_children(vec![FakeItem::new("No Results")])];

    let core = FakeRoonCore::start_with(library).await;
    let adapter = connected(&core).await;

    let root = adapter
        .content(
            "collections_browse",
            &json!({ "zone_id": "roon:zone_1", "limit": 20, "offset": 0 }),
        )
        .await
        .unwrap();
    let session_key = root["session_key"].as_str().unwrap().to_string();
    let radio_key = root["items"][0]["item_key"].as_str().unwrap().to_string();

    let level = adapter
        .content(
            "collections_browse",
            &json!({
                "zone_id": "roon:zone_1",
                "item_key": radio_key,
                "session_key": session_key,
                "limit": 20,
                "offset": 0,
            }),
        )
        .await
        .unwrap();
    assert!(
        level["items"].as_array().unwrap().is_empty(),
        "the placeholder must not become a playable row: {level:?}"
    );
    assert!(
        level["next_offset"].is_null(),
        "an empty level must not advertise Load more (#573 defect 8): {level:?}"
    );

    core.stop().await;
}

/// #587: a live Core marks radio station rows under "My Live Radio"
/// `hint: action` (browsing a station plays it immediately) -- see
/// `radio_station`'s docs for the captured wire shape. #578's classification
/// treated every `action` row as a play verb and filtered stations out, so
/// the node listed as empty despite stations existing. Stations must list as
/// playable leaves; the My Live Radio folder itself must be navigable but
/// not offer a Play that could only error.
#[tokio::test]
async fn roon_collections_my_live_radio_lists_stations_as_playable_leaves() {
    let mut library = FakeLibrary::standard();
    library.root_items = vec![
        FakeItem::list("My Live Radio").with_children(vec![radio_station(
            "WOSU-HD2 WOSU Public Media: Classical 101",
            "Columbus, Ohio, USA FM 89.7 HD2 English",
        )]),
    ];

    let core = FakeRoonCore::start_with(library).await;
    let adapter = connected(&core).await;

    let root = adapter
        .content(
            "collections_browse",
            &json!({ "zone_id": "roon:zone_1", "limit": 20, "offset": 0 }),
        )
        .await
        .unwrap();
    let radio_row = &root["items"][0];
    assert_eq!(radio_row["title"], "My Live Radio");
    assert_eq!(
        radio_row["navigable"], true,
        "the radio folder must stay browsable: {root:?}"
    );
    assert_eq!(
        radio_row["playable"], false,
        "a folder of stations offers no play verb to invoke -- a Play ref \
         here could only error (#587): {root:?}"
    );
    let session_key = root["session_key"].as_str().unwrap().to_string();
    let radio_key = radio_row["item_key"].as_str().unwrap().to_string();

    let level = adapter
        .content(
            "collections_browse",
            &json!({
                "zone_id": "roon:zone_1",
                "item_key": radio_key,
                "session_key": session_key,
                "limit": 20,
                "offset": 0,
            }),
        )
        .await
        .unwrap();
    let items = level["items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        1,
        "the station row must not be filtered as a play verb (#587): {level:?}"
    );
    assert_eq!(
        items[0]["title"],
        "WOSU-HD2 WOSU Public Media: Classical 101"
    );
    assert_eq!(
        items[0]["subtitle"], "Columbus, Ohio, USA FM 89.7 HD2 English",
        "the station's location/language subtitle survives the mapping"
    );
    assert_eq!(
        items[0]["playable"], true,
        "browsing a station plays it -- it must mint a play ref: {level:?}"
    );
    assert_eq!(
        items[0]["navigable"], false,
        "a station is a leaf: entering it would invoke playback, so it must \
         not be offered as a folder: {level:?}"
    );
    assert!(
        items[0]["image_key"].is_string(),
        "station artwork must survive the mapping: {level:?}"
    );

    core.stop().await;
}

/// #587 acceptance: a station added in Roon's own app while UHC is running
/// (modeled as a mid-session library mutation) appears on the next browse of
/// My Live Radio -- same adapter connection, no restart. Also pins the empty
/// state on the way: before the station exists, the node lists cleanly empty
/// (the "No Results" placeholder stays filtered).
#[tokio::test]
async fn roon_collections_station_added_mid_session_appears_without_restart() {
    let mut library = FakeLibrary::standard();
    library.root_items =
        vec![FakeItem::list("My Live Radio").with_children(vec![FakeItem::new("No Results")])];

    let core = FakeRoonCore::start_with(library).await;
    let adapter = connected(&core).await;

    // Walk 1: the node is empty (placeholder filtered, no phantom rows).
    let root = adapter
        .content(
            "collections_browse",
            &json!({ "zone_id": "roon:zone_1", "limit": 20, "offset": 0 }),
        )
        .await
        .unwrap();
    let session_key = root["session_key"].as_str().unwrap().to_string();
    let radio_key = root["items"][0]["item_key"].as_str().unwrap().to_string();
    let empty = adapter
        .content(
            "collections_browse",
            &json!({
                "zone_id": "roon:zone_1",
                "item_key": radio_key,
                "session_key": session_key,
                "limit": 20,
                "offset": 0,
            }),
        )
        .await
        .unwrap();
    assert!(
        empty["items"].as_array().unwrap().is_empty(),
        "before the station exists the node lists cleanly empty: {empty:?}"
    );

    // The operator adds a station in Roon's own app.
    core.set_children_by_title(
        "My Live Radio",
        vec![radio_station(
            "WOSU-HD2 WOSU Public Media: Classical 101",
            "Columbus, Ohio, USA FM 89.7 HD2 English",
        )],
    )
    .await;

    // Walk 2: a fresh root walk (what the UI does on every Browse-tab entry)
    // sees the station -- no adapter restart, same connection.
    let root2 = adapter
        .content(
            "collections_browse",
            &json!({ "zone_id": "roon:zone_1", "limit": 20, "offset": 0 }),
        )
        .await
        .unwrap();
    let session2 = root2["session_key"].as_str().unwrap().to_string();
    assert_ne!(
        session2, session_key,
        "every root walk mints a fresh browse session -- staleness cannot \
         come from session reuse"
    );
    let radio_key2 = root2["items"][0]["item_key"].as_str().unwrap().to_string();
    let level = adapter
        .content(
            "collections_browse",
            &json!({
                "zone_id": "roon:zone_1",
                "item_key": radio_key2,
                "session_key": session2,
                "limit": 20,
                "offset": 0,
            }),
        )
        .await
        .unwrap();
    let items = level["items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        1,
        "the added station appears without restart: {level:?}"
    );
    assert_eq!(
        items[0]["title"],
        "WOSU-HD2 WOSU Public Media: Classical 101"
    );
    assert_eq!(items[0]["playable"], true);

    core.stop().await;
}

/// The #593 library shape: Library -> Artists -> one artist (live capture --
/// see `artist_live`) -> albums, under the live-observed key scoping
/// (`PerSessionSilentRootReset`: a key browsed outside its own session
/// silently answers the root).
fn artists_library() -> FakeLibrary {
    let mut library = FakeLibrary::standard();
    library.root_items = vec![FakeItem::list("Library").with_children(vec![
        FakeItem::list("Artists").with_children(vec![artist_live(
            "/Passenger.",
            vec![
                album_live(
                    "Flight of the Crow",
                    "Passenger",
                    &["Month of Sundays", "What You're Thinking"],
                ),
                album_live("Runaway", "Passenger", &["Hell or High Water"]),
            ],
        )]),
        FakeItem::list("Albums").with_children(vec![album_live(
            "Runaway",
            "Passenger",
            &["Hell or High Water"],
        )]),
    ])];
    library
}

/// Walk root -> Local Library -> Artists -> artist, returning the final
/// level's `(session_key, value)` -- the album listing #593 is about.
async fn walk_to_artist(adapter: &Arc<RoonAdapter>, node: &str) -> (String, serde_json::Value) {
    let mut level = adapter
        .content(
            "collections_browse",
            &json!({ "zone_id": "roon:zone_1", "limit": 20, "offset": 0 }),
        )
        .await
        .unwrap();
    let mut session = level["session_key"].as_str().unwrap().to_string();
    for title in ["Local Library", node] {
        let key = level["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["title"] == title)
            .unwrap_or_else(|| panic!("row {title} missing: {level:?}"))["item_key"]
            .as_str()
            .unwrap()
            .to_string();
        level = adapter
            .content(
                "collections_browse",
                &json!({
                    "zone_id": "roon:zone_1",
                    "item_key": key,
                    "session_key": session,
                    "limit": 20,
                    "offset": 0,
                }),
            )
            .await
            .unwrap();
        session = level["session_key"].as_str().unwrap().to_string();
    }
    (session, level)
}

/// #593: albums listed at the Artists level must be dual navigable+playable.
///
/// The root cause this pins against: `peek_playability` used to browse the
/// album's `item_key` inside a disposable `peek_<nanos>` session, assuming
/// keys resolve across sessions. The operator's production Core scopes them
/// per session and *silently answers the root list* for a foreign key
/// (probed read-only, 2026-08 -- see `ItemKeyScope::PerSessionSilentRootReset`),
/// so the peek judged the root's rows, found no play verb, and every album
/// under Artists rendered navigable-only (chevron, no Play). Under
/// `ItemKeyScope::Global` -- the fake's default and the old assumption --
/// this test would pass even with the disposable-session peek, which is
/// exactly how the bug shipped; the scoped knob is the load-bearing part.
#[tokio::test]
async fn roon_collections_artist_level_albums_are_dual_under_session_scoped_keys() {
    let core = FakeRoonCore::start_with(artists_library()).await;
    core.set_item_key_scope(ItemKeyScope::PerSessionSilentRootReset)
        .await;
    let adapter = connected(&core).await;

    let (_, artists) = walk_to_artist(&adapter, "Artists").await;
    let artist_row = &artists["items"][0];
    assert_eq!(artist_row["title"], "/Passenger.");
    assert_eq!(
        artist_row["navigable"], true,
        "an artist opens to its albums: {artists:?}"
    );
    assert_eq!(
        artist_row["playable"], true,
        "the artist level leads with a Play Artist verb, so the artist row \
         itself is also directly playable: {artists:?}"
    );
    assert_eq!(
        artist_row["subtitle"], "2 Albums",
        "the album-count subtitle survives the mapping: {artists:?}"
    );

    let (session, artist_level) = walk_to_artist(&adapter, "Artists").await;
    let artist_key = artist_level["items"][0]["item_key"]
        .as_str()
        .unwrap()
        .to_string();
    let albums = adapter
        .content(
            "collections_browse",
            &json!({
                "zone_id": "roon:zone_1",
                "item_key": artist_key,
                "session_key": session,
                "limit": 20,
                "offset": 0,
            }),
        )
        .await
        .unwrap();
    let rows = albums["items"].as_array().unwrap();
    let titles: Vec<&str> = rows.iter().map(|i| i["title"].as_str().unwrap()).collect();
    assert_eq!(
        titles,
        vec!["Flight of the Crow", "Runaway"],
        "the Play Artist verb row stays filtered; the albums stay: {albums:?}"
    );
    for album in rows {
        assert_eq!(
            album["playable"], true,
            "#593: an album at the artist level must carry a play ref: {album:?}"
        );
        assert_eq!(
            album["navigable"], true,
            "an album must also still open to its tracks: {album:?}"
        );
        assert!(
            album["image_key"].is_string(),
            "album artwork must survive the mapping: {album:?}"
        );
    }

    // The play ref an artist-level album mints must actually resolve -- in
    // the same session it was listed in (the whole point of #593's fix).
    let album_key = rows[0]["item_key"].as_str().unwrap();
    let session_key = albums["session_key"].as_str().unwrap();
    let message = adapter
        .play_ref(
            album_key,
            session_key,
            "roon:zone_fake_1",
            PlayAction::Play,
            "Flight of the Crow",
        )
        .await
        .expect("playing an artist-level album ref must succeed");
    assert!(
        message.contains("Play Now") || message.contains("Play Album"),
        "the album's Play Album menu resolves to a real invocation: {message}"
    );

    core.stop().await;
}

/// #593 (companion report): the Library / Albums node must list its albums
/// dual navigable+playable under the same live-observed key scoping. The
/// row shape is identical to the artist level's (list-hinted, artist
/// subtitle, artwork, "Play Album" wrapper one level down), so this pins
/// the top-level node the operator's screenshot showed empty.
///
/// (The operator's Core *itself* currently answers the real Albums node
/// with `count: 0` -- verified read-only through the raw `/roon/browse`
/// passthrough, so no UHC-side change can affect that; this test pins that
/// whenever the Core does serve album rows, UHC lists them correctly.)
#[tokio::test]
async fn roon_collections_albums_node_lists_albums_dual_under_session_scoped_keys() {
    let core = FakeRoonCore::start_with(artists_library()).await;
    core.set_item_key_scope(ItemKeyScope::PerSessionSilentRootReset)
        .await;
    let adapter = connected(&core).await;

    let (_, albums) = walk_to_artist(&adapter, "Albums").await;
    let rows = albums["items"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "the album row must be listed: {albums:?}");
    assert_eq!(rows[0]["title"], "Runaway");
    assert_eq!(
        rows[0]["playable"], true,
        "#593: an album under the Albums node must carry a play ref: {albums:?}"
    );
    assert_eq!(rows[0]["navigable"], true, "{albums:?}");

    core.stop().await;
}

/// #593's fix peeks inside the caller's own session, which pushes the peeked
/// level onto that session's stack -- so the peek must pop back out, or
/// `content()`'s fetch-ahead loop (whose bare `load`s page the *current*
/// level) would page the last peeked album's tracks instead of the artist's
/// remaining albums. Forced here: limit 2 over [verb row, 4 albums] maps
/// one album short, so fetch-ahead loads offset 2 *after* two peeks ran.
#[tokio::test]
async fn roon_collections_peeks_do_not_derail_fetch_ahead_paging() {
    let mut library = FakeLibrary::standard();
    library.root_items =
        vec![
            FakeItem::list("Library").with_children(vec![FakeItem::list("Artists").with_children(
                vec![artist_live(
                    "Prolific",
                    vec![
                        album_live("Album One", "Prolific", &["T1"]),
                        album_live("Album Two", "Prolific", &["T2"]),
                        album_live("Album Three", "Prolific", &["T3"]),
                        album_live("Album Four", "Prolific", &["T4"]),
                    ],
                )],
            )]),
        ];
    let core = FakeRoonCore::start_with(library).await;
    core.set_item_key_scope(ItemKeyScope::PerSessionSilentRootReset)
        .await;
    let adapter = connected(&core).await;

    let (session, artists) = walk_to_artist(&adapter, "Artists").await;
    let artist_key = artists["items"][0]["item_key"].as_str().unwrap();
    let page = adapter
        .content(
            "collections_browse",
            &json!({
                "zone_id": "roon:zone_1",
                "item_key": artist_key,
                "session_key": session,
                "limit": 2,
                "offset": 0,
            }),
        )
        .await
        .unwrap();
    let titles: Vec<&str> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["title"].as_str().unwrap())
        .collect();
    assert!(
        titles.iter().all(|t| t.starts_with("Album")),
        "fetch-ahead must keep paging the artist's albums, not a peeked \
         album's tracks: {titles:?}"
    );
    assert!(
        titles.contains(&"Album Two"),
        "the fetch-ahead round after the filtered verb row delivers the \
         next album: {titles:?}"
    );
    // Raw rows: [Play Artist, four albums] = 5 total. limit 2 consumes 2
    // raw rows (1 album), fetch-ahead consumes 2 more (2 albums) and stops
    // with the page filled; one raw row genuinely remains.
    assert_eq!(
        page["next_offset"].as_u64(),
        Some(4),
        "further raw rows remain past what was consumed: {page:?}"
    );

    core.stop().await;
}

/// #593 review follow-up: when a peek's position-restoring `pop_levels`
/// browse fails, the session sits at an unknown level -- the listing must
/// refuse loudly instead of quietly continuing (an ordinary peek failure
/// degrades to "navigable, not playable"; a lost position must not, because
/// the fetch-ahead loop's bare `load`s would map the peeked child's rows as
/// the parent's).
#[tokio::test]
async fn roon_collections_lost_peek_position_refuses_instead_of_paging_blind() {
    let core = FakeRoonCore::start_with(artists_library()).await;
    let adapter = connected(&core).await;

    core.reject_next_pop_levels().await;
    let error = adapter
        .content(
            "collections_browse",
            &json!({ "zone_id": "roon:zone_1", "limit": 20, "offset": 0 }),
        )
        .await
        .expect_err("a lost browse position must fail the listing");
    assert!(
        format!("{error:#}").contains("browse position lost"),
        "the refusal must name the position loss: {error:#}"
    );

    // One-shot knob: the next walk restores normal service, and the level
    // maps correctly again -- the failure poisoned nothing durable.
    let root = adapter
        .content(
            "collections_browse",
            &json!({ "zone_id": "roon:zone_1", "limit": 20, "offset": 0 }),
        )
        .await
        .expect("a fresh walk after the transient failure succeeds");
    assert_eq!(root["items"][0]["title"], "Local Library");

    core.stop().await;
}

/// #573 defect 8: paging is computed against post-filter reality. A raw
/// page that filters to nothing (limit 1, first raw row is the "Play
/// Playlist" verb) fetches ahead and still delivers content, and
/// `next_offset` advertises a further page only when raw rows genuinely
/// remain.
#[tokio::test]
async fn roon_collections_paging_fetches_ahead_past_filtered_rows() {
    let mut library = FakeLibrary::standard();
    library.root_items = vec![
        FakeItem::list("Playlists").with_children(vec![playlist_live(
            "Mix",
            &[("Track One", "A"), ("Track Two", "B"), ("Track Three", "C")],
        )]),
    ];

    let core = FakeRoonCore::start_with(library).await;
    let adapter = connected(&core).await;

    let level = adapter
        .content(
            "collections_playlists",
            &json!({ "zone_id": "roon:zone_1", "limit": 20, "offset": 0 }),
        )
        .await
        .unwrap();
    let session_key = level["session_key"].as_str().unwrap().to_string();
    let playlist_key = level["items"][0]["item_key"].as_str().unwrap().to_string();

    // limit 1: the first raw row is the "Play Playlist" verb, which filters
    // to nothing. Pre-#573 this returned items: [] with next_offset: 1
    // forever ("Load more" over nothing).
    let page = adapter
        .content(
            "collections_browse",
            &json!({
                "zone_id": "roon:zone_1",
                "item_key": playlist_key,
                "session_key": session_key,
                "limit": 1,
                "offset": 0,
            }),
        )
        .await
        .unwrap();
    let items = page["items"].as_array().unwrap();
    assert!(
        !items.is_empty(),
        "a page whose raw rows filter away must fetch ahead, not return empty: {page:?}"
    );
    assert_eq!(items[0]["title"], "Track One");
    let next = page["next_offset"].as_u64().expect("more raw rows remain");
    assert!(next >= 2, "next_offset must sit past the consumed raw rows");

    core.stop().await;
}

/// #573 defect 11: a search-derived album continuation whose level is a
/// single self-referential row (the album again, same title as the level)
/// auto-descends to the real level -- one click opens the tracks, not a
/// pointless second copy of the album.
#[tokio::test]
async fn roon_collections_single_self_row_level_descends_to_tracks() {
    let mut library = FakeLibrary::standard();
    library.root_items = vec![
        FakeItem::list("Kind of Blue").with_children(vec![album_live(
            "Kind of Blue",
            "Miles Davis",
            &["So What", "Blue in Green"],
        )]),
    ];

    let core = FakeRoonCore::start_with(library).await;
    let adapter = connected(&core).await;

    let root = adapter
        .content(
            "collections_browse",
            &json!({ "zone_id": "roon:zone_1", "limit": 20, "offset": 0 }),
        )
        .await
        .unwrap();
    let session_key = root["session_key"].as_str().unwrap().to_string();
    let outer_key = root["items"][0]["item_key"].as_str().unwrap().to_string();

    let level = adapter
        .content(
            "collections_browse",
            &json!({
                "zone_id": "roon:zone_1",
                "item_key": outer_key,
                "session_key": session_key,
                "limit": 20,
                "offset": 0,
            }),
        )
        .await
        .unwrap();
    let titles: Vec<&str> = level["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["title"].as_str().unwrap())
        .collect();
    assert_eq!(
        titles,
        vec!["So What", "Blue in Green"],
        "the self-referential single-row level must descend to the tracks"
    );

    core.stop().await;
}

/// #573 defect 5: `[[id|Name]]` link markup is stripped to plain names at
/// the adapter mapping, including compound credits.
#[tokio::test]
async fn roon_collections_subtitles_are_stripped_of_link_markup() {
    let mut library = FakeLibrary::standard();
    library.root_items = vec![FakeItem::list("Ideal Discography").with_children(vec![
        FakeItem::list("Thriller").with_subtitle("[[55418|Michael Jackson]]"),
        FakeItem::list("Siembra")
            .with_subtitle("[[8827258|Willie Colón]] & [[1981050|Rubén Blades]]"),
    ])];

    let core = FakeRoonCore::start_with(library).await;
    let adapter = connected(&core).await;

    let root = adapter
        .content(
            "collections_browse",
            &json!({ "zone_id": "roon:zone_1", "limit": 20, "offset": 0 }),
        )
        .await
        .unwrap();
    let session_key = root["session_key"].as_str().unwrap().to_string();
    let node_key = root["items"][0]["item_key"].as_str().unwrap().to_string();

    let level = adapter
        .content(
            "collections_browse",
            &json!({
                "zone_id": "roon:zone_1",
                "item_key": node_key,
                "session_key": session_key,
                "limit": 20,
                "offset": 0,
            }),
        )
        .await
        .unwrap();
    let items = level["items"].as_array().unwrap();
    assert_eq!(items[0]["subtitle"], "Michael Jackson");
    assert_eq!(items[1]["subtitle"], "Willie Colón & Rubén Blades");

    core.stop().await;
}

// =============================================================================
// AppState for the HTTP-boundary test
// =============================================================================

async fn app_state_with(roon: Arc<RoonAdapter>) -> unified_hifi_control::api::AppState {
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
    let hqplayer = hqp_instances.get_default().await;
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

// =============================================================================
// Same-session concurrency (issue #416)
// =============================================================================
//
// These three tests were found by, and now guard, #416: three requests in flight
// under one `multi_session_key`, answered *out of order*, each caller must get the
// answer to its own request.
//
// The two `each_receive_their_own` tests were the defect-pinning tests #408 shipped
// (`concurrent_browses_in_one_session_cross_deliver_their_results`), inverted
// exactly as their own comment instructed. Both still run eight trials, because the
// old failure was probabilistic: the winning waiter came out of `HashMap` iteration
// order, so any single trial got the right answer by luck about one time in eight.
// Eight clean trials is the evidence that correlation is now exact rather than
// lucky. Do not reduce the trial count.
//
// What they do *not* prove is in the third test: correct correlation makes each
// *answer* honest, and does nothing about the Core-side level stack a shared
// session key implies. That is why `/roon/browse`'s handler carries a note rather
// than a guarantee.

/// **Found by this instrument, fixed by #416.** Three browses in flight under one
/// `multi_session_key`, answered in the reverse of the request order — each caller
/// gets the list it asked for.
///
/// Before #416, `pending_browses` was keyed by `req_id` but *scanned* by session key
/// (`src/adapters/roon.rs`, the `Parsed::BrowseResult` arm:
/// `.iter().find(|(_, (key, _))| key == &session_key)`), so an arbitrary matching
/// entry won and the caller that asked for `Library` could be handed `TIDAL`'s list
/// — no error, no way to tell. It reproduced in 7 of 8 trials, and it survived #405,
/// which made `req_id` primary for the *error* arms only.
///
/// The per-key delays make the Core answer request 3 first and request 1 last, so a
/// correlation that went by arrival order rather than by request would fail here
/// too. The Core's own answers are per-request correct throughout — every trial's
/// `expected` is what the fake sent, in request order — so anything else this test
/// sees was scrambled inside `src/adapters/roon.rs`.
///
/// Reachability of the defect it guards: the adapter's own callers (`search`,
/// `search_and_play`, `play_item`) each mint a private key and run sequentially, so
/// they were safe. `POST /roon/browse` takes a **caller-supplied** `session_key`, so
/// any external client could trigger it, and #399's navigation handle is that exact
/// shape.
#[tokio::test]
async fn concurrent_browses_in_one_session_each_receive_their_own_result() {
    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;

    const TRIALS: usize = 8;
    let expected = ["Library", "TIDAL", "Qobuz"];
    let mut observed = Vec::new();

    for trial in 0..TRIALS {
        let session = format!("shared_session_{trial}");
        adapter.browse(browse_root(&session)).await.unwrap();
        let root = adapter.load(load_all(&session)).await.unwrap();
        let keys: Vec<String> = (0..3)
            .map(|i| root.items[i].item_key.clone().unwrap())
            .collect();

        // Responses arrive in the reverse of the request order, so a correlation
        // that goes by arrival rather than by request is visible.
        for (i, key) in keys.iter().enumerate() {
            core.set_delay_for_item_key(key, Duration::from_millis(60 * (3 - i) as u64))
                .await;
        }

        let got: Vec<String> = futures::future::join_all(
            keys.iter()
                .map(|k| adapter.browse(browse_at(&session, Some(k)))),
        )
        .await
        .into_iter()
        .map(|r| {
            r.expect("every browse resolves; before #416 only the payload was wrong")
                .list
                .map(|l| l.title)
                .unwrap_or_default()
        })
        .collect();

        observed.push(got);
    }

    for (trial, got) in observed.iter().enumerate() {
        assert_eq!(
            got.as_slice(),
            expected.as_slice(),
            "trial {trial}: each caller must receive the list it asked for. \
             All trials: {observed:?}"
        );
    }

    core.stop().await;
}

/// The same defect on the load side, which `Parsed::LoadResult` shared verbatim.
///
/// Three loads in flight under one session key, each naming a different `level` of
/// that session's stack, answered in the reverse of the request order. Each caller
/// must get its own level's page — distinguishable here by both the list title and
/// the item count, so a cross-delivery cannot hide behind a coincidence.
///
/// A load carries no `item_key`, so forcing out-of-order load responses needed the
/// fake's `set_delay_for_load_level` hook, added by #416 alongside this test.
#[tokio::test]
async fn concurrent_loads_in_one_session_each_receive_their_own_page() {
    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;

    const TRIALS: usize = 8;
    // Three levels deep: root (4 rows), Library (3 rows), Artists (25 rows).
    let expected = [("Explore", 4), ("Library", 3), ("Artists", 25)];
    let mut observed = Vec::new();

    // Answer level 0 last and level 2 first.
    for (level, delay) in [(0u32, 180u64), (1, 120), (2, 60)] {
        core.set_delay_for_load_level(level, Duration::from_millis(delay))
            .await;
    }

    for trial in 0..TRIALS {
        let session = format!("shared_load_session_{trial}");
        adapter.browse(browse_root(&session)).await.unwrap();
        let root = adapter
            .load(load_at_level(&session, None))
            .await
            .expect("root level");
        let library_key = root.items[0].item_key.clone().unwrap();
        adapter
            .browse(browse_at(&session, Some(&library_key)))
            .await
            .unwrap();
        let library = adapter
            .load(load_at_level(&session, None))
            .await
            .expect("library level");
        let artists_key = library.items[1].item_key.clone().unwrap();
        adapter
            .browse(browse_at(&session, Some(&artists_key)))
            .await
            .unwrap();

        let got: Vec<(String, usize)> = futures::future::join_all(
            (0..3u32).map(|level| adapter.load(load_at_level(&session, Some(level)))),
        )
        .await
        .into_iter()
        .map(|r| {
            let page = r.expect("every load resolves; before #416 only the payload was wrong");
            (page.list.title, page.items.len())
        })
        .collect();

        observed.push(got);
    }

    for (trial, got) in observed.iter().enumerate() {
        let got: Vec<(&str, usize)> = got.iter().map(|(t, n)| (t.as_str(), *n)).collect();
        assert_eq!(
            got.as_slice(),
            expected.as_slice(),
            "trial {trial}: each caller must receive the page it asked for. \
             All trials: {observed:?}"
        );
    }

    core.stop().await;
}

/// What #416 does **not** fix, pinned so nobody reads the two tests above as a
/// licence to share a session key.
///
/// A `multi_session_key` names one browse *position* on the Core - a stack of
/// levels that `browse` pushes onto and `pop_all` / `pop_levels` unwind. Correct
/// correlation guarantees each caller is handed the answer to its own request; it
/// cannot make three callers' pushes onto one stack coherent. After three
/// concurrent browses in one session, the stack holds all three levels, and a
/// following `load` with no explicit level pages whichever landed last - not the
/// one this caller asked for.
///
/// Two clients that must navigate independently need two session keys. That is what
/// `roon_browse_handler`'s note says and what #399's nav handle has to enforce.
///
/// Scope of the evidence: the level stack is the *fake's* model of Roon's session
/// semantics (`tests/mock_servers/roon_core.rs`, `handle_browse`), derived from the
/// fork and from this repo's own `pop_all` usage. It has not been recorded off a
/// live Core. So read this as "UHC's own model of a session says sharing one is
/// incoherent", which is enough to justify the note - not as a measurement of Roon.
#[tokio::test]
async fn a_shared_session_key_is_one_level_stack_however_well_results_correlate() {
    let core = FakeRoonCore::start().await;
    let adapter = connected(&core).await;
    let session = "one_stack_three_callers";

    adapter.browse(browse_root(session)).await.unwrap();
    let root = adapter.load(load_all(session)).await.unwrap();
    let keys: Vec<String> = (0..3)
        .map(|i| root.items[i].item_key.clone().unwrap())
        .collect();
    for (i, key) in keys.iter().enumerate() {
        core.set_delay_for_item_key(key, Duration::from_millis(60 * (3 - i) as u64))
            .await;
    }

    let got: Vec<String> = futures::future::join_all(
        keys.iter()
            .map(|k| adapter.browse(browse_at(session, Some(k)))),
    )
    .await
    .into_iter()
    .map(|r| r.expect("browse").list.map(|l| l.title).unwrap_or_default())
    .collect();

    // Each caller was told the truth about its own request - which is order
    // independent, because each future asked for `keys[i]` and must be handed
    // `keys[i]`'s list whatever order the answers arrived in.
    assert_eq!(got, vec!["Library", "TIDAL", "Qobuz"]);

    // ...onto a single stack. Which of the three landed last is the Core's
    // scheduling business, so this asserts the shape and not the sequence: three
    // concurrent browses from one root produced one four-level stack, not three
    // independent two-level ones. (An earlier draft pinned the exact order the
    // delays happened to produce, which would have flaked on a loaded machine for
    // no extra claim.)
    let levels = core.session_levels(session).await;
    assert_eq!(
        levels.len(),
        4,
        "one session key is one level stack; three concurrent browses all push onto \
         it, got {levels:?}"
    );
    assert_eq!(levels[0], "Explore", "stack {levels:?}");
    let mut pushed = levels[1..].to_vec();
    pushed.sort();
    assert_eq!(
        pushed,
        vec!["Library", "Qobuz", "TIDAL"],
        "stack {levels:?}"
    );

    // So the session's own idea of "here" is whichever push landed last, not the
    // one any particular caller made.
    let here = adapter.load(load_all(session)).await.unwrap();
    assert_eq!(
        Some(&here.list.title),
        levels.last(),
        "a following load pages the level that landed last, whoever asked for it"
    );

    core.stop().await;
}

// =============================================================================
// Multiroom grouping: group_outputs / ungroup_outputs (issue #509)
// =============================================================================
//
// Roon confirms grouping asynchronously through the ordinary zone
// subscription (`RoonAdapter::set_group_members`'s own docs explain why), so
// these tests poll `RoonAdapter::get_zones` rather than asserting on the
// instant `set_group_members`/`ungroup_members` return.

/// Poll `RoonAdapter::get_zones` until it reports exactly `expected` zones or
/// a bound is hit, then return the last observation either way -- so a
/// failing assertion downstream shows what actually arrived instead of just
/// "timed out".
async fn wait_for_zone_count(
    adapter: &RoonAdapter,
    expected: usize,
) -> Vec<unified_hifi_control::adapters::roon::Zone> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let zones = adapter.get_zones().await;
        if zones.len() == expected || Instant::now() >= deadline {
            return zones;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn roon_multiroom_status_reports_no_groups_for_single_output_zones() {
    let core = FakeRoonCore::start().await;
    core.set_zones(vec![
        zone_with_grouping("zone_a", "Living Room", "output_a", &["output_b"]),
        zone_with_grouping("zone_b", "Kitchen", "output_b", &["output_a"]),
    ])
    .await;
    let adapter = connected(&core).await;
    // `connected()` only waits for Browse to register; the zone subscription
    // it triggers as a side effect can still be in flight, so wait for both
    // configured zones to have actually arrived before trusting the status
    // below to mean anything.
    wait_for_zone_count(&adapter, 2).await;

    let status = adapter.multiroom_status().await.unwrap();
    assert_eq!(
        status["groups"].as_array().unwrap().len(),
        0,
        "no zone has more than one output yet"
    );

    core.stop().await;
}

#[tokio::test]
async fn roon_set_group_members_merges_outputs_into_one_zone() {
    let core = FakeRoonCore::start().await;
    core.set_zones(vec![
        zone_with_grouping("zone_a", "Living Room", "output_a", &["output_b"]),
        zone_with_grouping("zone_b", "Kitchen", "output_b", &["output_a"]),
    ])
    .await;
    let adapter = connected(&core).await;
    // See the status test above: wait for both zones before grouping them.
    wait_for_zone_count(&adapter, 2).await;

    adapter
        .set_group_members("roon:zone_a", &["roon:zone_b".to_string()], &[])
        .await
        .expect("set_group_members should succeed");

    // Merging is confirmed asynchronously (the Core's `group_outputs` handler
    // runs in its own spawned task, and the merge itself is only visible once
    // its zone push round-trips back to the adapter), so wait for that before
    // inspecting either the wire log or the adapter's own zone state.
    let zones = wait_for_zone_count(&adapter, 1).await;
    assert_eq!(
        zones.len(),
        1,
        "the two zones merge into one once the Core's push arrives"
    );

    // The wire request the Core actually saw: `group_outputs` addressed by
    // output id, leader first, exactly as `Transport::group_outputs`
    // (`transport.rs:334-341`) sends it.
    let group_requests: Vec<_> = core
        .requests()
        .await
        .into_iter()
        .filter(|r| r.name == "com.roonlabs.transport:2/group_outputs")
        .collect();
    assert_eq!(group_requests.len(), 1, "exactly one group_outputs call");
    assert_eq!(
        group_requests[0].body["output_ids"],
        serde_json::json!(["output_a", "output_b"])
    );
    let merged = &zones[0];
    assert_eq!(merged.zone_id, "zone_a", "the leader's zone_id survives");
    let mut output_ids: Vec<&str> = merged
        .outputs
        .iter()
        .map(|o| o.output_id.as_str())
        .collect();
    output_ids.sort_unstable();
    assert_eq!(output_ids, vec!["output_a", "output_b"]);

    let status = adapter.multiroom_status().await.unwrap();
    let groups = status["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1, "the merged zone reports as one group");
    assert_eq!(
        groups[0]["leader_zone_id"],
        serde_json::json!("roon:zone_a")
    );
    assert_eq!(
        groups[0]["member_zone_ids"],
        serde_json::json!(["roon:output_b"]),
        "member zone_a was retired, so the surviving member is named by output id"
    );

    core.stop().await;
}

#[tokio::test]
async fn roon_ungroup_members_splits_outputs_back_into_separate_zones() {
    let core = FakeRoonCore::start().await;
    core.set_zones(vec![
        zone_with_grouping("zone_a", "Living Room", "output_a", &["output_b"]),
        zone_with_grouping("zone_b", "Kitchen", "output_b", &["output_a"]),
    ])
    .await;
    let adapter = connected(&core).await;
    wait_for_zone_count(&adapter, 2).await;

    adapter
        .set_group_members("roon:zone_a", &["roon:zone_b".to_string()], &[])
        .await
        .expect("initial grouping should succeed");
    wait_for_zone_count(&adapter, 1).await;

    adapter
        .ungroup_members(&["roon:output_b".to_string()])
        .await
        .expect("ungroup_members should succeed");

    // See the merge test above for why this waits before inspecting either
    // the wire log or the adapter's own zone state.
    let zones = wait_for_zone_count(&adapter, 2).await;
    assert_eq!(
        zones.len(),
        2,
        "output_b splits back into its own zone once the Core's push arrives"
    );

    let ungroup_requests: Vec<_> = core
        .requests()
        .await
        .into_iter()
        .filter(|r| r.name == "com.roonlabs.transport:2/ungroup_outputs")
        .collect();
    assert_eq!(
        ungroup_requests.len(),
        1,
        "exactly one ungroup_outputs call"
    );
    assert_eq!(
        ungroup_requests[0].body["output_ids"],
        serde_json::json!(["output_b"])
    );
    assert!(
        zones.iter().all(|z| z.outputs.len() == 1),
        "each zone is single-output again: {zones:?}"
    );

    let status = adapter.multiroom_status().await.unwrap();
    assert_eq!(
        status["groups"].as_array().unwrap().len(),
        0,
        "no zone has more than one output after the split"
    );

    core.stop().await;
}

#[tokio::test]
async fn roon_set_group_members_refuses_mixed_protocol_outputs() {
    let core = FakeRoonCore::start().await;
    // Neither output lists the other as groupable -- e.g. one is RAAT and the
    // other AirPlay. Real Roon Cores only group outputs that share a
    // streaming protocol; `can_group_with_output_ids` is the Core's own
    // compatibility list for exactly this.
    core.set_zones(vec![
        zone_with_grouping("zone_a", "Living Room", "output_a", &[]),
        zone_with_grouping("zone_b", "Kitchen", "output_b", &[]),
    ])
    .await;
    let adapter = connected(&core).await;
    wait_for_zone_count(&adapter, 2).await;

    let result = adapter
        .set_group_members("roon:zone_a", &["roon:zone_b".to_string()], &[])
        .await;

    let error = result.expect_err("mixed-protocol grouping must be refused, not attempted");
    let message = error.to_string();
    assert!(
        message.contains("protocol"),
        "refusal should explain why, got: {message}"
    );

    let group_requests: Vec<_> = core
        .requests()
        .await
        .into_iter()
        .filter(|r| r.name == "com.roonlabs.transport:2/group_outputs")
        .collect();
    assert!(
        group_requests.is_empty(),
        "the incompatible request must never reach the Core: {group_requests:?}"
    );

    core.stop().await;
}
