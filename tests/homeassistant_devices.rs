//! Home Assistant media_player entities as devices (#790, alpha), end to end through
//! `DeviceService`, against a mock Home Assistant (REST + WebSocket registry). Synthetic: shaped
//! from the Home Assistant developer docs, not from a live instance.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message;

use unified_hifi_control::adapters::devices::{DeviceFamily, FamilyContext};
use unified_hifi_control::adapters::homeassistant_devices::HomeAssistantDevices;
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::bus::create_bus;
use unified_hifi_control::devices::links::DeviceLinkStore;
use unified_hifi_control::devices::service::{DeviceRequest, DeviceService, DeviceTarget};
use unified_hifi_control::devices::{DeviceId, DevicePower};

#[derive(Default)]
struct MockHa {
    /// entity_id → state object
    states: Vec<Value>,
    calls: Vec<(String, Value)>,
}

type Shared = Arc<Mutex<MockHa>>;

fn amp_state(volume: f64, on: bool) -> Value {
    json!({
        "entity_id": "media_player.marantz",
        "state": if on { "on" } else { "off" },
        "attributes": {
            "friendly_name": "Marantz",
            "volume_level": volume,
            "is_volume_muted": false,
            "source": "TV Audio",
            "source_list": ["CD", "TV Audio"],
            "supported_features": 4 | 8 | 128 | 256 | 2048
        }
    })
}

async fn start_mock(registry_ok: bool) -> (Shared, String, String) {
    let shared: Shared = Arc::new(Mutex::new(MockHa {
        states: vec![
            amp_state(0.4, true),
            // UHC's own export: must never be offered.
            json!({"entity_id": "media_player.living_room", "state": "playing",
                   "attributes": {"friendly_name": "Living Room", "volume_level": 0.5,
                                  "supported_features": 4}}),
            json!({"entity_id": "light.kitchen", "state": "on", "attributes": {}}),
        ],
        calls: Vec::new(),
    }));

    async fn states(State(shared): State<Shared>) -> Json<Value> {
        Json(Value::Array(shared.lock().await.states.clone()))
    }
    async fn service(
        State(shared): State<Shared>,
        Path(service): Path<String>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        let mut ha = shared.lock().await;
        ha.calls.push((service.clone(), body.clone()));
        if let Some(entity) = ha
            .states
            .iter_mut()
            .find(|s| s["entity_id"] == body["entity_id"])
        {
            match service.as_str() {
                "volume_set" => entity["attributes"]["volume_level"] = body["volume_level"].clone(),
                "volume_mute" => {
                    entity["attributes"]["is_volume_muted"] = body["is_volume_muted"].clone()
                }
                "turn_on" => entity["state"] = json!("on"),
                "turn_off" => entity["state"] = json!("off"),
                "select_source" => entity["attributes"]["source"] = body["source"].clone(),
                _ => {}
            }
        }
        Json(json!([]))
    }
    let app = Router::new()
        .route("/api/states", get(states))
        .route("/api/services/media_player/{service}", post(service))
        .with_state(shared.clone());
    let rest = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest_addr = rest.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(rest, app).await.unwrap() });

    let ws = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_addr = ws.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = ws.accept().await {
            tokio::spawn(async move {
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                let send = |v: Value| Message::Text(v.to_string().into());
                socket
                    .send(send(json!({"type": "auth_required"})))
                    .await
                    .unwrap();
                let _auth = socket.next().await;
                socket.send(send(json!({"type": "auth_ok"}))).await.unwrap();
                let _request = socket.next().await;
                let result = if registry_ok {
                    json!({"id": 1, "type": "result", "success": true, "result": [
                        {"entity_id": "media_player.marantz", "platform": "denonavr", "unique_id": "0006781E0AB8"},
                        {"entity_id": "media_player.living_room", "platform": "unified_hifi_control", "unique_id": "roon:living"}
                    ]})
                } else {
                    json!({"id": 1, "type": "result", "success": false, "error": {"code": "unauthorized"}})
                };
                socket.send(send(result)).await.unwrap();
            });
        }
    });
    (
        shared,
        format!("http://{rest_addr}/api"),
        format!("ws://{ws_addr}/api/websocket"),
    )
}

