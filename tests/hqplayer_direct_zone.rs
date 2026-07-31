//! Direct HQPlayer zone conformance (issue #328).
//!
//! **These tests are written from the client's expectation, not from the implementation.** The
//! clients are the ESP32 knob firmware (`roon-knob`, which posts `{zone_id, action, value}` to
//! `POST /control` and renders `GET /knob/now_playing`) and the iOS/watch app (same two shapes).
//! Each test names the expectation in its doc comment; none of them was written by reading the
//! projection and asserting what it already does.
//!
//! No live HQPlayer is contacted. The daemon is the stateful wire/model fake from
//! `tests/mock_servers/hqplayer`, which is the same fake the native-protocol conformance suite
//! (#322) and the lifecycle suite (#162) drive.
//!
//! Two acceptance criteria of #328 were **already** satisfied before this issue and are deliberately
//! not re-tested here: continuous republication of external changes, and the stopped/fixed-volume
//! clear, both pinned by
//! `hqplayer_lifecycle::external_changes_converge_into_the_aggregator_without_a_surface_read`. What
//! this file adds is everything that observation is *projected into*: capability truthfulness,
//! source-vs-output metadata domains, explicit routing, and volume safety.

#[allow(dead_code, unused_imports, unused_variables)]
mod mock_servers;

use std::net::SocketAddr;
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use mock_servers::hqplayer::corpus::VERIFIED_PROFILE;
use mock_servers::hqplayer::model::{DaemonModel, Metadata};
use mock_servers::hqplayer::wire::{WirePolicy, WireServer};

use unified_hifi_control::adapters::hqplayer::{
    HqpAdapter, HqpInstanceManager, HqpRecoveryConfig, HqpStatus, HqpTimeouts, HqpZoneLinkService,
    VolumeRange,
};
use unified_hifi_control::adapters::lms::LmsAdapter;
use unified_hifi_control::adapters::openhome::OpenHomeAdapter;
use unified_hifi_control::adapters::roon::RoonAdapter;
use unified_hifi_control::adapters::upnp::UPnPAdapter;
use unified_hifi_control::adapters::Startable;
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::api::AppState;
use unified_hifi_control::bus::{create_bus, BusEvent, PlaybackState, SharedBus, Zone};
use unified_hifi_control::coordinator::AdapterCoordinator;
use unified_hifi_control::knobs::{self, KnobStore};

// =============================================================================
// Harness
// =============================================================================

/// Every test in this file writes zone links and instance config through the same process-wide
/// config path. Isolating it keeps the suite from reading the developer's real configuration and
/// keeps concurrent tests from linking each other's zones.
fn isolate_config_dir() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("uhc-hqp-direct-{}", std::process::id()));
        let subdir = dir.join("unified-hifi");
        std::fs::create_dir_all(&subdir).expect("create isolated direct-zone config dir");

        // The HQPlayer adapter defaults to **off**, and a zone whose adapter is disabled is a 404 by
        // design. Every test here is about a user who has turned it on, so the setting is written
        // rather than assumed — and writing it is also what makes the adapter-filter test meaningful:
        // with the toggle off, a filter bug and a correct filter are indistinguishable.
        std::fs::write(
            subdir.join("app-settings.json"),
            r#"{"adapters":{"roon":true,"upnp":false,"openhome":false,"lms":false,"hqplayer":true}}"#,
        )
        .expect("write isolated app settings");

        std::env::set_var("UHC_CONFIG_DIR", dir);
    });
}

fn fast_timeouts() -> HqpTimeouts {
    HqpTimeouts {
        connect: Duration::from_millis(100),
        response: Duration::from_millis(500),
        reconnect_delay: Duration::from_millis(10),
        max_attempts: 1,
    }
}

fn fast_recovery() -> HqpRecoveryConfig {
    HqpRecoveryConfig {
        poll_interval: Duration::from_millis(10),
        short_retry_delay: Duration::from_millis(10),
        restart_window: Duration::from_millis(40),
        backoff_initial: Duration::from_millis(20),
        backoff_cap: Duration::from_millis(40),
        stable_threshold: Duration::from_millis(20),
    }
}

/// The two client-facing routes under test, wired exactly as `main.rs` wires them.
///
/// `/control` and `/knob/now_playing` are the knob's whole world; `/zones` is what both clients read
/// to populate their picker. Nothing else is mounted, so a test cannot accidentally pass through a
/// route the client does not use.
fn client_router(state: AppState) -> Router {
    Router::new()
        .route("/control", post(knobs::knob_control_handler))
        .route("/knob/control", post(knobs::knob_control_handler))
        .route("/knob/now_playing", get(knobs::knob_now_playing_handler))
        .route("/zones", get(knobs::knob_zones_handler))
        .with_state(state)
}

struct Rig {
    app: Router,
    state: AppState,
    manager: Arc<HqpInstanceManager>,
    aggregator: Arc<ZoneAggregator>,
    bus: SharedBus,
    aggregator_task: tokio::task::JoinHandle<()>,
}

impl Rig {
    /// Build the surface stack over a disconnected Roon adapter.
    ///
    /// Roon being *disconnected* is load-bearing for the routing tests: a command that reaches Roon
    /// cannot silently succeed, so "did not fall through to Roon" is observable from the response
    /// as well as from the daemon's request log.
    async fn new() -> Self {
        isolate_config_dir();
        let bus = create_bus();
        let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
        let aggregator_task = {
            let aggregator = aggregator.clone();
            tokio::spawn(async move { aggregator.run().await })
        };
        tokio::task::yield_now().await;

        let coordinator = Arc::new(AdapterCoordinator::new(bus.clone()));
        let roon = Arc::new(RoonAdapter::new_disconnected(bus.clone()));
        let manager = Arc::new(HqpInstanceManager::new(bus.clone()));
        let hqplayer = manager.get_default().await;
        let hqp_zone_links = Arc::new(HqpZoneLinkService::new(manager.clone()));
        let lms = Arc::new(LmsAdapter::new(bus.clone()));
        let openhome = Arc::new(OpenHomeAdapter::new(bus.clone()));
        let upnp = Arc::new(UPnPAdapter::new(bus.clone()));

        let startable: Vec<Arc<dyn Startable>> = vec![];
        let state = AppState::new(
            roon,
            hqplayer,
            manager.clone(),
            hqp_zone_links,
            lms,
            openhome,
            upnp,
            KnobStore::new(),
            bus.clone(),
            aggregator.clone(),
            coordinator,
            startable,
            Instant::now(),
            CancellationToken::new(),
        );

        Self {
            app: client_router(state.clone()),
            state,
            manager,
            aggregator,
            bus,
            aggregator_task,
        }
    }

