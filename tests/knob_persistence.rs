//! Controller power settings must be durable, or their save must fail visibly.
use unified_hifi_control::knobs::store::KnobStore;

use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::{delete, get, post},
    Router,
};
use serde_json::Value;
use std::{sync::Arc, time::Instant};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use unified_hifi_control::{
    adapters::{
        hqplayer::{HqpInstanceManager, HqpZoneLinkService},
        lms::LmsAdapter,
        openhome::OpenHomeAdapter,
        roon::RoonAdapter,
        upnp::UPnPAdapter,
        Startable,
    },
    aggregator::ZoneAggregator,
    api::AppState,
    bus::{create_bus, runtime::build_runtime},
    coordinator::AdapterCoordinator,
    knobs,
};

// Tests serialize access to the temporary config environment and exercise real HTTP
// handlers, including a fresh router/store restart.
async fn app() -> Router {
    let bus = create_bus();

    // Create coordinator (tests don't need real lifecycle management)
    let coordinator = Arc::new(AdapterCoordinator::new(bus.clone()));

    // Create disconnected adapters
    let roon = Arc::new(RoonAdapter::new_disconnected(bus.clone()));
    let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
    let runtime = build_runtime(aggregator.clone(), 16, 32);
    let bridge = Arc::new(
        unified_hifi_control::adapters::hqplayer::HqpRuntimeBridge::new(
            runtime.projection_ingress.clone(),
            runtime.commands.clone(),
        ),
    );
    let reliable_commands = runtime.commands.clone();
    tokio::spawn(runtime.projection_actor.run());
    let hqp_instances = Arc::new(HqpInstanceManager::new_with_runtime(bus.clone(), bridge));
    let hqplayer = hqp_instances.get_default().await;
    let hqp_zone_links = Arc::new(HqpZoneLinkService::new(hqp_instances.clone()));
    let lms = Arc::new(LmsAdapter::new(bus.clone()));
    let openhome = Arc::new(OpenHomeAdapter::new(bus.clone()));
    let upnp = Arc::new(UPnPAdapter::new(bus.clone()));
    let knob_store = KnobStore::new();

    // Build startable adapters list
    let startable_adapters: Vec<Arc<dyn Startable>> =
        vec![roon.clone(), lms.clone(), openhome.clone(), upnp.clone()];

    let state = AppState::new(
        roon,
        hqplayer,
        hqp_instances,
        hqp_zone_links,
        lms,
        openhome,
        upnp,
        knob_store,
        bus,
        aggregator,
        coordinator,
        startable_adapters,
        Instant::now(),
        CancellationToken::new(),
    )
    .with_reliable_commands(reliable_commands);

    Router::new()
        .route("/zones", get(knobs::knob_zones_handler))
        .route("/knob/devices", get(knobs::knob_devices_handler))
        .route("/now_playing", get(knobs::knob_now_playing_handler))
        .route(
            "/knob/devices/{knob_id}",
            delete(knobs::knob_remove_handler),
        )
        .route(
            "/config/{knob_id}",
            get(knobs::knob_config_by_path_handler).put(knobs::knob_config_update_by_path_handler),
        )
        .route("/knob/config", post(knobs::knob_config_update_handler))
        .with_state(state)
}

