//! BluOS device family, end to end through `DeviceService` (#774, alpha).
//!
//! The device is `MockBluos`, an in-file **synthetic** BluOS HTTP/XML player shaped from the
//! pyblu parser expectations (see `src/adapters/bluos/protocol.rs`). It is not hardware
//! evidence: no BluOS player has been exercised. mDNS discovery is not exercised here (it needs
//! multicast); the probe it feeds is, through `attach_unit`.
//!
//! Tests named `critical_*` guard a physical failure mode: a level sent to the wrong player, a
//! level above the user's ceiling, a state-changing request sent on connect, a level invented for
//! a fixed-output player.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{StatusCode, Uri};
use axum::Router;
use tokio::net::TcpListener;

use unified_hifi_control::adapters::bluos::BluosAdapter;
use unified_hifi_control::adapters::devices::DeviceFamily;
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::bus::{create_bus, VolumeScale};
use unified_hifi_control::devices::links::DeviceLinkStore;
use unified_hifi_control::devices::service::{
    DeviceDriver, DeviceError, DeviceOperation, DeviceRequest, DeviceService, DeviceTarget,
};
use unified_hifi_control::devices::{CapabilityEvidence, ControlChannel, DeviceId, DevicePower};

// =============================================================================
// Synthetic BluOS player
// =============================================================================

struct MockState {
    mac: String,
    model_name: String,
    /// BluOS reports a negative volume for a fixed-output player.
    volume: i64,
    mute: bool,
    input_id: String,
    inputs: Vec<(&'static str, &'static str, &'static str)>,
    offline: bool,
    requests: Vec<String>,
}

impl MockState {
    fn new(mac: &str) -> Self {
        Self {
            mac: mac.to_string(),
            model_name: "PULSE 2i".to_string(),
            volume: 20,
            mute: false,
            input_id: "Spotify".to_string(),
            inputs: vec![
                (
                    "input0",
                    "Optical Input",
                    "Capture%3Ahw%3Aimxspdif%2Cbluez%2Cinput0",
                ),
                (
                    "input1",
                    "Analog Input",
                    "Capture%3Ahw%3Aimxspdif%2Cbluez%2Cinput1",
                ),
            ],
            offline: false,
            requests: Vec::new(),
        }
    }
}

#[derive(Clone)]
struct MockBluos {
    state: Arc<Mutex<MockState>>,
    port: u16,
}

impl MockBluos {
    async fn start(configure: impl FnOnce(&mut MockState)) -> Self {
        let mut initial = MockState::new("90:56:82:AA:BB:CC");
        configure(&mut initial);
        let state = Arc::new(Mutex::new(initial));
        let app = Router::new().fallback(handle).with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        Self { state, port }
    }

    fn with<R>(&self, f: impl FnOnce(&mut MockState) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }

    /// Requests that change state: everything but the three read endpoints.
    fn writes(&self) -> Vec<String> {
        self.with(|s| {
            s.requests
                .iter()
                .filter(|path| {
                    let route = path.split('?').next().unwrap_or("");
                    let reads = ["/Status", "/SyncStatus", "/RadioBrowse"];
                    let volume_read = route == "/Volume" && !path.contains('?');
                    !reads.contains(&route) && !volume_read
                })
                .cloned()
                .collect()
        })
    }
}

fn query(uri: &Uri, key: &str) -> Option<String> {
    url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.to_string())
}

fn volume_xml(s: &MockState) -> String {
    format!(
        r#"<volume db="-40.5" mute="{}" offsetDb="0">{}</volume>"#,
        u8::from(s.mute),
        s.volume
    )
}