    /// Attach a managed instance named `rig` to a wire daemon and run it to a published zone.
    async fn attach(&self, server: &WireServer) -> Arc<HqpAdapter> {
        let adapter = self
            .manager
            .add_instance(
                "rig".to_string(),
                "127.0.0.1".to_string(),
                Some(server.port()),
                None,
                None,
                None,
            )
            .await;
        adapter.set_timeouts(fast_timeouts()).await;
        adapter.set_recovery_config(fast_recovery()).await;
        self.manager.start().await.expect("start managed lifecycle");
        adapter
    }

    async fn zone_when(&self, mut condition: impl FnMut(&Zone) -> bool) -> Zone {
        for _ in 0..200 {
            if let Some(zone) = self.aggregator.get_zone("hqplayer:rig").await {
                if condition(&zone) {
                    return zone;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!(
            "zone never satisfied the condition inside the bounded budget; zones={:?}",
            self.aggregator.get_zones().await
        );
    }

    async fn post_control(&self, body: Value) -> (StatusCode, Value) {
        let request = Request::builder()
            .method("POST")
            .uri("/control")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("build control request");
        let response = self
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("control response");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read control body");
        let value = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "control response was not JSON ({e}): {}",
                String::from_utf8_lossy(&bytes)
            )
        });
        (status, value)
    }

    async fn get_json(&self, uri: &str) -> (StatusCode, Value) {
        let request = Request::builder()
            .uri(uri)
            .body(Body::empty())
            .expect("build get request");
        let response = self
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("get response");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 512 * 1024)
            .await
            .expect("read body");
        let value = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "{uri} response was not JSON ({e}): {}",
                String::from_utf8_lossy(&bytes)
            )
        });
        (status, value)
    }

    async fn shutdown(self) {
        self.manager.stop().await;
        self.bus.publish(BusEvent::ShuttingDown { reason: None });
        let _ = self.aggregator_task.await;
    }
}

/// A daemon mid-playback with a loaded, seekable track and a working decimal-dB control.
fn playing_daemon() -> DaemonModel {
    let model = DaemonModel::with_profile(VERIFIED_PROFILE);
    model.external_change(|s| {
        s.playback = 2;
        s.track = 3;
        s.track_id = "t-3".to_string();
        s.position = 42;
        s.length = 215;
        s.volume_db = -23.5;
        s.metadata = Some(Metadata::sample());
    });
    model
}

async fn start_daemon(model: &DaemonModel) -> WireServer {
    WireServer::start(Arc::new(model.clone()), WirePolicy::default()).await
}

// =============================================================================
// routing — AC2, and the "no fallback to Roon" constraint
// =============================================================================

/// **Client expectation.** A knob whose selected zone is `hqplayer:rig` posts
/// `{"zone_id":"hqplayer:rig","action":"pause"}` and expects that daemon to pause.
///
/// **RED before #328:** `knob_control_handler` dispatches `lms:`, `openhome:` and `upnp:` and then
/// *falls through* to `control_roon` for everything else. The command was executed against Roon
/// with the prefix stripped, so the daemon saw nothing and a disconnected Roon returned 500.
#[tokio::test]
async fn transport_reaches_the_selected_hqplayer_instance_and_not_roon() {
    let model = playing_daemon();
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    rig.zone_when(|z| z.state == PlaybackState::Playing).await;

    let before = model.request_count("Pause");
    let (status, body) = rig
        .post_control(json!({"zone_id":"hqplayer:rig","action":"pause"}))
        .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "pause on a direct HQPlayer zone must succeed; body={body}"
    );
    assert_eq!(
        model.request_count("Pause"),
        before + 1,
        "the selected daemon must have received exactly one Pause"
    );
    rig.zone_when(|z| z.state == PlaybackState::Paused).await;

    rig.shutdown().await;
    server.stop();
}

/// **Client expectation.** Every transport verb the zone advertises is routed, not just play/pause.
///
/// **RED before #328:** all of them reached Roon. `stop` and `seek` additionally had no route at
/// all — `control_roon` maps `stop`, but `seek` is not in any adapter's action vocabulary.
#[tokio::test]
async fn stop_next_previous_and_seek_all_route_to_the_daemon() {
    let model = playing_daemon();
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    rig.zone_when(|z| z.state == PlaybackState::Playing).await;

    for (action, element, value) in [
        ("next", "Next", None),
        ("previous", "Previous", None),
        ("seek", "Seek", Some(json!(90))),
        ("stop", "Stop", None),
    ] {
        let before = model.request_count(element);
        let mut body = json!({"zone_id":"hqplayer:rig","action":action});
        if let Some(v) = value {
            body["value"] = v;
        }
        let (status, response) = rig.post_control(body).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "action `{action}` must route to HQPlayer; body={response}"
        );
        assert_eq!(
            model.request_count(element),
            before + 1,
            "action `{action}` must send exactly one `{element}`"
        );
    }

    let seek = model
        .last_request("Seek")
        .expect("the daemon recorded the Seek");
    assert_eq!(
        mock_servers::hqplayer::model::request_attr(&seek, "position").as_deref(),
        Some("90"),
        "the requested position must reach the wire unmodified: {seek}"
    );

    rig.shutdown().await;
    server.stop();
}

