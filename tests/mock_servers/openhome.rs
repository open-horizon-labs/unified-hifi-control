//! Mock OpenHome device for testing
//!
//! Provides HTTP endpoints for device description and SOAP control.
//! OpenHome extends UPnP with richer metadata and transport controls.

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

/// Mock OpenHome device state
#[derive(Debug, Clone)]
pub struct MockOpenHomeState {
    pub uuid: String,
    pub name: String,
    pub manufacturer: String,
    pub model: String,
    pub state: String, // Playing, Paused, Stopped
    pub volume: u32,   // 0-100
    pub muted: bool,
    pub track_title: String,
    pub track_artist: String,
    pub track_album: String,
    pub track_art_url: String,
}

impl Default for MockOpenHomeState {
    fn default() -> Self {
        Self {
            uuid: "mock-openhome-uuid-12345".to_string(),
            name: "Mock OpenHome Device".to_string(),
            manufacturer: "Mock Corp".to_string(),
            model: "Mock OH Model".to_string(),
            state: "Stopped".to_string(),
            volume: 50,
            muted: false,
            track_title: String::new(),
            track_artist: String::new(),
            track_album: String::new(),
            track_art_url: String::new(),
        }
    }
}

/// Mock OpenHome device
pub struct MockOpenHomeDevice {
    addr: SocketAddr,
    state: Arc<RwLock<MockOpenHomeState>>,
    handle: JoinHandle<()>,
}

impl MockOpenHomeDevice {
    /// Start a mock OpenHome device on a random port
    pub async fn start() -> Self {
        Self::start_with_state(MockOpenHomeState::default()).await
    }

    /// Start with custom initial state
    pub async fn start_with_state(initial_state: MockOpenHomeState) -> Self {
        let state = Arc::new(RwLock::new(initial_state));

        let app = Router::new()
            .route("/description.xml", get(handle_description))
            .route("/Transport/control", post(handle_transport))
            .route("/Volume/control", post(handle_volume))
            .route("/Info/control", post(handle_info))
            .with_state(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        Self { addr, state, handle }
    }

    /// Get the server address
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Get the device description URL
    pub fn description_url(&self) -> String {
        format!("http://{}/description.xml", self.addr)
    }

    /// Get the UUID
    pub async fn uuid(&self) -> String {
        self.state.read().await.uuid.clone()
    }

    /// Set transport state (Playing, Paused, Stopped)
    pub async fn set_state(&self, state: &str) {
        self.state.write().await.state = state.to_string();
    }

    /// Set volume (0-100)
    pub async fn set_volume(&self, volume: u32) {
        self.state.write().await.volume = volume.min(100);
    }

    /// Set mute
    pub async fn set_muted(&self, muted: bool) {
        self.state.write().await.muted = muted;
    }

    /// Set now playing track info
    pub async fn set_track(&self, title: &str, artist: &str, album: &str, art_url: &str) {
        let mut state = self.state.write().await;
        state.track_title = title.to_string();
        state.track_artist = artist.to_string();
        state.track_album = album.to_string();
        state.track_art_url = art_url.to_string();
    }

    /// Stop the mock server
    pub async fn stop(self) {
        self.handle.abort();
    }
}

/// Handle device description request
async fn handle_description(State(state): State<Arc<RwLock<MockOpenHomeState>>>) -> impl IntoResponse {
    let state = state.read().await;

    let xml = format!(
        r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <device>
    <deviceType>urn:av-openhome-org:device:Source:1</deviceType>
    <friendlyName>{}</friendlyName>
    <manufacturer>{}</manufacturer>
    <modelName>{}</modelName>
    <UDN>uuid:{}</UDN>
    <serviceList>
      <service>
        <serviceType>urn:av-openhome-org:service:Transport:1</serviceType>
        <serviceId>urn:av-openhome-org:serviceId:Transport</serviceId>
        <controlURL>/Transport/control</controlURL>
        <eventSubURL>/Transport/event</eventSubURL>
        <SCPDURL>/Transport/scpd.xml</SCPDURL>
      </service>
      <service>
        <serviceType>urn:av-openhome-org:service:Volume:1</serviceType>
        <serviceId>urn:av-openhome-org:serviceId:Volume</serviceId>
        <controlURL>/Volume/control</controlURL>
        <eventSubURL>/Volume/event</eventSubURL>
        <SCPDURL>/Volume/scpd.xml</SCPDURL>
      </service>
      <service>
        <serviceType>urn:av-openhome-org:service:Info:1</serviceType>
        <serviceId>urn:av-openhome-org:serviceId:Info</serviceId>
        <controlURL>/Info/control</controlURL>
        <eventSubURL>/Info/event</eventSubURL>
        <SCPDURL>/Info/scpd.xml</SCPDURL>
      </service>
    </serviceList>
  </device>
</root>"#,
        state.name, state.manufacturer, state.model, state.uuid
    );

    Response::builder()
        .header(header::CONTENT_TYPE, "text/xml; charset=utf-8")
        .body(Body::from(xml))
        .unwrap()
}

/// Handle Transport SOAP requests
async fn handle_transport(
    State(state): State<Arc<RwLock<MockOpenHomeState>>>,
    headers: HeaderMap,
    _body: String,
) -> impl IntoResponse {
    let action = headers
        .get("soapaction")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let state_guard = state.read().await;

    let response_body = if action.contains("TransportState") {
        format!(
            r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <u:TransportStateResponse xmlns:u="urn:av-openhome-org:service:Transport:1">
      <Value>{}</Value>
    </u:TransportStateResponse>
  </s:Body>
</s:Envelope>"#,
            state_guard.state
        )
    } else if action.contains("Play") || action.contains("Pause") || action.contains("Stop")
        || action.contains("SkipNext") || action.contains("SkipPrevious")
    {
        let action_name = if action.contains("Play") {
            "Play"
        } else if action.contains("Pause") {
            "Pause"
        } else if action.contains("Stop") {
            "Stop"
        } else if action.contains("SkipNext") {
            "SkipNext"
        } else {
            "SkipPrevious"
        };

        format!(
            r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <u:{}Response xmlns:u="urn:av-openhome-org:service:Transport:1">
    </u:{}Response>
  </s:Body>
</s:Envelope>"#,
            action_name, action_name
        )
    } else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("Unknown action"))
            .unwrap();
    };

    Response::builder()
        .header(header::CONTENT_TYPE, "text/xml; charset=utf-8")
        .body(Body::from(response_body))
        .unwrap()
}

