//! OpenHome adapter - discovers and controls OpenHome Media Renderers
//!
//! Uses SSDP for discovery and UPnP/SOAP for control of OpenHome devices.
//! OpenHome is an extension of UPnP that provides richer metadata and more
//! control actions (next/previous track, playlists, etc.)

use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use quick_xml::de::from_str as xml_from_str;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use ssdp_client::{SearchTarget, URN};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::interval;
use tokio_util::sync::CancellationToken;

use crate::adapters::handle::{AdapterHandle, RetryConfig};
use crate::adapters::traits::{
    AdapterCommand, AdapterCommandResponse, AdapterContext, AdapterLogic,
};
use crate::bus::{
    BusEvent, PlaybackState, PrefixedZoneId, SharedBus, VolumeControl as BusVolumeControl, Zone,
};

/// OpenHome URNs to search for - devices may advertise different services
const OPENHOME_URNS: &[&str] = &[
    "urn:av-openhome-org:service:Product:1",
    "urn:av-openhome-org:service:Product:2",
    "urn:av-openhome-org:service:Transport:1",
    "urn:av-openhome-org:service:Volume:1",
    "urn:av-openhome-org:service:Volume:2",
];
const SSDP_SEARCH_INTERVAL: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const STALE_THRESHOLD: Duration = Duration::from_secs(90);
const SOAP_TIMEOUT: Duration = Duration::from_secs(5);

/// OpenHome device information
#[derive(Debug, Clone, Serialize)]
pub struct OpenHomeDevice {
    pub uuid: String,
    pub name: String,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub location: String,
    pub state: String,
    pub volume: Option<i32>,
    pub muted: bool,
    /// VolumeMax from Characteristics action
    pub volume_max: Option<u32>,
    /// VolumeSteps from Characteristics action (step = volume_max / volume_steps)
    pub volume_steps: Option<u32>,
    pub track_info: Option<TrackInfo>,
    #[serde(skip)]
    pub last_seen: std::time::Instant,
    #[serde(skip)]
    pub last_track_uri: Option<String>,
}

/// Track metadata from OpenHome device
#[derive(Debug, Clone, Serialize)]
pub struct TrackInfo {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_art_uri: Option<String>,
    pub genre: Option<String>,
}

/// OpenHome adapter status
#[derive(Debug, Clone, Serialize)]
pub struct OpenHomeStatus {
    pub connected: bool,
    pub device_count: usize,
    pub devices: Vec<OpenHomeDeviceSummary>,
}

/// Device summary for status response
#[derive(Debug, Clone, Serialize)]
pub struct OpenHomeDeviceSummary {
    pub uuid: String,
    pub name: String,
    pub state: String,
}