/// **Client expectation.** `play_pause` — the knob's single-press verb — resolves against the state
/// the server most recently published to that knob, not against a guess.
///
/// **RED before #328:** routed to Roon.
#[tokio::test]
async fn play_pause_resolves_against_the_published_state() {
    let model = playing_daemon();
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    rig.zone_when(|z| z.state == PlaybackState::Playing).await;

    let (status, _) = rig
        .post_control(json!({"zone_id":"hqplayer:rig","action":"play_pause"}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        model.request_count("Pause"),
        1,
        "playing + play_pause must pause"
    );
    assert_eq!(model.request_count("Play"), 0);

    rig.zone_when(|z| z.state == PlaybackState::Paused).await;
    let (status, _) = rig
        .post_control(json!({"zone_id":"hqplayer:rig","action":"play_pause"}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        model.request_count("Play"),
        1,
        "paused + play_pause must play"
    );

    rig.shutdown().await;
    server.stop();
}

/// **Client expectation.** A command naming an instance that does not exist is refused. It is never
/// quietly redirected to whichever backend happens to be the fallback.
///
/// **RED before #328:** `hqplayer:ghost` fell through to `control_roon`, which asked Roon for a zone
/// called `ghost`.
#[tokio::test]
async fn an_unknown_instance_is_refused_rather_than_redirected() {
    let model = playing_daemon();
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    rig.zone_when(|z| z.state == PlaybackState::Playing).await;

    let before = model.request_count("Play");
    let (status, body) = rig
        .post_control(json!({"zone_id":"hqplayer:ghost","action":"play"}))
        .await;

    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "an unknown HQPlayer instance must be a 404, not a fallback; body={body}"
    );
    assert_eq!(
        model.request_count("Play"),
        before,
        "the configured instance must not receive another instance's command"
    );

    rig.shutdown().await;
    server.stop();
}

/// **Client expectation.** An unrecognised *prefixed* zone id is an error. Only a legacy
/// **unprefixed** id may mean Roon — that is the documented Node.js compatibility behaviour and the
/// knob still sends it after an upgrade.
///
/// **RED before #328:** any unrecognised prefix was handed to Roon with nothing stripped.
#[tokio::test]
async fn an_unknown_prefix_is_refused_but_a_legacy_unprefixed_id_still_means_roon() {
    let rig = Rig::new().await;

    let (status, body) = rig
        .post_control(json!({"zone_id":"spotify:abc","action":"play"}))
        .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an unknown prefix must be refused, not routed to Roon; body={body}"
    );

    // Legacy shape: no colon at all. Roon is disconnected here, so the contract being pinned is
    // "it was offered to Roon", which a 5xx from the Roon adapter demonstrates and a 400 from the
    // router's own prefix check would not.
    let (status, _) = rig
        .post_control(json!({"zone_id":"1601bb42ed14351b99c2926214f6cbb80724","action":"play"}))
        .await;
    assert_ne!(
        status,
        StatusCode::BAD_REQUEST,
        "a legacy unprefixed zone id must still be offered to Roon"
    );

    rig.shutdown().await;
}

// =============================================================================
// capability — allowed controls must reflect observed state
// =============================================================================

/// **Client expectation.** The knob greys out what it cannot do. `GET /knob/now_playing` on a
/// stopped HQPlayer zone with nothing loaded must not claim seek, next and previous are available.
///
/// **RED before #328:** `hqp_status_to_zone` hard-coded `is_seekable: true`, `is_next_allowed: true`
/// and `is_previous_allowed: true` for every HQPlayer zone in every state.
#[tokio::test]
async fn a_stopped_empty_zone_advertises_no_seek_next_or_previous() {
    let model = DaemonModel::with_profile(VERIFIED_PROFILE);
    model.external_change(|s| {
        s.playback = 0;
        s.track = 0;
        s.track_id.clear();
        s.position = 0;
        s.length = 0;
        s.metadata = None;
    });
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    let zone = rig.zone_when(|z| z.state == PlaybackState::Stopped).await;

    assert!(
        !zone.is_seekable,
        "nothing is loaded, so nothing is seekable"
    );
    assert!(
        !zone.is_next_allowed,
        "no loaded track means no next to skip to"
    );
    assert!(!zone.is_previous_allowed);
    assert!(!zone.is_pause_allowed, "a stopped zone cannot be paused");

    let (status, body) = rig
        .get_json("/knob/now_playing?zone_id=hqplayer%3Arig")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["is_next_allowed"], json!(false));
    assert_eq!(body["is_previous_allowed"], json!(false));
    assert_eq!(body["is_pause_allowed"], json!(false));

    rig.shutdown().await;
    server.stop();
}

/// **Client expectation.** With a track loaded and a known duration, seek and skip *are* offered,
/// and the duration the knob draws its progress bar from is the daemon's.
#[tokio::test]
async fn a_loaded_playing_zone_advertises_the_transport_it_really_has() {
    let model = playing_daemon();
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    let zone = rig.zone_when(|z| z.state == PlaybackState::Playing).await;

    assert!(zone.is_seekable, "a 215 s track is seekable");
    assert!(zone.is_next_allowed);
    assert!(zone.is_previous_allowed);
    assert!(zone.is_pause_allowed);
    assert!(!zone.is_play_allowed, "already playing");
    assert!(zone.is_controllable);

    let np = zone
        .now_playing
        .expect("a loaded track publishes now_playing");
    assert_eq!(np.duration, Some(215.0));
    assert_eq!(np.seek_position, Some(42.0));

    rig.shutdown().await;
    server.stop();
}

/// **Client expectation.** A seek on a zone the server told the client was not seekable is refused
/// locally. The router must not send a command the same server advertised as unavailable.
///
/// **RED before #328:** seek routed to Roon, and there was no capability check anywhere.
#[tokio::test]
async fn seek_is_refused_when_the_published_zone_is_not_seekable() {
    let model = DaemonModel::with_profile(VERIFIED_PROFILE);
    model.external_change(|s| {
        s.playback = 2;
        s.track = 1;
        s.track_id = "stream".to_string();
        // A live stream: playing, but with no duration, so there is no position to seek to.
        s.length = 0;
        s.metadata = Some(Metadata::sample());
    });
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    let zone = rig.zone_when(|z| z.state == PlaybackState::Playing).await;
    assert!(!zone.is_seekable, "the RED needs an unseekable zone");

    let before = model.request_count("Seek");
    let (status, body) = rig
        .post_control(json!({"zone_id":"hqplayer:rig","action":"seek","value":30}))
        .await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "seek on an unseekable zone must be refused; body={body}"
    );
    assert_eq!(
        model.request_count("Seek"),
        before,
        "no Seek may reach a daemon whose zone advertises no seek"
    );

    rig.shutdown().await;
    server.stop();
}

// =============================================================================
// metadata — AC1 and the source/output domain split
// =============================================================================

