//! Regression tests for issue #407 — four verified LMS adapter defects.
//!
//! Every LMS response these tests feed the adapter is **replayed verbatim from a
//! recording of a live Lyrion Music Server 9.1.2** (`tests/fixtures/lms/`, see
//! that directory's `PROVENANCE.md`). Nothing here guesses at LMS's shape.
//!
//! That is the point. `search_library()` was written against a hand-written guess
//! that LMS emits `id` in its search loops; it emits `<type>_id`, so every branch
//! of that function failed its guard and it returned an empty `Vec` on every real
//! server for as long as it has existed. A mock that agreed with the code would
//! have kept agreeing with it forever.
//!
//! Defect coverage:
//!   1. `search_library()` field names   -> `search_library_*`
//!   2. dead JSON-RPC `error` check      -> `lms_failure_*`
//!   3. `is_muted` hard-coded `false`    -> `mute_*` / `players_query_*`
//!   4. wrong `tags:` legend in the docs -> `docs_tag_legend_*`

use axum::{extract::State, routing::post, Json, Router};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::broadcast;

use unified_hifi_control::adapters::lms::{LmsAdapter, LmsSearchResultType};
use unified_hifi_control::bus::{create_bus, BusEvent, SharedBus};

// =============================================================================
// Fixture loading
// =============================================================================

/// Load a recorded live LMS response and return its `result` member — exactly
/// what `LmsRpc::execute` hands back to its callers.
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

// =============================================================================
// Fixture-replay server
// =============================================================================

/// A JSON-RPC endpoint that replays recorded LMS responses and records every
/// request the adapter made, so tests can assert on the *request* too.
struct ReplayServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Value>>>,
    /// `players` fixture name; swappable so a test can simulate a mute change
    /// between two polls.
    players_fixture: Arc<Mutex<String>>,
    handle: tokio::task::JoinHandle<()>,
}

#[derive(Clone)]
struct ReplayState {
    requests: Arc<Mutex<Vec<Value>>>,
    players_fixture: Arc<Mutex<String>>,
    search_fixture: Option<String>,
    status_fixture: String,
}

impl ReplayServer {
    async fn start(
        players_fixture: &str,
        status_fixture: &str,
        search_fixture: Option<&str>,
    ) -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let players_name = Arc::new(Mutex::new(players_fixture.to_string()));

        let state = ReplayState {
            requests: requests.clone(),
            players_fixture: players_name.clone(),
            search_fixture: search_fixture.map(str::to_string),
            status_fixture: status_fixture.to_string(),
        };

        let app = Router::new()
            .route("/jsonrpc.js", post(replay))
            .with_state(state);

        let listener = match TcpListener::bind("127.0.0.1:0").await {
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
            players_fixture: players_name,
            handle,
        }
    }

    fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The command arrays the adapter sent, in order.
    fn commands(&self) -> Vec<Value> {
        match self.requests.lock() {
            Ok(g) => g.clone(),
            Err(e) => panic!("request log poisoned: {e}"),
        }
    }

    fn set_players_fixture(&self, name: &str) {
        match self.players_fixture.lock() {
            Ok(mut g) => *g = name.to_string(),
            Err(e) => panic!("fixture lock poisoned: {e}"),
        }
    }

    fn stop(self) {
        self.handle.abort();
    }
}