/// Handle Volume SOAP requests
async fn handle_volume(
    State(state): State<Arc<RwLock<MockOpenHomeState>>>,
    headers: HeaderMap,
    _body: String,
) -> impl IntoResponse {
    let action = headers
        .get("soapaction")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let state_guard = state.read().await;

    let response_body = if action.contains("#Volume") && !action.contains("Set") {
        format!(
            r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <u:VolumeResponse xmlns:u="urn:av-openhome-org:service:Volume:1">
      <Value>{}</Value>
    </u:VolumeResponse>
  </s:Body>
</s:Envelope>"#,
            state_guard.volume
        )
    } else if action.contains("#Mute") && !action.contains("Set") {
        format!(
            r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <u:MuteResponse xmlns:u="urn:av-openhome-org:service:Volume:1">
      <Value>{}</Value>
    </u:MuteResponse>
  </s:Body>
</s:Envelope>"#,
            if state_guard.muted { "true" } else { "false" }
        )
    } else if action.contains("SetVolume") || action.contains("SetMute") {
        let action_name = if action.contains("SetVolume") {
            "SetVolume"
        } else {
            "SetMute"
        };

        format!(
            r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <u:{}Response xmlns:u="urn:av-openhome-org:service:Volume:1">
    </u:{}Response>
  </s:Body>
</s:Envelope>"#,
            action_name, action_name
        )
    } else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("Unknown action"))
            .unwrap();
    };

    Response::builder()
        .header(header::CONTENT_TYPE, "text/xml; charset=utf-8")
        .body(Body::from(response_body))
        .unwrap()
}

/// Handle Info SOAP requests (track metadata)
async fn handle_info(
    State(state): State<Arc<RwLock<MockOpenHomeState>>>,
    headers: HeaderMap,
    _body: String,
) -> impl IntoResponse {
    let action = headers
        .get("soapaction")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let state_guard = state.read().await;

    let response_body = if action.contains("Track") {
        // DIDL-Lite encoded metadata (HTML entities for XML in XML)
        let metadata = if !state_guard.track_title.is_empty() {
            format!(
                "&lt;DIDL-Lite&gt;&lt;item&gt;\
                &lt;dc:title&gt;{}&lt;/dc:title&gt;\
                &lt;upnp:artist&gt;{}&lt;/upnp:artist&gt;\
                &lt;upnp:album&gt;{}&lt;/upnp:album&gt;\
                &lt;upnp:albumArtURI&gt;{}&lt;/upnp:albumArtURI&gt;\
                &lt;/item&gt;&lt;/DIDL-Lite&gt;",
                state_guard.track_title,
                state_guard.track_artist,
                state_guard.track_album,
                state_guard.track_art_url
            )
        } else {
            String::new()
        };

        format!(
            r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <u:TrackResponse xmlns:u="urn:av-openhome-org:service:Info:1">
      <Uri></Uri>
      <Metadata>{}</Metadata>
    </u:TrackResponse>
  </s:Body>
</s:Envelope>"#,
            metadata
        )
    } else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("Unknown action"))
            .unwrap();
    };

    Response::builder()
        .header(header::CONTENT_TYPE, "text/xml; charset=utf-8")
        .body(Body::from(response_body))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_openhome_starts_and_stops() {
        let server = MockOpenHomeDevice::start().await;
        let addr = server.addr();
        assert!(addr.port() > 0);
        server.stop().await;
    }

    #[tokio::test]
    async fn mock_openhome_returns_description() {
        let server = MockOpenHomeDevice::start().await;

        let client = reqwest::Client::new();
        let response = client
            .get(server.description_url())
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        assert!(response.contains("Mock OpenHome Device"));
        assert!(response.contains("av-openhome-org"));

        server.stop().await;
    }

    #[tokio::test]
    async fn mock_openhome_returns_transport_state() {
        let server = MockOpenHomeDevice::start().await;
        server.set_state("Playing").await;

        let client = reqwest::Client::new();
        let response = client
            .post(format!("http://{}/Transport/control", server.addr()))
            .header("Content-Type", "text/xml")
            .header("SOAPAction", "\"urn:av-openhome-org:service:Transport:1#TransportState\"")
            .body(r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:TransportState xmlns:u="urn:av-openhome-org:service:Transport:1"></u:TransportState></s:Body></s:Envelope>"#)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        assert!(response.contains("Playing"));

        server.stop().await;
    }
}