/// **Client expectation.** The knob's three text lines come from the track, and the format badge it
/// draws must describe the *source* material — not the DAC's output stage.
///
/// **RED before #328:** `TrackMetadata.bit_depth` was filled from `Status.active_bits`, which is the
/// **output** depth. With a 24-bit source upsampled to a 32-bit output stage the client was told the
/// track was 32-bit. `format` was filled from `Status.active_mode`, the output mode, which reads
/// back the literal string `[source]` under source-following.
#[tokio::test]
async fn track_metadata_is_the_source_domain_never_the_output_stage() {
    let model = DaemonModel::with_profile(VERIFIED_PROFILE);
    model.external_change(|s| {
        s.playback = 2;
        s.track = 1;
        s.track_id = "domain".to_string();
        s.length = 100;
        // Source: 24/44.1. The fake derives Status.active_bits/samplerate/bitrate from this child,
        // and active_rate separately, which is exactly the daemon's own split.
        s.metadata = Some(Metadata {
            artist: "Bill Evans".to_string(),
            album: "Sunday at the Village Vanguard".to_string(),
            song: "Gloria's Step".to_string(),
            samplerate: 44_100,
            bits: 24,
            channels: 2,
            bitrate: 1411,
        });
        // Output: upsampled to 352.8 kHz.
        s.active_rate_hz = 352_800;
    });
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    let zone = rig.zone_when(|z| z.state == PlaybackState::Playing).await;

    let np = zone.now_playing.expect("now_playing");
    assert_eq!(np.title, "Gloria's Step");
    assert_eq!(np.artist, "Bill Evans");
    assert_eq!(np.album, "Sunday at the Village Vanguard");

    let meta = np.metadata.expect("track metadata");
    assert_eq!(
        meta.sample_rate,
        Some(44_100),
        "the track's rate is the source rate, not the 352800 Hz output rate"
    );
    assert_eq!(
        meta.bit_depth,
        Some(24),
        "the track's depth is the source depth, not Status.active_bits"
    );
    assert_eq!(meta.bitrate, Some(1411));
    assert_eq!(
        meta.format, None,
        "HQPlayer supplies no source container/codec, and active_mode is an output fact; \
         publishing it as the track's format fabricates source metadata"
    );

    rig.shutdown().await;
    server.stop();
}

/// **Client expectation.** With the source at 24 bit and the output stage at 32 bit, the client is
/// told the *track* is 24 bit.
///
/// This one goes through the pure projection rather than the wire fake, because the fake derives
/// `Status.active_bits` from the very metadata child it derives the source depth from, so on that
/// wire the two values can never disagree and the defect is unexpressible. The wire test above
/// covers the rate half of the same split, where the fake does keep the domains apart.
///
/// **RED before #328:** `bit_depth` was `Some(status.active_bits as u8)` — the output stage.
#[test]
fn the_published_track_depth_is_the_source_depth_not_the_output_stage() {
    let mut status = HqpStatus {
        state: 2,
        track: 1,
        track_id: "domain".to_string(),
        position: 5,
        length: 100,
        volume: -24,
        volume_db: -23.5,
        active_mode: "PCM".to_string(),
        active_rate: 352_800,
        // The DAC is being fed 32-bit words.
        active_bits: 32,
        active_channels: 2,
        samplerate: 44_100,
        bitrate: 1411,
        title: Some("Gloria's Step".to_string()),
        artist: Some("Bill Evans".to_string()),
        album: Some("Sunday at the Village Vanguard".to_string()),
        ..HqpStatus::default()
    };
    // The source is 24/44.1.
    status.source_bits = Some(24);
    status.source_samplerate = Some(44_100);
    status.source_bitrate = Some(1411);

    let zone = HqpAdapter::project_direct_zone(
        "127.0.0.1",
        Some("rig"),
        &Default::default(),
        &status,
        &VolumeRange {
            min: -60,
            max: 0,
            step: 1,
            enabled: true,
            adaptive: false,
            min_db: -60.0,
            max_db: 0.0,
            step_db: Some(0.5),
        },
    );

    let meta = zone
        .now_playing
        .expect("a loaded track")
        .metadata
        .expect("track metadata");
    assert_eq!(
        meta.bit_depth,
        Some(24),
        "32 is the output stage's depth; publishing it as the track's fabricates the source"
    );
    assert_eq!(meta.sample_rate, Some(44_100));
}

/// **Client expectation.** Fields the daemon supplies are published; fields it does not supply are
/// absent rather than invented.
///
/// **RED before #328:** the parser read exactly three attributes — `song`, `artist`, `album` — and
/// hard-coded `composer: None` and `genre: None` in the projection, so a daemon that supplies them
/// could not surface them.
///
/// The attribute *spellings* `composer` and `genre` have no evidence anywhere in this repository.
/// This test does not assert that a real daemon sends them; it asserts that the parser is generic
/// over the child's attributes, so whatever is supplied is published and nothing is claimed.
#[test]
fn supplied_metadata_attributes_are_published_and_absent_ones_are_not_invented() {
    let with_extras = concat!(
        r#"<?xml version="1.0"?>"#,
        r#"<Status state="2" track="1" track_id="x" position="5" length="60" volume="-12.5" "#,
        r#"active_mode="PCM" active_rate="352800" active_bits="32" active_channels="2" "#,
        r#"samplerate="96000" bitrate="4608">"#,
        "\n",
        r#"<metadata artist="Miles Davis" album="Kind of Blue" song="So What" "#,
        r#"composer="Miles Davis" genre="Jazz" uri="file:///m/so-what.flac" "#,
        r#"samplerate="96000" bits="24" channels="2" bitrate="4608"/>"#,
        "\n</Status>",
    );

    let status = HqpAdapter::parse_status_for_test(with_extras);
    assert_eq!(status.composer.as_deref(), Some("Miles Davis"));
    assert_eq!(status.genre.as_deref(), Some("Jazz"));
    assert_eq!(status.uri.as_deref(), Some("file:///m/so-what.flac"));
    assert_eq!(status.source_bits, Some(24));
    assert_eq!(status.source_samplerate, Some(96_000));

    let without_extras = concat!(
        r#"<?xml version="1.0"?>"#,
        r#"<Status state="2" track="1" track_id="x" position="5" length="60" volume="-12.5" "#,
        r#"samplerate="44100" bitrate="1411">"#,
        "\n",
        r#"<metadata artist="Bill Evans" album="Vanguard" song="Gloria&apos;s Step"/>"#,
        "\n</Status>",
    );
    let status = HqpAdapter::parse_status_for_test(without_extras);
    assert_eq!(status.title.as_deref(), Some("Gloria's Step"));
    assert_eq!(
        status.composer, None,
        "an absent attribute must stay absent, never a placeholder"
    );
    assert_eq!(status.genre, None);
    assert_eq!(status.uri, None);
    assert_eq!(
        status.source_bits, None,
        "the child sent no bits; the root's output active_bits must not stand in for it"
    );
}

