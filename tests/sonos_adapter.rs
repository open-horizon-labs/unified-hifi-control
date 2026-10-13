//! Sonos device adapter, end to end through `DeviceService` (#774, alpha).
//!
//! `MockSonos` is a **synthetic** Sonos player on `127.0.0.1:0`: `device_description.xml` plus the
//! `RenderingControl` and `AVTransport` SOAP endpoints, shaped from the UPnP AV service templates
//! and the SoCo / node-sonos request formats. No Sonos hardware exists for these tests. Two mocks
//! of the same model run side by side so a test that routes by model, discovery order or "the
//! only Sonos" fails.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::Router;
use tokio::net::TcpListener;

use unified_hifi_control::adapters::devices::DeviceFamily;
use unified_hifi_control::adapters::sonos::protocol::{parse_description, Description};
use unified_hifi_control::adapters::sonos::SonosAdapter;
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::bus::{create_bus, VolumeScale};
use unified_hifi_control::devices::links::DeviceLinkStore;
use unified_hifi_control::devices::service::{
    DeviceCommandReport, DeviceDriver, DeviceError, DeviceOperation, DeviceRequest, DeviceService,
    DeviceTarget,
};
use unified_hifi_control::devices::{Device, DeviceId, DevicePower};

const MAC_LIVING: &str = "5CAAFD123456";
const MAC_DEN: &str = "5CAAFD654321";

#[derive(Debug, Default)]
struct MockState {
    mac: String,
    model: String,
    room: String,
    vol: u32,
    mute: bool,
    fixed: bool,
    track_uri: String,
    /// State-changing SOAP calls only, as `Action:arg`.
    writes: Vec<String>,
    /// Every SOAP action received.
    actions: Vec<String>,
}

fn tag(body: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let start = body.find(&open)? + open.len();
    let end = body[start..].find(&format!("</{name}>"))? + start;
    Some(body[start..end].to_string())
}

fn response(action: &str, inner: &str) -> String {
    format!(
        "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
         <u:{action}Response xmlns:u=\"urn:schemas-upnp-org:service:RenderingControl:1\">{inner}\
         </u:{action}Response></s:Body></s:Envelope>"
    )
}

async fn soap(
    State(state): State<Arc<Mutex<MockState>>>,
    headers: HeaderMap,
    body: String,
) -> (StatusCode, String) {
    let header = headers
        .get("SOAPACTION")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .trim_matches('"')
        .to_string();
    let action = header.rsplit('#').next().unwrap_or_default().to_string();
    let mut s = state.lock().unwrap();
    s.actions.push(action.clone());
    let ok = |inner: String| (StatusCode::OK, response(&action, &inner));
    match action.as_str() {
        "GetVolume" => ok(format!("<CurrentVolume>{}</CurrentVolume>", s.vol)),
        "GetMute" => ok(format!("<CurrentMute>{}</CurrentMute>", u8::from(s.mute))),
        "GetOutputFixed" => ok(format!(
            "<CurrentFixed>{}</CurrentFixed>",
            u8::from(s.fixed)
        )),
        "GetPositionInfo" => ok(format!("<TrackURI>{}</TrackURI>", s.track_uri)),
        "SetVolume" => match tag(&body, "DesiredVolume").and_then(|v| v.parse::<u32>().ok()) {
            Some(v) if tag(&body, "Channel").as_deref() == Some("Master") => {
                s.writes.push(format!("SetVolume:{v}"));
                s.vol = v.min(100);
                ok(String::new())
            }
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "<errorCode>402</errorCode>".into(),
            ),
        },
        "SetMute" => {
            let muted = tag(&body, "DesiredMute").as_deref() == Some("1");
            s.writes.push(format!("SetMute:{}", u8::from(muted)));
            s.mute = muted;
            ok(String::new())
        }
        "SetAVTransportURI" => {
            let uri = tag(&body, "CurrentURI").unwrap_or_default();
            s.writes.push(format!("SetAVTransportURI:{uri}"));
            s.track_uri = uri;
            ok(String::new())
        }
        "Play" => {
            s.writes.push("Play".to_string());
            ok(String::new())
        }
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "<errorCode>401</errorCode>".into(),
        ),
    }
}