/// Now playing info from OpenHome device
#[derive(Debug, Clone, Serialize)]
pub struct OpenHomeNowPlaying {
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
pub struct OpenHomeZone {
    pub zone_id: String,
    pub zone_name: String,
    pub state: String,
    pub output_count: u32,
    pub output_name: String,
    pub device_name: Option<String>,
    pub volume_control: Option<VolumeControl>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VolumeControl {
    #[serde(rename = "type")]
    pub vol_type: String,
    pub min: i32,
    pub max: i32,
    pub is_muted: bool,
}

struct OpenHomeState {
    devices: HashMap<String, OpenHomeDevice>,
    running: bool,
}

/// OpenHome adapter for discovering and controlling OpenHome devices
#[derive(Clone)]
pub struct OpenHomeAdapter {
    state: Arc<RwLock<OpenHomeState>>,
    bus: SharedBus,
    http: Client,
    /// Wrapped in RwLock to allow creating fresh token on restart
    shutdown: Arc<RwLock<CancellationToken>>,
}

impl OpenHomeAdapter {
    /// Create new OpenHome adapter
    pub fn new(bus: SharedBus) -> Self {
        Self {
            state: Arc::new(RwLock::new(OpenHomeState {
                devices: HashMap::new(),
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
        // Use write lock to atomically check and set running flag
        // This prevents race conditions where multiple starts could pass the check
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

        // Create AdapterHandle and spawn run_with_retry
        let handle = AdapterHandle::new(self.clone(), self.bus.clone(), shutdown);

        tokio::spawn(async move {
            let _ = handle.run_with_retry(RetryConfig::default()).await;
        });

        tracing::info!("OpenHome adapter started");
        Ok(())
    }

    async fn discovery_loop(
        state: Arc<RwLock<OpenHomeState>>,
        bus: SharedBus,
        http: Client,
        shutdown: CancellationToken,
    ) {
        let mut search_interval = interval(SSDP_SEARCH_INTERVAL);

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::info!("OpenHome discovery loop shutting down");
                    break;
                }
                _ = search_interval.tick() => {
                    // Perform SSDP search
                    if let Err(e) = Self::perform_search(&state, &bus, &http).await {
                        tracing::warn!("SSDP search failed: {}", e);
                    }

                    // Cleanup stale devices
                    Self::cleanup_stale(&state, &bus).await;
                }
            }
        }

        tracing::info!("OpenHome discovery loop stopped");
    }

    async fn perform_search(
        state: &Arc<RwLock<OpenHomeState>>,
        bus: &SharedBus,
        http: &Client,
    ) -> anyhow::Result<()> {
        // Search for all known OpenHome URNs - devices may advertise different services
        for urn_str in OPENHOME_URNS {
            let urn: URN = match urn_str.parse() {
                Ok(u) => u,
                Err(e) => {
                    tracing::warn!("Failed to parse URN {}: {}", urn_str, e);
                    continue;
                }
            };
            let search_target = SearchTarget::URN(urn);

            match ssdp_client::search(&search_target, Duration::from_secs(2), 2, None).await {
                Ok(responses) => {
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

                        // Log what we're finding
                        tracing::debug!("OpenHome SSDP response: usn={} loc={}", usn, location);

                        // Extract UUID from USN
                        let uuid = match usn.split("::").next() {
                            Some(s) if s.starts_with("uuid:") => {
                                s.trim_start_matches("uuid:").to_string()
                            }
                            _ => continue,
                        };

                        // Update existing or add new
                        let mut s = state.write().await;
                        if let Some(device) = s.devices.get_mut(&uuid) {
                            device.last_seen = std::time::Instant::now();
                            continue;
                        }

                        tracing::info!(
                            "Discovered OpenHome device: {} at {} (via {})",
                            uuid,
                            location,
                            urn_str
                        );

                        // New device
                        let device = OpenHomeDevice {
                            uuid: uuid.clone(),
                            name: format!("OpenHome {}", &uuid[..8.min(uuid.len())]),
                            manufacturer: None,
                            model: None,
                            location: location.clone(),
                            state: "stopped".to_string(),
                            volume: None,
                            muted: false,
                            volume_max: None,
                            volume_steps: None,
                            track_info: None,
                            last_seen: std::time::Instant::now(),
                            last_track_uri: None,
                        };

                        s.devices.insert(uuid.clone(), device);
                        drop(s);

                        // Fetch device description
                        let state_clone = state.clone();
                        let http_clone = http.clone();
                        let bus_clone = bus.clone();
                        let uuid_clone = uuid.clone();

                        tokio::spawn(async move {
                            if let Err(e) = Self::fetch_device_info(
                                &state_clone,
                                &http_clone,
                                &uuid_clone,
                                &location,
                            )
                            .await
                            {
                                tracing::warn!(
                                    "Failed to fetch device info for {}: {}",
                                    uuid_clone,
                                    e
                                );
                            }
                            // Emit ZoneDiscovered with full zone info
                            let s = state_clone.read().await;
                            if let Some(device) = s.devices.get(&uuid_clone) {
                                let zone = openhome_device_to_zone(device);
                                bus_clone.publish(BusEvent::ZoneDiscovered { zone });
                            }
                        });
                    }
                }
                Err(e) => {
                    tracing::debug!("SSDP search for {} failed: {}", urn_str, e);
                }
            }
        }

        Ok(())
    }

