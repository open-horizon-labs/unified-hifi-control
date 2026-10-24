//! Synthetic protocol cases derived from LMS Slim/Control/Queries.pm.
//! These are not recordings and do not establish physical-player behavior.
use axum::{extract::State, routing::post, Json, Router};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use unified_hifi_control::{adapters::lms::LmsAdapter, bus::create_bus};

#[derive(Clone)]
struct ReplayState {
    log: Arc<Mutex<Vec<Value>>>,
    index: Value,
}
async fn replay(State(state): State<ReplayState>, Json(body): Json<Value>) -> Json<Value> {
    let cmd = body["params"][1].clone();
    state.log.lock().unwrap().push(cmd.clone());
    let result = match cmd[0].as_str().unwrap() {
        "serverstatus" => json!({}),
        "playlists" => json!({"count":1,"playlists_loop":[{"id":42,"playlist":"Evening"}]}),
        "favorites" => {
            json!({"count":1,"loop_loop":[{"name":"Station","url":"https://example.test/radio","hasitems":0}]})
        }
        "artists" | "albums" if cmd.as_array().unwrap().iter().any(|v| v == "tags:ZZ") => {
            json!({"indexList":state.index})
        }
        "albums" => {
            let start = cmd[1].as_u64().unwrap();
            let size = cmd[2].as_u64().unwrap();
            json!({"count":1016,"albums_loop":(start..start+size).map(|i|json!({"id":i+1,"album":format!("Album {i}")})).collect::<Vec<_>>()})
        }
        "artists" => {
            let start = cmd[1].as_u64().unwrap();
            let size = cmd[2].as_u64().unwrap();
            json!({"count":1016,"artists_loop":(start..start+size).map(|i|json!({"id":i+1,"artist":format!("Artist {i}")})).collect::<Vec<_>>()})
        }
        _ => json!({}),
    };
    Json(json!({"result":result}))
}
async fn setup_index(
    index: Value,
) -> (
    LmsAdapter,
    Arc<Mutex<Vec<Value>>>,
    tokio::task::JoinHandle<()>,
) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/jsonrpc.js", post(replay))
        .with_state(ReplayState {
            log: log.clone(),
            index,
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let adapter = LmsAdapter::new(create_bus());
    adapter
        .configure(addr.ip().to_string(), Some(addr.port()), None, None)
        .await;
    (adapter, log, task)
}
async fn setup() -> (
    LmsAdapter,
    Arc<Mutex<Vec<Value>>>,
    tokio::task::JoinHandle<()>,
) {
    setup_index(json!([["A", 1000], ["B", 14], ["Å", 2]])).await
}
async fn browse(a: &LmsAdapter, path: &str, offset: usize) -> Value {
    a.collections_content(
        "collections_browse",
        &json!({"path":path,"offset":offset,"limit":12}),
    )
    .await
    .unwrap()
}
#[tokio::test]
async fn root_exposes_favorites_and_letter_menus() {
    let (a, _, task) = setup().await;
    let root = browse(&a, "root", 0).await;
    for path in ["favorites", "artists-index", "albums-index"] {
        assert!(
            root["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["path"] == path),
            "missing {path}"
        );
    }
    task.abort();
}
#[tokio::test]
async fn playlists_are_playable_without_losing_browsing() {
    let (a, _, task) = setup().await;
    let page = browse(&a, "playlists", 0).await;
    assert_eq!(page["items"][0]["path"], "playlist:42");
    assert_eq!(page["items"][0]["kind"], "playlist");
    assert_eq!(page["items"][0]["id"], 42);
    task.abort();
}
#[tokio::test]
async fn letters_use_native_counts_and_stop_before_next_letter() {
    let (a, log, task) = setup().await;
    let index = browse(&a, "artists-index", 0).await;
    assert_eq!(index["items"][2]["title"], "Å");
    let page = browse(&a, "artists-letter:B", 0).await;
    assert_eq!(page["items"][0]["title"], "Artist 1000");
    assert_eq!(page["next_offset"], 12);
    let tail = browse(&a, "artists-letter:B", 12).await;
    assert_eq!(tail["items"].as_array().unwrap().len(), 2);
    assert!(tail["next_offset"].is_null());
    assert!(log
        .lock()
        .unwrap()
        .iter()
        .any(|c| c[0] == "artists" && c[1] == 1012 && c[2] == 2));
    task.abort();
}

#[tokio::test]
async fn favorites_browse_returns_playable_urls() {
    let (a, log, task) = setup().await;
    let page = browse(&a, "favorites", 0).await;
    assert_eq!(page["items"][0]["title"], "Station");
    assert_eq!(page["items"][0]["url"], "https://example.test/radio");
    assert!(log.lock().unwrap().iter().any(|cmd| cmd
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "want_url:1")));
    task.abort();
}

#[tokio::test]
async fn malformed_indexes_do_not_issue_a_library_page_query() {
    for index in [
        json!([["A", -1]]),
        json!([["A", "100"]]),
        json!([["A", 1], ["A", 2]]),
        json!([["A", u64::MAX], ["B", 1]]),
        json!(null),
    ] {
        let (a, log, task) = setup_index(index).await;
        assert!(a
            .collections_content(
                "collections_browse",
                &json!({"path":"artists-letter:A","limit":12})
            )
            .await
            .is_err());
        assert_eq!(log.lock().unwrap().len(), 1);
        task.abort();
    }
}
#[tokio::test]
async fn missing_letter_and_exhausted_letter_are_empty_without_library_query() {
    let (a, log, task) = setup().await;
    for (path, offset) in [("artists-letter:Z", 0), ("artists-letter:B", 14)] {
        let page = browse(&a, path, offset).await;
        assert_eq!(page["items"], json!([]));
        assert!(page["next_offset"].is_null());
    }
    assert_eq!(log.lock().unwrap().len(), 2);
    task.abort();
}

#[tokio::test]
async fn album_letter_uses_same_order_and_bounds_as_artist_letter() {
    let (a, log, task) = setup().await;
    let index = browse(&a, "albums-index", 0).await;
    assert_eq!(index["items"][2]["path"], "albums-letter:Å");
    let page = browse(&a, "albums-letter:Å", 0).await;
    assert_eq!(page["items"][0]["title"], "Album 1014");
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    assert!(page["next_offset"].is_null());
    assert!(log
        .lock()
        .unwrap()
        .iter()
        .any(|c| c[0] == "albums" && c[1] == 1014 && c[2] == 2));
    task.abort();
}