/// **Client expectation.** A malformed `metadata` child costs the metadata and nothing else. The
/// knob keeps showing the right play state and level even when the title is unreadable.
///
/// The `metadata` child is the one part of a `Status` document observed to be malformed in the wild
/// (`src/adapters/hqplayer.rs:310`, and the framing note on the corpus fixture).
#[test]
fn a_malformed_metadata_child_cannot_wedge_the_transport_state() {
    // Unterminated child: the attribute value's closing quote and the element's `/>` are both gone.
    let wedged = concat!(
        r#"<?xml version="1.0"?>"#,
        r#"<Status state="2" track="7" track_id="ok" position="11" length="60" volume="-18.5" "#,
        r#"samplerate="44100" bitrate="1411">"#,
        "\n",
        r#"<metadata artist="Bill Evans" album="Vanguard" song="Gloria"#,
        "\n</Status>",
    );

    let status = HqpAdapter::parse_status_for_test(wedged);
    assert_eq!(status.state, 2, "playback state survives a bad child");
    assert_eq!(status.position, 11);
    assert_eq!(status.length, 60);
    assert_eq!(
        status.volume_db, -18.5,
        "the level survives, and is not defaulted to 0 dB / full scale"
    );
    assert_eq!(status.track, 7);
}

// =============================================================================
// stale_state — capability and identity transitions
// =============================================================================

/// **Client expectation.** Stopping clears the track *and* the capabilities that only made sense
/// while it was loaded. A knob must not be left offering seek on a zone that stopped.
///
/// The now-playing clear itself is already pinned by the #162 lifecycle suite; what is new here is
/// that `is_seekable` / `is_next_allowed` transition with it.
#[tokio::test]
async fn stopping_clears_the_track_and_the_capabilities_that_depended_on_it() {
    let model = playing_daemon();
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    let playing = rig.zone_when(|z| z.state == PlaybackState::Playing).await;
    assert!(playing.is_seekable && playing.is_next_allowed);

    model.external_change(|s| {
        s.playback = 0;
        s.track = 0;
        s.track_id.clear();
        s.position = 0;
        s.length = 0;
        s.metadata = None;
    });

    let stopped = rig.zone_when(|z| z.state == PlaybackState::Stopped).await;
    assert!(stopped.now_playing.is_none(), "stale track must clear");
    assert!(
        !stopped.is_seekable,
        "seekability must clear with the track, not persist as a stale capability"
    );
    assert!(!stopped.is_next_allowed);
    assert!(!stopped.is_previous_allowed);

    rig.shutdown().await;
    server.stop();
}