async fn description(State(state): State<Arc<Mutex<MockState>>>) -> String {
    let s = state.lock().unwrap();
    format!(
        "<?xml version=\"1.0\"?><root xmlns=\"urn:schemas-upnp-org:device-1-0\"><device>\
         <deviceType>urn:schemas-upnp-org:device:ZonePlayer:1</deviceType>\
         <friendlyName>127.0.0.1 - {model}</friendlyName>\
         <manufacturer>Sonos, Inc.</manufacturer><modelName>{model}</modelName>\
         <serialNum>{dashed}:5</serialNum><UDN>uuid:RINCON_{mac}01400</UDN>\
         <roomName>{room}</roomName></device></root>",
        model = s.model,
        room = s.room,
        mac = s.mac,
        dashed = s
            .mac
            .as_bytes()
            .chunks(2)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join("-"),
    )
}

struct MockSonos {
    state: Arc<Mutex<MockState>>,
    port: u16,
}

impl MockSonos {
    async fn start(mac: &str, model: &str, room: &str, vol: u32) -> Self {
        let state = Arc::new(Mutex::new(MockState {
            mac: mac.to_string(),
            model: model.to_string(),
            room: room.to_string(),
            vol,
            track_uri: "x-sonos-spotify:spotify%3atrack%3aabc".to_string(),
            ..Default::default()
        }));
        let app = Router::new()
            .route("/xml/device_description.xml", get(description))
            .route("/MediaRenderer/RenderingControl/Control", post(soap))
            .route("/MediaRenderer/AVTransport/Control", post(soap))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { state, port }
    }

    fn writes(&self) -> Vec<String> {
        self.state.lock().unwrap().writes.clone()
    }

    fn vol(&self) -> u32 {
        self.state.lock().unwrap().vol
    }

    fn set_fixed(&self, fixed: bool) {
        self.state.lock().unwrap().fixed = fixed;
    }

    /// The description exactly as the mock serves it, parsed by the adapter's own parser.
    fn description(&self) -> Description {
        let s = self.state.lock().unwrap();
        let dashed = s
            .mac
            .as_bytes()
            .chunks(2)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join("-");
        parse_description(&format!(
            "<manufacturer>Sonos, Inc.</manufacturer><modelName>{}</modelName>\
             <serialNum>{dashed}:5</serialNum><UDN>uuid:RINCON_{}01400</UDN>\
             <roomName>{}</roomName>",
            s.model, s.mac, s.room
        ))
        .unwrap()
    }
}

struct Harness {
    service: Arc<DeviceService>,
    aggregator: Arc<ZoneAggregator>,
    adapter: Arc<SonosAdapter>,
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
    let service = Arc::new(
        DeviceService::new(
            aggregator.clone(),
            bus.clone(),
            DeviceLinkStore::open(dir.path().join("device-links.json")),
        )
        .with_confirm_timeout(Duration::from_millis(2500)),
    );
    let adapter = Arc::new(SonosAdapter::new(bus));
    service.register_driver(adapter.clone()).await;
    Harness {
        service,
        aggregator,
        adapter,
        _dir: dir,
    }
}

fn id(mac: &str) -> DeviceId {
    DeviceId::new("sonos", mac).unwrap()
}

impl Harness {
    async fn attach(&self, mac: &str, mock: &MockSonos) -> DeviceId {
        let device_id = self
            .adapter
            .attach_unit("127.0.0.1", mock.port, mock.description())
            .await
            .expect("attached");
        assert_eq!(device_id, id(mac));
        self.wait_device(&device_id, |d| {
            d.volume.value.is_some() || d.volume.is_muted.is_some()
        })
        .await;
        device_id
    }

