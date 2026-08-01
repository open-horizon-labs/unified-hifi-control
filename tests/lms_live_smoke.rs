//! Opt-in smoke tests that drive a **real** Lyrion Music Server (#407).
//!
//! Every test here is `#[ignore]`d, so `cargo test` never runs them and CI is
//! unaffected. They exist because #407's defects all survived unit tests that
//! agreed with the code, and the only thing that caught them was a live server.
//!
//! ```sh
//! LMS_LIVE_HOST=127.0.0.1 LMS_LIVE_PORT=9000 \
//!   cargo test --test lms_live_smoke -- --ignored --nocapture
//! ```
//!
//! `LMS_LIVE_HOST` defaults to `127.0.0.1` and `LMS_LIVE_PORT` to `9000`.
//!
//! `live_mute_round_trip` **mutes and unmutes a real player**. It restores the
//! previous state, and skips itself unless `LMS_LIVE_PLAYER` names the player to
//! use, so it can never surprise a listener on someone's actual system.
//!
//! ## If you stand up a throwaway LMS for this, do not publish port 3483
//!
//! Publishing 3483 lets every SlimProto player on the LAN auto-discover the test
//! server and attach to it. That happened once during the survey behind #402/#403
//! and pulled about ten of the operator's real players onto a throwaway
//! container. Bind 9000 to `127.0.0.1`.

use std::time::Duration;
use unified_hifi_control::adapters::lms::{LmsAdapter, LmsSearchResultType};
use unified_hifi_control::bus::{create_bus, BusEvent};

fn live_host() -> String {
    std::env::var("LMS_LIVE_HOST").unwrap_or_else(|_| "127.0.0.1".to_string())
}