async fn replay(State(state): State<ReplayState>, Json(body): Json<Value>) -> Json<Value> {
    let cmd = body["params"][1].clone();
    if let Ok(mut log) = state.requests.lock() {
        log.push(cmd.clone());
    }

    let verb = cmd
        .get(0)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    let result = match verb.as_str() {
        "players" => {
            let name = match state.players_fixture.lock() {
                Ok(g) => g.clone(),
                Err(_) => String::new(),
            };
            let mut res = fixture_result(&name);
            // The recorded response carries whatever `playerprefs:` the recording
            // asked for. If the adapter under test does NOT ask for the mute pref,
            // a real LMS would not send it back (proved by
            // players_no_playerprefs.json), so strip it to model that faithfully.
            let asked_for_mute = cmd
                .as_array()
                .map(|a| {
                    a.iter().any(|v| {
                        v.as_str()
                            .map(|s| s.starts_with("playerprefs:") && s.contains("mute"))
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false);
            if !asked_for_mute {
                if let Some(players) = res.get_mut("players_loop").and_then(|v| v.as_array_mut()) {
                    for p in players.iter_mut() {
                        if let Some(obj) = p.as_object_mut() {
                            obj.remove("mute");
                        }
                    }
                }
            }
            res
        }
        "status" => fixture_result(&state.status_fixture),
        "search" => match state.search_fixture.as_deref() {
            Some(name) => fixture_result(name),
            None => json!({}),
        },
        _ => json!({}),
    };

    Json(json!({ "id": body["id"], "method": "slim.request", "result": result }))
}

// =============================================================================
// Raw TCP servers reproducing LMS's real failure modes
// =============================================================================

/// LMS's actual error signal: accept the request, then close the socket having
/// written **zero bytes**. No HTTP status line at all.
///
/// Recorded from live Lyrion 9.1.2 for unknown command (104), unknown player
/// (103), bad config (105) and Perl exceptions — see
/// `tests/fixtures/lms/PROVENANCE.md`. There is no response body to record as a
/// fixture, which is precisely why this has to be reproduced at the socket.
async fn start_closing_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    start_raw_server(None).await
}

/// A 200 with a zero-length body — the same information content, reachable
/// through a proxy that turns the closed socket into a valid empty response.
async fn start_empty_body_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    start_raw_server(Some(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n"
            .to_string(),
    ))
    .await
}

/// A 200 carrying JSON with **no `result` member**. LMS never does this; this is
/// the guard against something that is not LMS answering on port 9000.
async fn start_no_result_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let body = r#"{"id":217,"method":"slim.request"}"#;
    start_raw_server(Some(format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    )))
    .await
}

async fn start_raw_server(response: Option<String>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(e) => panic!("bind raw server: {e}"),
    };
    let addr = match listener.local_addr() {
        Ok(a) => a,
        Err(e) => panic!("local_addr: {e}"),
    };
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let response = response.clone();
            tokio::spawn(async move {
                // Read the request so the client sees it delivered, then answer
                // (or do not) exactly as configured.
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                if let Some(body) = response {
                    let _ = sock.write_all(body.as_bytes()).await;
                    let _ = sock.flush().await;
                }
                drop(sock);
            });
        }
    });
    (addr, handle)
}

// =============================================================================
// Harness helpers
// =============================================================================

fn test_bus() -> (SharedBus, broadcast::Receiver<BusEvent>) {
    let bus = create_bus();
    let rx = bus.subscribe();
    (bus, rx)
}

fn clear_lms_config() {
    use unified_hifi_control::config::get_config_file_path;
    let _ = std::fs::remove_file(get_config_file_path("lms-config.json"));
}

async fn configured_adapter(bus: SharedBus, addr: SocketAddr) -> LmsAdapter {
    let adapter = LmsAdapter::new(bus);
    adapter
        .configure(addr.ip().to_string(), Some(addr.port()), None, None)
        .await;
    adapter
}

fn drain_zones(rx: &mut broadcast::Receiver<BusEvent>) -> Vec<unified_hifi_control::bus::Zone> {
    let mut zones = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let BusEvent::ZoneDiscovered { zone } = event {
            zones.push(zone);
        }
    }
    zones
}

// =============================================================================
// Defect 1 — search_library() could never return a result
// =============================================================================

