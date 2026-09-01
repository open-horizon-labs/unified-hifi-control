//! UPnP/DLNA adapter - discovers and controls UPnP Media Renderers
//!
//! Uses SSDP for discovery and UPnP AV Transport service for control.
//!
//! Track metadata comes from `AVTransport::GetPositionInfo`, whose
//! `TrackMetaData` carries DIDL-Lite — the same format OpenHome uses, parsed by
//! the shared [`crate::adapters::didl`] module. The real difference from
//! OpenHome is the smaller set of *control* actions, not metadata.

use crate::adapters::didl::{self, TrackInfo};
use crate::adapters::handle::{AdapterHandle, RetryConfig};
use crate::adapters::traits::{
    AdapterCommand, AdapterCommandResponse, AdapterContext, AdapterLogic,
};
use crate::bus::{
    runtime::{
        CommandEndpoint, CommandGateway, NativeResult, ProjectionEntry, ProjectionIngress,
        ProjectionKind, ProjectionPayload, ProjectionSource, ProjectionUpdate, RuntimeCommand,
    },
    BusEvent, Command, PlaybackState, PrefixedZoneId, SharedBus, VolumeControl as BusVolumeControl,
    Zone,
};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures::StreamExt;
use quick_xml::de::from_str as xml_from_str;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use ssdp_client::{SearchTarget, URN};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio::time::interval;
use tokio_util::sync::CancellationToken;

const MEDIA_RENDERER_URN: &str = "urn:schemas-upnp-org:device:MediaRenderer:1";
const AV_TRANSPORT_URN: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RENDERING_CONTROL_URN: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
const SSDP_SEARCH_INTERVAL: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const STALE_THRESHOLD: Duration = Duration::from_secs(90);
const SOAP_TIMEOUT: Duration = Duration::from_secs(5);

/// Transport actions [`UPnPAdapter::control`] refuses outright.
///
/// # Why this is a const and not just two match arms
///
/// `AVTransport:1` — the service this adapter already speaks — *does* define
/// `Next` and `Previous` actions. A pure renderer holds no playlist, so most
/// devices would reject the call, but that is the device's answer to give and
/// UHC's refusal pre-empts it. So this is a UHC choice, not a protocol limit, and
/// `crate::mcp::capabilities` reads this list to report the `transport_skip`
/// capability as *not yet implemented* rather than as a provider limitation.
///
/// The match arm in `control` is driven by this same list, so the capability
/// report cannot drift from the behavior: implementing skip means removing the
/// entries, and the reported capability flips with them.
pub const REFUSED_TRANSPORT_ACTIONS: &[&str] = &["next", "previous", "prev"];

/// The refusal sentence for a skip action, unchanged from before it was derived
/// from [`REFUSED_TRANSPORT_ACTIONS`].
fn refused_transport_action_message(action: &str) -> String {
    let what = if action == "next" {
        "Next track"
    } else {
        "Previous track"
    };
    format!("{} not supported by pure UPnP renderers", what)
}

/// Strip "upnp:" prefix from renderer UUIDs.
/// MCP and aggregator use prefixed IDs, but UPnP API expects bare UUIDs.
fn strip_upnp_prefix(id: &str) -> &str {
    id.strip_prefix("upnp:").unwrap_or(id)
}

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
    pub track_info: Option<TrackInfo>,
    #[serde(skip)]
    pub av_transport_url: Option<String>,
    #[serde(skip)]
    pub rendering_control_url: Option<String>,
    /// Last seen TrackURI, used to skip re-parsing unchanged metadata.
    #[serde(skip)]
    pub last_track_uri: Option<String>,
    /// Last seen raw TrackMetaData. Internet radio and many DLNA servers keep
    /// one stream URI for a whole session and change only the metadata, so the
    /// URI alone cannot tell us whether the track changed.
    #[serde(skip)]
    pub last_track_metadata: Option<String>,
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
#[derive(Clone)]
pub struct UPnPAdapter {
    state: Arc<RwLock<UPnPState>>,
    bus: SharedBus,
    http: Client,
    /// Wrapped in RwLock to allow creating fresh token on restart
    shutdown: Arc<RwLock<CancellationToken>>,
    /// Composed-server-only reliable lanes. When present, adapter observations
    /// never mutate visible state through the lossy notification broadcast.
    runtime_bridge: Option<Arc<UPnPRuntimeBridge>>,
}

