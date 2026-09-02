//! LMS internet radio, the Library page's Radio tab.
//!
//! Every response is replayed verbatim from a recording of a live Lyrion Music
//! Server 9.1.2 with the stock TuneIn plugin (`tests/fixtures/lms/`, see that
//! directory's `PROVENANCE.md`).
//!
//! # What this surface is, and what it is not
//!
//! The Library page derives its Favorites and Radio tabs from the one
//! `Favorites` capability and tells them apart by `media_type`. LMS keeps the
//! two in unrelated places: saved favorites are a flat list, while radio is the
//! radio plugin's own browse hierarchy behind `radios`. The adapter ignored
//! `media_type` and served favorites for both, so the two tabs showed identical
//! lists while LMS's station directory was never called at all.
//!
//! # Three things this hierarchy demands that the library queries do not
//!
//! 1. **A player.** `<menu> items` closes the socket on a server-level request
//!    (LMS's documented bad-params failure), so the zone has to reach the
//!    adapter. Every library query in `lms.rs` is server-level.
//! 2. **`want_url:1`.** A station's stream url is a requested field and its only
//!    playback handle -- the same rule that emptied Favorites. Recorded both
//!    ways in `collections_radio_stations_with_want_url.json` and
//!    `..._without_want_url.json`.
//! 3. **`hasitems` and `isaudio` read independently.** A category is navigable
//!    and plays nothing; a station plays and contains nothing. A category also
//!    carries a `Browse.ashx` url when `want_url` is on, so treating "has a url"
//!    as "is playable" would offer a play button that browses.

use axum::{extract::State, routing::post, Json, Router};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use unified_hifi_control::adapters::lms::LmsAdapter;
use unified_hifi_control::bus::create_bus;

const ZONE: &str = "lms:02:00:00:00:00:01";

fn fixture_result(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/lms/{}.json",
        env!("CARGO_MANIFEST_DIR"),
        name
    );
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) => panic!("recorded fixture {path} is missing: {e}"),
    };
    let parsed: Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => panic!("recorded fixture {path} is not valid JSON: {e}"),
    };
    match parsed.get("result") {
        Some(r) => r.clone(),
        None => panic!("recorded fixture {path} has no `result` member"),
    }
}

fn has_param(cmd: &Value, prefix: &str) -> bool {
    cmd.as_array().is_some_and(|a| {
        a.iter()
            .any(|p| p.as_str().is_some_and(|s| s.starts_with(prefix)))
    })
}

// =============================================================================
// Replay server
// =============================================================================

#[derive(Clone)]
struct ReplayState {
    requests: Arc<Mutex<Vec<Value>>>,
    players: Arc<Mutex<Vec<String>>>,
}

struct RadioReplayServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Value>>>,
    players: Arc<Mutex<Vec<String>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl RadioReplayServer {
    async fn start() -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let players = Arc::new(Mutex::new(Vec::new()));
        let state = ReplayState {
            requests: Arc::clone(&requests),
            players: Arc::clone(&players),
        };
        let app = Router::new()
            .route("/jsonrpc.js", post(replay))
            .with_state(state);
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(l) => l,
            Err(e) => panic!("bind replay server: {e}"),
        };
        let addr = match listener.local_addr() {
            Ok(a) => a,
            Err(e) => panic!("local_addr: {e}"),
        };
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            addr,
            requests,
            players,
            handle,
        }
    }

    fn addr(&self) -> SocketAddr {
        self.addr
    }

    fn commands(&self) -> Vec<Value> {
        match self.requests.lock() {
            Ok(g) => g.clone(),
            Err(e) => panic!("request log poisoned: {e}"),
        }
    }

    /// The player id each request was addressed to; `""` is server-level.
    fn players(&self) -> Vec<String> {
        match self.players.lock() {
            Ok(g) => g.clone(),
            Err(e) => panic!("player log poisoned: {e}"),
        }
    }

    fn stop(self) {
        self.handle.abort();
    }
}