/// The load-bearing test. With no player connected, `search()` falls through to
/// `search_library()`, which issues `search <start> <n> term:` and parses the
/// reply. Fed the **recorded live 9.1.2 response**, current `v3` returns an empty
/// `Vec` because every branch reads `id` and LMS emits `<type>_id`.
#[tokio::test]
#[serial_test::serial(lms_config)]
async fn search_library_returns_results_from_recorded_live_response() {
    clear_lms_config();
    // `players_no_playerprefs` deliberately supplies players the adapter never
    // caches (search() reads its own state, not this response), so search()
    // takes the no-player path straight into search_library().
    let server = ReplayServer::start(
        "players_no_playerprefs",
        "status_tags_aAdltKc",
        Some("search_term_ember"),
    )
    .await;
    let (bus, _rx) = test_bus();
    let adapter = configured_adapter(bus, server.addr()).await;

    let results = match adapter.search("Ember", None, Some(10)).await {
        Ok(r) => r,
        Err(e) => panic!("search failed: {e}"),
    };

    assert!(
        !results.is_empty(),
        "search_library() returned nothing for a recorded live LMS response that \
         contains 3 albums, 1 contributor and 8 tracks. This is defect #407.1: the \
         parser reads `id`, LMS emits `album_id` / `contributor_id` / `track_id`."
    );

    let album = results
        .iter()
        .find(|r| r.result_type == LmsSearchResultType::Album)
        .cloned();
    let album = match album {
        Some(a) => a,
        None => panic!("no Album result parsed from albums_loop[].album_id"),
    };
    assert_eq!(album.title, "Ember Light");
    assert_eq!(album.id, 1, "album id must come from `album_id`, not `id`");

    let artist = results
        .iter()
        .find(|r| r.result_type == LmsSearchResultType::Artist)
        .cloned();
    let artist = match artist {
        Some(a) => a,
        None => panic!("no Artist result parsed from contributors_loop[].contributor_id"),
    };
    assert_eq!(artist.title, "Ember Valley Quartet");
    assert_eq!(
        artist.id, 3,
        "artist id must come from `contributor_id`, not `id`"
    );

    let track = results
        .iter()
        .find(|r| r.result_type == LmsSearchResultType::Track)
        .cloned();
    let track = match track {
        Some(t) => t,
        None => panic!("no Track result parsed from tracks_loop[].track_id"),
    };
    assert_eq!(track.id, 1, "track id must come from `track_id`, not `id`");
    assert_eq!(track.title, "Ember Rising");

    // Every result must carry a usable playback handle, or search_and_play cannot
    // act on it — an id of 0 is what the broken parser would have produced had its
    // guards not rejected the row outright.
    for r in &results {
        assert!(
            r.id > 0,
            "result {:?} has no usable entity id; playlistcontrol needs one",
            r.title
        );
    }

    server.stop();
}

/// A search term whose only match is a genre still yields the tracks LMS found.
/// Guards against a fix that special-cases one loop name.
#[tokio::test]
#[serial_test::serial(lms_config)]
async fn search_library_parses_recorded_response_with_absent_loops() {
    clear_lms_config();
    // search_term_jazz has genres_loop + tracks_loop and NO albums_loop or
    // contributors_loop at all — LMS omits empty loops rather than returning them
    // empty. The parser must tolerate that.
    let server = ReplayServer::start(
        "players_no_playerprefs",
        "status_tags_aAdltKc",
        Some("search_term_jazz"),
    )
    .await;
    let (bus, _rx) = test_bus();
    let adapter = configured_adapter(bus, server.addr()).await;

    let results = match adapter.search("Jazz", None, Some(10)).await {
        Ok(r) => r,
        Err(e) => panic!("search failed: {e}"),
    };

    assert!(
        !results.is_empty(),
        "recorded response has 5 tracks in tracks_loop"
    );
    assert!(
        results
            .iter()
            .all(|r| r.result_type == LmsSearchResultType::Track),
        "only tracks_loop is present in this recording, so only Track results are possible"
    );
    server.stop();
}

/// Zero matches must stay zero — `{"count": 0}` with no loops at all.
#[tokio::test]
#[serial_test::serial(lms_config)]
async fn search_library_returns_empty_for_recorded_no_match_response() {
    clear_lms_config();
    let server = ReplayServer::start(
        "players_no_playerprefs",
        "status_tags_aAdltKc",
        Some("search_no_results"),
    )
    .await;
    let (bus, _rx) = test_bus();
    let adapter = configured_adapter(bus, server.addr()).await;

    let results = match adapter.search("zzznotfound", None, Some(10)).await {
        Ok(r) => r,
        Err(e) => panic!("search failed: {e}"),
    };
    assert!(results.is_empty(), "no matches must produce no results");
    server.stop();
}