struct ConfigEnvironment {
    directory: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
}
impl ConfigEnvironment {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("UHC_CONFIG_DIR");
        std::env::set_var("UHC_CONFIG_DIR", directory.path());
        Self {
            directory,
            previous,
        }
    }
    fn file(&self) -> std::path::PathBuf {
        self.directory.path().join("unified-hifi/knobs.json")
    }
}
impl Drop for ConfigEnvironment {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var("UHC_CONFIG_DIR", value),
            None => std::env::remove_var("UHC_CONFIG_DIR"),
        }
    }
}
async fn request(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
fn sleep_settings() -> Value {
    serde_json::json!({
        "name": "My dial", "sleep_poll_stopped_sec": 113,
        "dim_charging": {"enabled": true, "timeout_sec": 321},
        "sleep_charging": {"enabled": true, "timeout_sec": 654},
        "deep_sleep_charging": {"enabled": true, "timeout_sec": 987},
        "dim_battery": {"enabled": true, "timeout_sec": 123},
        "sleep_battery": {"enabled": true, "timeout_sec": 456},
        "deep_sleep_battery": {"enabled": true, "timeout_sec": 789}
    })
}
#[tokio::test]
#[serial_test::serial]
async fn customized_sleep_settings_survive_a_bridge_restart() {
    let _environment = ConfigEnvironment::new();
    let app = app().await;
    assert_eq!(
        request(&app, "GET", "/config/dial", Value::Null).await.0,
        StatusCode::OK
    );
    let (status, saved) = request(&app, "PUT", "/config/dial", sleep_settings()).await;
    assert_eq!(status, StatusCode::OK);
    let restarted = self::app().await;
    let (status, restored) = request(&restarted, "GET", "/config/dial", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(restored["config_sha"], saved["config_sha"]);
    for (key, value) in sleep_settings().as_object().unwrap() {
        assert_eq!(&restored["config"][key], value, "{key}");
    }
}
#[tokio::test]
#[serial_test::serial]
async fn unsuccessful_saves_are_not_acknowledged_or_published_to_the_knob() {
    let environment = ConfigEnvironment::new();
    let app = app().await;
    request(&app, "GET", "/config/dial", Value::Null).await;
    let (_, before) = request(&app, "PUT", "/config/dial", sleep_settings()).await;
    let mqtt_store = KnobStore::new();
    let parent = environment.file().parent().unwrap().to_owned();
    let backup = environment.directory.path().join("last-good");
    std::fs::rename(&parent, &backup).unwrap();
    std::fs::write(&parent, "not a directory").unwrap();
    for (method, path) in [
        ("PUT", "/config/dial"),
        ("POST", "/knob/config?knob_id=dial"),
    ] {
        let (status, body) = request(
            &app,
            method,
            path,
            serde_json::json!({"sleep_battery": {"enabled": false, "timeout_sec": 1}}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "must not claim a durable save: {body}"
        );
        assert!(body["error"].is_string());
    }
    assert_eq!(
        request(&app, "DELETE", "/knob/devices/dial", Value::Null)
            .await
            .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let (status, unchanged) = request(&app, "GET", "/config/dial", Value::Null).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "known durable settings remain usable during a disk failure"
    );
    assert_eq!(unchanged["config"], before["config"]);
    use unified_hifi_control::mqtt::knob_command::{dispatch, DispatchOutcome, ParsedKnobAction};
    assert_eq!(
        dispatch(&mqtt_store, "dial", ParsedKnobAction::VolumeStep(9.0)).await,
        DispatchOutcome::StorageFailure
    );
    assert!(mqtt_store
        .get("dial")
        .await
        .unwrap()
        .config
        .volume_step_override
        .is_none());
    std::fs::remove_file(&parent).unwrap();
    std::fs::rename(&backup, &parent).unwrap();
    let (_, unchanged) = request(&app, "GET", "/config/dial", Value::Null).await;
    assert_eq!(unchanged["config_sha"], before["config_sha"]);
    assert_eq!(unchanged["config"], before["config"]);
    let restarted = self::app().await;
    let (_, restored) = request(&restarted, "GET", "/config/dial", Value::Null).await;
    assert_eq!(restored["config"], before["config"]);
}
#[tokio::test]
#[serial_test::serial]
async fn corrupt_saved_configuration_is_not_replaced_by_registration_defaults() {
    let environment = ConfigEnvironment::new();
    std::fs::create_dir_all(environment.file().parent().unwrap()).unwrap();
    let broken = b"{\"dial\": ";
    std::fs::write(environment.file(), broken).unwrap();
    let app = app().await;
    let (status, body) = request(&app, "GET", "/config/dial", Value::Null).await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "must not send factory defaults: {body}"
    );
    assert_eq!(std::fs::read(environment.file()).unwrap(), broken);
}

#[tokio::test]
#[serial_test::serial]
async fn saving_replaces_the_complete_file_instead_of_truncating_the_old_one() {
    let environment = ConfigEnvironment::new();
    let app = app().await;
    request(&app, "GET", "/config/dial", Value::Null).await;
    let old_bytes = std::fs::read(environment.file()).unwrap();
    let old_file = environment.directory.path().join("previous-record");
    std::fs::hard_link(environment.file(), &old_file).unwrap();
    assert_eq!(
        request(&app, "PUT", "/config/dial", sleep_settings())
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        std::fs::read(old_file).unwrap(),
        old_bytes,
        "the previously committed inode must remain intact until atomic replacement"
    );
    assert_ne!(std::fs::read(environment.file()).unwrap(), old_bytes);
}

#[tokio::test]
#[serial_test::serial]
async fn unreadable_saved_configuration_is_not_treated_as_a_new_installation() {
    let environment = ConfigEnvironment::new();
    // A directory is deterministically unreadable as a settings file, even as root.
    std::fs::create_dir_all(environment.file()).unwrap();
    let app = app().await;
    assert_eq!(
        request(&app, "GET", "/config/dial", Value::Null).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert!(environment.file().is_dir());
    assert_eq!(std::fs::read_dir(environment.file()).unwrap().count(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial]
async fn concurrent_devices_preserve_each_others_complete_settings() {
    let _environment = ConfigEnvironment::new();
    let app = app().await;
    let mut requests = tokio::task::JoinSet::new();
    for id in 0..16 {
        let app = app.clone();
        requests.spawn(async move {
            let path = format!("/config/dial-{id}");
            assert_eq!(
                request(&app, "GET", &path, Value::Null).await.0,
                StatusCode::OK
            );
            let update =
                serde_json::json!({"sleep_battery": {"enabled": true, "timeout_sec": 100 + id}});
            assert_eq!(request(&app, "PUT", &path, update).await.0, StatusCode::OK);
        });
    }
    while let Some(result) = requests.join_next().await {
        result.unwrap();
    }
    let restarted = self::app().await;
    for id in 0..16 {
        let (_, restored) = request(
            &restarted,
            "GET",
            &format!("/config/dial-{id}"),
            Value::Null,
        )
        .await;
        assert_eq!(restored["config"]["sleep_battery"]["timeout_sec"], 100 + id);
    }
}

#[tokio::test]
#[serial_test::serial]
async fn routine_polls_do_not_rewrite_durable_settings() {
    let environment = ConfigEnvironment::new();
    let store = KnobStore::new();
    store.get_or_create("dial", None).await.unwrap();
    let before = std::fs::read(environment.file()).unwrap();
    store.get_or_create("dial", None).await.unwrap();
    store
        .update_status(
            "dial",
            knobs::store::KnobStatusUpdate {
                battery_level: Some(42),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(std::fs::read(environment.file()).unwrap(), before);
    assert_eq!(
        store.get("dial").await.unwrap().status.battery_level,
        Some(42)
    );
}

#[tokio::test]
#[serial_test::serial]
async fn moving_to_stable_requires_a_supported_device_and_the_exact_confirmed_release() {
    const RUNNING: &str = "2.8.3-alpha.1";
    const DEVICE: &str = "frame-test";
    const PATH: &str = "/config/frame-test";
    const MOVE: &str = "/knob/config?knob_id=frame-test";

    let environment = ConfigEnvironment::new();
    let app = app().await;
    let poll = |version: &str, supported| {
        let mut req = Request::builder()
            .uri(PATH)
            .header("x-device-type", "hiphi-frame")
            .header("x-knob-version", version);
        if supported {
            req = req.header("x-firmware-channel-switch", "1");
        }
        let app = app.clone();
        async move {
            let response = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            serde_json::from_slice::<Value>(
                &axum::body::to_bytes(response.into_body(), 64 * 1024)
                    .await
                    .unwrap(),
            )
            .unwrap()
        }
    };
    let command = |from, to| {
        serde_json::json!({
            "firmware_update": "move_to_stable",
            "from_version": from,
            "to_version": to
        })
    };
    let offered = || async {
        let (_, list) = request(&app, "GET", "/knob/devices", Value::Null).await;
        list["knobs"][0]["stable_version"].clone()
    };
    let stable = environment.directory.path().join("firmware/stable/frame");
    std::fs::create_dir_all(&stable).unwrap();
    let put_image = |version: &str, project: &str| {
        // ESP image header, first segment header and embedded app descriptor.
        let mut image = vec![0u8; 288];
        image[0] = 0xe9;
        image[1] = 1;
        image[3] = 0x40; // 16 MiB
        image[12..14].copy_from_slice(&9u16.to_le_bytes()); // ESP32-S3
        image[28..32].copy_from_slice(&256u32.to_le_bytes());
        image[32..36].copy_from_slice(&0xabcd5432u32.to_le_bytes());
        image[48..48 + version.len()].copy_from_slice(version.as_bytes());
        image[80..80 + project.len()].copy_from_slice(project.as_bytes());
        std::fs::write(stable.join("hiphi_frame.bin"), image).unwrap();
        std::fs::write(
            stable.join("version.json"),
            serde_json::json!({
                "version": version, "file": "hiphi_frame.bin"
            })
            .to_string(),
        )
        .unwrap();
    };
    const GATE: &str = "UHC_FRAME_OTA_VALIDATION_DEVICE";
    struct RestoreGate(Option<std::ffi::OsString>);
    impl Drop for RestoreGate {
        fn drop(&mut self) {
            match &self.0 {
                Some(value) => std::env::set_var(GATE, value),
                None => std::env::remove_var(GATE),
            }
        }
    }
    let _restore = RestoreGate(std::env::var_os(GATE));
    std::env::remove_var(GATE);
    put_image("2.8.3", "hiphi_frame");
    poll(RUNNING, true).await;
    assert!(
        offered().await.is_null(),
        "disabled by default before hardware validation"
    );

    std::env::set_var(GATE, DEVICE);
    poll(RUNNING, false).await;
    assert!(
        offered().await.is_null(),
        "no advertisement means no switch"
    );
    assert_eq!(
        request(&app, "POST", MOVE, command(RUNNING, "2.8.3"))
            .await
            .0,
        StatusCode::CONFLICT
    );

    // Devices can retain their config SHA on reboot and advertise through ordinary polling.
    let now_playing = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/now_playing?zone_id=roon:missing")
                .header("x-knob-id", DEVICE)
                .header("x-device-type", "hiphi-frame")
                .header("x-knob-version", RUNNING)
                .header("x-firmware-channel-switch", "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(now_playing.status(), StatusCode::NOT_FOUND); // no playback zone in fixture
    assert_eq!(offered().await, "2.8.3");

    std::fs::remove_file(stable.join("hiphi_frame.bin")).unwrap();
    assert!(
        offered().await.is_null(),
        "metadata alone does not make an offer"
    );
    assert_eq!(
        request(&app, "PUT", PATH, command(RUNNING, "2.8.3"))
            .await
            .0,
        StatusCode::CONFLICT
    );
    put_image("2.8.3", "hiphi_dial");
    assert!(
        offered().await.is_null(),
        "a renamed image for the wrong hardware is not offered"
    );
    assert_eq!(
        request(&app, "POST", MOVE, command(RUNNING, "2.8.3"))
            .await
            .0,
        StatusCode::CONFLICT
    );
    put_image("2.8.2", "hiphi_frame");
    assert!(
        offered().await.is_null(),
        "an older Stable version is not offered"
    );

    put_image("2.8.3", "hiphi_frame");
    assert_eq!(offered().await, "2.8.3");
    assert_eq!(
        request(
            &app,
            "POST",
            MOVE,
            serde_json::json!({"firmware_update":"install"})
        )
        .await
        .0,
        StatusCode::CONFLICT,
        "ordinary Update stays on Alpha"
    );
    assert_eq!(
        request(&app, "POST", MOVE, command("2.8.3-alpha.0", "2.8.3"))
            .await
            .0,
        StatusCode::CONFLICT,
        "stale source"
    );
    put_image("2.8.4", "hiphi_frame");
    assert_eq!(
        request(&app, "POST", MOVE, command(RUNNING, "2.8.3"))
            .await
            .0,
        StatusCode::CONFLICT,
        "the offer changed while confirmation was open"
    );
    assert_eq!(
        request(&app, "POST", MOVE, command(RUNNING, "2.8.4"))
            .await
            .0,
        StatusCode::OK
    );

    let config = poll(RUNNING, true).await;
    let pending = &config["config"]["firmware_update"];
    assert_eq!(pending["action"], "move_to_stable");
    assert_eq!(pending["device_id"], DEVICE);
    assert_eq!(pending["from_version"], RUNNING);
    assert_eq!(pending["to_version"], "2.8.4");
    put_image("2.8.5", "hiphi_frame");
    assert_eq!(
        poll(RUNNING, true).await["config"]["firmware_update"]["to_version"],
        "2.8.4",
        "a newer release must not rewrite an already queued request"
    );

    std::env::remove_var(GATE);
    assert!(
        poll(RUNNING, true).await["config"]["firmware_update"].is_null(),
        "withdrawing the validation gate cancels an outstanding test request"
    );
    std::env::set_var(GATE, DEVICE);
    assert_eq!(
        request(&app, "POST", MOVE, command(RUNNING, "2.8.5"))
            .await
            .0,
        StatusCode::OK
    );
    assert!(poll("2.8.5", true).await["config"]["firmware_update"].is_null());
    assert!(
        offered().await.is_null(),
        "Stable is now the running channel"
    );
    assert_eq!(
        request(
            &app,
            "POST",
            MOVE,
            serde_json::json!({"firmware_update":"install"})
        )
        .await
        .0,
        StatusCode::OK,
        "ordinary Update now uses Stable"
    );
}
