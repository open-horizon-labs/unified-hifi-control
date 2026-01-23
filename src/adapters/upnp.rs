//! UPnP/DLNA adapter - discovers and controls UPnP Media Renderers
//!
//! Uses SSDP for discovery and UPnP AV Transport service for control.
//! Pure UPnP/DLNA has limited metadata support compared to OpenHome.
//! Specifically, next/previous track are NOT supported by pure UPnP.

use crate::bus::{BusEvent, PlaybackState, SharedBus, VolumeControl as BusVolumeControl, Zone};
use futures::StreamExt;
use quick_xml::de::from_str as xml_from_str;
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use ssdp_client::{SearchTarget, URN};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::interval;
use tokio_util::sync::CancellationToken;

const MEDIA_RENDERER_URN: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
const AV_TRANSPORT_URN: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RENDERING_CONTROL_URN: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
const SSDP_SEARCH_INTERVAL: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const STALE_THRESHOLD: Duration = Duration::from_secs(90);
const SOAP_TIMEOUT: Duration = Duration::from_secs(5);

/// UPnP Media Renderer information
#[derive(Debug, Clone, Serialize)]
pub struct UPnPRenderer {
    pub uuid: String,
    pub name: String,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub location: String,
    pub state: String,
    pub volume: Option<i32>,
    pub muted: bool,
    #[serde(skip)]
    pub last_seen: std::time::Instant,
    #[serde(skip)]
    pub av_transport_url: Option<String>,
    #[serde(skip)]
    pub rendering_control_url: Option<String>,
}

/// UPnP adapter status
#[derive(Debug, Clone, Serialize)]
pub struct UPnPStatus {
    pub connected: bool,
    pub renderer_count: usize,
    pub renderers: Vec<UPnPRendererSummary>,
}

/// Renderer summary for status response
#[derive(Debug, Clone, Serialize)]
pub struct UPnPRendererSummary {
    pub uuid: String,
    pub name: String,
    pub state: String,
}

/// Now playing info from UPnP renderer (limited metadata)
#[derive(Debug, Clone, Serialize)]
pub struct UPnPNowPlaying {
    pub zone_id: String,
    pub line1: String,
    pub line2: String,
    pub line3: String,
    pub is_playing: bool,
    pub volume: Option<i32>,
    pub volume_min: i32,
    pub volume_max: i32,
    pub seek_position: Option<i64>,
    pub length: Option<u32>,
    pub image_key: Option<String>,
}

/// Zone info for API responses
#[derive(Debug, Clone, Serialize)]
pub struct UPnPZone {
    pub zone_id: String,
    pub zone_name: String,
    pub state: String,
    pub output_count: u32,
    pub output_name: String,
    pub device_name: Option<String>,
    pub volume_control: Option<VolumeControl>,
    /// UPnP doesn't support these features
    pub unsupported: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VolumeControl {
    #[serde(rename = "type")]
    pub vol_type: String,
    pub min: i32,
    pub max: i32,
    pub is_muted: bool,
}

struct UPnPState {
    renderers: HashMap<String, UPnPRenderer>,
    running: bool,
}

/// UPnP adapter for discovering and controlling DLNA Media Renderers
pub struct UPnPAdapter {
    state: Arc<RwLock<UPnPState>>,
    bus: SharedBus,
    http: Client,
    /// Wrapped in RwLock to allow creating fresh token on restart
    shutdown: Arc<RwLock<CancellationToken>>,
}

impl UPnPAdapter {
    /// Create new UPnP adapter
    pub fn new(bus: SharedBus) -> Self {
        Self {
            state: Arc::new(RwLock::new(UPnPState {
                renderers: HashMap::new(),
                running: false,
            })),
            bus,
            http: Client::builder()
                .timeout(SOAP_TIMEOUT)
                .build()
                .unwrap_or_default(),
            shutdown: Arc::new(RwLock::new(CancellationToken::new())),
        }
    }

