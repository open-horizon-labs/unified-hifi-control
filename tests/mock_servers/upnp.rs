//! Mock UPnP MediaRenderer for testing
//!
//! Provides HTTP endpoints for device description and SOAP control.
//! Note: Does not implement SSDP discovery - tests should directly configure adapter.

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

/// Mock UPnP renderer state
#[derive(Debug, Clone)]
pub struct MockUpnpState {
    pub uuid: String,
    pub name: String,
    pub manufacturer: String,
    pub model: String,
    pub state: String, // PLAYING, PAUSED_PLAYBACK, STOPPED
    pub volume: u32,   // 0-100
    pub muted: bool,
    /// Current track, as `GetPositionInfo` would report it. `None` means the
    /// renderer reports an empty `TrackMetaData`, which real devices do when
    /// stopped or when they carry no metadata at all.
    pub track: Option<MockUpnpTrack>,
}

/// Track metadata the mock renderer will serialise into DIDL-Lite.
#[derive(Debug, Clone)]
pub struct MockUpnpTrack {
    pub uri: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_art_uri: String,
}

impl Default for MockUpnpState {
    fn default() -> Self {
        Self {
            uuid: "mock-upnp-uuid-12345".to_string(),
            name: "Mock UPnP Renderer".to_string(),
            manufacturer: "Mock Corp".to_string(),
            model: "Mock Model".to_string(),
            state: "STOPPED".to_string(),
            volume: 50,
            muted: false,
            track: None,
        }
    }
}

/// Mock UPnP MediaRenderer
pub struct MockUpnpRenderer {
    addr: SocketAddr,
    state: Arc<RwLock<MockUpnpState>>,
    handle: JoinHandle<()>,
}

impl MockUpnpRenderer {
    /// Start a mock UPnP renderer on a random port
    pub async fn start() -> Self {
        Self::start_with_state(MockUpnpState::default()).await
    }

