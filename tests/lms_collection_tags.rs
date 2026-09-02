//! Regression tests for the LMS collection tag set.
//!
//! Every response here is replayed verbatim from a recording of a live Lyrion
//! Music Server 9.1.2 (`tests/fixtures/lms/`, see that directory's
//! `PROVENANCE.md`). Nothing guesses at LMS's shape.
//!
//! # The defect these exist for
//!
//! An LMS `tags:` parameter **replaces** the query's default tag set; it does
//! not extend it. #549 added `tags:cJ` to the `albums`, `titles` and
//! `playlists tracks` queries so artwork had a handle to read, which silently
//! dropped the fields those queries return by default. For `albums` that
//! included `album` -- the title `query_albums` requires -- so its `filter_map`
//! discarded every row and the Albums level, plus every artist's album list,
//! came back empty from a real server at any library size. Tracks and playlist
//! tracks kept their titles but lost the artist subtitle.
//!
//! `favorites items` is the same rule wearing different clothes: it omits `url`
//! unless the request asks `want_url:1`, and a favorite has no other playback
//! handle, so the `url` guard in `list_favorites` discarded every row and the
//! Favorites and Radio tabs came back empty too.
//!
//! # Why the replay server is tag-aware
//!
//! A fixture server that always returns the fully tagged response would pass
//! just as happily with the broken `tags:cJ` request, which is exactly the
//! mock-agrees-with-the-code trap `lms_adapter_defects.rs` was written to
//! avoid. So this server picks its fixture the way LMS itself does: from the
//! tags the request actually asked for. Ask for artwork alone and you get the
//! stripped rows a real server sends, and these tests fail.

use axum::{extract::State, routing::post, Json, Router};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use unified_hifi_control::adapters::lms::LmsAdapter;
use unified_hifi_control::bus::create_bus;

// =============================================================================
// Fixtures
// =============================================================================

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

/// The letters of the request's `tags:` parameter, or `None` when it sent none.
fn requested_tags(cmd: &Value) -> Option<String> {
    cmd.as_array()?.iter().find_map(|part| {
        part.as_str()
            .and_then(|s| s.strip_prefix("tags:"))
            .map(str::to_string)
    })
}

// =============================================================================
// Tag-aware replay server
// =============================================================================

#[derive(Clone)]
struct ReplayState {
    requests: Arc<Mutex<Vec<Value>>>,
}