    async fn fetch_device_info(
        state: &Arc<RwLock<OpenHomeState>>,
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
        }

        let root: Root = xml_from_str(&xml)?;

        let mut s = state.write().await;
        if let Some(device) = s.devices.get_mut(uuid) {
            device.name = root
                .device
                .friendly_name
                .unwrap_or_else(|| format!("OpenHome {}", &uuid[..8.min(uuid.len())]));
            device.manufacturer = root.device.manufacturer;
            device.model = root.device.model_name;

            tracing::info!(
                "Got OpenHome device info: {} - {} {}",
                device.name,
                device.manufacturer.as_deref().unwrap_or("Unknown"),
                device.model.as_deref().unwrap_or("")
            );
        }

        Ok(())
    }

    async fn cleanup_stale(state: &Arc<RwLock<OpenHomeState>>, bus: &SharedBus) {
        let mut s = state.write().await;
        let now = std::time::Instant::now();

        let stale: Vec<String> = s
            .devices
            .iter()
            .filter(|(_, d)| now.duration_since(d.last_seen) > STALE_THRESHOLD)
            .map(|(uuid, _)| uuid.clone())
            .collect();

        for uuid in stale {
            tracing::info!("Removing stale OpenHome device: {}", uuid);
            s.devices.remove(&uuid);
            bus.publish(BusEvent::ZoneRemoved {
                zone_id: PrefixedZoneId::openhome(&uuid),
            });
        }
    }