/// One ordered source for UPnP discovery, polling, removal, and command
/// readbacks. The aggregator owns all canonical state; this owns admission only.
#[derive(Clone)]
pub struct UPnPRuntimeBridge {
    ingress: ProjectionIngress,
    commands: CommandGateway,
    publication: Arc<Mutex<u64>>,
    publication_gate: Arc<Semaphore>,
}

impl UPnPRuntimeBridge {
    pub fn new(ingress: ProjectionIngress, commands: CommandGateway) -> Self {
        Self {
            ingress,
            commands,
            publication: Arc::new(Mutex::new(0)),
            publication_gate: Arc::new(Semaphore::new(1)),
        }
    }

    fn commands(&self) -> CommandGateway {
        self.commands.clone()
    }

    async fn publish_zone(
        &self,
        zone: Zone,
        caused_by: Option<crate::bus::runtime::CommandId>,
    ) -> Result<()> {
        let _permit = self
            .publication_gate
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("UPnP reliable publication lane closed"))?;
        let sequence = {
            let mut sequence = self.publication.lock().await;
            *sequence = sequence.saturating_add(1);
            *sequence
        };
        self.ingress
            .submit(ProjectionUpdate {
                source: ProjectionSource {
                    adapter: "upnp".to_string(),
                    instance: None,
                    epoch: 0,
                },
                sequence,
                kind: ProjectionKind::Snapshot,
                caused_by,
                entries: vec![ProjectionEntry {
                    key: format!("zone:{}", zone.zone_id),
                    payload: ProjectionPayload::Zone(Box::new(zone)),
                }],
            })
            .await
            .map_err(|_| anyhow!("UPnP reliable projection ingress stopped"))?;
        Ok(())
    }

    async fn publish_removed(&self, zone_id: PrefixedZoneId) -> Result<()> {
        let _permit = self
            .publication_gate
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("UPnP reliable publication lane closed"))?;
        let sequence = {
            let mut sequence = self.publication.lock().await;
            *sequence = sequence.saturating_add(1);
            *sequence
        };
        self.ingress
            .submit(ProjectionUpdate {
                source: ProjectionSource {
                    adapter: "upnp".to_string(),
                    instance: None,
                    epoch: 0,
                },
                sequence,
                kind: ProjectionKind::Snapshot,
                caused_by: None,
                entries: vec![ProjectionEntry {
                    key: format!("zone:{zone_id}"),
                    payload: ProjectionPayload::ZoneRemoved { zone_id },
                }],
            })
            .await
            .map_err(|_| anyhow!("UPnP reliable projection ingress stopped"))?;
        Ok(())
    }
}

impl UPnPAdapter {
    /// Create new UPnP adapter
    pub fn new(bus: SharedBus) -> Self {
        Self::new_with_runtime(bus, None)
    }

    /// Compose with private reliable command/projection lanes while preserving
    /// legacy standalone construction for existing embedders and fixtures.
    pub fn new_with_runtime(
        bus: SharedBus,
        runtime_bridge: Option<Arc<UPnPRuntimeBridge>>,
    ) -> Self {
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
            runtime_bridge,
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

        // Create AdapterHandle and spawn with retry
        let adapter = self.clone();
        let bus = self.bus.clone();

        tokio::spawn(async move {
            let handle = AdapterHandle::new(adapter, bus, shutdown);
            handle.run_with_retry(RetryConfig::default()).await
        });

        tracing::info!("UPnP adapter started");
        Ok(())
    }