struct TagReplayServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Value>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl TagReplayServer {
    async fn start() -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = ReplayState {
            requests: Arc::clone(&requests),
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

    fn stop(self) {
        self.handle.abort();
    }
}

/// Serve the recording that matches the request's own tags, as LMS does.
async fn replay(State(state): State<ReplayState>, Json(body): Json<Value>) -> Json<Value> {
    let cmd = body["params"][1].clone();
    if let Ok(mut log) = state.requests.lock() {
        log.push(cmd.clone());
    }

    let verb = cmd
        .get(0)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let sub = cmd.get(1).and_then(Value::as_str).unwrap_or_default();
    let tags = requested_tags(&cmd).unwrap_or_default();

    let result = match (verb.as_str(), sub) {
        // `l` is the album title. Without it a real server sends no `album`.
        ("albums", _) => {
            if tags.contains('l') {
                fixture_result("collections_albums_with_display_tags")
            } else {
                fixture_result("collections_albums_artwork_only")
            }
        }
        // `a` is the artist. Without it a real server sends no `artist`.
        ("titles", _) => {
            if tags.contains('a') {
                fixture_result("collections_titles_with_display_tags")
            } else {
                fixture_result("collections_titles_artwork_only")
            }
        }
        // `favorites items` sends `url` only when asked, and a favorite has no
        // other handle to play.
        ("favorites", "items") => {
            let asked_for_url = cmd
                .as_array()
                .is_some_and(|a| a.iter().any(|p| p.as_str() == Some("want_url:1")));
            if asked_for_url {
                fixture_result("collections_favorites_with_want_url")
            } else {
                fixture_result("collections_favorites_without_want_url")
            }
        }
        ("playlists", "tracks") => {
            if tags.contains('a') {
                fixture_result("collections_playlisttracks_with_display_tags")
            } else {
                fixture_result("collections_playlisttracks_artwork_only")
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

async fn browse(adapter: &LmsAdapter, path: &str) -> Value {
    match adapter
        .collections_content(
            "collections_browse",
            &json!({"path": path, "limit": 3, "offset": 0}),
        )
        .await
    {
        Ok(v) => v,
        Err(e) => panic!("collections_browse {path:?} failed: {e:#}"),
    }
}

fn rows(page: &Value) -> Vec<Value> {
    page.get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

// =============================================================================
// The Albums level, and every artist's album list
// =============================================================================

/// The load-bearing test: with artwork-only tags this page is empty, which is
/// what a real server produced for every album row.
#[tokio::test]
async fn browsing_albums_returns_titled_rows() {
    let server = TagReplayServer::start().await;
    let adapter = adapter_for(server.addr()).await;

    let page = browse(&adapter, "albums").await;
    let items = rows(&page);

    assert!(
        !items.is_empty(),
        "the Albums level came back empty; LMS sends no `album` key unless the \
         query asks for the album-title tag (see this file's module docs)"
    );
    for item in &items {
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(!title.is_empty(), "album row has no title: {item}");
        let path = item.get("path").and_then(Value::as_str).unwrap_or_default();
        assert!(
            path.starts_with("album:"),
            "album row is not navigable: {item}"
        );
    }
    server.stop();
}

/// One artist's albums take the same query, so the same tags have to reach it.
#[tokio::test]
async fn browsing_one_artists_albums_returns_titled_rows() {
    let server = TagReplayServer::start().await;
    let adapter = adapter_for(server.addr()).await;

    let page = browse(&adapter, "artist:2").await;

    assert!(
        !rows(&page).is_empty(),
        "an artist's album list came back empty"
    );
    let asked = server.commands().iter().any(|cmd| {
        cmd.as_array()
            .is_some_and(|a| a.iter().any(|p| p.as_str() == Some("artist_id:2")))
    });
    assert!(
        asked,
        "the artist filter never reached LMS: {:?}",
        server.commands()
    );
    server.stop();
}

/// The request itself is the defect, so assert on it directly: a future edit
/// that drops a display tag for artwork's sake fails here even if some mock
/// would still hand back full rows.
#[tokio::test]
async fn album_query_asks_for_title_artist_and_artwork_tags() {
    let server = TagReplayServer::start().await;
    let adapter = adapter_for(server.addr()).await;

    browse(&adapter, "albums").await;

    let tags = server
        .commands()
        .iter()
        .find_map(requested_tags)
        .unwrap_or_else(|| panic!("no tags: parameter was sent: {:?}", server.commands()));
    for (letter, field) in [
        ('l', "album title"),
        ('a', "artist"),
        ('c', "coverid"),
        ('J', "artwork_track_id"),
    ] {
        assert!(
            tags.contains(letter),
            "tags:{tags} omits `{letter}` ({field}); a tags: parameter replaces \
             the default set, so an omitted field is simply absent from the rows"
        );
    }
    server.stop();
}

// =============================================================================
// Tracks and playlist tracks keep their subtitles
// =============================================================================

#[tokio::test]
async fn album_tracks_keep_their_artist_subtitle() {
    let server = TagReplayServer::start().await;
    let adapter = adapter_for(server.addr()).await;

    let page = browse(&adapter, "album:2").await;
    let items = rows(&page);

    assert!(!items.is_empty(), "an album's track list came back empty");
    assert!(
        items
            .iter()
            .all(|i| i.get("subtitle").and_then(Value::as_str).is_some()),
        "track rows lost their artist subtitle: {items:?}"
    );
    server.stop();
}

#[tokio::test]
async fn playlist_tracks_keep_their_artist_subtitle() {
    let server = TagReplayServer::start().await;
    let adapter = adapter_for(server.addr()).await;

    let page = browse(&adapter, "playlist:482").await;
    let items = rows(&page);

    assert!(!items.is_empty(), "a playlist's track list came back empty");
    assert!(
        items
            .iter()
            .all(|i| i.get("subtitle").and_then(Value::as_str).is_some()),
        "playlist track rows lost their artist subtitle: {items:?}"
    );
    server.stop();
}

// =============================================================================
// Favorites (and the Radio tab, which is the same query)
// =============================================================================

async fn favorites(adapter: &LmsAdapter) -> Value {
    match adapter
        .collections_content("collections_favorites", &json!({"limit": 20, "offset": 0}))
        .await
    {
        Ok(v) => v,
        Err(e) => panic!("collections_favorites failed: {e:#}"),
    }
}

/// Favorites must arrive playable. A favorite carries no durable entity id, so
/// the url is its only handle -- and LMS withholds it unless asked.
#[tokio::test]
async fn favorites_arrive_with_a_playback_url() {
    let server = TagReplayServer::start().await;
    let adapter = adapter_for(server.addr()).await;

    let page = favorites(&adapter).await;
    let items = rows(&page);

    assert!(
        !items.is_empty(),
        "the Favorites level came back empty; LMS omits `url` unless the request \
         asks `want_url:1`, and `list_favorites` drops any row without one"
    );
    for item in &items {
        assert!(
            item.get("url")
                .and_then(Value::as_str)
                .is_some_and(|u| !u.is_empty()),
            "favorite row has no playback url, so nothing can play it: {item}"
        );
    }
    server.stop();
}

/// Assert on the request as well: the url is a *requested* field, and a future
/// edit that drops the flag would empty both tabs again.
#[tokio::test]
async fn favorites_query_asks_for_the_url() {
    let server = TagReplayServer::start().await;
    let adapter = adapter_for(server.addr()).await;

    favorites(&adapter).await;

    let asked = server.commands().iter().any(|cmd| {
        cmd.as_array()
            .is_some_and(|a| a.iter().any(|p| p.as_str() == Some("want_url:1")))
    });
    assert!(
        asked,
        "the favorites query never asked for `want_url:1`: {:?}",
        server.commands()
    );
    server.stop();
}

/// A folder-shaped favorite (`hasitems: 1`) has no url of its own. It is
/// deliberately not browsable in this slice, so it must be dropped rather than
/// listed as a dead end -- the recorded fixture contains one.
#[tokio::test]
async fn folder_shaped_favorites_are_not_listed_as_dead_ends() {
    let server = TagReplayServer::start().await;
    let adapter = adapter_for(server.addr()).await;

    let items = rows(&favorites(&adapter).await);

    assert!(
        !items.iter().any(|i| i
            .get("title")
            .and_then(Value::as_str)
            .is_some_and(|t| t == "Test Folder")),
        "a folder favorite was listed with no url and no way in: {items:?}"
    );
    server.stop();
}