fn live_port() -> u16 {
    std::env::var("LMS_LIVE_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(9000)
}

async fn live_adapter() -> LmsAdapter {
    let bus = create_bus();
    let adapter = LmsAdapter::new(bus);
    adapter
        .configure(live_host(), Some(live_port()), None, None)
        .await;
    adapter
}

/// Defect 1, end to end against a real server: the library fallback must return
/// results with usable entity ids.
///
/// Pass `LMS_LIVE_TERM` to match your own library; the default suits the
/// generated library described in `tests/fixtures/lms/PROVENANCE.md`.
#[tokio::test]
#[ignore = "requires a live LMS; see module docs"]
async fn live_search_library_returns_addressable_results() {
    let adapter = live_adapter().await;
    let term = std::env::var("LMS_LIVE_TERM").unwrap_or_else(|_| "Ember".to_string());

    // player_id None with no cached players forces the search_library() path,
    // which is the one that never worked.
    let results = match adapter.search(&term, None, Some(10)).await {
        Ok(r) => r,
        Err(e) => panic!("live search failed: {e:#}"),
    };

    println!(
        "live search for {term:?} returned {} results:",
        results.len()
    );
    for r in &results {
        println!("  {:?} id={} {:?}", r.result_type, r.id, r.title);
    }

    assert!(
        !results.is_empty(),
        "no results for {term:?}. Set LMS_LIVE_TERM to something in your library."
    );
    for r in &results {
        assert!(
            r.id > 0 || r.item_id.is_some() || r.url.is_some(),
            "result {:?} carries no playback handle, so nothing can act on it",
            r.title
        );
    }
    assert!(
        results.iter().any(|r| matches!(
            r.result_type,
            LmsSearchResultType::Album | LmsSearchResultType::Artist | LmsSearchResultType::Track
        )),
        "results must be typed"
    );
}

/// Defect 2, end to end: a request LMS rejects must surface as a diagnostic that
/// names LMS's real behaviour, not an opaque transport error. Uses `get_players`
/// against a port with nothing listening, which is the closest reachable analogue
/// of the closed socket without sending LMS a bogus command.
#[tokio::test]
#[ignore = "requires a live LMS; see module docs"]
async fn live_transport_failure_is_diagnosable() {
    let bus = create_bus();
    let adapter = LmsAdapter::new(bus);
    // A port nothing is listening on: connection refused rather than an LMS
    // close, but it must still name the command.
    adapter.configure(live_host(), Some(1), None, None).await;

    match adapter.get_players().await {
        Ok(_) => panic!("a dead port must not look like success"),
        Err(e) => {
            let msg = format!("{e:#}");
            println!("dead-port error: {msg}");
            assert!(
                msg.contains("players"),
                "error must name the command: {msg}"
            );
        }
    }
}

/// Defects 3 and the negative-volume finding, end to end on a real player.
///
/// This is the check the #407 gate reports asked the operator to run on real
/// hardware: mute a player, confirm UHC reports it muted and does **not** report
/// a negative volume, then restore.
///
/// Set `LMS_LIVE_PLAYER` to the bare player id (no `lms:` prefix). Without it the
/// test skips, so it cannot mute a stranger's speakers.
#[tokio::test]
#[ignore = "requires a live LMS AND an explicitly named player; see module docs"]
async fn live_mute_round_trip() {
    let Ok(player) = std::env::var("LMS_LIVE_PLAYER") else {
        println!("skipping: set LMS_LIVE_PLAYER to a bare player id to run this");
        return;
    };

    let bus = create_bus();
    let mut rx = bus.subscribe();
    let adapter = LmsAdapter::new(bus);
    adapter
        .configure(live_host(), Some(live_port()), None, None)
        .await;

    let raw = match adapter.get_players().await {
        Ok(p) => p,
        Err(e) => panic!("get_players failed: {e:#}"),
    };
    let Some(was_muted) = raw.iter().find(|p| p.playerid == player).map(|p| p.muted) else {
        panic!("player {player} not found on this server");
    };
    println!("before: muted={was_muted} (from playerprefs:mute)");

    // First poll establishes the adapter's baseline for this player and emits
    // ZoneDiscovered - the zone snapshot clients read on connect.
    if let Err(e) = adapter.update_players().await {
        panic!("baseline update_players failed: {e:#}");
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut baseline_muted = None;
    while let Ok(event) = rx.try_recv() {
        if let BusEvent::ZoneDiscovered { zone } = event {
            if zone.zone_id.ends_with(&player) {
                if let Some(vc) = zone.volume_control {
                    println!(
                        "ZoneDiscovered: value={} is_muted={}",
                        vc.value, vc.is_muted
                    );
                    assert!(vc.value >= 0.0, "zone volume must not be negative");
                    baseline_muted = Some(vc.is_muted);
                }
            }
        }
    }
    assert_eq!(
        baseline_muted,
        Some(was_muted),
        "the zone snapshot must agree with the server's mute pref"
    );

    // Mute via LMS directly. Mute *control* through the adapter is #403's work,
    // not #407's, so this drives the server rather than widening the adapter's
    // surface just to test it.
    set_mute_on_server(&player, !was_muted).await;
    // Let the fade finish so the negated volume has landed. That is the window an
    // earlier reading of this landed inside, wrongly concluding LMS keeps the
    // volume positive while muted.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // Second poll: mute changed, volume did not. This must still reach the bus.
    if let Err(e) = adapter.update_players().await {
        panic!("second update_players failed: {e:#}");
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut observed = None;
    while let Ok(event) = rx.try_recv() {
        if let BusEvent::VolumeChanged {
            output_id,
            value,
            is_muted,
        } = event
        {
            if output_id.ends_with(&player) {
                println!("VolumeChanged: value={value} is_muted={is_muted}");
                assert!(
                    value >= 0.0,
                    "published volume must never be negative; LMS negates the \
                     volume pref while muted and VolumeControl declares min: 0.0"
                );
                observed = Some(is_muted);
            }
        }
    }
    assert_eq!(
        observed,
        Some(!was_muted),
        "a mute change with no volume change must reach the bus as VolumeChanged"
    );

    // Restore whatever the player had before this test touched it.
    set_mute_on_server(&player, was_muted).await;
}

/// Set mute on the live server over the same transport the adapter uses.
async fn set_mute_on_server(player: &str, muted: bool) {
    let url = format!("http://{}:{}/jsonrpc.js", live_host(), live_port());
    let body = serde_json::json!({
        "id": 217,
        "method": "slim.request",
        "params": [player, ["mixer", "muting", if muted { 1 } else { 0 }]]
    });
    let response = reqwest::Client::new().post(&url).json(&body).send().await;
    match response {
        Ok(r) if r.status().is_success() => {}
        Ok(r) => panic!("mixer muting returned HTTP {}", r.status()),
        Err(e) => panic!("mixer muting failed: {e}"),
    }
}