/// **Client expectation.** A transient failure to read the volume capability must not tell the
/// client the zone has become a fixed-volume device. That is a capability *lie*, and on a knob it
/// removes the control the user is actively turning.
///
/// **RED before #328:** `ensure_volume_range` answered any transient failure with
/// `VolumeRange::default()`, whose `enabled` is `false`, and `hqp_status_to_zone` maps `!enabled` to
/// `volume_control: None`. One dropped `VolumeRange` reply erased a working −60…0 dB control.
///
/// Distinct from an *observed* transition to fixed volume, which must clear — pinned by the #162
/// suite and left alone here.
#[tokio::test]
async fn a_transient_volume_read_failure_retains_the_last_observed_capability() {
    let model = playing_daemon();
    model.external_change(|s| {
        s.volume_db = -23.5;
        s.volume_range.min_db = -60.0;
        s.volume_range.max_db = 0.0;
        s.volume_range.step_db = Some(0.5);
    });
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    let observed = rig.zone_when(|z| z.volume_control.is_some()).await;
    assert_eq!(
        observed.volume_control.as_ref().map(|v| v.min),
        Some(-60.0),
        "the RED needs a capability that was genuinely observed first"
    );

    // The daemon stops answering `VolumeRange` — a lost reply, not a refusal. A refusal
    // (`result="Error"`) is `HqpRejected` and already aborts the whole read; this is the path that
    // instead substituted a synthetic fixed-volume capability and published it.
    let polls_before = server.stats().element_count("Status");
    server.set_policy(WirePolicy {
        silent_for_element: Some("VolumeRange".to_string()),
        ..WirePolicy::default()
    });

    // Let the worker complete several coherent-observation cycles under the fault.
    for _ in 0..200 {
        if server.stats().element_count("Status") > polls_before + 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    let zone = rig
        .aggregator
        .get_zone("hqplayer:rig")
        .await
        .expect("the zone must not disappear because one optional read timed out");
    let vc = zone.volume_control.as_ref().unwrap_or_else(|| {
        panic!(
            "a dropped VolumeRange reply is not evidence that the daemon became fixed-volume; \
             the last observed -60..0 dB capability must be retained. zone={zone:?}"
        )
    });
    assert_eq!(vc.min, -60.0, "the retained bounds are the observed ones");
    assert_eq!(vc.max, 0.0);
    assert_eq!(vc.step, 0.5);

    rig.shutdown().await;
    server.stop();
}

/// **Client expectation.** The zone id a knob has stored survives a daemon restart. A knob that
/// reconnects to a renamed zone silently controls nothing.
#[tokio::test]
async fn the_direct_zone_id_is_stable_across_a_reconnect() {
    let model = playing_daemon();
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    let adapter = rig.attach(&server).await;
    rig.zone_when(|z| z.state == PlaybackState::Playing).await;

    adapter.disconnect().await;
    let recovered = rig.zone_when(|z| z.state == PlaybackState::Playing).await;
    assert_eq!(
        recovered.zone_id, "hqplayer:rig",
        "the id is the instance name, and a reconnect must not renumber it to the host"
    );
    assert_eq!(recovered.source, "hqplayer");

    rig.shutdown().await;
    server.stop();
}

// =============================================================================
// volume — decimal dB, clamping, and refusing to guess
// =============================================================================

/// **Client expectation.** A knob configured for 0.5 dB steps moves the level by 0.5 dB. Decimal dB
/// must survive from the daemon's `VolumeRange`, through the published zone, into the level written
/// back — with no rounding step anywhere in between.
#[tokio::test]
async fn decimal_db_survives_the_whole_round_trip() {
    let model = playing_daemon();
    model.external_change(|s| {
        s.volume_db = -23.5;
        s.volume_range.min_db = -60.0;
        s.volume_range.max_db = 0.0;
        s.volume_range.step_db = Some(0.5);
    });
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    let zone = rig.zone_when(|z| z.state == PlaybackState::Playing).await;

    let vc = zone.volume_control.expect("a decimal-dB control");
    assert_eq!(vc.value, -23.5);
    assert_eq!(vc.min, -60.0);
    assert_eq!(vc.max, 0.0);
    assert_eq!(vc.step, 0.5, "the daemon's own step, not a rounded 1 dB");

    let (status, body) = rig
        .post_control(json!({"zone_id":"hqplayer:rig","action":"vol_up"}))
        .await;
    assert_eq!(status, StatusCode::OK, "body={body}");

    rig.zone_when(|z| {
        z.volume_control
            .as_ref()
            .map(|v| (v.value - (-23.0)).abs() < 1e-6)
            .unwrap_or(false)
    })
    .await;

    let (status, _) = rig
        .post_control(json!({"zone_id":"hqplayer:rig","action":"vol_abs","value":-31.5}))
        .await;
    assert_eq!(status, StatusCode::OK);
    let written = model
        .last_request("Volume")
        .expect("the daemon recorded the level write");
    assert_eq!(
        mock_servers::hqplayer::model::request_attr(&written, "value").as_deref(),
        Some("-31.5"),
        "the fractional level must reach the wire unrounded: {written}"
    );

    rig.shutdown().await;
    server.stop();
}

/// **Client expectation, and the safety-critical one.** A volume command whose level is missing or
/// unparseable is **refused**. It is never turned into a number.
///
/// **RED before #328:** the `vol_abs` arm ended `.unwrap_or(50.0)`. For a dB zone +50 dB is above
/// every possible maximum, and the existing safety lint does not see it — that lint scans
/// `src/adapters` only, and matches `.value.unwrap_or(50.0)`, not `.as_f64()).unwrap_or(50.0)`.
#[tokio::test]
async fn a_missing_or_unparseable_level_is_refused_never_defaulted() {
    let model = playing_daemon();
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    rig.zone_when(|z| z.state == PlaybackState::Playing).await;

    for body in [
        json!({"zone_id":"hqplayer:rig","action":"vol_abs"}),
        json!({"zone_id":"hqplayer:rig","action":"vol_abs","value":null}),
        json!({"zone_id":"hqplayer:rig","action":"vol_abs","value":"loud"}),
    ] {
        let before = model.request_count("Volume");
        let (status, response) = rig.post_control(body.clone()).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "an absolute volume with no usable level must be refused: {body} -> {response}"
        );
        assert_eq!(
            model.request_count("Volume"),
            before,
            "no level may be written when none was supplied: {body}"
        );
    }

    rig.shutdown().await;
    server.stop();
}

