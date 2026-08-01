//! MCP ref lifecycle tests (issue #396): search -> hold ref -> play by ref.
//!
//! These drive `handle_search` / `handle_play_ref` directly against real
//! adapters -- `RoonAdapter` connected to `FakeRoonCore`
//! (`tests/mock_servers/roon_core.rs`, issue #408), and `LmsAdapter` connected
//! to `MockLmsServer` extended just enough for this issue's ref tests (see
//! that mock's own doc comments; it is not a #417 fix). Calling the tool
//! handlers directly, rather than round-tripping through the axum `/mcp`
//! route as `tests/mcp_contract.rs` does, keeps this file's setup to the
//! minimum needed to exercise the new resolution paths -- the wire-transport
//! layer is already covered by that file's own tests and is unchanged here.
//!
//! # Roon test strategy (the issue asks this to be stated explicitly)
//!
//! Chosen: exercise the ref mint/resolve path against `FakeRoonCore`, a real
//! protocol-level fake, rather than inventing new unit-only mocking. Reasons:
//!
//! - It is the only thing in this repo that can prove a Roon *success* path
//!   at all (`tests/roon_protocol.rs`'s own docs).
//! - It already models the exact ambiguity this issue's design rests on --
//!   `ItemKeyScope::Global` vs `ItemKeyScope::PerSession` -- so the same fake
//!   that pins `a_foreign_item_key_is_rejected_when_keys_are_session_scoped`
//!   (proving `play_item`'s fresh-session approach is broken under
//!   `PerSession`) can prove `play_ref`'s session-reentry approach is *not*
//!   broken under the same setting. That is a stronger claim than any
//!   hand-rolled mock could support without duplicating the fake's own
//!   session-stack model.
//! - It is validated by the real `roon_api` client library, not by a
//!   hand-written client (`tests/mock_servers/roon_core.rs`'s own docs), so a
//!   wire-format regression in the adapter fails here rather than reading as
//!   coverage.

mod mock_servers;

use std::sync::Arc;
use std::time::{Duration, Instant};

use mock_servers::roon_core::{FakeRoonCore, ItemKeyScope};
use mock_servers::MockLmsServer;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use rust_mcp_sdk::schema::{schema_utils::CallToolError, CallToolResult, ContentBlock};

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
use unified_hifi_control::knobs::KnobStore;
use unified_hifi_control::mcp::tools::library::{
    handle_play_ref, handle_search, HifiPlayRefTool, HifiSearchTool,
};

// =============================================================================
// Shared helpers
// =============================================================================

type ToolResult = Result<CallToolResult, CallToolError>;

fn text_of(result: &ToolResult) -> String {
    let result = result
        .as_ref()
        .expect("tool must not return a transport-level error");
    match result.content.first() {
        Some(ContentBlock::TextContent(t)) => t.text.clone(),
        other => panic!("expected exactly one text block, got {other:?}"),
    }
}

fn structured_of(result: &ToolResult) -> serde_json::Map<String, Value> {
    let result = result
        .as_ref()
        .expect("tool must not return a transport-level error");
    result
        .structured_content
        .clone()
        .expect("every tool result must carry an envelope")
}

fn outcome_of(result: &ToolResult) -> String {
    structured_of(result)
        .get("outcome")
        .and_then(Value::as_str)
        .unwrap_or("<missing>")
        .to_string()
}