// =============================================================================
// Defect 2 — the dead JSON-RPC `error` check
// =============================================================================

/// LMS never returns a JSON-RPC `error` object. Its error signal is closing the
/// socket with zero bytes written, so the adapter must produce a diagnostic that
/// names that, not an opaque transport error.
#[tokio::test]
#[serial_test::serial(lms_config)]
async fn lms_failure_closed_socket_is_reported_as_lms_failure() {
    clear_lms_config();
    let (addr, handle) = start_closing_server().await;
    let (bus, _rx) = test_bus();
    let adapter = configured_adapter(bus, addr).await;

    let err = match adapter.get_players().await {
        Ok(_) => panic!("a server that writes nothing must not look like success"),
        Err(e) => format!("{e:#}"),
    };

    let lowered = err.to_lowercase();
    assert!(
        lowered.contains("closed") || lowered.contains("no response"),
        "error must name LMS's actual failure mode (socket closed with no \
         response), got: {err}"
    );
    assert!(
        lowered.contains("players"),
        "error must name the command that failed so a log is actionable, got: {err}"
    );
    handle.abort();
}

/// Same information reaching us as a valid empty 200 (e.g. via a proxy).
#[tokio::test]
#[serial_test::serial(lms_config)]
async fn lms_failure_empty_body_is_reported_as_lms_failure() {
    clear_lms_config();
    let (addr, handle) = start_empty_body_server().await;
    let (bus, _rx) = test_bus();
    let adapter = configured_adapter(bus, addr).await;

    let err = match adapter.get_players().await {
        Ok(_) => panic!("an empty body must not look like success"),
        Err(e) => format!("{e:#}"),
    };
    let lowered = err.to_lowercase();
    assert!(
        lowered.contains("empty") || lowered.contains("no response"),
        "error must name the empty body, got: {err}"
    );
    handle.abort();
}

/// A JSON reply with no `result` member. LMS always sends `result` on success;
/// this is the replacement for the dead `error` check — it fires for anything
/// that is not LMS, including a hypothetical responder that *did* send `error`.
#[tokio::test]
#[serial_test::serial(lms_config)]
async fn lms_failure_missing_result_member_is_reported() {
    clear_lms_config();
    let (addr, handle) = start_no_result_server().await;
    let (bus, _rx) = test_bus();
    let adapter = configured_adapter(bus, addr).await;

    match adapter.get_players().await {
        Ok(players) => panic!(
            "a reply with no `result` member must be an error, got {} players",
            players.len()
        ),
        Err(e) => {
            let msg = format!("{e:#}").to_lowercase();
            assert!(
                msg.contains("result"),
                "error must say the `result` member was missing, got: {e:#}"
            );
        }
    }
    handle.abort();
}

/// An unreachable server and a rejected request are different problems and must
/// not share a message: one is configuration, the other is LMS refusing the
/// command with no detail about why.
#[tokio::test]
#[serial_test::serial(lms_config)]
async fn unreachable_server_is_not_reported_as_a_rejected_request() {
    clear_lms_config();
    let (bus, _rx) = test_bus();
    // Port 1 on loopback: privileged and unused, so this is a connect refusal
    // rather than an accepted-then-closed socket.
    let adapter = LmsAdapter::new(bus);
    adapter
        .configure("127.0.0.1".to_string(), Some(1), None, None)
        .await;

    let err = match adapter.get_players().await {
        Ok(_) => panic!("an unreachable port must not look like success"),
        Err(e) => format!("{e:#}"),
    };
    let lowered = err.to_lowercase();
    assert!(
        lowered.contains("cannot reach"),
        "an unreachable server must be reported as unreachable, got: {err}"
    );
    assert!(
        !lowered.contains("closed the connection"),
        "an unreachable server must not be described as LMS closing the \
         connection - that is a different diagnosis: {err}"
    );
}