    async fn poll_loop(
        state: Arc<RwLock<OpenHomeState>>,
        bus: SharedBus,
        http: Client,
        shutdown: CancellationToken,
    ) {
        let mut poll_interval = interval(POLL_INTERVAL);

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    tracing::info!("OpenHome poll loop shutting down");
                    break;
                }
                _ = poll_interval.tick() => {
                    // Get list of devices to poll
                    let devices: Vec<(String, String)> = {
                        let s = state.read().await;
                        s.devices
                            .iter()
                            .map(|(uuid, d)| (uuid.clone(), d.location.clone()))
                            .collect()
                    };

                    for (uuid, location) in devices {
                        if let Err(e) = Self::poll_device(&state, &bus, &http, &uuid, &location).await {
                            tracing::debug!("Failed to poll {}: {}", uuid, e);
                        }
                    }
                }
            }
        }

        tracing::info!("OpenHome poll loop stopped");
    }

    async fn poll_device(
        state: &Arc<RwLock<OpenHomeState>>,
        bus: &SharedBus,
        http: &Client,
        uuid: &str,
        location: &str,
    ) -> anyhow::Result<()> {
        let base_url = Self::get_base_url(location)?;

        // Poll transport state
        let transport_state = Self::soap_call(
            http,
            &format!("{}/Transport", base_url),
            "urn:av-openhome-org:service:Transport:1",
            "TransportState",
            "",
        )
        .await;

        if let Ok(response) = transport_state {
            if let Some(new_state) = Self::extract_xml_value(&response, "Value") {
                let new_state = new_state.to_lowercase();
                let mut s = state.write().await;
                if let Some(device) = s.devices.get_mut(uuid) {
                    if device.state != new_state {
                        device.state = new_state.clone();
                        bus.publish(BusEvent::ZoneUpdated {
                            zone_id: PrefixedZoneId::openhome(uuid),
                            display_name: device.name.clone(),
                            state: new_state,
                        });
                    }
                }
            }
        }

        // Poll volume
        let volume = Self::soap_call(
            http,
            &format!("{}/Volume", base_url),
            "urn:av-openhome-org:service:Volume:1",
            "Volume",
            "",
        )
        .await;

        if let Ok(response) = volume {
            if let Some(vol_str) = Self::extract_xml_value(&response, "Value") {
                if let Ok(vol) = vol_str.parse::<i32>() {
                    let mut s = state.write().await;
                    if let Some(device) = s.devices.get_mut(uuid) {
                        device.volume = Some(vol);
                    }
                }
            }
        }

        // Poll mute state
        let mute = Self::soap_call(
            http,
            &format!("{}/Volume", base_url),
            "urn:av-openhome-org:service:Volume:1",
            "Mute",
            "",
        )
        .await;

        if let Ok(response) = mute {
            if let Some(mute_str) = Self::extract_xml_value(&response, "Value") {
                let is_muted = mute_str == "true" || mute_str == "1";
                let mut s = state.write().await;
                if let Some(device) = s.devices.get_mut(uuid) {
                    device.muted = is_muted;
                }
            }
        }

        // Poll volume characteristics for step calculation
        // See: http://wiki.openhome.org/wiki/Av:Developer:VolumeService
        let characteristics = Self::soap_call(
            http,
            &format!("{}/Volume", base_url),
            "urn:av-openhome-org:service:Volume:1",
            "Characteristics",
            "",
        )
        .await;

        if let Ok(response) = characteristics {
            let mut s = state.write().await;
            if let Some(device) = s.devices.get_mut(uuid) {
                if let Some(max_str) = Self::extract_xml_value(&response, "VolumeMax") {
                    device.volume_max = max_str.parse().ok();
                }
                if let Some(steps_str) = Self::extract_xml_value(&response, "VolumeSteps") {
                    device.volume_steps = steps_str.parse().ok();
                }
            }
        }

        // Poll track info
        let track = Self::soap_call(
            http,
            &format!("{}/Info", base_url),
            "urn:av-openhome-org:service:Info:1",
            "Track",
            "",
        )
        .await;

        if let Ok(response) = track {
            let uri = Self::extract_xml_value(&response, "Uri");
            let metadata = Self::extract_xml_value(&response, "Metadata");

            let mut s = state.write().await;
            if let Some(device) = s.devices.get_mut(uuid) {
                // Only parse if URI changed
                if uri.as_ref() != device.last_track_uri.as_ref() {
                    device.last_track_uri = uri;

                    if let Some(meta) = metadata {
                        // Decode HTML entities and parse DIDL-Lite
                        let decoded = html_decode(&meta);
                        if let Some(track_info) = Self::parse_didl_lite(&decoded) {
                            let title = Some(track_info.title.clone());
                            let artist = Some(track_info.artist.clone());
                            let album = Some(track_info.album.clone());
                            let image_key = track_info.album_art_uri.clone();
                            device.track_info = Some(track_info);
                            bus.publish(BusEvent::NowPlayingChanged {
                                zone_id: PrefixedZoneId::openhome(uuid),
                                title,
                                artist,
                                album,
                                image_key,
                            });
                        }
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

    fn extract_xml_value(xml: &str, tag: &str) -> Option<String> {
        let start_tag = format!("<{}>", tag);
        let end_tag = format!("</{}>", tag);

        let start = xml.find(&start_tag)? + start_tag.len();
        let end = xml[start..].find(&end_tag)? + start;

        Some(xml[start..end].to_string())
    }

    fn parse_didl_lite(xml: &str) -> Option<TrackInfo> {
        // Simple extraction from DIDL-Lite
        let title = Self::extract_xml_value(xml, "dc:title")
            .or_else(|| Self::extract_xml_value(xml, "title"))
            .unwrap_or_default();

        let artist = Self::extract_xml_value(xml, "upnp:artist")
            .or_else(|| Self::extract_xml_value(xml, "dc:creator"))
            .unwrap_or_default();

        let album = Self::extract_xml_value(xml, "upnp:album").unwrap_or_default();

        let album_art_uri = Self::extract_xml_value(xml, "upnp:albumArtURI");

        let genre = Self::extract_xml_value(xml, "upnp:genre");

        Some(TrackInfo {
            title,
            artist,
            album,
            album_art_uri,
            genre,
        })
    }

    /// Stop discovery (internal - use Startable trait)
    async fn stop_internal(&self) {
        // Cancel background tasks first
        self.shutdown.read().await.cancel();

        let mut state = self.state.write().await;
        state.running = false;
        state.devices.clear();
        tracing::info!("OpenHome adapter stopped");
    }

    /// Get adapter status
    pub async fn get_status(&self) -> OpenHomeStatus {
        let state = self.state.read().await;
        OpenHomeStatus {
            connected: !state.devices.is_empty(),
            device_count: state.devices.len(),
            devices: state
                .devices
                .values()
                .map(|d| OpenHomeDeviceSummary {
                    uuid: d.uuid.clone(),
                    name: d.name.clone(),
                    state: d.state.clone(),
                })
                .collect(),
        }
    }

    /// Get all discovered zones
    pub async fn get_zones(&self) -> Vec<OpenHomeZone> {
        let state = self.state.read().await;
        state
            .devices
            .values()
            .map(|d| {
                let device_name = match (&d.manufacturer, &d.model) {
                    (Some(m), Some(model)) => Some(format!("{} {}", m, model)),
                    (Some(m), None) => Some(m.clone()),
                    _ => None,
                };

                OpenHomeZone {
                    zone_id: d.uuid.clone(),
                    zone_name: d.name.clone(),
                    state: d.state.clone(),
                    output_count: 1,
                    output_name: d.name.clone(),
                    device_name,
                    volume_control: Some(VolumeControl {
                        vol_type: "number".to_string(),
                        min: 0,
                        max: d.volume_max.map(|m| m as i32).unwrap_or(100),
                        is_muted: d.muted,
                    }),
                }
            })
            .collect()
    }

    /// Get specific zone by UUID
    pub async fn get_zone(&self, uuid: &str) -> Option<OpenHomeDevice> {
        let state = self.state.read().await;
        state.devices.get(uuid).cloned()
    }

    /// Get now playing info for a zone
    pub async fn get_now_playing(&self, uuid: &str) -> Option<OpenHomeNowPlaying> {
        let state = self.state.read().await;
        let device = state.devices.get(uuid)?;
        let track = device.track_info.as_ref();

        Some(OpenHomeNowPlaying {
            zone_id: uuid.to_string(),
            line1: track
                .map(|t| t.title.clone())
                .unwrap_or_else(|| device.name.clone()),
            line2: track.map(|t| t.artist.clone()).unwrap_or_default(),
            line3: track.map(|t| t.album.clone()).unwrap_or_default(),
            is_playing: device.state == "playing",
            volume: device.volume,
            volume_min: 0,
            volume_max: device.volume_max.map(|m| m as i32).unwrap_or(100),
            seek_position: None,
            length: None,
            image_key: track.and_then(|t| t.album_art_uri.clone()),
        })
    }

    /// Send control command to a zone
    pub async fn control(
        &self,
        uuid: &str,
        action: &str,
        value: Option<i32>,
    ) -> anyhow::Result<()> {
        let location = {
            let state = self.state.read().await;
            state
                .devices
                .get(uuid)
                .map(|d| d.location.clone())
                .ok_or_else(|| anyhow::anyhow!("Device not found: {}", uuid))?
        };

        let base_url = Self::get_base_url(&location)?;
        let transport_url = format!("{}/Transport", base_url);
        let volume_url = format!("{}/Volume", base_url);

        match action {
            "play" => {
                Self::soap_call(
                    &self.http,
                    &transport_url,
                    "urn:av-openhome-org:service:Transport:1",
                    "Play",
                    "",
                )
                .await?;
            }
            "pause" => {
                Self::soap_call(
                    &self.http,
                    &transport_url,
                    "urn:av-openhome-org:service:Transport:1",
                    "Pause",
                    "",
                )
                .await?;
            }
            "play_pause" => {
                let state = self.state.read().await;
                let is_playing = state
                    .devices
                    .get(uuid)
                    .map(|d| d.state == "playing")
                    .unwrap_or(false);
                drop(state);

                let action = if is_playing { "Pause" } else { "Play" };
                Self::soap_call(
                    &self.http,
                    &transport_url,
                    "urn:av-openhome-org:service:Transport:1",
                    action,
                    "",
                )
                .await?;
            }
            "stop" => {
                Self::soap_call(
                    &self.http,
                    &transport_url,
                    "urn:av-openhome-org:service:Transport:1",
                    "Stop",
                    "",
                )
                .await?;
            }
            "next" => {
                Self::soap_call(
                    &self.http,
                    &transport_url,
                    "urn:av-openhome-org:service:Transport:1",
                    "SkipNext",
                    "",
                )
                .await?;
            }
            "previous" | "prev" => {
                Self::soap_call(
                    &self.http,
                    &transport_url,
                    "urn:av-openhome-org:service:Transport:1",
                    "SkipPrevious",
                    "",
                )
                .await?;
            }
            "vol_abs" | "volume" => {
                let vol = value.unwrap_or(50).clamp(0, 100);
                Self::soap_call(
                    &self.http,
                    &volume_url,
                    "urn:av-openhome-org:service:Volume:1",
                    "SetVolume",
                    &format!("<Value>{}</Value>", vol),
                )
                .await?;

                let mut state = self.state.write().await;
                if let Some(device) = state.devices.get_mut(uuid) {
                    device.volume = Some(vol);
                }
            }
            "vol_rel" => {
                let delta = value.unwrap_or(0);
                let current = {
                    let state = self.state.read().await;
                    state.devices.get(uuid).and_then(|d| d.volume).unwrap_or(50)
                };
                let new_vol = (current + delta).clamp(0, 100);

                Self::soap_call(
                    &self.http,
                    &volume_url,
                    "urn:av-openhome-org:service:Volume:1",
                    "SetVolume",
                    &format!("<Value>{}</Value>", new_vol),
                )
                .await?;

                let mut state = self.state.write().await;
                if let Some(device) = state.devices.get_mut(uuid) {
                    device.volume = Some(new_vol);
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
        let location = location.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _ = Self::poll_device(&state, &bus, &http, &uuid, &location).await;
        });

        Ok(())
    }

    /// Fetch album art image
    pub async fn get_image(&self, image_url: &str) -> anyhow::Result<ImageData> {
        if !image_url.starts_with("http://") && !image_url.starts_with("https://") {
            anyhow::bail!("Invalid image URL");
        }

        let response = self.http.get(image_url).send().await?;
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("image/jpeg")
            .to_string();

        let body = response.bytes().await?;

        Ok(ImageData {
            content_type,
            data: body.to_vec(),
        })
    }
}

/// Album art image data
#[derive(Debug)]
pub struct ImageData {
    pub content_type: String,
    pub data: Vec<u8>,
}

/// Decode HTML entities
fn html_decode(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

/// Convert an OpenHome device to a unified Zone representation
fn openhome_device_to_zone(device: &OpenHomeDevice) -> Zone {
    Zone {
        zone_id: format!("openhome:{}", device.uuid),
        zone_name: device.name.clone(),
        state: PlaybackState::from(device.state.as_str()),
        volume_control: device.volume.map(|v| {
            // Calculate step from Characteristics: step = VolumeMax / VolumeSteps
            // See: http://wiki.openhome.org/wiki/Av:Developer:VolumeService
            let step = match (device.volume_max, device.volume_steps) {
                (Some(max), Some(steps)) if steps > 0 => max as f32 / steps as f32,
                _ => 1.0, // Default if Characteristics not available
            };
            BusVolumeControl {
                value: v as f32,
                min: 0.0,
                max: device.volume_max.map(|m| m as f32).unwrap_or(100.0),
                step,
                is_muted: device.muted,
                scale: crate::bus::VolumeScale::Percentage,
                // Use prefixed output_id for consistent aggregator matching
                output_id: Some(format!("openhome:{}", device.uuid)),
            }
        }),
        now_playing: device.track_info.as_ref().map(|t| crate::bus::NowPlaying {
            title: t.title.clone(),
            artist: t.artist.clone(),
            album: t.album.clone(),
            image_key: t.album_art_uri.clone(),
            seek_position: None,
            duration: None,
            metadata: None,
        }),
        source: "openhome".to_string(),
        is_controllable: true,
        is_seekable: false, // OpenHome seek support varies
        last_updated: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        is_play_allowed: device.state != "playing",
        is_pause_allowed: device.state == "playing",
        is_next_allowed: true,
        is_previous_allowed: true,
    }
}

// AdapterLogic implementation for unified retry handling
#[async_trait]
impl AdapterLogic for OpenHomeAdapter {
    fn prefix(&self) -> &'static str {
        "openhome"
    }

    async fn run(&self, ctx: AdapterContext) -> Result<()> {
        // Mark as running
        {
            let mut state = self.state.write().await;
            state.running = true;
        }

        // Run discovery and poll loops concurrently with shutdown check
        let discovery_state = self.state.clone();
        let discovery_bus = self.bus.clone();
        let discovery_http = self.http.clone();
        let discovery_shutdown = ctx.shutdown.clone();

        let poll_state = self.state.clone();
        let poll_bus = self.bus.clone();
        let poll_http = self.http.clone();
        let poll_shutdown = ctx.shutdown.clone();

        tokio::select! {
            _ = ctx.shutdown.cancelled() => {
                tracing::info!("OpenHome adapter received shutdown signal");
            }
            _ = Self::discovery_loop(discovery_state, discovery_bus, discovery_http, discovery_shutdown) => {
                tracing::info!("OpenHome discovery loop ended");
            }
            _ = Self::poll_loop(poll_state, poll_bus, poll_http, poll_shutdown) => {
                tracing::info!("OpenHome poll loop ended");
            }
        }

        // Clean up state on exit
        {
            let mut state = self.state.write().await;
            state.running = false;
            state.devices.clear();
        }

        tracing::info!("OpenHome adapter stopped");
        Ok(())
    }

    async fn handle_command(
        &self,
        zone_id: &str,
        command: AdapterCommand,
    ) -> Result<AdapterCommandResponse> {
        // Strip prefix if present
        let uuid = zone_id.strip_prefix("openhome:").unwrap_or(zone_id);

        let result = match command {
            AdapterCommand::Play => self.control(uuid, "play", None).await,
            AdapterCommand::Pause => self.control(uuid, "pause", None).await,
            AdapterCommand::PlayPause => self.control(uuid, "play_pause", None).await,
            AdapterCommand::Stop => self.control(uuid, "stop", None).await,
            AdapterCommand::Next => self.control(uuid, "next", None).await,
            AdapterCommand::Previous => self.control(uuid, "previous", None).await,
            AdapterCommand::VolumeAbsolute(v) => self.control(uuid, "vol_abs", Some(v)).await,
            AdapterCommand::VolumeRelative(v) => self.control(uuid, "vol_rel", Some(v)).await,
            AdapterCommand::Mute(_) => {
                // OpenHome mute not directly supported via this adapter yet
                return Ok(AdapterCommandResponse {
                    success: false,
                    error: Some("Mute not supported by OpenHome adapter".to_string()),
                });
            }
        };

        match result {
            Ok(()) => Ok(AdapterCommandResponse {
                success: true,
                error: None,
            }),
            Err(e) => Ok(AdapterCommandResponse {
                success: false,
                error: Some(e.to_string()),
            }),
        }
    }
}

// Startable trait implementation via macro
crate::impl_startable!(OpenHomeAdapter, "openhome");