    async fn wait_device(
        &self,
        device_id: &DeviceId,
        condition: impl Fn(&Device) -> bool,
    ) -> Device {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(device) = self.aggregator.get_device(device_id).await {
                if condition(&device) {
                    return device;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("condition not met within 5s");
    }

    async fn control(
        &self,
        device_id: &DeviceId,
        request: DeviceRequest,
    ) -> Result<DeviceCommandReport, DeviceError> {
        self.service
            .control(DeviceTarget::Device(device_id.clone()), request)
            .await
    }

    async fn ceiling(&self, device_id: &DeviceId, ceiling: f32) {
        self.service
            .set_ceiling(device_id.as_str(), Some(ceiling))
            .await
            .unwrap();
        self.wait_device(device_id, |d| d.volume.effective_max() == Some(ceiling))
            .await;
    }
}

#[tokio::test]
async fn observed_with_rincon_mac_identity_percentage_scale_and_no_power() {
    let h = harness().await;
    let mock = MockSonos::start(MAC_LIVING, "Sonos Beam", "Living Room", 18).await;
    let device_id = h.attach(MAC_LIVING, &mock).await;

    let device = h.aggregator.get_device(&device_id).await.unwrap();
    assert_eq!(device.device_id.as_str(), format!("sonos:{MAC_LIVING}"));
    assert_eq!(device.vendor, "Sonos");
    assert_eq!(device.model, "Beam");
    assert_eq!(device.name, "Living Room");
    assert_eq!(device.volume.scale, VolumeScale::Percentage);
    assert_eq!(device.volume.value, Some(18.0));
    assert_eq!(device.volume.min, 0.0);
    assert_eq!(device.volume.step, 1.0);
    assert_eq!(device.volume.hardware_max, Some(100.0));
    assert_eq!(device.volume.is_muted, Some(false));
    assert_eq!(device.power, DevicePower::Unknown);
    assert!(!device.capabilities.power.supported);
    // A Beam has a TV input and no line-in; streaming content is not an input.
    assert_eq!(
        device
            .inputs
            .iter()
            .map(|i| i.id.as_str())
            .collect::<Vec<_>>(),
        vec!["tv"]
    );
    assert_eq!(device.input, None);
    assert!(device.capabilities.input_select.supported);
}

/// CRITICAL: connecting only asks.
#[tokio::test]
async fn critical_no_state_changing_call_on_connect() {
    let h = harness().await;
    let mock = MockSonos::start(MAC_LIVING, "Sonos Beam", "Living Room", 18).await;
    h.attach(MAC_LIVING, &mock).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        mock.writes().is_empty(),
        "connect wrote {:?}",
        mock.writes()
    );
    let actions = mock.state.lock().unwrap().actions.clone();
    assert!(actions.iter().any(|a| a == "GetVolume"));
    assert!(actions.iter().all(|a| a.starts_with("Get")), "{actions:?}");
}

/// CRITICAL: with a ceiling, nothing above it reaches the player, and the write is confirmed by a fresh readback.
#[tokio::test]
async fn critical_ceiling_bounds_volume_and_write_is_confirmed_by_readback() {
    let h = harness().await;
    let mock = MockSonos::start(MAC_LIVING, "Sonos Beam", "Living Room", 10).await;
    let device_id = h.attach(MAC_LIVING, &mock).await;

    // The protocol's 0–100 is the hardware maximum; the ceiling below lowers it.

    h.ceiling(&device_id, 30.0).await;
    let report = h
        .control(&device_id, DeviceRequest::VolumeAbsolute(85.0))
        .await
        .expect("confirmed");
    assert!(report.clamped);
    assert_eq!(report.sent_volume, Some(30.0));
    assert_eq!(report.observed.volume.value, Some(30.0));
    assert_eq!(mock.writes(), vec!["SetVolume:30".to_string()]);

    h.control(&device_id, DeviceRequest::VolumeRelative(25.0))
        .await
        .expect("confirmed");
    assert!(
        mock.vol() <= 30,
        "level above the ceiling reached the player: {}",
        mock.vol()
    );
}

#[tokio::test]
async fn mute_is_written_and_read_back() {
    let h = harness().await;
    let mock = MockSonos::start(MAC_LIVING, "Sonos Beam", "Living Room", 10).await;
    let device_id = h.attach(MAC_LIVING, &mock).await;
    let report = h
        .control(&device_id, DeviceRequest::Mute(true))
        .await
        .expect("confirmed");
    assert_eq!(report.observed.volume.is_muted, Some(true));
    assert_eq!(mock.writes(), vec!["SetMute:1".to_string()]);
}

#[tokio::test]
async fn tv_input_selects_the_htastream_uri_then_plays_and_reads_back() {
    let h = harness().await;
    let mock = MockSonos::start(MAC_LIVING, "Sonos Beam", "Living Room", 10).await;
    let device_id = h.attach(MAC_LIVING, &mock).await;
    let report = h
        .control(&device_id, DeviceRequest::SelectInput("tv".into()))
        .await
        .expect("confirmed");
    assert_eq!(report.observed.input.as_deref(), Some("tv"));
    assert_eq!(
        mock.writes(),
        vec![
            format!("SetAVTransportURI:x-sonos-htastream:RINCON_{MAC_LIVING}01400:spdif"),
            "Play".to_string()
        ]
    );
    // A Beam has no line-in: refused before anything is sent.
    let before = mock.writes().len();
    assert!(h
        .control(&device_id, DeviceRequest::SelectInput("line-in".into()))
        .await
        .is_err());
    assert_eq!(mock.writes().len(), before);
}

/// A model with no known input has an empty catalog and refuses input selection.
#[tokio::test]
async fn model_without_inputs_offers_none_and_refuses_selection() {
    let h = harness().await;
    let mock = MockSonos::start(MAC_LIVING, "Sonos One", "Bedroom", 10).await;
    let device_id = h.attach(MAC_LIVING, &mock).await;
    let device = h.aggregator.get_device(&device_id).await.unwrap();
    assert!(device.inputs.is_empty());
    assert!(!device.capabilities.input_select.supported);
    let error = h
        .adapter
        .execute(&device_id, DeviceOperation::SelectInput("tv".into()))
        .await
        .unwrap_err();
    assert!(error.contains("not available"), "{error}");
    assert!(mock.writes().is_empty());
}

#[tokio::test]
async fn fixed_output_reports_no_level_and_refuses_volume() {
    let h = harness().await;
    let mock = MockSonos::start(MAC_LIVING, "Sonos Port", "Rack", 100).await;
    mock.set_fixed(true);
    let device_id = h.attach(MAC_LIVING, &mock).await;
    let device = h
        .wait_device(&device_id, |d| !d.capabilities.volume.supported)
        .await;
    assert_eq!(device.volume.value, None);
    h.ceiling(&device_id, 50.0).await;
    assert!(h
        .control(&device_id, DeviceRequest::VolumeAbsolute(20.0))
        .await
        .is_err());
    assert!(mock.writes().is_empty());
}

/// CRITICAL: two units of one model never cross-route.
#[tokio::test]
async fn critical_two_units_of_one_model_never_cross_route() {
    let h = harness().await;
    let living = MockSonos::start(MAC_LIVING, "Sonos One", "Living Room", 20).await;
    let den = MockSonos::start(MAC_DEN, "Sonos One", "Den", 25).await;
    let living_id = h.attach(MAC_LIVING, &living).await;
    let den_id = h.attach(MAC_DEN, &den).await;
    assert_ne!(living_id, den_id);

    h.ceiling(&living_id, 60.0).await;
    h.control(&living_id, DeviceRequest::VolumeAbsolute(55.0))
        .await
        .expect("confirmed");
    h.control(&living_id, DeviceRequest::Mute(true))
        .await
        .expect("confirmed");

    assert_eq!(
        living.writes(),
        vec!["SetVolume:55".to_string(), "SetMute:1".to_string()]
    );
    assert!(den.writes().is_empty(), "den received {:?}", den.writes());
    assert_eq!(den.vol(), 25);
    let den_device = h.aggregator.get_device(&den_id).await.unwrap();
    assert_eq!(den_device.volume.value, Some(25.0));
    assert_eq!(den_device.volume.is_muted, Some(false));
}

#[tokio::test]
async fn unsupported_power_errors_instead_of_succeeding() {
    let h = harness().await;
    let mock = MockSonos::start(MAC_LIVING, "Sonos Beam", "Living Room", 10).await;
    let device_id = h.attach(MAC_LIVING, &mock).await;
    assert!(h.control(&device_id, DeviceRequest::PowerOn).await.is_err());
    assert!(h
        .control(&device_id, DeviceRequest::PowerOff)
        .await
        .is_err());
    for operation in [DeviceOperation::PowerOn, DeviceOperation::PowerStandby] {
        let error = h.adapter.execute(&device_id, operation).await.unwrap_err();
        assert!(error.contains("not supported"), "{error}");
    }
    assert!(mock.writes().is_empty());
}

#[tokio::test]
async fn stopping_the_family_removes_the_device() {
    let h = harness().await;
    let mock = MockSonos::start(MAC_LIVING, "Sonos Beam", "Living Room", 10).await;
    let device_id = h.attach(MAC_LIVING, &mock).await;
    h.adapter.stop_family().await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.aggregator.get_device(&device_id).await.is_some() {
        assert!(
            Instant::now() < deadline,
            "device still observed after stop"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