struct Harness {
    service: Arc<DeviceService>,
    aggregator: Arc<ZoneAggregator>,
    _dir: tempfile::TempDir,
}

async fn harness(registry_ok: bool) -> (Harness, Shared) {
    let (shared, rest, ws) = start_mock(registry_ok).await;
    let bus = create_bus();
    let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    {
        let aggregator = aggregator.clone();
        tokio::spawn(async move { aggregator.run_with_ready(ready_tx).await });
    }
    ready_rx.await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let service = Arc::new(
        DeviceService::new(
            aggregator.clone(),
            bus.clone(),
            DeviceLinkStore::open(dir.path().join("device-links.json")),
        )
        .with_confirm_timeout(Duration::from_secs(3)),
    );
    let family = Arc::new(HomeAssistantDevices::with_endpoint(
        bus,
        Some((rest, ws, "token".to_string())),
    ));
    service.register_driver(family.clone()).await;
    family.start_family(FamilyContext::default()).await.unwrap();
    (
        Harness {
            service,
            aggregator,
            _dir: dir,
        },
        shared,
    )
}

fn amp_id() -> DeviceId {
    DeviceId::new("ha", "0006781E0AB8").unwrap()
}

async fn wait_for_device(aggregator: &ZoneAggregator) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while aggregator.get_device(&amp_id()).await.is_none() {
        assert!(Instant::now() < deadline, "HA device never observed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn media_players_are_offered_with_a_linear_zero_to_one_scale() {
    let (h, _) = harness(true).await;
    wait_for_device(&h.aggregator).await;
    let device = h.aggregator.get_device(&amp_id()).await.unwrap();
    assert_eq!(device.maturity, "alpha");
    assert_eq!(device.power, DevicePower::On);
    assert_eq!(device.volume.value, Some(0.4));
    assert_eq!(device.volume.hardware_max, Some(1.0));
    assert_eq!(device.input.as_deref(), Some("TV Audio"));
    // Only the real amplifier: UHC's own export and non-media_player entities are absent.
    assert_eq!(h.aggregator.get_devices().await.len(), 1);
}

/// CRITICAL: an unreadable registry offers nothing — it never falls back to "everything", which
/// would put UHC's own reflected zones on the list.
#[tokio::test]
async fn critical_unreadable_registry_offers_no_devices() {
    let (h, _) = harness(false).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(h.aggregator.get_devices().await.is_empty());
}

/// Volume works without a ceiling (bounded by 0–1); an optional ceiling lowers the bound.
#[tokio::test]
async fn critical_volume_works_without_a_ceiling_and_a_ceiling_bounds_it() {
    let (h, shared) = harness(true).await;
    wait_for_device(&h.aggregator).await;

    let report = h
        .service
        .control(
            DeviceTarget::Device(amp_id()),
            DeviceRequest::VolumeAbsolute(0.6),
        )
        .await
        .expect("no ceiling needed");
    assert_eq!(report.sent_volume, Some(0.6));
    shared.lock().await.calls.clear();

    h.service
        .set_ceiling(amp_id().as_str(), Some(0.5))
        .await
        .unwrap();
    let report = h
        .service
        .control(
            DeviceTarget::Device(amp_id()),
            DeviceRequest::VolumeAbsolute(0.9),
        )
        .await
        .expect("confirmed by readback");
    assert_eq!(report.sent_volume, Some(0.5));
    let calls = shared.lock().await.calls.clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "volume_set");
    assert!(calls[0].1["volume_level"].as_f64().unwrap() <= 0.5);
}

#[tokio::test]
async fn power_and_source_are_confirmed_by_readback() {
    let (h, _) = harness(true).await;
    wait_for_device(&h.aggregator).await;
    h.service
        .control(DeviceTarget::Device(amp_id()), DeviceRequest::PowerOff)
        .await
        .expect("power off confirmed");
    assert_eq!(
        h.aggregator.get_device(&amp_id()).await.unwrap().power,
        DevicePower::Standby
    );
    h.service
        .control(
            DeviceTarget::Device(amp_id()),
            DeviceRequest::SelectInput("CD".to_string()),
        )
        .await
        .expect("source confirmed");
    assert_eq!(
        h.aggregator
            .get_device(&amp_id())
            .await
            .unwrap()
            .input
            .as_deref(),
        Some("CD")
    );
}