async fn replay(State(state): State<ReplayState>, Json(body): Json<Value>) -> Json<Value> {
    let player = body["params"][0].as_str().unwrap_or_default().to_string();
    let cmd = body["params"][1].clone();
    if let Ok(mut log) = state.requests.lock() {
        log.push(cmd.clone());
    }
    if let Ok(mut log) = state.players.lock() {
        log.push(player);
    }

    let verb = cmd.get(0).and_then(Value::as_str).unwrap_or_default();
    let sub = cmd.get(1).and_then(Value::as_str).unwrap_or_default();

    let result = match (verb, sub) {
        ("radios", _) => fixture_result("collections_radios_root"),
        // Inside a menu: an `item_id` means a level below its top.
        (_, "items") => {
            if has_param(&cmd, "item_id:") {
                if has_param(&cmd, "want_url:") {
                    fixture_result("collections_radio_stations_with_want_url")
                } else {
                    fixture_result("collections_radio_stations_without_want_url")
                }
            } else {
                fixture_result("collections_radio_menu_items")
            }
        }
        other => panic!("replay server got an unexpected command: {other:?} ({cmd})"),
    };
    Json(json!({"result": result}))
}

async fn adapter_for(addr: SocketAddr) -> LmsAdapter {
    let adapter = LmsAdapter::new(create_bus());
    adapter
        .configure(addr.ip().to_string(), Some(addr.port()), None, None)
        .await;
    adapter
}

/// The Radio tab's request: the favorites action, told apart by `media_type`.
async fn radio_tab(adapter: &LmsAdapter) -> Value {
    match adapter
        .collections_content(
            "collections_favorites",
            &json!({"media_type": "radio", "zone_id": ZONE, "limit": 20, "offset": 0}),
        )
        .await
    {
        Ok(v) => v,
        Err(e) => panic!("radio tab failed: {e:#}"),
    }
}

async fn browse(adapter: &LmsAdapter, path: &str, zone: Option<&str>) -> anyhow::Result<Value> {
    let mut params = json!({"path": path, "limit": 20, "offset": 0});
    if let Some(zone) = zone {
        params["zone_id"] = json!(zone);
    }
    adapter
        .collections_content("collections_browse", &params)
        .await
}