/// **Client expectation.** A level outside the daemon's range is clamped into it, not rejected and
/// not passed through. A knob spun to its stop should land on the daemon's maximum.
#[tokio::test]
async fn out_of_range_levels_clamp_to_the_observed_range() {
    let model = playing_daemon();
    model.external_change(|s| {
        s.volume_range.min_db = -60.0;
        s.volume_range.max_db = 0.0;
    });
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    rig.zone_when(|z| z.volume_control.is_some()).await;

    let (status, _) = rig
        .post_control(json!({"zone_id":"hqplayer:rig","action":"vol_abs","value":12.0}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        mock_servers::hqplayer::model::request_attr(
            &model.last_request("Volume").expect("a Volume write"),
            "value"
        )
        .as_deref(),
        Some("0"),
        "+12 dB must clamp to the observed maximum of 0 dB"
    );

    let (status, _) = rig
        .post_control(json!({"zone_id":"hqplayer:rig","action":"vol_abs","value":-400.0}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        mock_servers::hqplayer::model::request_attr(
            &model.last_request("Volume").expect("a Volume write"),
            "value"
        )
        .as_deref(),
        Some("-60"),
        "-400 dB must clamp to the observed minimum"
    );

    rig.shutdown().await;
    server.stop();
}

/// **Client expectation.** A zone the server published without a volume control gets no volume
/// commands. Sending one to a fixed-volume daemon produces a bare `result="Error"` and, on a
/// hardware-attenuated setup, is the request that has no safe interpretation.
#[tokio::test]
async fn volume_commands_are_refused_when_the_zone_advertises_no_control() {
    let model = playing_daemon();
    model.external_change(|s| s.volume_range.enabled = false);
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    let zone = rig.zone_when(|z| z.state == PlaybackState::Playing).await;
    assert!(
        zone.volume_control.is_none(),
        "the RED needs an observed fixed-volume zone"
    );

    for action in ["vol_up", "vol_down", "mute"] {
        let (status, body) = rig
            .post_control(json!({"zone_id":"hqplayer:rig","action":action}))
            .await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "`{action}` must be refused on a fixed-volume zone; body={body}"
        );
    }
    let (status, _) = rig
        .post_control(json!({"zone_id":"hqplayer:rig","action":"vol_abs","value":-20.0}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    assert_eq!(
        model.request_count("Volume")
            + model.request_count("VolumeUp")
            + model.request_count("VolumeDown")
            + model.request_count("VolumeMute"),
        0,
        "not one volume command may reach a daemon with no volume control"
    );

    rig.shutdown().await;
    server.stop();
}

/// **Client expectation.** Mute is reported only because it is observable. `VolumeMute` is a
/// verified absolute move to the range floor with no mute flag on the wire (#322, live 6.0.2), so
/// "muted" means the level is at the floor — and that is what the client is told.
///
/// **RED before #328:** `is_muted` was the literal `false`, with the comment "HQPlayer doesn't
/// report mute separately".
#[tokio::test]
async fn mute_is_routed_and_reported_from_the_observable_floor() {
    let model = playing_daemon();
    model.external_change(|s| {
        s.volume_db = -23.5;
        s.volume_range.min_db = -60.0;
        s.volume_range.max_db = 0.0;
    });
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    let zone = rig.zone_when(|z| z.volume_control.is_some()).await;
    assert!(
        !zone.volume_control.expect("control").is_muted,
        "-23.5 dB is not muted"
    );

    let (status, body) = rig
        .post_control(json!({"zone_id":"hqplayer:rig","action":"mute"}))
        .await;
    assert_eq!(status, StatusCode::OK, "body={body}");
    assert_eq!(model.request_count("VolumeMute"), 1);

    let muted = rig
        .zone_when(|z| {
            z.volume_control
                .as_ref()
                .map(|v| v.is_muted)
                .unwrap_or(false)
        })
        .await;
    let vc = muted.volume_control.expect("control");
    assert_eq!(vc.value, -60.0, "the floor is where VolumeMute leaves it");
    assert!(vc.is_muted);

    rig.shutdown().await;
    server.stop();
}

// =============================================================================
// dsp_boundary — AC7, no ambiguous double routing
// =============================================================================

/// **Client expectation.** A direct HQPlayer zone is one zone with one control path. It must not
/// also advertise itself as a DSP-enrichment target, which would give the client two ways to reach
/// the same daemon with no rule for which wins.
///
/// **RED before #328:** `link_zone` accepted any `zone_id`, so `hqplayer:rig` → `rig` was storable,
/// after which `get_all_zones_internal` attached a `dsp` block to the direct zone.
#[tokio::test]
async fn a_direct_zone_cannot_be_its_own_dsp_enrichment_target() {
    let model = playing_daemon();
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    rig.zone_when(|z| z.state == PlaybackState::Playing).await;

    let refused = rig
        .state
        .hqp_zone_links
        .link_zone("hqplayer:rig".to_string(), "rig".to_string())
        .await;
    assert!(
        refused.is_err(),
        "linking a direct HQPlayer zone as a DSP target must be refused"
    );
    assert!(
        rig.state
            .hqp_zone_links
            .get_instance_for_zone("hqplayer:rig")
            .await
            .is_none(),
        "the refused link must not have been stored"
    );

    let (status, body) = rig.get_json("/zones").await;
    assert_eq!(status, StatusCode::OK);
    let direct = body["zones"]
        .as_array()
        .expect("zones array")
        .iter()
        .find(|z| z["zone_id"] == "hqplayer:rig")
        .expect("the direct zone is listed");
    assert!(
        direct.get("dsp").is_none(),
        "a direct zone carries no dsp block: {direct}"
    );

    rig.shutdown().await;
    server.stop();
}

/// **Client expectation.** Making the direct zone truthful must not cost the existing feature:
/// a Roon or LMS zone linked to an HQPlayer instance keeps its DSP enrichment.
#[tokio::test]
async fn a_linked_roon_zone_keeps_its_dsp_enrichment() {
    let model = playing_daemon();
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    rig.zone_when(|z| z.state == PlaybackState::Playing).await;

    // A Roon zone published the ordinary way, then linked for DSP enrichment.
    rig.bus.publish(BusEvent::ZoneDiscovered {
        zone: Zone {
            zone_id: "roon:living".to_string(),
            zone_name: "Living Room".to_string(),
            state: PlaybackState::Playing,
            volume_control: None,
            now_playing: None,
            source: "roon".to_string(),
            is_controllable: true,
            is_seekable: true,
            last_updated: 0,
            is_play_allowed: false,
            is_pause_allowed: true,
            is_next_allowed: true,
            is_previous_allowed: true,
        },
    });
    for _ in 0..50 {
        if rig.aggregator.get_zone("roon:living").await.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    rig.state
        .hqp_zone_links
        .link_zone("roon:living".to_string(), "rig".to_string())
        .await
        .expect("a Roon zone may be linked for DSP enrichment");

    let (_, body) = rig.get_json("/zones").await;
    let linked = body["zones"]
        .as_array()
        .expect("zones array")
        .iter()
        .find(|z| z["zone_id"] == "roon:living")
        .expect("the Roon zone is listed");
    assert_eq!(
        linked["dsp"]["type"], "hqplayer",
        "the linked Roon zone keeps its enrichment: {linked}"
    );
    assert_eq!(linked["dsp"]["instance"], "rig");

    rig.state.hqp_zone_links.unlink_zone("roon:living").await;
    rig.shutdown().await;
    server.stop();
}

/// **Client expectation.** Turning the HQPlayer adapter off in settings hides HQPlayer zones, the
/// same way it does for every other backend.
///
/// **RED before #328:** the zone filter tested the prefix `hqp:` while zones are published as
/// `hqplayer:`, so the test never matched and HQPlayer zones fell into the "unknown prefix, include
/// by default" arm. The toggle did nothing.
#[test]
fn the_zone_filter_matches_the_prefix_hqplayer_zones_are_actually_published_with() {
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/knobs/routes.rs"),
    )
    .expect("read the routing surface");

    assert!(
        !source.contains(r#"starts_with("hqp:")"#),
        "the adapter filter must test `hqplayer:` — the prefix PrefixedZoneId::hqplayer emits — \
         not `hqp:`, which no zone id ever carries"
    );
    assert!(
        source.contains(r#"starts_with("hqplayer:")"#),
        "the routing surface must recognise the `hqplayer:` prefix"
    );
}

// Keep the unused-import guard honest: `SocketAddr` is referenced only by the router type in some
// axum versions, so name it once rather than conditionally importing it.
#[allow(dead_code)]
fn _addr_type_is_used(_: Option<SocketAddr>) {}

// =============================================================================
// review — defects found by the falsification pass, pinned so they stay fixed
// =============================================================================

/// **Found by review.** The safety floor on the published volume step must not coarsen a *finer*
/// step the daemon actually reported.
///
/// The first version of the #328 projection took `.max(MIN_PUBLISHED_VOLUME_STEP_DB)` over the
/// reported value, so a daemon offering 0.1 dB was published as offering 0.5 dB. That is a
/// fabrication in the opposite direction from the one the floor exists to prevent, and a
/// fine-step user would feel it on the first turn of the knob. The floor is for a step that was
/// never reported, not for one that was.
#[tokio::test]
async fn a_finer_reported_step_is_not_coarsened_by_the_safety_floor() {
    let model = playing_daemon();
    model.external_change(|s| {
        s.volume_db = -23.5;
        s.volume_range.min_db = -60.0;
        s.volume_range.max_db = 0.0;
        s.volume_range.step_db = Some(0.1);
    });
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    let zone = rig.zone_when(|z| z.volume_control.is_some()).await;

    assert_eq!(
        zone.volume_control.expect("control").step,
        0.1,
        "the daemon reported 0.1 dB; publishing 0.5 dB overstates its granularity"
    );

    let (status, _) = rig
        .post_control(json!({"zone_id":"hqplayer:rig","action":"vol_up"}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        mock_servers::hqplayer::model::request_attr(
            &model.last_request("Volume").expect("a Volume write"),
            "value"
        )
        .as_deref(),
        Some("-23.4"),
        "one 0.1 dB step up from -23.5 dB is -23.4 dB"
    );

    rig.shutdown().await;
    server.stop();
}

/// **Found by review.** A computed level must reach the wire as a clean decimal, not as the
/// `f32`→`f64` representation error of the arithmetic that produced it.
///
/// The published zone carries the level and step as `f32`. Widening them back to `f64` and adding
/// turned `-23.5 + 0.1` into `-23.399999998509884`, and `set_volume_db` formats with `{}` — so that
/// is the string the daemon would have received, where the reference client sends `-23.4`.
#[tokio::test]
async fn a_computed_level_reaches_the_wire_without_float_noise() {
    let model = playing_daemon();
    model.external_change(|s| {
        s.volume_db = -23.5;
        s.volume_range.min_db = -60.0;
        s.volume_range.max_db = 0.0;
        s.volume_range.step_db = Some(0.1);
    });
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    rig.zone_when(|z| z.volume_control.is_some()).await;

    for action in ["vol_up", "vol_down"] {
        let (status, _) = rig
            .post_control(json!({"zone_id":"hqplayer:rig","action":action}))
            .await;
        assert_eq!(status, StatusCode::OK);
        let value = mock_servers::hqplayer::model::request_attr(
            &model.last_request("Volume").expect("a Volume write"),
            "value",
        )
        .expect("a value attribute");
        let decimals = value.split_once('.').map(|(_, d)| d.len()).unwrap_or(0);
        assert!(
            decimals <= 2,
            "`{action}` put {decimals} decimal places on the wire ({value}); a dB level is not a \
             place to leak float representation error"
        );
    }

    rig.shutdown().await;
    server.stop();
}

/// **Found by review.** A raw `>` inside an attribute value must not truncate the metadata scan.
///
/// XML forbids `<` and `&` in attribute values but **permits a bare `>`**, so `song="a/>b"` is
/// well-formed. A plain search for the child's `/>` terminator cut inside that value and silently
/// lost every attribute after it — here the title, the album *and* the source bit depth, to a track
/// name containing a slash before a bracket.
#[test]
fn a_raw_terminator_inside_an_attribute_value_does_not_truncate_the_metadata_scan() {
    let doc = concat!(
        r#"<?xml version="1.0"?>"#,
        r#"<Status state="2" track="1" track_id="x" position="5" length="60" volume="-12.5" "#,
        r#"samplerate="44100" bitrate="1411">"#,
        "\n",
        r#"<metadata artist="Nine Inch Nails" song="a/>b" album="Broken" bits="24"/>"#,
        "\n</Status>",
    );

    let status = HqpAdapter::parse_status_for_test(doc);
    assert_eq!(status.artist.as_deref(), Some("Nine Inch Nails"));
    assert_eq!(status.title.as_deref(), Some("a/>b"));
    assert_eq!(
        status.album.as_deref(),
        Some("Broken"),
        "attributes after a value containing `/>` must still be read"
    );
    assert_eq!(status.source_bits, Some(24));

    // And the bound still holds in the direction it was added for: a child that never terminates
    // must not start reporting the root's own attributes as source metadata.
    let unterminated = concat!(
        r#"<?xml version="1.0"?>"#,
        r#"<Status state="2" track="1" track_id="x" position="5" length="60" volume="-12.5" "#,
        r#"active_bits="32" samplerate="44100">"#,
        "\n",
        r#"<metadata artist="Bill Evans"#,
        "\n</Status>",
    );
    let status = HqpAdapter::parse_status_for_test(unterminated);
    assert_eq!(status.state, 2, "the root survives");
    assert_eq!(status.volume_db, -12.5);
    assert_eq!(
        status.source_bits, None,
        "the root's output active_bits must never be read as the source depth"
    );
}

/// **Found by review.** A command for a zone the aggregator has withdrawn must be refused.
///
/// The capability checks are evaluated against the published zone, so with no published zone there
/// is nothing to evaluate — but the `play`/`pause`/`stop` arms did not consult it at all and
/// answered `{"ok":true}` for a zone that had already been withdrawn, purely because the instance
/// still existed in the manager's map. A client that shows no such zone should not be able to
/// command it.
#[tokio::test]
async fn transport_is_refused_for_a_zone_the_aggregator_has_withdrawn() {
    let model = playing_daemon();
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    rig.zone_when(|z| z.state == PlaybackState::Playing).await;

    // Stopping the producer withdraws the zone; the instance itself remains configured.
    rig.manager.stop().await;
    for _ in 0..200 {
        if rig.aggregator.get_zone("hqplayer:rig").await.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        rig.aggregator.get_zone("hqplayer:rig").await.is_none(),
        "the RED needs the zone actually withdrawn"
    );

    for action in ["play", "pause", "stop", "next", "seek", "vol_up", "mute"] {
        let mut body = json!({"zone_id":"hqplayer:rig","action":action});
        if action == "seek" {
            body["value"] = json!(10);
        }
        let (status, response) = rig.post_control(body).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "`{action}` on a withdrawn zone must be refused; got {response}"
        );
    }

    rig.bus.publish(BusEvent::ShuttingDown { reason: None });
    let _ = rig.aggregator_task.await;
    server.stop();
}