/// The dead check being replaced must not be replaced by *another* dead check:
/// a well-formed LMS reply still has to succeed.
#[tokio::test]
#[serial_test::serial(lms_config)]
async fn lms_success_path_still_works() {
    clear_lms_config();
    let server = ReplayServer::start("players_mute_mixed", "status_tags_aAdltKc", None).await;
    let (bus, _rx) = test_bus();
    let adapter = configured_adapter(bus, server.addr()).await;

    let players = match adapter.get_players().await {
        Ok(p) => p,
        Err(e) => panic!("valid recorded response must parse: {e:#}"),
    };
    assert_eq!(players.len(), 3, "recording has three players");
    server.stop();
}

// =============================================================================
// Defect 3 — is_muted hard-coded false
// =============================================================================

/// The batched-cost claim, as a test: the adapter must ask for the mute pref on
/// the `players` call it already makes, and must not add a per-player
/// `mixer muting ?` round-trip.
#[tokio::test]
#[serial_test::serial(lms_config)]
async fn players_query_requests_mute_pref_without_extra_round_trips() {
    clear_lms_config();
    let server = ReplayServer::start("players_mute_mixed", "status_tags_aAdltKc", None).await;
    let (bus, _rx) = test_bus();
    let adapter = configured_adapter(bus, server.addr()).await;

    if let Err(e) = adapter.update_players().await {
        panic!("update_players failed: {e:#}");
    }

    let commands = server.commands();

    let players_calls: Vec<&Value> = commands
        .iter()
        .filter(|c| c.get(0).and_then(|v| v.as_str()) == Some("players"))
        .collect();
    assert_eq!(
        players_calls.len(),
        1,
        "one poll cycle must issue exactly one `players` call"
    );
    let asks_for_mute = players_calls[0]
        .as_array()
        .map(|a| a.iter().any(|v| v.as_str() == Some("playerprefs:mute")))
        .unwrap_or(false);
    assert!(
        asks_for_mute,
        "`players` must carry `playerprefs:mute` so mute costs zero extra \
         round-trips; got {:?}",
        players_calls[0]
    );

    let muting_calls = commands
        .iter()
        .filter(|c| {
            c.get(0).and_then(|v| v.as_str()) == Some("mixer")
                && c.get(1).and_then(|v| v.as_str()) == Some("muting")
        })
        .count();
    assert_eq!(
        muting_calls, 0,
        "mute must not cost a `mixer muting ?` round-trip per player per poll"
    );

    server.stop();
}