fn refusal_reason_of(result: &ToolResult) -> Option<String> {
    structured_of(result)
        .get("refusal")
        .and_then(|r| r.get("reason"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn operation_of(result: &ToolResult) -> String {
    structured_of(result)
        .get("operation")
        .and_then(Value::as_str)
        .unwrap_or("<missing>")
        .to_string()
}

/// Search results as the JSON array `hifi_search`'s envelope carries in `data`.
fn search_results_of(result: &ToolResult) -> Vec<Value> {
    structured_of(result)
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

// =============================================================================
// Roon: FakeRoonCore-backed AppState
// =============================================================================

/// Mirrors `tests/roon_protocol.rs::isolate_config_dir` -- keeps Roon pairing
/// state out of the operator's real config directory. Both test binaries set
/// the same env var; `OnceLock` makes the redundant `set_var` harmless within
/// this binary's own process.
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

async fn connected_roon(core: &FakeRoonCore) -> Arc<RoonAdapter> {
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
    panic!("adapter never reported Browse connected");
}

/// An `AppState` wired to a real (connected) Roon adapter and disconnected LMS
/// -- enough to exercise routing, cross-provider refusal, and both providers'
/// ref resolution from one state.
async fn app_state_with_roon(roon: Arc<RoonAdapter>) -> AppState {
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
        CancellationToken::new(),
    )
}

/// Same shape, but with a real (connected) LMS adapter and disconnected Roon
/// -- for the LMS-side ref tests.
async fn app_state_with_lms(lms: Arc<LmsAdapter>) -> AppState {
    let bus = create_bus();
    let roon = Arc::new(RoonAdapter::new_disconnected(bus.clone()));
    let hqp_instances = Arc::new(HqpInstanceManager::new(bus.clone()));
    let hqplayer = hqp_instances.get_default().await;
    let hqp_zone_links = Arc::new(HqpZoneLinkService::new(hqp_instances.clone()));
    let openhome = Arc::new(OpenHomeAdapter::new(bus.clone()));
    let upnp = Arc::new(UPnPAdapter::new(bus.clone()));
    let startable: Vec<Arc<dyn Startable>> = vec![lms.clone()];

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
        CancellationToken::new(),
    )
}

fn search_args(query: &str) -> HifiSearchTool {
    HifiSearchTool {
        query: query.to_string(),
        zone_id: None,
        source: None,
    }
}

fn play_ref_args(r#ref: &str, zone_id: &str, action: Option<&str>) -> HifiPlayRefTool {
    HifiPlayRefTool {
        r#ref: r#ref.to_string(),
        zone_id: zone_id.to_string(),
        action: action.map(str::to_string),
    }
}

// =============================================================================
// Roon: mint, resolve, and the session-reentry design this issue turns on
// =============================================================================

/// The straight-line path: search mints a `ref`, `hifi_play_ref` resolves it
/// to the same action `play_item` would reach, and the Core was actually
/// asked to invoke Play Now on the album `hifi_search` found.
#[tokio::test]
async fn roon_search_mints_a_ref_and_play_ref_resolves_it() {
    let core = FakeRoonCore::start().await;
    let adapter = connected_roon(&core).await;
    let state = app_state_with_roon(adapter).await;

    let search = handle_search(&state, search_args("kind of blue")).await;
    let results = search_results_of(&search);
    assert_eq!(results.len(), 1);
    let hit = &results[0];
    assert_eq!(
        hit.get("title"),
        Some(&Value::String("Kind of Blue".to_string()))
    );
    let ref_token = hit
        .get("ref")
        .and_then(Value::as_str)
        .expect("a Roon hit with an item_key must carry a ref")
        .to_string();
    assert!(ref_token.starts_with("ref_"), "got {ref_token:?}");

    let played = handle_play_ref(&state, play_ref_args(&ref_token, "roon:zone_fake_1", None)).await;
    assert_eq!(
        outcome_of(&played),
        "accepted",
        "text was: {}",
        text_of(&played)
    );
    assert_eq!(text_of(&played), "Play Now: Play Album");

    // The Core was actually asked to invoke Play Now, reached by entering the
    // album `hifi_search` returned and then its action list -- not merely
    // "some" playable action.
    assert_eq!(
        core.browsed_titles().await,
        vec![
            "Library",
            "Search",
            "Kind of Blue",
            "Play Album",
            "Play Now"
        ]
    );

    core.stop().await;
}

/// **The money test.** Flip the Core to `ItemKeyScope::PerSession` -- the
/// setting the issue's own evidence (home-assistant/core#137605; Roon Labs
/// community thread 23129) points toward being the real one -- and
/// `play_item`'s fresh-session approach is proven broken by
/// `tests/roon_protocol.rs::a_foreign_item_key_is_rejected_when_keys_are_session_scoped`.
/// This is that same setup, through `hifi_play_ref` instead: it must still
/// succeed, because resolution re-enters the exact session that minted the
/// ref rather than a fresh, unrelated one.
#[tokio::test]
async fn roon_ref_resolves_under_per_session_item_key_scope() {
    let core = FakeRoonCore::start().await;
    let adapter = connected_roon(&core).await;
    let state = app_state_with_roon(adapter).await;

    let search = handle_search(&state, search_args("kind of blue")).await;
    let ref_token = search_results_of(&search)[0]
        .get("ref")
        .and_then(Value::as_str)
        .expect("ref")
        .to_string();

    // Now that the ref exists, make the Core reject any item_key used outside
    // the session that minted it.
    core.set_item_key_scope(ItemKeyScope::PerSession).await;

    let played = handle_play_ref(&state, play_ref_args(&ref_token, "roon:zone_fake_1", None)).await;
    assert_eq!(
        outcome_of(&played),
        "accepted",
        "resolving inside the minting session must succeed even when keys are \
         session-scoped; text was: {}",
        text_of(&played)
    );
    assert_eq!(text_of(&played), "Play Now: Play Album");

    core.stop().await;
}

/// Two refs minted by the *same* search share one `multi_session_key`
/// (`RoonAdapter::search_with_session` mints exactly one per call). Resolving
/// both in sequence must not let the first resolution's navigation strand the
/// second -- the `pop_all: true` reset in `RoonAdapter::play_ref` is exactly
/// what this proves.
#[tokio::test]
async fn two_refs_from_one_search_each_resolve_independently() {
    let core = FakeRoonCore::start().await;
    let adapter = connected_roon(&core).await;
    let state = app_state_with_roon(adapter).await;

    // "blue" matches two library hits (see tests/roon_protocol.rs's
    // `search_source_selects_a_different_branch`): Kind of Blue and Blue Train.
    let search = handle_search(&state, search_args("blue")).await;
    let results = search_results_of(&search);
    assert_eq!(results.len(), 2, "expected two hits, got {results:?}");

    let refs: Vec<String> = results
        .iter()
        .map(|r| {
            r.get("ref")
                .and_then(Value::as_str)
                .expect("each hit should carry a ref")
                .to_string()
        })
        .collect();
    assert_ne!(refs[0], refs[1], "two different items must not share a ref");

    // Resolve the first, then the second, on the same shared session.
    let first = handle_play_ref(&state, play_ref_args(&refs[0], "roon:zone_fake_1", None)).await;
    assert_eq!(
        outcome_of(&first),
        "accepted",
        "first resolve: {}",
        text_of(&first)
    );

    let second = handle_play_ref(&state, play_ref_args(&refs[1], "roon:zone_fake_1", None)).await;
    assert_eq!(
        outcome_of(&second),
        "accepted",
        "second resolve must not be stranded by the first's navigation: {}",
        text_of(&second)
    );

    core.stop().await;
}

/// **Live-rig regression** (#396 ship-gate re-review): an earlier version of
/// `RoonAdapter::play_ref` reset the minting session's browse stack with
/// `pop_all: true` before entering the ref's `item_key`, in the same browse
/// request. That hangs against a real Core (verified live: nuc14, Roon 2.70)
/// -- the Core never answers, and the caller waits out the full
/// `BROWSE_TIMEOUT`. `hifi_search` worked, `hifi_play_ref` on the very same
/// zone and query did not; the two paths' Roon calls had to be diffed to find
/// where they diverged, which is exactly what this test pins so it cannot
/// happen silently again.
///
/// This asserts the *structural* claim directly, independent of timing:
/// `play_ref` must never send a browse request combining `pop_all: true` with
/// a present `item_key`. `FakeRoonCore` answers that combination with a fast,
/// loud `InvalidItemKey` (see `tests/mock_servers/roon_core.rs::handle_browse`)
/// rather than reproducing the real hang, precisely so a regression here
/// fails in milliseconds, not after a real 10-second timeout.
#[tokio::test]
async fn play_ref_never_combines_pop_all_with_an_item_key() {
    let core = FakeRoonCore::start().await;
    let adapter = connected_roon(&core).await;
    let state = app_state_with_roon(adapter).await;

    let search = handle_search(&state, search_args("kind of blue")).await;
    let ref_token = search_results_of(&search)[0]
        .get("ref")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    let started = Instant::now();
    let played = handle_play_ref(&state, play_ref_args(&ref_token, "roon:zone_fake_1", None)).await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "resolution must be fast; a regression to the pop_all+item_key combination \
         degrades to a ~10s BROWSE_TIMEOUT wait even against the fake"
    );
    assert_eq!(
        outcome_of(&played),
        "accepted",
        "text was: {}",
        text_of(&played)
    );

    assert!(
        core.illegal_pop_all_with_item_key_attempts()
            .await
            .is_empty(),
        "play_ref must never combine pop_all:true with a present item_key in one \
         browse request -- this hangs against a real Roon Core"
    );

    core.stop().await;
}

/// `action=queue` reaches Queue, not Play Now -- proving the action parameter
/// actually threads through `play_ref`, not just the default.
#[tokio::test]
async fn roon_play_ref_honors_the_queue_action() {
    let core = FakeRoonCore::start().await;
    let adapter = connected_roon(&core).await;
    let state = app_state_with_roon(adapter).await;

    let search = handle_search(&state, search_args("kind of blue")).await;
    let ref_token = search_results_of(&search)[0]
        .get("ref")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    let played = handle_play_ref(
        &state,
        play_ref_args(&ref_token, "roon:zone_fake_1", Some("queue")),
    )
    .await;
    assert_eq!(outcome_of(&played), "accepted");
    // `operation` reflects the resolved action, matching hifi_play's own
    // convention -- a client reading only `operation` can tell this apart
    // from a plain play, without also reading `params.action`.
    assert_eq!(operation_of(&played), "queue");
    let invoked = core.browsed_titles().await;
    assert!(invoked.contains(&"Queue".to_string()), "got {invoked:?}");
    assert!(
        !invoked.contains(&"Play Now".to_string()),
        "got {invoked:?}"
    );

    core.stop().await;
}

/// `action=next` is LMS-only; a Roon ref must refuse it capability-honestly
/// rather than silently falling back to "play" -- `PlayAction::parse` would
/// otherwise do exactly that.
#[tokio::test]
async fn roon_play_ref_refuses_next_instead_of_silently_defaulting() {
    let core = FakeRoonCore::start().await;
    let adapter = connected_roon(&core).await;
    let state = app_state_with_roon(adapter).await;

    let search = handle_search(&state, search_args("kind of blue")).await;
    let ref_token = search_results_of(&search)[0]
        .get("ref")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    // Snapshot what the search itself already browsed (Library, then Search),
    // so the assertion below is "nothing *new* happened", not "nothing ever
    // happened" -- the latter would be wrong regardless of the refusal.
    let before = core.browsed_titles().await;

    let played = handle_play_ref(
        &state,
        play_ref_args(&ref_token, "roon:zone_fake_1", Some("next")),
    )
    .await;
    assert_eq!(
        outcome_of(&played),
        "invalid",
        "text was: {}",
        text_of(&played)
    );
    assert_eq!(
        refusal_reason_of(&played).as_deref(),
        Some("invalid_parameter")
    );
    // `operation` must be a normalized sentinel, not an echo of the raw
    // (invalid) request -- matching hifi_control's own `unknown_action`
    // convention (src/mcp/tools/transport.rs) for the same class of refusal.
    // A client branching on `operation` should never see arbitrary,
    // unvalidated client input reflected back as if it were a recognized
    // value.
    assert_eq!(operation_of(&played), "invalid_action");
    // And nothing new was invoked on the Core -- the refusal happened before
    // dispatch, not after a failed attempt.
    assert_eq!(
        core.browsed_titles().await,
        before,
        "an invalid action must refuse before touching the Core"
    );

    core.stop().await;
}

// =============================================================================
// Cross-provider and unknown-ref refusals (provider-agnostic; Roon-backed
// state is enough to exercise both, since routing is by zone_id prefix)
// =============================================================================

/// A ref minted for Roon, resolved against an `lms:` zone, must be refused
/// capability-honestly rather than the server guessing which provider to
/// trust.
#[tokio::test]
async fn a_roon_ref_used_against_an_lms_zone_is_refused() {
    let core = FakeRoonCore::start().await;
    let adapter = connected_roon(&core).await;
    let state = app_state_with_roon(adapter).await;

    let search = handle_search(&state, search_args("kind of blue")).await;
    let ref_token = search_results_of(&search)[0]
        .get("ref")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    let played = handle_play_ref(
        &state,
        play_ref_args(&ref_token, "lms:aa:bb:cc:dd:ee:ff", None),
    )
    .await;
    assert_eq!(
        outcome_of(&played),
        "invalid",
        "text was: {}",
        text_of(&played)
    );
    assert_eq!(
        refusal_reason_of(&played).as_deref(),
        Some("invalid_parameter")
    );
    // Cross-provider mismatch is caught before either provider's action set
    // is even consulted, so `operation` is the tool-shaped constant, not a
    // half-validated action name from either side.
    assert_eq!(operation_of(&played), "play_ref");
    let text = text_of(&played);
    assert!(text.to_lowercase().contains("roon"), "got {text:?}");
    assert!(text.to_lowercase().contains("lms"), "got {text:?}");

    // And the ref is still good against its own provider -- a rejected
    // cross-provider attempt must not burn the ref.
    let retried =
        handle_play_ref(&state, play_ref_args(&ref_token, "roon:zone_fake_1", None)).await;
    assert_eq!(
        outcome_of(&retried),
        "accepted",
        "text was: {}",
        text_of(&retried)
    );

    core.stop().await;
}

/// A ref that never existed (or has expired/been evicted -- indistinguishable
/// by design, see `src/mcp/refs.rs`) produces a distinct, retryable outcome
/// that names `hifi_search` as the recovery path, never a silent fallback to
/// first-match.
#[tokio::test]
async fn an_unknown_ref_is_refused_and_names_hifi_search_as_the_recovery() {
    let core = FakeRoonCore::start().await;
    let adapter = connected_roon(&core).await;
    let state = app_state_with_roon(adapter).await;

    let played = handle_play_ref(
        &state,
        play_ref_args("ref_this-was-never-minted", "roon:zone_fake_1", None),
    )
    .await;
    assert_eq!(
        outcome_of(&played),
        "invalid",
        "text was: {}",
        text_of(&played)
    );
    assert_eq!(
        refusal_reason_of(&played).as_deref(),
        Some("unknown_target")
    );
    // An unresolvable ref means no provider action set was ever consulted,
    // so `operation` stays the tool-shaped constant.
    assert_eq!(operation_of(&played), "play_ref");
    let structured = structured_of(&played);
    let discover_with = structured
        .get("refusal")
        .and_then(|r| r.get("discover_with"))
        .and_then(Value::as_str);
    assert_eq!(discover_with, Some("hifi_search"));
    // Nothing was ever asked of the Core -- the table lookup failed before
    // any adapter call.
    assert!(core
        .requests()
        .await
        .iter()
        .all(|r| r.name.contains("registry") || r.name.contains("subscribe_zones")));

    core.stop().await;
}

/// A hand-mangled ref (a real one, with one character flipped) is refused
/// the same way an unknown one is -- never misresolved to a different, real
/// item. Complements `src/mcp/refs.rs`'s table-level test of the same claim,
/// this time through the actual MCP tool.
#[tokio::test]
async fn a_mangled_ref_is_refused_through_the_tool_not_misresolved() {
    let core = FakeRoonCore::start().await;
    let adapter = connected_roon(&core).await;
    let state = app_state_with_roon(adapter).await;

    let search = handle_search(&state, search_args("kind of blue")).await;
    let ref_token = search_results_of(&search)[0]
        .get("ref")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    let mut mangled = ref_token.clone();
    let last = mangled.pop().unwrap();
    mangled.push(if last == 'A' { 'B' } else { 'A' });
    assert_ne!(mangled, ref_token);

    let played = handle_play_ref(&state, play_ref_args(&mangled, "roon:zone_fake_1", None)).await;
    assert_eq!(outcome_of(&played), "invalid");
    assert_eq!(
        refusal_reason_of(&played).as_deref(),
        Some("unknown_target")
    );

    core.stop().await;
}

// =============================================================================
// LMS: durable Library refs, validate-before-mutate
// =============================================================================

async fn connected_lms(mock: &MockLmsServer) -> Arc<LmsAdapter> {
    let bus = create_bus();
    let lms = Arc::new(LmsAdapter::new(bus));
    lms.configure(
        mock.addr().ip().to_string(),
        Some(mock.addr().port()),
        None,
        None,
    )
    .await;
    lms.start().await.expect("LMS adapter must start");
    lms
}

const LMS_PLAYER: &str = "aa:bb:cc:dd:ee:ff";

/// A durable `Library` ref: minted from the `search_library` fallback path
/// (globalsearch is unmodeled by the mock, so this is the addressable path
/// LMS actually falls back to), resolved through `hifi_play_ref`, landing on
/// exactly the `playlistcontrol` command `LmsAdapter::play_target` builds --
/// not `search_and_play`, which this path never touches.
#[tokio::test]
async fn lms_search_mints_a_durable_ref_and_play_ref_resolves_it() {
    let mock = MockLmsServer::start().await;
    mock.add_player(LMS_PLAYER, "Living Room").await;
    mock.set_library_albums(vec![(101, "Kind of Blue", "Miles Davis")])
        .await;
    let lms = connected_lms(&mock).await;
    let state = app_state_with_lms(lms).await;
    let zone_id = format!("lms:{LMS_PLAYER}");

    let search = handle_search(
        &state,
        HifiSearchTool {
            query: "kind of blue".to_string(),
            zone_id: Some(zone_id.clone()),
            source: None,
        },
    )
    .await;
    let results = search_results_of(&search);
    assert_eq!(results.len(), 1, "got {results:?}");
    let ref_token = results[0]
        .get("ref")
        .and_then(Value::as_str)
        .expect("a Library-backed LMS result must carry a ref")
        .to_string();

    mock.clear_commands().await;
    let played = handle_play_ref(&state, play_ref_args(&ref_token, &zone_id, None)).await;
    assert_eq!(
        outcome_of(&played),
        "accepted",
        "text was: {}",
        text_of(&played)
    );

    let commands = mock.write_commands(LMS_PLAYER).await;
    assert!(
        commands.contains(&vec![
            "playlistcontrol".to_string(),
            "cmd:load".to_string(),
            "album_id:101".to_string(),
        ]),
        "got {commands:?}"
    );

    mock.stop().await;
    lms_stop(&state).await;
}

/// **Validate before mutate.** The library was rescanned (id 101 no longer
/// exists) between mint and resolve. `hifi_play_ref` must refuse rather than
/// issue `playlistcontrol cmd:load`, which the issue's own evidence says
/// clears the queue *and then* fails -- the wipe-reported-as-failure trap
/// this method exists to avoid.
#[tokio::test]
async fn lms_stale_library_ref_is_refused_without_mutating_the_queue() {
    let mock = MockLmsServer::start().await;
    mock.add_player(LMS_PLAYER, "Living Room").await;
    mock.set_library_albums(vec![(101, "Kind of Blue", "Miles Davis")])
        .await;
    let lms = connected_lms(&mock).await;
    let state = app_state_with_lms(lms).await;
    let zone_id = format!("lms:{LMS_PLAYER}");

    let search = handle_search(
        &state,
        HifiSearchTool {
            query: "kind of blue".to_string(),
            zone_id: Some(zone_id.clone()),
            source: None,
        },
    )
    .await;
    let ref_token = search_results_of(&search)[0]
        .get("ref")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    // Simulate a rescan: the album is gone.
    mock.set_library_albums(vec![]).await;

    mock.clear_commands().await;
    let played = handle_play_ref(&state, play_ref_args(&ref_token, &zone_id, None)).await;
    assert_eq!(
        outcome_of(&played),
        "error",
        "text was: {}",
        text_of(&played)
    );

    // The load-bearing assertion: playlistcontrol was never sent.
    let commands = mock.write_commands(LMS_PLAYER).await;
    assert!(
        commands
            .iter()
            .all(|c| c.first().map(String::as_str) != Some("playlistcontrol")),
        "a stale ref must not reach playlistcontrol: {commands:?}"
    );

    mock.stop().await;
    lms_stop(&state).await;
}

/// **The dissent's own finding, checked empirically rather than left as a
/// hypothesis.** `assert_library_id_exists` re-validates a `Library` ref by
/// searching the *title captured at mint time* and checking whether the
/// target id appears among the (at most 50) results that search returns. If
/// many albums share the exact same title -- a box set re-release, a
/// self-titled album by different artists, a generic "Live" -- and the
/// target is not among the first 50 the mock (or a real server) happens to
/// return, does a genuinely still-valid id get wrongly refused?
///
/// This seeds 60 albums sharing one identical title ("Common Album", more
/// than `search_library`'s hardcoded limit of 50) with distinct artists, and
/// mints a ref for the 60th one specifically -- disambiguated at mint time by
/// artist, which the mock's search also matches on -- so it lands last in the
/// mock's insertion-order-preserving results and therefore *outside* a
/// `skip(0).take(50)` window keyed only on the shared title. If this test
/// fails, the dissent's D1 finding is confirmed as a real, shipped bug and
/// `assert_library_id_exists` needs a wider limit (or a different query
/// shape, e.g. a combined title+id filter) before this is safe to rely on
/// for large libraries. If it passes, the current limit was enough for this
/// specific shape and the risk is bounded to libraries with more than 50
/// exact-title-duplicates -- worth knowing either way, not merely asserting.
#[tokio::test]
async fn lms_a_valid_ref_beyond_the_validation_search_limit_still_resolves() {
    let mock = MockLmsServer::start().await;
    mock.add_player(LMS_PLAYER, "Living Room").await;

    let mut albums: Vec<(i64, String, String)> = Vec::new();
    for i in 1..=60i64 {
        albums.push((100 + i, "Common Album".to_string(), format!("Artist {i}")));
    }
    let albums_ref: Vec<(i64, &str, &str)> = albums
        .iter()
        .map(|(id, t, a)| (*id, t.as_str(), a.as_str()))
        .collect();
    mock.set_library_albums(albums_ref).await;

    let lms = connected_lms(&mock).await;
    let state = app_state_with_lms(lms).await;
    let zone_id = format!("lms:{LMS_PLAYER}");

    // Mint a ref for the 60th album specifically, disambiguated by artist --
    // its title alone ("Common Album") matches all 60.
    let search = handle_search(
        &state,
        HifiSearchTool {
            query: "Artist 60".to_string(),
            zone_id: Some(zone_id.clone()),
            source: None,
        },
    )
    .await;
    let results = search_results_of(&search);
    assert_eq!(
        results.len(),
        1,
        "expected an exact artist match, got {results:?}"
    );
    let ref_token = results[0]
        .get("ref")
        .and_then(Value::as_str)
        .expect("ref")
        .to_string();

    let played = handle_play_ref(&state, play_ref_args(&ref_token, &zone_id, None)).await;
    assert_eq!(
        outcome_of(&played),
        "accepted",
        "a still-valid id must not be refused merely because a broader re-validation \
         search (by its shared title) would not have surfaced it within its own limit; \
         text was: {}",
        text_of(&played)
    );

    mock.stop().await;
    lms_stop(&state).await;
}

async fn lms_stop(state: &AppState) {
    state.lms.stop().await;
}