    /// Start SSDP discovery (internal - use Startable trait)
    async fn start_internal(&self) -> anyhow::Result<()> {
        {
            let mut state = self.state.write().await;
            if state.running {
                return Ok(());
            }
            state.running = true;
        }

        // Create fresh cancellation token for this run (previous token may be cancelled)
        let shutdown = {
            let mut token = self.shutdown.write().await;
            *token = CancellationToken::new();
            token.clone()
        };

        // Spawn discovery task
        let state = self.state.clone();
        let bus = self.bus.clone();
        let http = self.http.clone();
        let shutdown_clone = shutdown.clone();

        tokio::spawn(async move {
            Self::discovery_loop(state.clone(), bus.clone(), http.clone(), shutdown_clone).await;
        });

        // Spawn polling task
        let state = self.state.clone();
        let bus = self.bus.clone();
        let http = self.http.clone();

        tokio::spawn(async move {
            Self::poll_loop(state, bus, http, shutdown).await;
        });

        tracing::info!("UPnP adapter started");
        Ok(())
    }

    async fn discovery_loop(
        state: Arc<RwLock<UPnPState>>,
        bus: SharedBus,
        http: Client,
        shutdown: CancellationToken,
    ) {
        let mut search_interval = interval(SSDP_SEARCH_INTERVAL);

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::info!("UPnP discovery loop shutting down");
                    break;
                }
                _ = search_interval.tick() => {
                    // Perform SSDP search
                    if let Err(e) = Self::perform_search(&state, &bus, &http).await {
                        tracing::warn!("SSDP search failed: {}", e);
                    }

                    // Cleanup stale renderers
                    Self::cleanup_stale(&state, &bus).await;
                }
            }
        }

        tracing::info!("UPnP discovery loop stopped");
    }

    async fn perform_search(
        state: &Arc<RwLock<UPnPState>>,
        bus: &SharedBus,
        http: &Client,
    ) -> anyhow::Result<()> {
        let urn: URN = MEDIA_RENDERER_URN.parse()?;
        let search_target = SearchTarget::URN(urn);
        let responses =
            ssdp_client::search(&search_target, Duration::from_secs(3), 2, None).await?;

        futures::pin_mut!(responses);

        while let Some(response) = responses.next().await {
            let response = match response {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!("SSDP response error: {}", e);
                    continue;
                }
            };

            let location = response.location().to_string();
            let usn = response.usn();

            // Extract UUID from USN
            let uuid = match usn.split("::").next() {
                Some(s) if s.starts_with("uuid:") => s.trim_start_matches("uuid:").to_string(),
                _ => continue,
            };

            // Update existing or add new
            let mut s = state.write().await;
            if let Some(renderer) = s.renderers.get_mut(&uuid) {
                renderer.last_seen = std::time::Instant::now();
                continue;
            }

            tracing::info!("Discovered UPnP MediaRenderer: {} at {}", uuid, location);

            // New renderer
            let renderer = UPnPRenderer {
                uuid: uuid.clone(),
                name: format!("Renderer {}", &uuid[..8.min(uuid.len())]),
                manufacturer: None,
                model: None,
                location: location.clone(),
                state: "stopped".to_string(),
                volume: None,
                muted: false,
                last_seen: std::time::Instant::now(),
                av_transport_url: None,
                rendering_control_url: None,
            };

            s.renderers.insert(uuid.clone(), renderer);
            drop(s);

            // Fetch device description
            let state_clone = state.clone();
            let http_clone = http.clone();
            let bus_clone = bus.clone();
            let uuid_clone = uuid.clone();

            tokio::spawn(async move {
                if let Err(e) =
                    Self::fetch_device_info(&state_clone, &http_clone, &uuid_clone, &location).await
                {
                    tracing::warn!("Failed to fetch device info for {}: {}", uuid_clone, e);
                }
                // Emit ZoneDiscovered with full zone info
                let s = state_clone.read().await;
                if let Some(renderer) = s.renderers.get(&uuid_clone) {
                    let zone = upnp_renderer_to_zone(renderer);
                    bus_clone.publish(BusEvent::ZoneDiscovered { zone });
                }
            });
        }

        Ok(())
    }

    async fn fetch_device_info(
        state: &Arc<RwLock<UPnPState>>,
        http: &Client,
        uuid: &str,
        location: &str,
    ) -> anyhow::Result<()> {
        let response = http.get(location).send().await?;
        let xml = response.text().await?;

        // Parse device description
        #[derive(Deserialize)]
        struct Root {
            device: DeviceDesc,
        }

        #[derive(Deserialize)]
        struct DeviceDesc {
            #[serde(rename = "friendlyName")]
            friendly_name: Option<String>,
            manufacturer: Option<String>,
            #[serde(rename = "modelName")]
            model_name: Option<String>,
            #[serde(rename = "serviceList")]
            service_list: Option<ServiceList>,
        }

        #[derive(Deserialize)]
        struct ServiceList {
            service: Vec<ServiceDesc>,
        }

        #[derive(Deserialize)]
        struct ServiceDesc {
            #[serde(rename = "serviceType")]
            service_type: String,
            #[serde(rename = "controlURL")]
            control_url: Option<String>,
        }

        let root: Root = xml_from_str(&xml)?;

        // Get base URL
        let base_url = Self::get_base_url(location)?;

        let mut s = state.write().await;
        if let Some(renderer) = s.renderers.get_mut(uuid) {
            renderer.name = root
                .device
                .friendly_name
                .unwrap_or_else(|| format!("Renderer {}", &uuid[..8.min(uuid.len())]));
            renderer.manufacturer = root.device.manufacturer;
            renderer.model = root.device.model_name;

            // Extract service URLs
            if let Some(services) = root.device.service_list {
                for service in services.service {
                    if service.service_type.contains("AVTransport") {
                        if let Some(url) = service.control_url {
                            renderer.av_transport_url = Some(format!("{}{}", base_url, url));
                        }
                    } else if service.service_type.contains("RenderingControl") {
                        if let Some(url) = service.control_url {
                            renderer.rendering_control_url = Some(format!("{}{}", base_url, url));
                        }
                    }
                }
            }

            tracing::info!(
                "Got UPnP device info: {} - {} {}",
                renderer.name,
                renderer.manufacturer.as_deref().unwrap_or("Unknown"),
                renderer.model.as_deref().unwrap_or("")
            );
        }

        Ok(())
    }

    async fn cleanup_stale(state: &Arc<RwLock<UPnPState>>, bus: &SharedBus) {
        let mut s = state.write().await;
        let now = std::time::Instant::now();

        let stale: Vec<String> = s
            .renderers
            .iter()
            .filter(|(_, r)| now.duration_since(r.last_seen) > STALE_THRESHOLD)
            .map(|(uuid, _)| uuid.clone())
            .collect();

        for uuid in stale {
            tracing::info!("Removing stale UPnP renderer: {}", uuid);
            s.renderers.remove(&uuid);
            bus.publish(BusEvent::ZoneRemoved {
                zone_id: format!("upnp:{}", uuid),
            });
        }
    }

    async fn poll_loop(
        state: Arc<RwLock<UPnPState>>,
        bus: SharedBus,
        http: Client,
        shutdown: CancellationToken,
    ) {
        let mut poll_interval = interval(POLL_INTERVAL);

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::info!("UPnP poll loop shutting down");
                    break;
                }
                _ = poll_interval.tick() => {
                    // Get list of renderers to poll
                    let renderers: Vec<(String, Option<String>, Option<String>)> = {
                        let s = state.read().await;
                        s.renderers
                            .iter()
                            .map(|(uuid, r)| {
                                (
                                    uuid.clone(),
                                    r.av_transport_url.clone(),
                                    r.rendering_control_url.clone(),
                                )
                            })
                            .collect()
                    };

                    for (uuid, av_url, rc_url) in renderers {
                        if let Err(e) = Self::poll_renderer(
                            &state,
                            &bus,
                            &http,
                            &uuid,
                            av_url.as_deref(),
                            rc_url.as_deref(),
                        )
                        .await
                        {
                            tracing::debug!("Failed to poll {}: {}", uuid, e);
                        }
                    }
                }
            }
        }

        tracing::info!("UPnP poll loop stopped");
    }

    async fn poll_renderer(
        state: &Arc<RwLock<UPnPState>>,
        bus: &SharedBus,
        http: &Client,
        uuid: &str,
        av_url: Option<&str>,
        rc_url: Option<&str>,
    ) -> anyhow::Result<()> {
        // Poll transport state
        if let Some(url) = av_url {
            let transport_info = Self::soap_call(
                http,
                url,
                AV_TRANSPORT_URN,
                "GetTransportInfo",
                "<InstanceID>0</InstanceID>",
            )
            .await;

            if let Ok(response) = transport_info {
                if let Some(new_state) = Self::extract_xml_value(&response, "CurrentTransportState")
                {
                    let new_state = match new_state.as_str() {
                        "PLAYING" => "playing",
                        "PAUSED_PLAYBACK" => "paused",
                        "STOPPED" => "stopped",
                        "TRANSITIONING" => "loading",
                        _ => "stopped",
                    }
                    .to_string();

                    let mut s = state.write().await;
                    if let Some(renderer) = s.renderers.get_mut(uuid) {
                        if renderer.state != new_state {
                            renderer.state = new_state.clone();
                            bus.publish(BusEvent::ZoneUpdated {
                                zone_id: format!("upnp:{}", uuid),
                                display_name: renderer.name.clone(),
                                state: new_state,
                            });
                        }
                    }
                }
            }
        }

        // Poll volume
        if let Some(url) = rc_url {
            let volume = Self::soap_call(
                http,
                url,
                RENDERING_CONTROL_URN,
                "GetVolume",
                "<InstanceID>0</InstanceID><Channel>Master</Channel>",
            )
            .await;

            if let Ok(response) = volume {
                if let Some(vol_str) = Self::extract_xml_value(&response, "CurrentVolume") {
                    if let Ok(vol) = vol_str.parse::<i32>() {
                        let mut s = state.write().await;
                        if let Some(renderer) = s.renderers.get_mut(uuid) {
                            renderer.volume = Some(vol);
                        }
                    }
                }
            }

            // Poll mute
            let mute = Self::soap_call(
                http,
                url,
                RENDERING_CONTROL_URN,
                "GetMute",
                "<InstanceID>0</InstanceID><Channel>Master</Channel>",
            )
            .await;

            if let Ok(response) = mute {
                if let Some(mute_str) = Self::extract_xml_value(&response, "CurrentMute") {
                    let mut s = state.write().await;
                    if let Some(renderer) = s.renderers.get_mut(uuid) {
                        renderer.muted = mute_str == "1" || mute_str.eq_ignore_ascii_case("true");
                    }
                }
            }
        }

        Ok(())
    }

    fn get_base_url(location: &str) -> anyhow::Result<String> {
        let url = url::Url::parse(location)?;
        let port = url.port().map(|p| format!(":{}", p)).unwrap_or_default();
        Ok(format!(
            "{}://{}{}",
            url.scheme(),
            url.host_str().unwrap_or("localhost"),
            port
        ))
    }

    async fn soap_call(
        http: &Client,
        url: &str,
        service_type: &str,
        action: &str,
        body_content: &str,
    ) -> anyhow::Result<String> {
        let soap_body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:{action} xmlns:u="{service_type}">{body}</u:{action}>
  </s:Body>
</s:Envelope>"#,
            action = action,
            service_type = service_type,
            body = body_content
        );

        let response = http
            .post(url)
            .header("Content-Type", "text/xml; charset=utf-8")
            .header("SOAPAction", format!("\"{}#{}\"", service_type, action))
            .body(soap_body)
            .send()
            .await?;

        Ok(response.text().await?)
    }

    /// Extract XML value, handling optional namespace prefixes (e.g., <u:Volume> or <Volume>)
    fn extract_xml_value(xml: &str, tag: &str) -> Option<String> {
        // Build regex pattern to match tag with optional namespace prefix and attributes
        // Matches: <prefix:tag attr="...">value</prefix:tag> or <tag attr="...">value</tag>
        let pattern = format!(
            r"<(?:[^:>]+:)?{}\b[^>]*>([^<]*)</(?:[^:>]+:)?{}>",
            regex::escape(tag),
            regex::escape(tag)
        );

        let re = Regex::new(&pattern).ok()?;
        re.captures(xml)
            .and_then(|caps| caps.get(1))
            .map(|m| m.as_str().to_string())
    }

    /// Stop discovery (internal - use Startable trait)
    async fn stop_internal(&self) {
        // Cancel background tasks first
        self.shutdown.read().await.cancel();

        let mut state = self.state.write().await;
        state.running = false;
        state.renderers.clear();
        tracing::info!("UPnP adapter stopped");
    }

    /// Get adapter status
    pub async fn get_status(&self) -> UPnPStatus {
        let state = self.state.read().await;
        UPnPStatus {
            connected: !state.renderers.is_empty(),
            renderer_count: state.renderers.len(),
            renderers: state
                .renderers
                .values()
                .map(|r| UPnPRendererSummary {
                    uuid: r.uuid.clone(),
                    name: r.name.clone(),
                    state: r.state.clone(),
                })
                .collect(),
        }
    }

    /// Get all discovered renderers as zones
    pub async fn get_zones(&self) -> Vec<UPnPZone> {
        let state = self.state.read().await;
        state
            .renderers
            .values()
            .map(|r| {
                let device_name = match (&r.manufacturer, &r.model) {
                    (Some(m), Some(model)) => Some(format!("{} {}", m, model)),
                    (Some(m), None) => Some(m.clone()),
                    _ => None,
                };

                UPnPZone {
                    zone_id: r.uuid.clone(),
                    zone_name: r.name.clone(),
                    state: r.state.clone(),
                    output_count: 1,
                    output_name: r.name.clone(),
                    device_name,
                    volume_control: r.volume.map(|_| VolumeControl {
                        vol_type: "number".to_string(),
                        min: 0,
                        max: 100,
                        is_muted: r.muted,
                    }),
                    // Pure UPnP doesn't support these
                    unsupported: vec![
                        "next".to_string(),
                        "previous".to_string(),
                        "track_metadata".to_string(),
                        "album_art".to_string(),
                    ],
                }
            })
            .collect()
    }

    /// Get all discovered renderers
    pub async fn get_renderers(&self) -> Vec<UPnPRenderer> {
        let state = self.state.read().await;
        state.renderers.values().cloned().collect()
    }

    /// Get specific renderer by UUID
    pub async fn get_renderer(&self, uuid: &str) -> Option<UPnPRenderer> {
        let state = self.state.read().await;
        state.renderers.get(uuid).cloned()
    }

    /// Get now playing info for a renderer
    pub async fn get_now_playing(&self, uuid: &str) -> Option<UPnPNowPlaying> {
        let state = self.state.read().await;
        let renderer = state.renderers.get(uuid)?;

        // Pure UPnP doesn't provide track metadata
        Some(UPnPNowPlaying {
            zone_id: uuid.to_string(),
            line1: renderer.name.clone(),
            line2: String::new(),
            line3: String::new(),
            is_playing: renderer.state == "playing",
            volume: renderer.volume,
            volume_min: 0,
            volume_max: 100,
            seek_position: None,
            length: None,
            image_key: None,
        })
    }

    /// Send control command to a renderer
    pub async fn control(
        &self,
        uuid: &str,
        action: &str,
        value: Option<i32>,
    ) -> anyhow::Result<()> {
        let (av_url, rc_url) = {
            let state = self.state.read().await;
            let renderer = state
                .renderers
                .get(uuid)
                .ok_or_else(|| anyhow::anyhow!("Renderer not found: {}", uuid))?;
            (
                renderer.av_transport_url.clone(),
                renderer.rendering_control_url.clone(),
            )
        };

        match action {
            "play" => {
                let url = av_url
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("No AVTransport URL"))?;
                Self::soap_call(
                    &self.http,
                    url,
                    AV_TRANSPORT_URN,
                    "Play",
                    "<InstanceID>0</InstanceID><Speed>1</Speed>",
                )
                .await?;
            }
            "pause" => {
                let url = av_url
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("No AVTransport URL"))?;
                Self::soap_call(
                    &self.http,
                    url,
                    AV_TRANSPORT_URN,
                    "Pause",
                    "<InstanceID>0</InstanceID>",
                )
                .await?;
            }
            "play_pause" => {
                let url = av_url
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("No AVTransport URL"))?;
                let is_playing = {
                    let state = self.state.read().await;
                    state
                        .renderers
                        .get(uuid)
                        .map(|r| r.state == "playing")
                        .unwrap_or(false)
                };

                if is_playing {
                    Self::soap_call(
                        &self.http,
                        url,
                        AV_TRANSPORT_URN,
                        "Pause",
                        "<InstanceID>0</InstanceID>",
                    )
                    .await?;
                } else {
                    Self::soap_call(
                        &self.http,
                        url,
                        AV_TRANSPORT_URN,
                        "Play",
                        "<InstanceID>0</InstanceID><Speed>1</Speed>",
                    )
                    .await?;
                }
            }
            "stop" => {
                let url = av_url
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("No AVTransport URL"))?;
                Self::soap_call(
                    &self.http,
                    url,
                    AV_TRANSPORT_URN,
                    "Stop",
                    "<InstanceID>0</InstanceID>",
                )
                .await?;
            }
            "next" => {
                anyhow::bail!("Next track not supported by pure UPnP renderers");
            }
            "previous" | "prev" => {
                anyhow::bail!("Previous track not supported by pure UPnP renderers");
            }
            "vol_abs" | "volume" => {
                let url = rc_url
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("No RenderingControl URL"))?;
                let vol = value.unwrap_or(50).clamp(0, 100);
                Self::soap_call(
                    &self.http,
                    url,
                    RENDERING_CONTROL_URN,
                    "SetVolume",
                    &format!("<InstanceID>0</InstanceID><Channel>Master</Channel><DesiredVolume>{}</DesiredVolume>", vol),
                ).await?;

                let mut state = self.state.write().await;
                if let Some(renderer) = state.renderers.get_mut(uuid) {
                    renderer.volume = Some(vol);
                }
            }
            "vol_rel" => {
                let url = rc_url
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("No RenderingControl URL"))?;
                let delta = value.unwrap_or(0);
                let current = {
                    let state = self.state.read().await;
                    state
                        .renderers
                        .get(uuid)
                        .and_then(|r| r.volume)
                        .unwrap_or(50)
                };
                let new_vol = (current + delta).clamp(0, 100);

                Self::soap_call(
                    &self.http,
                    url,
                    RENDERING_CONTROL_URN,
                    "SetVolume",
                    &format!("<InstanceID>0</InstanceID><Channel>Master</Channel><DesiredVolume>{}</DesiredVolume>", new_vol),
                ).await?;

                let mut state = self.state.write().await;
                if let Some(renderer) = state.renderers.get_mut(uuid) {
                    renderer.volume = Some(new_vol);
                }
            }
            "mute" => {
                let url = rc_url
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("No RenderingControl URL"))?;
                let mute = value.map(|v| v != 0).unwrap_or(true);
                Self::soap_call(
                    &self.http,
                    url,
                    RENDERING_CONTROL_URN,
                    "SetMute",
                    &format!("<InstanceID>0</InstanceID><Channel>Master</Channel><DesiredMute>{}</DesiredMute>", if mute { "1" } else { "0" }),
                ).await?;

                let mut state = self.state.write().await;
                if let Some(renderer) = state.renderers.get_mut(uuid) {
                    renderer.muted = mute;
                }
            }
            _ => {
                anyhow::bail!("Unknown action: {}", action);
            }
        }

        // Trigger immediate poll
        let state = self.state.clone();
        let bus = self.bus.clone();
        let http = self.http.clone();
        let uuid = uuid.to_string();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _ = Self::poll_renderer(
                &state,
                &bus,
                &http,
                &uuid,
                av_url.as_deref(),
                rc_url.as_deref(),
            )
            .await;
        });

        Ok(())
    }
}

/// Convert a UPnP renderer to a unified Zone representation
fn upnp_renderer_to_zone(renderer: &UPnPRenderer) -> Zone {
    Zone {
        zone_id: format!("upnp:{}", renderer.uuid),
        zone_name: renderer.name.clone(),
        state: PlaybackState::from(renderer.state.as_str()),
        volume_control: renderer.volume.map(|v| BusVolumeControl {
            value: v as f32,
            min: 0.0,
            max: 100.0,
            step: 1.0,
            is_muted: renderer.muted,
            scale: crate::bus::VolumeScale::Percentage,
            output_id: Some(renderer.uuid.clone()),
        }),
        now_playing: None, // UPnP track info would need separate DIDL-Lite parsing
        source: "upnp".to_string(),
        is_controllable: renderer.av_transport_url.is_some(),
        is_seekable: false, // Pure UPnP seek support is limited
        last_updated: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        is_play_allowed: renderer.state != "playing",
        is_pause_allowed: renderer.state == "playing",
        is_next_allowed: false,
        is_previous_allowed: false,
    }
}

// Startable trait implementation via macro
crate::impl_startable!(UPnPAdapter, "upnp");