fn rows(page: &Value) -> Vec<Value> {
    page.get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

// =============================================================================
// The tab, and the top of the hierarchy
// =============================================================================

/// The Radio tab must reach the station directory, not the favorites list.
#[tokio::test]
async fn radio_tab_serves_the_station_directory() {
    let server = RadioReplayServer::start().await;
    let adapter = adapter_for(server.addr()).await;

    let items = rows(&radio_tab(&adapter).await);

    assert!(!items.is_empty(), "the Radio tab came back empty");
    let titles: Vec<&str> = items
        .iter()
        .filter_map(|i| i.get("title").and_then(Value::as_str))
        .collect();
    assert!(
        titles.contains(&"Local Radio") && titles.contains(&"Search TuneIn"),
        "these are not the radio menus LMS serves: {titles:?}"
    );
    assert!(
        server
            .commands()
            .iter()
            .any(|cmd| cmd.get(0).and_then(Value::as_str) == Some("radios")),
        "the Radio tab never called `radios`: {:?}",
        server.commands()
    );
    server.stop();
}

/// Nothing at the top level plays; every row is a menu to walk into.
#[tokio::test]
async fn radio_menus_are_navigable_and_never_playable() {
    let server = RadioReplayServer::start().await;
    let adapter = adapter_for(server.addr()).await;

    for item in rows(&radio_tab(&adapter).await) {
        let path = item.get("path").and_then(Value::as_str).unwrap_or_default();
        assert!(
            path.starts_with("radio:"),
            "radio menu row is not navigable: {item}"
        );
        assert!(
            item.get("url").is_none(),
            "a radio menu is not playable, but this row carries a url: {item}"
        );
    }
    server.stop();
}

// =============================================================================
// Inside a menu
// =============================================================================

/// A category is navigable only -- even though LMS hands it a `Browse.ashx` url
/// once `want_url` is on. Reading "has a url" as "plays" would put a play
/// button on a genre.
#[tokio::test]
async fn categories_inside_a_menu_are_navigable_only() {
    let server = RadioReplayServer::start().await;
    let adapter = adapter_for(server.addr()).await;

    let page = match browse(&adapter, "radio:music", Some(ZONE)).await {
        Ok(p) => p,
        Err(e) => panic!("browsing a radio menu failed: {e:#}"),
    };
    let items = rows(&page);
    assert!(!items.is_empty(), "a radio menu's level is empty: {page}");
    for item in &items {
        assert!(
            item.get("path")
                .and_then(Value::as_str)
                .is_some_and(|p| p.starts_with("radio:music:")),
            "category row is not navigable within its own menu: {item}"
        );
        assert!(
            item.get("url").is_none(),
            "category row was made playable by its Browse.ashx url: {item}"
        );
    }
    server.stop();
}

/// Stations must arrive playable, which means the stream url has to be asked for.
#[tokio::test]
async fn stations_are_playable_with_a_stream_url() {
    let server = RadioReplayServer::start().await;
    let adapter = adapter_for(server.addr()).await;

    let page = match browse(&adapter, "radio:music:b4426ee5.0.1", Some(ZONE)).await {
        Ok(p) => p,
        Err(e) => panic!("browsing a radio station level failed: {e:#}"),
    };
    let items = rows(&page);

    assert!(!items.is_empty(), "a station level is empty: {page}");
    for item in &items {
        assert!(
            item.get("url")
                .and_then(Value::as_str)
                .is_some_and(|u| u.starts_with("http")),
            "station row has no stream url, so nothing can play it: {item}"
        );
        assert!(
            item.get("path").is_none(),
            "a station contains nothing, so it must not be navigable: {item}"
        );
    }
    server.stop();
}

/// Assert on the request too: without the flag LMS omits the url and every
/// station becomes unplayable.
#[tokio::test]
async fn station_query_asks_for_the_stream_url() {
    let server = RadioReplayServer::start().await;
    let adapter = adapter_for(server.addr()).await;

    let _ = browse(&adapter, "radio:music:b4426ee5.0.1", Some(ZONE)).await;

    assert!(
        server.commands().iter().any(|cmd| cmd
            .as_array()
            .is_some_and(|a| a.iter().any(|p| p.as_str() == Some("want_url:1")))),
        "the station query never asked for `want_url:1`: {:?}",
        server.commands()
    );
    server.stop();
}

// =============================================================================
// The player requirement
// =============================================================================

/// XMLBrowser refuses a server-level request by closing the socket, so a radio
/// browse with no zone must fail loudly here rather than reach LMS and come
/// back as an empty level.
#[tokio::test]
async fn radio_browse_without_a_zone_is_refused_before_the_request() {
    let server = RadioReplayServer::start().await;
    let adapter = adapter_for(server.addr()).await;

    let error = match browse(&adapter, "radio:music", None).await {
        Ok(page) => panic!("a zoneless radio browse should not have succeeded: {page}"),
        Err(e) => format!("{e:#}"),
    };

    assert!(
        error.contains("zone"),
        "the refusal should say a zone is required, got: {error}"
    );
    assert!(
        server.commands().is_empty(),
        "nothing should have been sent to LMS: {:?}",
        server.commands()
    );
    server.stop();
}

/// The level queries must be addressed to the zone's own player.
#[tokio::test]
async fn radio_level_queries_are_addressed_to_the_zone_player() {
    let server = RadioReplayServer::start().await;
    let adapter = adapter_for(server.addr()).await;

    let _ = browse(&adapter, "radio:music", Some(ZONE)).await;

    assert_eq!(
        server.players(),
        vec!["02:00:00:00:00:01".to_string()],
        "the radio level query was not addressed to the zone's player (and the \
         `lms:` prefix must be stripped)"
    );
    server.stop();
}