    async fn discovery_loop(
        state: Arc<RwLock<UPnPState>>,
        bus: SharedBus,
        http: Client,
        shutdown: CancellationToken,
        runtime_bridge: Option<Arc<UPnPRuntimeBridge>>,
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
                    if let Err(e) = Self::perform_search(&state, &bus, &http, runtime_bridge.clone()).await {
                        tracing::warn!("SSDP search failed: {}", e);
                    }

                    // Cleanup stale renderers
                    Self::cleanup_stale(&state, &bus, runtime_bridge.clone()).await;
                }
            }
        }

        tracing::info!("UPnP discovery loop stopped");
    }

    async fn perform_search(
        state: &Arc<RwLock<UPnPState>>,
        bus: &SharedBus,
        http: &Client,
        runtime_bridge: Option<Arc<UPnPRuntimeBridge>>,
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
                track_info: None,
                last_track_uri: None,
                last_track_metadata: None,
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
            let bridge_clone = runtime_bridge.clone();
            let uuid_clone = uuid.clone();

            tokio::spawn(async move {
                if let Err(e) =
                    Self::fetch_device_info(&state_clone, &http_clone, &uuid_clone, &location).await
                {
                    tracing::warn!("Failed to fetch device info for {}: {}", uuid_clone, e);
                }
                // Emit ZoneDiscovered with full zone info
                let zone = {
                    let renderers = state_clone.read().await;
                    renderers
                        .renderers
                        .get(&uuid_clone)
                        .map(upnp_renderer_to_zone)
                };
                if let Some(zone) = zone {
                    if let Some(bridge) = bridge_clone {
                        if let Err(error) = bridge.publish_zone(zone, None).await {
                            tracing::warn!(%error, "UPnP discovery projection failed");
                        }
                    } else {
                        bus_clone.publish(BusEvent::ZoneDiscovered { zone });
                    }
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

    async fn cleanup_stale(
        state: &Arc<RwLock<UPnPState>>,
        bus: &SharedBus,
        runtime_bridge: Option<Arc<UPnPRuntimeBridge>>,
    ) {
        let removed: Vec<PrefixedZoneId> = {
            let mut renderers = state.write().await;
            let now = std::time::Instant::now();
            let stale: Vec<String> = renderers
                .renderers
                .iter()
                .filter(|(_, r)| now.duration_since(r.last_seen) > STALE_THRESHOLD)
                .map(|(uuid, _)| uuid.clone())
                .collect();
            stale
                .into_iter()
                .map(|uuid| {
                    tracing::info!("Removing stale UPnP renderer: {}", uuid);
                    renderers.renderers.remove(&uuid);
                    PrefixedZoneId::upnp(uuid)
                })
                .collect()
        };
        for zone_id in removed {
            if let Some(bridge) = runtime_bridge.as_ref() {
                if let Err(error) = bridge.publish_removed(zone_id).await {
                    tracing::warn!(%error, "UPnP stale-removal projection failed");
                }
            } else {
                bus.publish(BusEvent::ZoneRemoved { zone_id });
            }
        }
    }

    async fn poll_loop(
        state: Arc<RwLock<UPnPState>>,
        bus: SharedBus,
        http: Client,
        shutdown: CancellationToken,
        runtime_bridge: Option<Arc<UPnPRuntimeBridge>>,
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
                            runtime_bridge.clone(),
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
        runtime_bridge: Option<Arc<UPnPRuntimeBridge>>,
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
                if let Some(new_state) = didl::extract_xml_value(&response, "CurrentTransportState")
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
                            if runtime_bridge.is_none() {
                                bus.publish(BusEvent::ZoneUpdated {
                                    zone_id: PrefixedZoneId::upnp(uuid),
                                    display_name: renderer.name.clone(),
                                    state: new_state,
                                });
                            }
                        }
                    }
                }
            }
        }

        // Poll track metadata. AVTransport::GetPositionInfo carries DIDL-Lite in
        // TrackMetaData, XML-escaped inside the SOAP envelope. Re-parsed only
        // when TrackURI changes, mirroring the OpenHome adapter so a steady
        // stream does not re-parse on every tick.
        if let Some(url) = av_url {
            let position_info = Self::soap_call(
                http,
                url,
                AV_TRANSPORT_URN,
                "GetPositionInfo",
                "<InstanceID>0</InstanceID>",
            )
            .await;

            if let Ok(response) = position_info {
                let track_uri =
                    didl::extract_xml_value(&response, "TrackURI").filter(|u| !u.is_empty());
                let metadata =
                    didl::extract_xml_value(&response, "TrackMetaData").filter(|m| !m.is_empty());

                let mut s = state.write().await;
                if let Some(renderer) = s.renderers.get_mut(uuid) {
                    // Compare metadata too: a constant TrackURI (internet radio,
                    // many DLNA servers) would otherwise pin now-playing to the
                    // first track for the whole session.
                    let changed = renderer.last_track_uri.as_deref() != track_uri.as_deref()
                        || renderer.last_track_metadata.as_deref() != metadata.as_deref();
                    if changed {
                        renderer.last_track_uri = track_uri;
                        renderer.last_track_metadata = metadata.clone();

                        match metadata {
                            Some(meta) => {
                                let decoded = didl::html_decode(&meta);
                                if let Some(track) = didl::parse_didl_lite(&decoded) {
                                    let (title, artist, album) = (
                                        Some(track.title.clone()),
                                        Some(track.artist.clone()),
                                        Some(track.album.clone()),
                                    );
                                    let image_key = track.album_art_uri.clone();
                                    renderer.track_info = Some(track);
                                    if runtime_bridge.is_none() {
                                        bus.publish(BusEvent::NowPlayingChanged {
                                            zone_id: PrefixedZoneId::upnp(uuid),
                                            title,
                                            artist,
                                            album,
                                            image_key,
                                        });
                                    }
                                }
                            }
                            // A renderer that reports no metadata (commonly when
                            // stopped) clears rather than retaining a stale track.
                            None => {
                                if renderer.track_info.take().is_some() && runtime_bridge.is_none()
                                {
                                    bus.publish(BusEvent::NowPlayingChanged {
                                        zone_id: PrefixedZoneId::upnp(uuid),
                                        title: None,
                                        artist: None,
                                        album: None,
                                        image_key: None,
                                    });
                                }
                            }
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
                if let Some(vol_str) = didl::extract_xml_value(&response, "CurrentVolume") {
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
                if let Some(mute_str) = didl::extract_xml_value(&response, "CurrentMute") {
                    let mut s = state.write().await;
                    if let Some(renderer) = s.renderers.get_mut(uuid) {
                        renderer.muted = mute_str == "1" || mute_str.eq_ignore_ascii_case("true");
                    }
                }
            }
        }

        if let Some(bridge) = runtime_bridge {
            let zone = {
                let renderers = state.read().await;
                renderers
                    .renderers
                    .get(uuid)
                    .map(upnp_renderer_to_zone)
                    .ok_or_else(|| anyhow!("Renderer not found: {}", uuid))?
            };
            bridge.publish_zone(zone, None).await?;
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
            .await?
            // A SOAP fault is usually an HTTP error. Treating its body as a
            // successful read is how a failed renderer command could otherwise
            // be confirmed from stale local state.
            .error_for_status()?;

        Ok(response.text().await?)
    }

    /// Extract XML value, handling optional namespace prefixes (e.g., <u:Volume> or <Volume>)
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
                    // track_metadata is now read from GetPositionInfo, so it
                    // must not be advertised as unsupported - clients that read
                    // this list would hide it. album_art stays: the URI is
                    // parsed, but UHC does not yet fetch or proxy the bytes
                    // (#418).
                    unsupported: vec![
                        "next".to_string(),
                        "previous".to_string(),
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
        let uuid = strip_upnp_prefix(uuid);
        let state = self.state.read().await;
        state.renderers.get(uuid).cloned()
    }

    /// Register a renderer and poll it once, as SSDP discovery followed by the
    /// poll loop would. Exists so tests can drive the real poll path against a
    /// mock renderer; SSDP cannot reach one.
    #[doc(hidden)]
    pub async fn probe_renderer_for_test(
        &self,
        uuid: &str,
        name: &str,
        av_url: &str,
        rc_url: &str,
    ) -> anyhow::Result<()> {
        {
            let mut s = self.state.write().await;
            s.renderers
                .entry(uuid.to_string())
                .or_insert_with(|| UPnPRenderer {
                    uuid: uuid.to_string(),
                    name: name.to_string(),
                    manufacturer: None,
                    model: None,
                    location: av_url.to_string(),
                    state: "stopped".to_string(),
                    volume: None,
                    muted: false,
                    track_info: None,
                    last_seen: std::time::Instant::now(),
                    av_transport_url: Some(av_url.to_string()),
                    rendering_control_url: Some(rc_url.to_string()),
                    last_track_uri: None,
                    last_track_metadata: None,
                });
        }
        Self::poll_renderer(
            &self.state,
            &self.bus,
            &self.http,
            uuid,
            Some(av_url),
            Some(rc_url),
            self.runtime_bridge.clone(),
        )
        .await
    }

    /// Get now playing info for a renderer
    pub async fn get_now_playing(&self, uuid: &str) -> Option<UPnPNowPlaying> {
        let uuid = strip_upnp_prefix(uuid);
        let state = self.state.read().await;
        let renderer = state.renderers.get(uuid)?;

        // Track metadata comes from AVTransport::GetPositionInfo when the
        // renderer supplies it; otherwise fall back to the renderer's own name,
        // which is all this returned before metadata was wired.
        let track = renderer.track_info.as_ref();
        Some(UPnPNowPlaying {
            zone_id: uuid.to_string(),
            line1: track
                .map(|t| t.title.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| renderer.name.clone()),
            line2: track.map(|t| t.artist.clone()).unwrap_or_default(),
            line3: track.map(|t| t.album.clone()).unwrap_or_default(),
            is_playing: renderer.state == "playing",
            volume: renderer.volume,
            volume_min: 0,
            volume_max: 100,
            seek_position: None,
            length: None,
            image_key: track.and_then(|t| t.album_art_uri.clone()),
        })
    }

    /// Read every renderer fact represented in a Zone after a native write.
    /// Unlike the periodic poller, a command confirmation requires each SOAP
    /// read to succeed: accepted bytes plus a stale cache is never success.
    async fn coherent_zone_readback(&self, uuid: &str) -> Result<Zone> {
        let uuid = strip_upnp_prefix(uuid);
        let (av_url, rc_url) = {
            let state = self.state.read().await;
            let renderer = state
                .renderers
                .get(uuid)
                .ok_or_else(|| anyhow!("Renderer not found: {}", uuid))?;
            (
                renderer
                    .av_transport_url
                    .clone()
                    .ok_or_else(|| anyhow!("No AVTransport URL"))?,
                renderer
                    .rendering_control_url
                    .clone()
                    .ok_or_else(|| anyhow!("No RenderingControl URL"))?,
            )
        };
        let transport = Self::soap_call(
            &self.http,
            &av_url,
            AV_TRANSPORT_URN,
            "GetTransportInfo",
            "<InstanceID>0</InstanceID>",
        )
        .await?;
        let state =
            didl::extract_xml_value(&transport, "CurrentTransportState").ok_or_else(|| {
                anyhow!("UPnP GetTransportInfo response had no CurrentTransportState")
            })?;
        let state = match state.as_str() {
            "PLAYING" => "playing",
            "PAUSED_PLAYBACK" => "paused",
            "STOPPED" => "stopped",
            "TRANSITIONING" => "loading",
            _ => "stopped",
        }
        .to_string();
        let position = Self::soap_call(
            &self.http,
            &av_url,
            AV_TRANSPORT_URN,
            "GetPositionInfo",
            "<InstanceID>0</InstanceID>",
        )
        .await?;
        let track_uri =
            didl::extract_xml_value(&position, "TrackURI").filter(|uri| !uri.is_empty());
        let metadata =
            didl::extract_xml_value(&position, "TrackMetaData").filter(|meta| !meta.is_empty());
        let track_info = metadata
            .as_deref()
            .map(didl::html_decode)
            .and_then(|decoded| didl::parse_didl_lite(&decoded));
        let volume = Self::soap_call(
            &self.http,
            &rc_url,
            RENDERING_CONTROL_URN,
            "GetVolume",
            "<InstanceID>0</InstanceID><Channel>Master</Channel>",
        )
        .await?;
        let volume = didl::extract_xml_value(&volume, "CurrentVolume")
            .ok_or_else(|| anyhow!("UPnP GetVolume response had no CurrentVolume"))?
            .parse::<i32>()?;
        let mute = Self::soap_call(
            &self.http,
            &rc_url,
            RENDERING_CONTROL_URN,
            "GetMute",
            "<InstanceID>0</InstanceID><Channel>Master</Channel>",
        )
        .await?;
        let muted = didl::extract_xml_value(&mute, "CurrentMute")
            .ok_or_else(|| anyhow!("UPnP GetMute response had no CurrentMute"))?;
        let muted = muted == "1" || muted.eq_ignore_ascii_case("true");

        let mut renderers = self.state.write().await;
        let renderer = renderers
            .renderers
            .get_mut(uuid)
            .ok_or_else(|| anyhow!("Renderer not found: {}", uuid))?;
        renderer.state = state;
        renderer.volume = Some(volume);
        renderer.muted = muted;
        renderer.last_track_uri = track_uri;
        renderer.last_track_metadata = metadata;
        renderer.track_info = track_info;
        Ok(upnp_renderer_to_zone(renderer))
    }

    /// Issue only the native SOAP write. Cache changes are reserved for an
    /// authoritative observation, either periodic polling or a required command
    /// readback in the runtime endpoint.
    async fn execute_control_native(
        &self,
        uuid: &str,
        action: &str,
        value: Option<i32>,
    ) -> Result<()> {
        let uuid = strip_upnp_prefix(uuid);
        let (av_url, rc_url, current_volume, is_playing) = {
            let state = self.state.read().await;
            let renderer = state
                .renderers
                .get(uuid)
                .ok_or_else(|| anyhow!("Renderer not found: {}", uuid))?;
            (
                renderer.av_transport_url.clone(),
                renderer.rendering_control_url.clone(),
                renderer.volume,
                renderer.state == "playing",
            )
        };
        match action {
            "play" => {
                Self::soap_call(
                    &self.http,
                    av_url
                        .as_deref()
                        .ok_or_else(|| anyhow!("No AVTransport URL"))?,
                    AV_TRANSPORT_URN,
                    "Play",
                    "<InstanceID>0</InstanceID><Speed>1</Speed>",
                )
                .await?;
            }
            "pause" => {
                Self::soap_call(
                    &self.http,
                    av_url
                        .as_deref()
                        .ok_or_else(|| anyhow!("No AVTransport URL"))?,
                    AV_TRANSPORT_URN,
                    "Pause",
                    "<InstanceID>0</InstanceID>",
                )
                .await?;
            }
            "play_pause" => {
                let (action, body) = if is_playing {
                    ("Pause", "<InstanceID>0</InstanceID>")
                } else {
                    ("Play", "<InstanceID>0</InstanceID><Speed>1</Speed>")
                };
                Self::soap_call(
                    &self.http,
                    av_url
                        .as_deref()
                        .ok_or_else(|| anyhow!("No AVTransport URL"))?,
                    AV_TRANSPORT_URN,
                    action,
                    body,
                )
                .await?;
            }
            "stop" => {
                Self::soap_call(
                    &self.http,
                    av_url
                        .as_deref()
                        .ok_or_else(|| anyhow!("No AVTransport URL"))?,
                    AV_TRANSPORT_URN,
                    "Stop",
                    "<InstanceID>0</InstanceID>",
                )
                .await?;
            }
            refused if REFUSED_TRANSPORT_ACTIONS.contains(&refused) => {
                anyhow::bail!("{}", refused_transport_action_message(refused));
            }
            "vol_abs" | "volume" | "vol_rel" => {
                let desired = match action {
                    "vol_rel" => {
                        // The last observer snapshot is the only local input;
                        // the readback below remains the confirmation authority.
                        (current_volume.unwrap_or(50) + value.unwrap_or(0)).clamp(0, 100)
                    }
                    _ => value.unwrap_or(50).clamp(0, 100),
                };
                Self::soap_call(
                    &self.http,
                    rc_url
                        .as_deref()
                        .ok_or_else(|| anyhow!("No RenderingControl URL"))?,
                    RENDERING_CONTROL_URN,
                    "SetVolume",
                    &format!("<InstanceID>0</InstanceID><Channel>Master</Channel><DesiredVolume>{desired}</DesiredVolume>"),
                )
                .await?;
            }
            "mute" => {
                let mute = value.map(|v| v != 0).unwrap_or(true);
                Self::soap_call(
                    &self.http,
                    rc_url
                        .as_deref()
                        .ok_or_else(|| anyhow!("No RenderingControl URL"))?,
                    RENDERING_CONTROL_URN,
                    "SetMute",
                    &format!("<InstanceID>0</InstanceID><Channel>Master</Channel><DesiredMute>{}</DesiredMute>", if mute { "1" } else { "0" }),
                )
                .await?;
            }
            _ => anyhow::bail!("Unknown action: {}", action),
        }
        Ok(())
    }

    /// Send control command to a renderer
    pub async fn control(
        &self,
        uuid: &str,
        action: &str,
        value: Option<i32>,
    ) -> anyhow::Result<()> {
        let uuid = strip_upnp_prefix(uuid);
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
        self.execute_control_native(uuid, action, value).await?;

        // Trigger immediate poll
        let state = self.state.clone();
        let bus = self.bus.clone();
        let http = self.http.clone();
        let uuid = uuid.to_string();
        let runtime_bridge = self.runtime_bridge.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let _ = Self::poll_renderer(
                &state,
                &bus,
                &http,
                &uuid,
                av_url.as_deref(),
                rc_url.as_deref(),
                runtime_bridge,
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
            // Use prefixed output_id for consistent aggregator matching
            output_id: Some(format!("upnp:{}", renderer.uuid)),
        }),
        now_playing: renderer
            .track_info
            .as_ref()
            .map(|t| crate::bus::NowPlaying {
                title: t.title.clone(),
                artist: t.artist.clone(),
                album: t.album.clone(),
                image_key: t.album_art_uri.clone(),
                seek_position: None,
                duration: None,
                metadata: None,
                repeat_mode: None,
                shuffle: None,
            }),
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

#[async_trait]
impl AdapterLogic for UPnPAdapter {
    fn prefix(&self) -> &'static str {
        "upnp"
    }

    async fn run(&self, ctx: AdapterContext) -> Result<()> {
        // Mark as running
        {
            let mut state = self.state.write().await;
            state.running = true;
        }

        // One provider endpoint serializes native writes. A SOAP acknowledgement
        // is only acceptance; the matching coherent Zone projection confirms it.
        let command_shutdown = ctx.shutdown.child_token();
        let command_join = self.runtime_bridge.as_ref().and_then(|bridge| {
            match bridge.commands().register_provider("upnp", 32) {
                Ok(endpoint) => {
                    let adapter = self.clone();
                    let bridge = bridge.clone();
                    let shutdown = command_shutdown.clone();
                    Some(tokio::spawn(async move {
                        run_upnp_command_endpoint(adapter, endpoint, shutdown, bridge).await;
                    }))
                }
                Err(error) => {
                    tracing::error!(?error, "UPnP reliable endpoint registration failed");
                    None
                }
            }
        });

        // Run discovery and poll loops concurrently with shutdown check
        let state = self.state.clone();
        let bus = ctx.bus.clone();
        let http = self.http.clone();
        let shutdown = ctx.shutdown.clone();

        let discovery_state = state.clone();
        let discovery_bus = bus.clone();
        let discovery_http = http.clone();
        let discovery_shutdown = shutdown.clone();
        let discovery_bridge = self.runtime_bridge.clone();

        let poll_state = state.clone();
        let poll_bus = bus.clone();
        let poll_http = http.clone();
        let poll_shutdown = shutdown.clone();
        let poll_bridge = self.runtime_bridge.clone();

        tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::info!("UPnP adapter shutting down");
            }
            _ = async {
                tokio::join!(
                    Self::discovery_loop(discovery_state, discovery_bus, discovery_http, discovery_shutdown, discovery_bridge),
                    Self::poll_loop(poll_state, poll_bus, poll_http, poll_shutdown, poll_bridge)
                );
            } => {}
        }

        command_shutdown.cancel();
        if let Some(join) = command_join {
            if let Err(error) = join.await {
                tracing::warn!(%error, "UPnP reliable command endpoint failed to join");
            }
        }

        // Cleanup state on exit
        {
            let mut state = self.state.write().await;
            state.running = false;
            state.renderers.clear();
        }

        Ok(())
    }

    async fn handle_command(
        &self,
        zone_id: &str,
        command: AdapterCommand,
    ) -> Result<AdapterCommandResponse> {
        // Strip "upnp:" prefix if present (bus/aggregator uses prefixed IDs)
        let uuid = zone_id.strip_prefix("upnp:").unwrap_or(zone_id);

        let result = match command {
            AdapterCommand::Play => self.control(uuid, "play", None).await,
            AdapterCommand::Pause => self.control(uuid, "pause", None).await,
            AdapterCommand::PlayPause => self.control(uuid, "play_pause", None).await,
            AdapterCommand::Stop => self.control(uuid, "stop", None).await,
            AdapterCommand::Next => {
                return Ok(AdapterCommandResponse {
                    success: false,
                    error: Some("Next track not supported by pure UPnP renderers".to_string()),
                });
            }
            AdapterCommand::Previous => {
                return Ok(AdapterCommandResponse {
                    success: false,
                    error: Some("Previous track not supported by pure UPnP renderers".to_string()),
                });
            }
            AdapterCommand::VolumeAbsolute(vol) => self.control(uuid, "vol_abs", Some(vol)).await,
            AdapterCommand::VolumeRelative(delta) => {
                self.control(uuid, "vol_rel", Some(delta)).await
            }
            AdapterCommand::Mute(mute) => {
                self.control(uuid, "mute", Some(if mute { 1 } else { 0 }))
                    .await
            }
            AdapterCommand::SetRepeat(_) | AdapterCommand::SetShuffle(_) => {
                return Ok(AdapterCommandResponse {
                    success: false,
                    error: Some(
                        "Repeat and shuffle are not implemented by the UPnP adapter".to_string(),
                    ),
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
crate::impl_startable!(UPnPAdapter, "upnp");

/// Native acknowledgement crosses exactly one confirmation edge: the complete
/// SOAP readback committed by [`UPnPRuntimeBridge`].
async fn run_upnp_command_endpoint(
    adapter: UPnPAdapter,
    mut endpoint: CommandEndpoint,
    shutdown: CancellationToken,
    bridge: Arc<UPnPRuntimeBridge>,
) {
    loop {
        let work = tokio::select! {
            _ = shutdown.cancelled() => break,
            work = endpoint.recv() => work,
        };
        let Some(work) = work else { break };
        let permit = match work.begin_dispatch() {
            Ok(permit) => permit,
            Err(_) => continue,
        };
        let command_id = permit.id();
        let target = permit.request().target.to_string();
        let uuid = strip_upnp_prefix(&target).to_string();
        let native = match permit.request().command.clone() {
            RuntimeCommand::Control(command) => {
                execute_upnp_runtime_command(&adapter, &uuid, command).await
            }
            RuntimeCommand::Hqplayer(_) => {
                Err(anyhow!("HQPlayer runtime command routed to UPnP endpoint"))
            }
        };
        if let Err(error) = native {
            permit.complete_native(NativeResult::Failed(error.to_string()));
            continue;
        }
        permit.complete_native(NativeResult::Accepted);
        match adapter.coherent_zone_readback(&uuid).await {
            Ok(zone) => {
                if let Err(error) = bridge.publish_zone(zone, Some(command_id)).await {
                    tracing::warn!(%error, command_id = command_id.get(), "UPnP native write accepted but readback projection could not commit");
                }
            }
            Err(error) => {
                tracing::warn!(%error, command_id = command_id.get(), "UPnP native write accepted but coherent readback failed");
            }
        }
    }
}

async fn execute_upnp_runtime_command(
    adapter: &UPnPAdapter,
    uuid: &str,
    command: Command,
) -> Result<()> {
    match command {
        Command::Play => adapter.execute_control_native(uuid, "play", None).await,
        Command::Pause => adapter.execute_control_native(uuid, "pause", None).await,
        Command::PlayPause => {
            adapter
                .execute_control_native(uuid, "play_pause", None)
                .await
        }
        Command::Stop => adapter.execute_control_native(uuid, "stop", None).await,
        Command::Next => adapter.execute_control_native(uuid, "next", None).await,
        Command::Previous => adapter.execute_control_native(uuid, "previous", None).await,
        Command::VolumeAbsolute { value, output_id } if output_id.is_none() => {
            adapter
                .execute_control_native(uuid, "vol_abs", Some(value.round() as i32))
                .await
        }
        Command::VolumeRelative { delta, output_id } if output_id.is_none() => {
            adapter
                .execute_control_native(uuid, "vol_rel", Some(delta.round() as i32))
                .await
        }
        Command::Mute { muted, output_id } if output_id.is_none() => {
            adapter
                .execute_control_native(uuid, "mute", Some(if muted { 1 } else { 0 }))
                .await
        }
        Command::MuteToggle { .. }
        | Command::Mute { .. }
        | Command::Seek { .. }
        | Command::SeekRelative { .. }
        | Command::Shuffle { .. }
        | Command::Repeat { .. }
        | Command::VolumeAbsolute { .. }
        | Command::VolumeRelative { .. } => Err(anyhow!(
            "UPnP command was not resolved to a supported native operation before dispatch"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregator::ZoneAggregator;
    use crate::bus::runtime::build_runtime;

    fn zone() -> Zone {
        Zone {
            zone_id: "upnp:living-room".to_string(),
            zone_name: "Living room".to_string(),
            state: PlaybackState::Playing,
            volume_control: Some(BusVolumeControl {
                value: 42.0,
                min: 0.0,
                max: 100.0,
                step: 1.0,
                is_muted: false,
                scale: crate::bus::VolumeScale::Percentage,
                output_id: Some(PrefixedZoneId::upnp("living-room").to_string()),
            }),
            now_playing: None,
            source: "upnp".to_string(),
            is_controllable: true,
            is_seekable: false,
            last_updated: 0,
            is_play_allowed: false,
            is_pause_allowed: true,
            is_next_allowed: false,
            is_previous_allowed: false,
        }
    }

    /// TDD guard for the migration: a composed UPnP observer cannot make a
    /// notification visible before its complete Zone is canonical.
    #[tokio::test]
    async fn reliable_upnp_observations_commit_before_compatibility_notification() {
        let bus = crate::bus::create_bus();
        let mut notifications = bus.subscribe();
        let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
        let runtime = build_runtime(aggregator.clone(), 2, 4);
        let bridge =
            UPnPRuntimeBridge::new(runtime.projection_ingress.clone(), runtime.commands.clone());
        let actor = tokio::spawn(runtime.projection_actor.run());

        bridge.publish_zone(zone(), None).await.expect("projection");
        assert_eq!(
            aggregator
                .get_zone("upnp:living-room")
                .await
                .expect("canonical zone")
                .volume_control
                .expect("volume")
                .value,
            42.0
        );
        match notifications
            .recv()
            .await
            .expect("post-commit notification")
        {
            BusEvent::ZoneUpdated { zone_id, .. } => {
                assert_eq!(zone_id.as_str(), "upnp:living-room")
            }
            BusEvent::ZoneDiscovered { zone } => assert_eq!(zone.zone_id, "upnp:living-room"),
            event => panic!("expected post-commit UPnP notification, got {event:?}"),
        }

        drop(bridge);
        actor.abort();
    }
}