    /// Start with custom initial state
    pub async fn start_with_state(initial_state: MockUpnpState) -> Self {
        let state = Arc::new(RwLock::new(initial_state));

        let app = Router::new()
            .route("/description.xml", get(handle_description))
            .route("/AVTransport/control", post(handle_av_transport))
            .route("/RenderingControl/control", post(handle_rendering_control))
            .with_state(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        Self {
            addr,
            state,
            handle,
        }
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

    /// Set transport state (PLAYING, PAUSED_PLAYBACK, STOPPED)
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

    /// Set the current track, reported via `GetPositionInfo`'s `TrackMetaData`.
    pub async fn set_track(&self, uri: &str, title: &str, artist: &str, album: &str, art: &str) {
        self.state.write().await.track = Some(MockUpnpTrack {
            uri: uri.to_string(),
            title: title.to_string(),
            artist: artist.to_string(),
            album: album.to_string(),
            album_art_uri: art.to_string(),
        });
    }

    /// Report no track metadata, as a renderer does when stopped or when it
    /// simply carries none.
    pub async fn clear_track(&self) {
        self.state.write().await.track = None;
    }

    /// Stop the mock server
    pub async fn stop(self) {
        self.handle.abort();
    }
}

/// Handle device description request
async fn handle_description(State(state): State<Arc<RwLock<MockUpnpState>>>) -> impl IntoResponse {
    let state = state.read().await;

    let xml = format!(
        r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <specVersion><major>1</major><minor>0</minor></specVersion>
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName>{}</friendlyName>
    <manufacturer>{}</manufacturer>
    <modelName>{}</modelName>
    <UDN>uuid:{}</UDN>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:AVTransport</serviceId>
        <controlURL>/AVTransport/control</controlURL>
        <eventSubURL>/AVTransport/event</eventSubURL>
        <SCPDURL>/AVTransport/scpd.xml</SCPDURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
        <serviceId>urn:upnp-org:serviceId:RenderingControl</serviceId>
        <controlURL>/RenderingControl/control</controlURL>
        <eventSubURL>/RenderingControl/event</eventSubURL>
        <SCPDURL>/RenderingControl/scpd.xml</SCPDURL>
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

/// Handle AVTransport SOAP requests
async fn handle_av_transport(
    State(state): State<Arc<RwLock<MockUpnpState>>>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    let action = headers
        .get("soapaction")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let state_guard = state.read().await;

    let response_body = if action.contains("GetTransportInfo") {
        format!(
            r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <u:GetTransportInfoResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
      <CurrentTransportState>{}</CurrentTransportState>
      <CurrentTransportStatus>OK</CurrentTransportStatus>
      <CurrentSpeed>1</CurrentSpeed>
    </u:GetTransportInfoResponse>
  </s:Body>
</s:Envelope>"#,
            state_guard.state
        )
    } else if action.contains("GetPositionInfo") {
        // TrackMetaData carries DIDL-Lite XML-escaped inside the SOAP envelope,
        // exactly as real renderers send it. albumArtURI carries the
        // dlna:profileID attribute that the DLNA convention puts there.
        let (track_uri, metadata) = match &state_guard.track {
            Some(t) => {
                let didl = format!(
                    concat!(
                        r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" "#,
                        r#"xmlns:dc="http://purl.org/dc/elements/1.1/" "#,
                        r#"xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">"#,
                        r#"<item id="1" parentID="0" restricted="1">"#,
                        "<dc:title>{}</dc:title>",
                        "<upnp:artist>{}</upnp:artist>",
                        "<upnp:album>{}</upnp:album>",
                        r#"<upnp:albumArtURI dlna:profileID="JPEG_TN">{}</upnp:albumArtURI>"#,
                        "</item></DIDL-Lite>"
                    ),
                    t.title, t.artist, t.album, t.album_art_uri
                );
                let escaped = didl
                    .replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
                    .replace('"', "&quot;");
                (t.uri.clone(), escaped)
            }
            None => (String::new(), String::new()),
        };

        format!(
            r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <u:GetPositionInfoResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
      <Track>1</Track>
      <TrackDuration>0:03:21</TrackDuration>
      <TrackMetaData>{}</TrackMetaData>
      <TrackURI>{}</TrackURI>
      <RelTime>0:00:12</RelTime>
      <AbsTime>0:00:12</AbsTime>
      <RelCount>2147483647</RelCount>
      <AbsCount>2147483647</AbsCount>
    </u:GetPositionInfoResponse>
  </s:Body>
</s:Envelope>"#,
            metadata, track_uri
        )
    } else if action.contains("Play") || action.contains("Pause") || action.contains("Stop") {
        // Control action - return success
        let action_name = if action.contains("Play") {
            "Play"
        } else if action.contains("Pause") {
            "Pause"
        } else {
            "Stop"
        };

        format!(
            r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <u:{}Response xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
    </u:{}Response>
  </s:Body>
</s:Envelope>"#,
            action_name, action_name
        )
    } else {
        // Unknown action
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

/// Handle RenderingControl SOAP requests
async fn handle_rendering_control(
    State(state): State<Arc<RwLock<MockUpnpState>>>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    let action = headers
        .get("soapaction")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let state_guard = state.read().await;

    let response_body = if action.contains("GetVolume") {
        format!(
            r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <u:GetVolumeResponse xmlns:u="urn:schemas-upnp-org:service:RenderingControl:1">
      <CurrentVolume>{}</CurrentVolume>
    </u:GetVolumeResponse>
  </s:Body>
</s:Envelope>"#,
            state_guard.volume
        )
    } else if action.contains("GetMute") {
        format!(
            r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <u:GetMuteResponse xmlns:u="urn:schemas-upnp-org:service:RenderingControl:1">
      <CurrentMute>{}</CurrentMute>
    </u:GetMuteResponse>
  </s:Body>
</s:Envelope>"#,
            if state_guard.muted { "1" } else { "0" }
        )
    } else if action.contains("SetVolume") || action.contains("SetMute") {
        // Control action - return success
        let action_name = if action.contains("SetVolume") {
            "SetVolume"
        } else {
            "SetMute"
        };

        format!(
            r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <u:{}Response xmlns:u="urn:schemas-upnp-org:service:RenderingControl:1">
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_upnp_starts_and_stops() {
        let server = MockUpnpRenderer::start().await;
        let addr = server.addr();
        assert!(addr.port() > 0);
        server.stop().await;
    }

    #[tokio::test]
    async fn mock_upnp_returns_description() {
        let server = MockUpnpRenderer::start().await;

        let client = reqwest::Client::new();
        let response = client
            .get(server.description_url())
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        assert!(response.contains("Mock UPnP Renderer"));
        assert!(response.contains("MediaRenderer"));

        server.stop().await;
    }

    #[tokio::test]
    async fn mock_upnp_returns_transport_state() {
        let server = MockUpnpRenderer::start().await;
        server.set_state("PLAYING").await;

        let client = reqwest::Client::new();
        let response = client
            .post(format!("http://{}/AVTransport/control", server.addr()))
            .header("Content-Type", "text/xml")
            .header("SOAPAction", "\"urn:schemas-upnp-org:service:AVTransport:1#GetTransportInfo\"")
            .body(r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:GetTransportInfo xmlns:u="urn:schemas-upnp-org:service:AVTransport:1"><InstanceID>0</InstanceID></u:GetTransportInfo></s:Body></s:Envelope>"#)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        assert!(response.contains("PLAYING"));

        server.stop().await;
    }
}