async fn handle(State(state): State<Arc<Mutex<MockState>>>, uri: Uri) -> (StatusCode, String) {
    let mut s = state.lock().unwrap();
    s.requests.push(
        uri.path_and_query()
            .map(|p| p.as_str().to_string())
            .unwrap_or_default(),
    );
    if s.offline {
        return (StatusCode::SERVICE_UNAVAILABLE, String::new());
    }
    let body = match uri.path() {
        "/SyncStatus" => format!(
            r#"<SyncStatus icon="/i.png" volume="{}" modelName="{}" name="Living" model="P300"
                brand="Bluesound" etag="e1" mac="{}" id="192.168.1.30:11000" db="-40.5"
                initialized="true"/>"#,
            s.volume, s.model_name, s.mac
        ),
        "/Status" => format!(
            r#"<status etag="e2"><db>-40.5</db><inputId>{}</inputId><mute>{}</mute>
               <service>Capture</service><state>stream</state><volume>{}</volume></status>"#,
            s.input_id,
            u8::from(s.mute && s.volume >= 0),
            s.volume
        ),
        "/RadioBrowse" => {
            let items: String = s
                .inputs
                .iter()
                .map(|(id, text, url)| {
                    format!(
                        r#"<item id="{id}" text="{text}" image="/x.png" type="audio" URL="{url}"/>"#
                    )
                })
                .collect();
            format!(r#"<radiotime service="Capture">{items}</radiotime>"#)
        }
        "/Volume" => {
            if s.volume >= 0 {
                if let Some(level) = query(&uri, "level").and_then(|v| v.parse::<i64>().ok()) {
                    if (0..=100).contains(&level) {
                        s.volume = level;
                    }
                }
                if let Some(mute) = query(&uri, "mute") {
                    s.mute = mute == "1";
                }
            }
            volume_xml(&s)
        }
        "/Play" => {
            let wanted = query(&uri, "url").unwrap_or_default();
            let found = s
                .inputs
                .iter()
                .find(|(_, _, url)| {
                    urlencoding::decode(url)
                        .map(|u| u == wanted)
                        .unwrap_or(false)
                })
                .map(|(id, _, _)| id.to_string());
            if let Some(id) = found {
                s.input_id = id;
            }
            "<state>stream</state>".to_string()
        }
        _ => return (StatusCode::NOT_FOUND, String::new()),
    };
    (StatusCode::OK, body)
}

// =============================================================================
// Harness
// =============================================================================

struct Harness {
    aggregator: Arc<ZoneAggregator>,
    devices: Arc<DeviceService>,
    adapter: Arc<BluosAdapter>,
    _dir: tempfile::TempDir,
}

async fn harness() -> Harness {
    let bus = create_bus();
    let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    {
        let aggregator = aggregator.clone();
        tokio::spawn(async move { aggregator.run_with_ready(ready_tx).await });
    }
    ready_rx.await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let devices = Arc::new(
        DeviceService::new(
            aggregator.clone(),
            bus.clone(),
            DeviceLinkStore::open(dir.path().join("device-links.json")),
        )
        .with_confirm_timeout(Duration::from_millis(3000)),
    );
    let adapter = Arc::new(BluosAdapter::new(bus));
    devices.register_driver(adapter.clone()).await;
    Harness {
        aggregator,
        devices,
        adapter,
        _dir: dir,
    }
}

impl Harness {
    async fn attach(&self, mock: &MockBluos) -> DeviceId {
        let id = self
            .adapter
            .attach_unit("127.0.0.1", mock.port)
            .await
            .expect("the mock answers /SyncStatus");
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.aggregator.get_device(&id).await.is_none() {
            assert!(Instant::now() < deadline, "{id} never observed");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        id
    }

    async fn control(
        &self,
        id: &DeviceId,
        request: DeviceRequest,
    ) -> Result<unified_hifi_control::devices::service::DeviceCommandReport, DeviceError> {
        self.devices
            .control(DeviceTarget::Device(id.clone()), request)
            .await
    }
}

// =============================================================================
// Observation
// =============================================================================

#[tokio::test]
async fn observed_with_mac_identity_and_percentage_scale() {
    let h = harness().await;
    let mock = MockBluos::start(|_| {}).await;
    let id = h.attach(&mock).await;
    assert_eq!(id, DeviceId::new("bluos", "90:56:82:AA:BB:CC").unwrap());
    assert_eq!(id.as_str(), "bluos:905682AABBCC");

    let device = h.aggregator.get_device(&id).await.unwrap();
    assert_eq!(device.vendor, "Bluesound");
    assert_eq!(device.model, "PULSE 2i");
    assert_eq!(device.name, "Living");
    assert_eq!(device.maturity, "alpha");
    assert_eq!(device.connection.control_channel, ControlChannel::Polling);
    // BluOS has no universal power state: never rounded to standby.
    assert_eq!(device.power, DevicePower::Unknown);
    assert_eq!(device.volume.scale, VolumeScale::Percentage);
    assert_eq!(device.volume.value, Some(20.0));
    assert_eq!(
        (
            device.volume.min,
            device.volume.hardware_max,
            device.volume.step
        ),
        (0.0, Some(100.0), 1.0)
    );
    assert_eq!(device.volume.is_muted, Some(false));
    // On a streaming service, not on a capture input.
    assert_eq!(device.input, None);
    let inputs: Vec<_> = device
        .inputs
        .iter()
        .map(|i| (i.id.as_str(), i.name.as_str()))
        .collect();
    assert_eq!(
        inputs,
        [("input0", "Optical Input"), ("input1", "Analog Input")]
    );

    let c = &device.capabilities;
    assert!(
        c.observe.supported && c.volume.supported && c.mute.supported && c.input_select.supported
    );
    assert!(!c.power.supported);
    for claim in [&c.observe, &c.volume, &c.mute, &c.power, &c.input_select] {
        assert_eq!(claim.evidence, CapabilityEvidence::ImplementationDerived);
    }
}

#[tokio::test]
async fn a_player_without_a_mac_is_not_accepted_and_nothing_is_invented_from_its_address() {
    let h = harness().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .fallback(|| async { r#"<SyncStatus id="192.168.1.30:11000" name="x"/>"# }),
        )
        .await
        .ok();
    });
    assert!(h.adapter.attach_unit("127.0.0.1", port).await.is_err());
    assert_eq!(h.adapter.device_count().await, 0);
}

#[tokio::test]
async fn the_current_capture_input_is_reported_by_its_catalog_id() {
    let h = harness().await;
    let mock = MockBluos::start(|s| s.input_id = "input1".to_string()).await;
    let id = h.attach(&mock).await;
    let device = h.aggregator.get_device(&id).await.unwrap();
    assert_eq!(device.input.as_deref(), Some("input1"));
    assert_eq!(device.input_name(), Some("Analog Input"));
}

// =============================================================================
// Fixed output
// =============================================================================

/// CRITICAL: a fixed-output player has no volume. No level is published or sent for it.
#[tokio::test]
async fn critical_fixed_output_has_no_volume_and_refuses_writes() {
    let h = harness().await;
    let mock = MockBluos::start(|s| s.volume = -1).await;
    let id = h.attach(&mock).await;
    let device = h.aggregator.get_device(&id).await.unwrap();
    assert!(!device.capabilities.volume.supported);
    assert!(!device.capabilities.mute.supported);
    assert!(device.capabilities.volume.detail.contains("fixed-level"));
    assert_eq!(device.volume.value, None);
    assert_eq!(device.volume.is_muted, None);
    assert!(
        device.volume_control().is_none(),
        "no volume_control may be published"
    );

    for operation in [
        DeviceOperation::SetVolume(30.0),
        DeviceOperation::SetMute(true),
    ] {
        assert!(
            h.adapter.execute(&id, operation.clone()).await.is_err(),
            "{operation:?} must be refused"
        );
    }
    assert!(matches!(
        h.control(&id, DeviceRequest::VolumeAbsolute(30.0)).await,
        Err(DeviceError::RangeUnknown { .. })
    ));
    assert!(mock.writes().is_empty(), "sent {:?}", mock.writes());
}

// =============================================================================
// Safety
// =============================================================================

/// CRITICAL: connecting only *asks*.
#[tokio::test]
async fn critical_no_state_changing_request_on_connect() {
    let h = harness().await;
    let mock = MockBluos::start(|_| {}).await;
    h.attach(&mock).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(mock.writes().is_empty(), "sent {:?}", mock.writes());
    let requests = mock.with(|s| s.requests.clone());
    for expected in ["/SyncStatus", "/Status", "/RadioBrowse"] {
        assert!(
            requests.iter().any(|r| r.starts_with(expected)),
            "{expected}: {requests:?}"
        );
    }
}

/// CRITICAL: the ceiling bounds the level (in percent) and the write is confirmed by readback.
#[tokio::test]
async fn critical_ceiling_clamps_a_write_and_readback_confirms_it() {
    let h = harness().await;
    let mock = MockBluos::start(|_| {}).await;
    let id = h.attach(&mock).await;
    h.devices
        .set_ceiling(id.as_str(), Some(40.0))
        .await
        .unwrap();

    let report = h
        .control(&id, DeviceRequest::VolumeAbsolute(95.0))
        .await
        .expect("confirmed");
    assert_eq!(report.sent_volume, Some(40.0));
    assert!(report.clamped);
    assert_eq!(report.observed.volume.value, Some(40.0));
    assert_eq!(mock.with(|s| s.volume), 40);
    assert_eq!(mock.writes(), ["/Volume?level=40"]);

    h.control(&id, DeviceRequest::VolumeRelative(30.0))
        .await
        .expect("confirmed");
    assert_eq!(mock.with(|s| s.volume), 40);
}

/// CRITICAL: two players of one model never cross-route; each is addressed by its MAC.
#[tokio::test]
async fn critical_two_units_of_one_model_never_cross_route() {
    let h = harness().await;
    let a = MockBluos::start(|s| s.mac = "90:56:82:00:00:01".to_string()).await;
    let b = MockBluos::start(|s| s.mac = "90:56:82:00:00:02".to_string()).await;
    let id_a = h.attach(&a).await;
    let id_b = h.attach(&b).await;
    assert_ne!(id_a, id_b);

    h.control(&id_a, DeviceRequest::VolumeAbsolute(55.0))
        .await
        .expect("confirmed");
    h.control(&id_a, DeviceRequest::Mute(true))
        .await
        .expect("confirmed");

    assert_eq!(a.with(|s| (s.volume, s.mute)), (55, true));
    assert!(b.writes().is_empty(), "player B received {:?}", b.writes());
    assert_eq!(b.with(|s| (s.volume, s.mute)), (20, false));
}

/// CRITICAL: an address that now answers with another MAC never receives a write.
#[tokio::test]
async fn critical_a_reassigned_address_is_refused_before_any_write() {
    let h = harness().await;
    let mock = MockBluos::start(|_| {}).await;
    let id = h.attach(&mock).await;
    mock.with(|s| s.mac = "90:56:82:FF:FF:FF".to_string());

    let error = h
        .control(&id, DeviceRequest::VolumeAbsolute(10.0))
        .await
        .expect_err("must refuse");
    assert!(matches!(error, DeviceError::Dispatch { .. }), "{error:?}");
    assert!(mock.writes().is_empty(), "{:?}", mock.writes());
    assert_eq!(mock.with(|s| s.volume), 20);
}

// =============================================================================
// Operations
// =============================================================================

#[tokio::test]
async fn volume_mute_and_input_are_confirmed_by_readback() {
    let h = harness().await;
    let mock = MockBluos::start(|_| {}).await;
    let id = h.attach(&mock).await;

    let report = h
        .control(&id, DeviceRequest::VolumeAbsolute(33.0))
        .await
        .expect("confirmed");
    assert_eq!(report.observed.volume.value, Some(33.0));

    let report = h
        .control(&id, DeviceRequest::Mute(true))
        .await
        .expect("confirmed");
    assert_eq!(report.observed.volume.is_muted, Some(true));
    let report = h
        .control(&id, DeviceRequest::MuteToggle)
        .await
        .expect("confirmed");
    assert_eq!(report.observed.volume.is_muted, Some(false));

    let report = h
        .control(&id, DeviceRequest::SelectInput("optical input".into()))
        .await
        .expect("confirmed");
    assert_eq!(report.observed.input.as_deref(), Some("input0"));
    assert_eq!(
        mock.writes().last().map(String::as_str),
        Some("/Play?url=Capture%3Ahw%3Aimxspdif%2Cbluez%2Cinput0")
    );
}

#[tokio::test]
async fn a_noop_volume_write_confirms_without_timeout() {
    let h = harness().await;
    let mock = MockBluos::start(|_| {}).await;
    let id = h.attach(&mock).await;
    let started = Instant::now();
    h.control(&id, DeviceRequest::VolumeAbsolute(20.0))
        .await
        .expect("confirmed");
    assert!(started.elapsed() < Duration::from_secs(2));
}

/// Power is not universal on BluOS: it errors rather than succeeding, and sends nothing.
#[tokio::test]
async fn power_requests_error_and_send_nothing() {
    let h = harness().await;
    let mock = MockBluos::start(|_| {}).await;
    let id = h.attach(&mock).await;
    for request in [DeviceRequest::PowerOn, DeviceRequest::PowerOff] {
        match h.control(&id, request).await {
            Err(DeviceError::InvalidValue {
                parameter: "action",
                detail,
            }) => {
                assert!(detail.contains("not supported"), "{detail}")
            }
            other => panic!("expected capability admission refusal, got {other:?}"),
        }
    }
    // Power toggle has no direction when power is unknown.
    assert!(h.control(&id, DeviceRequest::PowerToggle).await.is_err());
    assert!(h
        .adapter
        .execute(&id, DeviceOperation::PowerOn)
        .await
        .is_err());
    assert!(mock.writes().is_empty(), "sent {:?}", mock.writes());
}

#[tokio::test]
async fn an_input_the_player_never_reported_is_refused_before_it_is_sent() {
    let h = harness().await;
    let mock = MockBluos::start(|_| {}).await;
    let id = h.attach(&mock).await;
    assert!(h
        .adapter
        .execute(
            &id,
            DeviceOperation::SelectInput("Capture:x&level=100".to_string())
        )
        .await
        .is_err());
    assert!(matches!(
        h.control(&id, DeviceRequest::SelectInput("phono".into()))
            .await,
        Err(DeviceError::UnknownInput { .. })
    ));
    assert!(mock.writes().is_empty());
}

#[tokio::test]
async fn an_unreachable_player_reports_no_channel() {
    let h = harness().await;
    let mock = MockBluos::start(|_| {}).await;
    let id = h.attach(&mock).await;
    mock.with(|s| s.offline = true);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let device = h.aggregator.get_device(&id).await.unwrap();
        if device.connection.control_channel == ControlChannel::None {
            assert_eq!(device.power, DevicePower::Unknown);
            assert_eq!(device.volume.value, None);
            break;
        }
        assert!(Instant::now() < deadline, "the outage was never observed");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn the_family_is_named_bluos_and_serves_the_bluos_kind() {
    let adapter = BluosAdapter::new(create_bus());
    assert_eq!(adapter.family(), "bluos");
    assert_eq!(adapter.kinds(), &["bluos"]);
}
