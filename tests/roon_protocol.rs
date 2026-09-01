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

use mock_servers::roon_core::{album, FakeItem, FakeLibrary, FakeRoonCore, Hint, ItemKeyScope};
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
    let adapter = connected(&core).await;

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
    // Every test in this binary shares the one config directory, so the file is
    // rewritten concurrently and a read can catch it mid-truncate. Retry until it
    // parses and carries this Core's own id.
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