/// Zones must report the mute state the recorded response actually carries — all
/// three shapes LMS uses, in one response: string "0", key absent, string "1".
#[tokio::test]
#[serial_test::serial(lms_config)]
async fn mute_state_from_recorded_players_response_reaches_zones() {
    clear_lms_config();
    let server = ReplayServer::start("players_mute_mixed", "status_tags_aAdltKc", None).await;
    let (bus, mut rx) = test_bus();
    let adapter = configured_adapter(bus, server.addr()).await;

    if let Err(e) = adapter.update_players().await {
        panic!("update_players failed: {e:#}");
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let zones = drain_zones(&mut rx);
    assert_eq!(zones.len(), 3, "three players in the recording");

    let muted_of = |zone_id: &str| -> bool {
        let zone = zones.iter().find(|z| z.zone_id == zone_id);
        let zone = match zone {
            Some(z) => z,
            None => panic!(
                "zone {zone_id} not discovered; got {:?}",
                zones.iter().map(|z| &z.zone_id).collect::<Vec<_>>()
            ),
        };
        match zone.volume_control.as_ref() {
            Some(vc) => vc.is_muted,
            None => panic!("zone {zone_id} has no volume_control"),
        }
    };

    // Recording: Kitchen has `"mute": "0"` — a *string*. `.as_i64()` alone yields
    // None on that, which is the same failure class as defect 1.
    assert!(
        !muted_of("lms:02:00:00:00:00:01"),
        r#"`"mute": "0"` (string) means not muted"#
    );
    // Recording: this player has no `mute` key at all — the pref was never
    // written. LMS omits undefined prefs.
    assert!(
        !muted_of("lms:02:00:00:00:00:02"),
        "an absent `mute` key means the pref was never set, i.e. not muted"
    );
    // Recording: Study has `"mute": "1"`. This is the assertion that fails
    // against current v3, where is_muted is hard-coded false.
    assert!(
        muted_of("lms:02:00:00:00:00:11"),
        r#"`"mute": "1"` means muted — defect #407.3 hard-codes is_muted false"#
    );

    server.stop();
}

/// A mute change with no volume change must reach clients. Zones are served from
/// the aggregator's event-fed cache, so without this the fixed snapshot would
/// only ever be right at discovery time.
#[tokio::test]
#[serial_test::serial(lms_config)]
async fn mute_change_alone_publishes_volume_changed() {
    clear_lms_config();
    let server = ReplayServer::start("players_mute_mixed", "status_tags_aAdltKc", None).await;
    let (bus, mut rx) = test_bus();
    let adapter = configured_adapter(bus, server.addr()).await;

    // Baseline poll: Study muted.
    if let Err(e) = adapter.update_players().await {
        panic!("baseline update failed: {e:#}");
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    while rx.try_recv().is_ok() {}

    // Second poll: everything unmuted, volumes unchanged (both recordings come
    // from the same players with the same volumes).
    server.set_players_fixture("players_mute_all_unmuted");
    if let Err(e) = adapter.update_players().await {
        panic!("second update failed: {e:#}");
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut saw_unmute = false;
    while let Ok(event) = rx.try_recv() {
        if let BusEvent::VolumeChanged {
            output_id,
            is_muted,
            ..
        } = event
        {
            if output_id == "lms:02:00:00:00:00:11" && !is_muted {
                saw_unmute = true;
            }
        }
    }
    assert!(
        saw_unmute,
        "a mute change with no volume change must publish VolumeChanged, or the \
         aggregator's zone cache never learns about it"
    );

    server.stop();
}

/// LMS **negates** `mixer volume` once its mute fade completes: the recorded
/// muted `model: squeezelite` player reports `"mixer volume": -42`. The adapter
/// hands `mixer volume` to a `VolumeControl` declared `min: 0.0`, so a negative
/// must never reach a client. (`tests/volume_safety.rs` exists because a volume
/// range bug once risked equipment damage.)
#[tokio::test]
#[serial_test::serial(lms_config)]
async fn muted_player_volume_is_not_negated() {
    clear_lms_config();
    let server = ReplayServer::start("players_mute_mixed", "status_squeezelite_muted", None).await;
    let (bus, mut rx) = test_bus();
    let adapter = configured_adapter(bus, server.addr()).await;

    if let Err(e) = adapter.update_players().await {
        panic!("update_players failed: {e:#}");
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    for zone in drain_zones(&mut rx) {
        if let Some(vc) = zone.volume_control {
            assert!(
                vc.value >= 0.0,
                "zone {} reported volume {} — LMS never negates `mixer volume` on \
                 mute, so a negative here is a parse bug",
                zone.zone_id,
                vc.value
            );
        }
    }
    server.stop();
}

/// The second mute signal, which needs no tagged parameter at all: a negative
/// `mixer volume` is a sufficient condition for muted. It lags a mute by ~0.8 s
/// (the fade), so it can only ever produce a false *negative*, never a false
/// positive — which is what makes it safe to OR with the pref, and what keeps
/// mute working if `playerprefs:` is ever unavailable.
#[tokio::test]
#[serial_test::serial(lms_config)]
async fn negative_volume_alone_reports_muted() {
    clear_lms_config();
    // players_no_playerprefs was recorded *without* the tag, so it carries no
    // `mute` key for any player — the pref signal is entirely absent here.
    let server =
        ReplayServer::start("players_no_playerprefs", "status_squeezelite_muted", None).await;
    let (bus, mut rx) = test_bus();
    let adapter = configured_adapter(bus, server.addr()).await;

    if let Err(e) = adapter.update_players().await {
        panic!("update_players failed: {e:#}");
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    let zones = drain_zones(&mut rx);
    assert!(!zones.is_empty(), "expected zones from the recording");
    for zone in zones {
        match zone.volume_control {
            Some(vc) => assert!(
                vc.is_muted,
                "zone {} saw a recorded `mixer volume: -42` and no mute pref; the \
                 negative sign alone must report muted",
                zone.zone_id
            ),
            None => panic!("zone {} has no volume_control", zone.zone_id),
        }
    }
    server.stop();
}

// =============================================================================
// Defect 4 — the tags legend in docs/lyrion.md
// =============================================================================

/// The doc claimed `l=album_id, A=album`. LMS's `%tagMap` says `l` is the album
/// *title*, `e` is `album_id`, and `A` expands to per-role contributor names.
/// Pinned against the recording so the doc cannot drift back.
#[test]
fn docs_tag_legend_matches_recorded_response() {
    let status = fixture_result("status_tags_aAdltKc");
    let track = status["playlist_loop"][0].clone();

    // `l` yields an album *title*.
    assert_eq!(
        track["album"].as_str(),
        Some("Ember Light"),
        "tag `l` yields the album title in the `album` key"
    );
    // `e` (album_id) was NOT requested, so no album id is present. This is the
    // direct refutation of the old legend's `l=album_id`.
    assert!(
        track.get("album_id").is_none(),
        "tags:aAdltKc does not request `e`, so no album_id is returned"
    );
    // `A` yields per-role contributor names, not an album.
    assert!(
        track.get("albumartist").is_some() || track.get("trackartist").is_some(),
        "tag `A` yields per-role contributor keys such as albumartist/trackartist"
    );
    // `J` (artwork_track_id) is not requested either — which is why the
    // artwork_track_id fallback in get_player_status is dead. Documented, not
    // silently changed: see the PR body.
    assert!(
        track.get("artwork_track_id").is_none(),
        "tags:aAdltKc does not request `J`, so artwork_track_id is never present"
    );

    let doc =
        match std::fs::read_to_string(format!("{}/docs/lyrion.md", env!("CARGO_MANIFEST_DIR"))) {
            Ok(d) => d,
            Err(e) => panic!("docs/lyrion.md unreadable: {e}"),
        };
    assert!(
        !doc.contains("l=album_id"),
        "docs/lyrion.md still claims `l=album_id`; `l` is the album title and `e` \
         is album_id"
    );
    assert!(
        !doc.contains("A=album\n") && !doc.contains("A=album,"),
        "docs/lyrion.md still claims `A=album`; `A` expands to per-role \
         contributor names"
    );
    assert!(
        doc.contains("e=album_id") || doc.contains("`e`"),
        "docs/lyrion.md must document `e` as album_id"
    );
}

/// Guard against fixtures drifting into hand-written mocks — the thing that let
/// defect 1 survive.
#[test]
fn fixtures_are_recorded_not_authored() {
    let dir = format!("{}/tests/fixtures/lms", env!("CARGO_MANIFEST_DIR"));
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => panic!("fixture dir {dir} unreadable: {e}"),
    };
    let counted = AtomicUsize::new(0);
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => panic!("{path:?}: {e}"),
        };
        let parsed: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => panic!("{path:?} is not valid JSON: {e}"),
        };
        // A recording carries LMS's own echo of the request that produced it and
        // this repo's request id. An authored mock generally will not.
        assert_eq!(
            parsed["method"].as_str(),
            Some("slim.request"),
            "{path:?} is missing LMS's `method` echo — is it a recording?"
        );
        assert!(
            parsed.get("params").is_some(),
            "{path:?} is missing LMS's `params` echo, which is its provenance"
        );
        assert_eq!(
            parsed["id"].as_i64(),
            Some(217),
            "{path:?} was not recorded through this repo's request id (217)"
        );
        counted.fetch_add(1, Ordering::Relaxed);
    }
    assert!(
        counted.load(Ordering::Relaxed) >= 8,
        "expected the recorded LMS fixture set to be present"
    );
}
