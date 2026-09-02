//! HTTP API handlers

use crate::adapters::hqplayer::{
    HqpAdapter, HqpAdvancedOptionsSnapshot, HqpInstanceManager, HqpProfile, HqpZoneLinkService,
};
use crate::adapters::lms::LmsAdapter;
use crate::adapters::musicassistant::ReconfigurableMusicAssistant;
use crate::adapters::openhome::OpenHomeAdapter;
use crate::adapters::roon::RoonAdapter;
use crate::adapters::upnp::UPnPAdapter;
use crate::adapters::{
    AdapterCommand, AdapterCommandResponse, AdapterLogic, LibraryAdapter, LibrarySearchResult,
    Startable,
};
use crate::aggregator::{HqpSnapshotPresence, ZoneAggregator};
use crate::bus::runtime::{
    CommandDeadlines, CommandGateway, CommandLane, CommandRequest, CommandStatus,
    HqpRuntimeCommand, RuntimeCommand,
};
use crate::bus::{Command, HqpImageSource, PrefixedZoneId, ProviderAccount, SharedBus};
use crate::coordinator::AdapterCoordinator;
use crate::knobs::KnobStore;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Redirect,
    },
    Json,
};
use futures::stream::Stream;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

pub mod apple_bridge;
pub mod browse;
pub mod controller_auth;
pub mod credentials;
pub mod ha_integration;
pub mod hiphi_pairing;
pub mod ingress;
pub mod mqtt_bootstrap;
pub mod mqtt_settings;
pub mod provider_auth;
pub mod spotify_callback_listener;
pub mod spotify_tunnel;

const MAX_REMOTE_ARTWORK_BYTES: usize = 10 * 1024 * 1024;

/// Registry for provider adapters whose transport is not represented by a
/// dedicated field on `AppState` (for example cloud and bridge-backed sources).
/// The aggregator remains the read-side source of truth; this registry only
/// dispatches commands to the owning adapter.
#[derive(Default)]
pub struct AdapterRegistry {
    adapters: tokio::sync::RwLock<std::collections::HashMap<String, Arc<dyn AdapterLogic>>>,
    libraries: tokio::sync::RwLock<std::collections::HashMap<String, Arc<dyn LibraryAdapter>>>,
    startables: tokio::sync::RwLock<std::collections::HashMap<String, Arc<dyn Startable>>>,
}

impl AdapterRegistry {
    pub async fn register(&self, adapter: Arc<dyn AdapterLogic>) {
        self.adapters
            .write()
            .await
            .insert(adapter.prefix().to_string(), adapter);
    }

    pub async fn register_with_lifecycle(
        &self,
        adapter: Arc<dyn AdapterLogic>,
        startable: Arc<dyn Startable>,
    ) {
        self.register(adapter).await;
        self.startables
            .write()
            .await
            .insert(startable.name().to_string(), startable);
    }

    /// Start a registered lifecycle owner through the coordinator without
    /// exposing its concrete adapter type to an API handler.
    pub async fn start_registered_if_enabled(
        &self,
        coordinator: &AdapterCoordinator,
        name: &str,
    ) -> anyhow::Result<bool> {
        let startable = self
            .startables
            .read()
            .await
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("adapter `{name}` is not registered"))?;
        coordinator.start_enabled(&startable).await
    }

    /// Register the optional content-library surface for a provider. Keeping
    /// this separate from transport registration prevents a provider that can
    /// control a zone from being advertised as searchable by accident.
    pub async fn register_library(&self, prefix: &str, adapter: Arc<dyn LibraryAdapter>) {
        self.libraries
            .write()
            .await
            .insert(prefix.to_string(), adapter);
    }

    pub async fn start(&self, prefix: &str) -> anyhow::Result<()> {
        let adapter = self
            .startables
            .read()
            .await
            .get(prefix)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("adapter `{prefix}` is not registered"))?;
        adapter.start().await
    }

    pub async fn stop(&self, prefix: &str) {
        if let Some(adapter) = self.startables.read().await.get(prefix).cloned() {
            adapter.stop().await;
        }
    }

    pub async fn command(
        &self,
        prefix: &str,
        zone_id: &str,
        command: AdapterCommand,
    ) -> anyhow::Result<AdapterCommandResponse> {
        let adapter = self
            .adapters
            .read()
            .await
            .get(prefix)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("adapter `{prefix}` is not configured"))?;
        adapter.handle_command(zone_id, command).await
    }

    /// Whether a provider owns transport commands through this registry.
    /// Legacy adapters have dedicated fields and routes, so the shared knob
    /// route uses this to distinguish registry-backed providers from unknown
    /// zone prefixes without maintaining another hard-coded list.
    pub async fn has_adapter(&self, prefix: &str) -> bool {
        self.adapters.read().await.contains_key(prefix)
    }

    pub async fn search_library(
        &self,
        prefix: &str,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<LibrarySearchResult>> {
        let adapter = self
            .libraries
            .read()
            .await
            .get(prefix)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("adapter `{prefix}` is not configured"))?;
        adapter.search(query, limit).await
    }

    pub async fn search_library_for_zone(
        &self,
        prefix: &str,
        zone_id: &str,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<LibrarySearchResult>> {
        let adapter = self
            .libraries
            .read()
            .await
            .get(prefix)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("adapter `{prefix}` is not configured"))?;
        adapter.search_for_zone(zone_id, query, limit).await
    }

    pub async fn play_library_uri(
        &self,
        prefix: &str,
        zone_id: &str,
        uri: &str,
    ) -> anyhow::Result<String> {
        let adapter = self
            .libraries
            .read()
            .await
            .get(prefix)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("adapter `{prefix}` is not configured"))?;
        adapter.play_uri(zone_id, uri).await
    }

    pub async fn queue_library_uri(
        &self,
        prefix: &str,
        zone_id: &str,
        uri: &str,
    ) -> anyhow::Result<()> {
        let adapter = self
            .libraries
            .read()
            .await
            .get(prefix)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("adapter `{prefix}` is not configured"))?;
        adapter.queue_uri(zone_id, uri).await
    }

    pub async fn play_next_library_uri(
        &self,
        prefix: &str,
        zone_id: &str,
        uri: &str,
    ) -> anyhow::Result<()> {
        let adapter = self
            .libraries
            .read()
            .await
            .get(prefix)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("adapter `{prefix}` is not configured"))?;
        adapter.play_next_uri(zone_id, uri).await
    }

    pub async fn read_library_queue(
        &self,
        prefix: &str,
        zone_id: &str,
    ) -> anyhow::Result<serde_json::Value> {
        let adapter = self
            .libraries
            .read()
            .await
            .get(prefix)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("adapter `{prefix}` is not configured"))?;
        adapter.read_queue(zone_id).await
    }

    pub async fn library_content(
        &self,
        prefix: &str,
        operation: &str,
        params: &serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let adapter = self
            .libraries
            .read()
            .await
            .get(prefix)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("adapter `{prefix}` is not configured"))?;
        adapter.content(operation, params).await
    }
}

/// Shared application state
#[derive(Clone)]
pub struct AppState {
    pub roon: Arc<RoonAdapter>,
    pub hqplayer: Arc<HqpAdapter>,
    pub hqp_instances: Arc<HqpInstanceManager>,
    hqp_images: Arc<dyn HqpImageSource>,
    pub hqp_zone_links: Arc<HqpZoneLinkService>,
    pub lms: Arc<LmsAdapter>,
    pub openhome: Arc<OpenHomeAdapter>,
    pub upnp: Arc<UPnPAdapter>,
    /// Stable owner for the reconfigurable outbound Music Assistant client.
    pub musicassistant: Arc<ReconfigurableMusicAssistant>,
    pub adapter_registry: Arc<AdapterRegistry>,
    pub provider_auth: Arc<provider_auth::ProviderAuthState>,
    pub controller_auth: controller_auth::ControllerAuthState,
    pub apple_bridges: apple_bridge::AppleBridgeRegistry,
    pub knobs: KnobStore,
    pub bus: SharedBus,
    pub aggregator: Arc<ZoneAggregator>,
    pub coordinator: Arc<AdapterCoordinator>,
    pub startable_adapters: Arc<Vec<Arc<dyn Startable>>>,
    pub start_time: Instant,
    /// Cancellation token for graceful shutdown (terminates SSE streams)
    pub shutdown: CancellationToken,
    /// Count of active SSE connections (for shutdown diagnostics)
    pub sse_connections: Arc<AtomicUsize>,
    /// The MCP ref table (#396): opaque tokens `hifi_search` mints and
    /// `hifi_play_ref` resolves. Constructed here rather than taken as a
    /// constructor parameter -- like `sse_connections` above -- so every
    /// existing `AppState::new` call site is untouched by this addition.
    pub mcp_refs: crate::mcp::refs::RefTable,
    /// Durable provider-private collection identities behind canonical
    /// `/library/<source>/<view>/loc_…` URLs. Unlike `mcp_refs`, these survive
    /// process and provider-session restarts.
    pub collection_locations: crate::mcp::collection_locations::CollectionLocationStore,
    /// Bounded in-process cache from a durable collection location token to
    /// the provider-session-scoped `(session_key, item_key)` pair that last
    /// served it. Purely an optimisation for Roon browsing: it turns the
    /// steady state (each level, each "Load more" page) into one browse call
    /// instead of a full re-walk of the saved trail, and is evicted the moment
    /// the provider rejects a pair. Deliberately not durable -- the pairs it
    /// holds are only meaningful inside a live provider session.
    pub collection_location_keys:
        std::sync::Arc<crate::mcp::collection_locations::LocationKeyCache>,
    /// Opaque image refs (#549): what a `hifi_collections`/`/api/collections`
    /// item's `image` field resolves through, so a collection browse row can
    /// carry a per-item artwork reference without ever handing a client the
    /// provider-native image handle behind it. Deliberately a separate table
    /// from `mcp_refs` -- see `crate::mcp::refs::ImageRef`'s docs for why.
    pub image_refs: crate::mcp::refs::ImageRefTable,
    /// Bounded cache of fully composed, dithered, and packed Frame artwork.
    pub eink_artwork_cache: crate::knobs::image::EinkArtworkCache,
    /// UHC-owned requested listening sequences, separate from provider queue
    /// observations (#483).
    pub listening_plans: crate::mcp::listening_plan::ListeningPlanStore,
    /// Explicit Apple Music feedback and bounded adaptation context (#485).
    pub apple_feedback: crate::mcp::feedback::FeedbackStore,
    /// Private reliable command ingress.  Surfaces retain their existing request/response shapes
    /// and use this only for provider paths that have migrated to a correlated readback.
    pub reliable_commands: Option<CommandGateway>,
    /// Optional MQTT publisher exposing every zone to Home Assistant (#508).
    /// Fully inert until both configured and enabled; see
    /// `crate::mqtt::MqttPublisher`.
    pub mqtt: Arc<crate::mqtt::MqttPublisher>,
    /// Process-owned HiPhi connector lifecycle. It is package-agnostic and
    /// survives Settings reloads by reconstructing from owner-only config.
    pub hiphi_connector: crate::cloud_connector::runtime::ConnectorSupervisor,
}

impl AppState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        roon: Arc<RoonAdapter>,
        hqplayer: Arc<HqpAdapter>,
        hqp_instances: Arc<HqpInstanceManager>,
        hqp_zone_links: Arc<HqpZoneLinkService>,
        lms: Arc<LmsAdapter>,
        openhome: Arc<OpenHomeAdapter>,
        upnp: Arc<UPnPAdapter>,
        knobs: KnobStore,
        bus: SharedBus,
        aggregator: Arc<ZoneAggregator>,
        coordinator: Arc<AdapterCoordinator>,
        startable_adapters: Vec<Arc<dyn Startable>>,
        start_time: Instant,
        shutdown: CancellationToken,
    ) -> Self {
        let hqp_images: Arc<dyn HqpImageSource> = hqp_instances.clone();
        let adapter_registry = Arc::new(AdapterRegistry::default());
        let mqtt = Arc::new(crate::mqtt::MqttPublisher::new(
            bus.clone(),
            aggregator.clone(),
            adapter_registry.clone(),
            knobs.clone(),
        ));
        let provider_auth = Arc::new(provider_auth::ProviderAuthState::default());
        // The Spotify tunnel (#538) spawns an `ssh` child process; wire the
        // server's own shutdown token in so a graceful shutdown kills it
        // too, instead of relying solely on the manual stop / OAuth
        // completion / lifetime-cap paths.
        provider_auth.bind_shutdown(shutdown.clone());
        Self {
            roon,
            hqplayer,
            hqp_instances,
            hqp_images,
            hqp_zone_links,
            lms,
            openhome,
            upnp,
            musicassistant: Arc::new(ReconfigurableMusicAssistant::new(bus.clone())),
            adapter_registry,
            provider_auth,
            controller_auth: controller_auth::ControllerAuthState::new(),
            apple_bridges: apple_bridge::AppleBridgeRegistry::from_env().unwrap_or_else(|error| {
                tracing::error!("Apple Music companion credential store is unavailable: {error}");
                apple_bridge::AppleBridgeRegistry::default()
            }),
            knobs,
            bus,
            aggregator,
            coordinator,
            startable_adapters: Arc::new(startable_adapters),
            start_time,
            shutdown,
            sse_connections: Arc::new(AtomicUsize::new(0)),
            mcp_refs: crate::mcp::refs::RefTable::new(),
            collection_locations:
                crate::mcp::collection_locations::CollectionLocationStore::default(),
            collection_location_keys: std::sync::Arc::new(
                crate::mcp::collection_locations::LocationKeyCache::new(),
            ),
            image_refs: crate::mcp::refs::ImageRefTable::new(),
            eink_artwork_cache: crate::knobs::image::EinkArtworkCache::default(),
            listening_plans: crate::mcp::listening_plan::ListeningPlanStore::from_config(),
            apple_feedback: crate::mcp::feedback::FeedbackStore::from_config(),
            reliable_commands: None,
            mqtt,
            hiphi_connector: crate::cloud_connector::runtime::ConnectorSupervisor::default(),
        }
    }

    pub fn with_reliable_commands(mut self, commands: CommandGateway) -> Self {
        // The MQTT publisher (#508) resolves inbound HA commands for legacy zones through this
        // same gateway (#529); it cannot receive one in its own constructor because `Arc<AppState>`
        // itself is not assembled yet at that point (see `MqttPublisher::set_reliable_commands`).
        self.mqtt.set_reliable_commands(commands.clone());
        self.reliable_commands = Some(commands);
        self
    }

    /// Get the count of active SSE connections
    pub fn active_sse_connections(&self) -> usize {
        self.sse_connections.load(Ordering::Relaxed)
    }

    /// Fetch image from the appropriate adapter based on zone_id prefix
    ///
    /// Routes to the correct backend (Roon, LMS, OpenHome, HQPlayer) based on the zone_id
    /// prefix and fetches the image using that adapter's API.
    ///
    /// Note: UPnP zones don't support image retrieval as the protocol doesn't
    /// expose album art URLs in a standardized way that can be proxied.
    ///
    /// If `format` is Some("rgb565"), converts to RGB565 format for ESP32 LCDs.
    pub async fn get_image(
        &self,
        zone_id: &str,
        image_key: &str,
        width: Option<u32>,
        height: Option<u32>,
        format: Option<&str>,
        resize_policy: Option<crate::knobs::image::Rgb565ResizePolicy>,
    ) -> anyhow::Result<crate::bus::ImageData> {
        self.get_image_with_source_limit(
            zone_id,
            image_key,
            width,
            height,
            format,
            resize_policy,
            None,
        )
        .await
    }

    /// Connector-only artwork fetch with a hard streaming source ceiling.
    /// Existing LAN image behavior keeps its provider-native limits; the
    /// hosted artwork lane must stop reading before its signed request bound
    /// can be exceeded in memory.
    pub async fn get_image_bounded(
        &self,
        zone_id: &str,
        image_key: &str,
        max_source_bytes: usize,
    ) -> anyhow::Result<crate::bus::ImageData> {
        anyhow::ensure!(
            max_source_bytes > 0,
            "artwork source limit must be positive"
        );
        self.get_image_with_source_limit(
            zone_id,
            image_key,
            None,
            None,
            None,
            None,
            Some(max_source_bytes),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn get_image_with_source_limit(
        &self,
        zone_id: &str,
        image_key: &str,
        width: Option<u32>,
        height: Option<u32>,
        format: Option<&str>,
        resize_policy: Option<crate::knobs::image::Rgb565ResizePolicy>,
        source_limit: Option<usize>,
    ) -> anyhow::Result<crate::bus::ImageData> {
        use crate::bus::ImageData;
        use crate::knobs::image::{
            image_to_eink_acep6_with_policy, jpeg_to_rgb565_with_policy, EinkCacheKey,
            Rgb565ResizePolicy,
        };

        let dimensions = match (width, height) {
            (Some(w), Some(h)) => (w, h),
            (Some(w), None) => (w, w),
            (None, Some(h)) => (h, h),
            (None, None) if format == Some("eink_acep6") => (800, 450),
            (None, None) => (240, 240),
        };
        let resize_policy = resize_policy.unwrap_or(Rgb565ResizePolicy::Exact);
        let eink_cache_key = (format == Some("eink_acep6")).then(|| EinkCacheKey {
            zone_id: zone_id.to_string(),
            image_key: image_key.to_string(),
            width: dimensions.0,
            height: dimensions.1,
            resize_policy,
            converter_version: 1,
        });
        if let Some(key) = eink_cache_key.as_ref() {
            if let Some(data) = self.eink_artwork_cache.get(key) {
                return Ok(ImageData {
                    content_type: "application/octet-stream".to_string(),
                    data: data.as_ref().clone(),
                });
            }
        }

        // Smart composition needs the source aspect ratio. Asking a provider
        // for the final rectangle first can irreversibly stretch or crop it
        // before UHC gets a chance to apply the crop budget/gallery mat.
        let provider_dimensions = if resize_policy == Rgb565ResizePolicy::Exact {
            (width, height)
        } else {
            (None, None)
        };

        // Fetch raw image from appropriate adapter
        let raw_image = if zone_id.starts_with("lms:") {
            let (content_type, data) = match source_limit {
                Some(limit) => {
                    self.lms
                        .get_artwork_bounded(
                            image_key,
                            provider_dimensions.0,
                            provider_dimensions.1,
                            limit,
                        )
                        .await?
                }
                None => {
                    self.lms
                        .get_artwork(image_key, provider_dimensions.0, provider_dimensions.1)
                        .await?
                }
            };
            ImageData { content_type, data }
        } else if zone_id.starts_with("openhome:") {
            let img = match source_limit {
                Some(limit) => self.openhome.get_image_bounded(image_key, limit).await?,
                None => self.openhome.get_image(image_key).await?,
            };
            ImageData {
                content_type: img.content_type,
                data: img.data,
            }
        } else if zone_id.starts_with("upnp:") {
            anyhow::bail!(
                "UPnP zones don't support image retrieval - the protocol doesn't expose album art URLs"
            )
        } else if zone_id.starts_with("spotify:") {
            fetch_remote_artwork_url_bounded(
                reqwest::Url::parse(image_key)?,
                source_limit.unwrap_or(MAX_REMOTE_ARTWORK_BYTES),
            )
            .await?
        } else if zone_id.starts_with("applemusic:") {
            fetch_apple_music_artwork(image_key, source_limit.unwrap_or(MAX_REMOTE_ARTWORK_BYTES))
                .await?
        } else if zone_id.starts_with("musicassistant:") {
            fetch_musicassistant_artwork(
                image_key,
                source_limit.unwrap_or(MAX_REMOTE_ARTWORK_BYTES),
            )
            .await?
        } else if let Some(instance) = zone_id.strip_prefix("hqplayer:") {
            let image = self.hqp_images.get_current_cover(instance).await?;
            if source_limit.is_some_and(|limit| image.data.len() > limit) {
                anyhow::bail!("remote artwork is too large");
            }
            image
        } else if zone_id.starts_with("roon:") || !zone_id.contains(':') {
            let img = self
                .roon
                .get_image(image_key, provider_dimensions.0, provider_dimensions.1)
                .await?;
            ImageData {
                content_type: img.content_type,
                data: img.data,
            }
        } else {
            anyhow::bail!("Unknown zone type for image: {}", zone_id)
        };
        if source_limit.is_some_and(|limit| raw_image.data.len() > limit) {
            anyhow::bail!("remote artwork is too large");
        }

        // Convert to RGB565 if requested (for ESP32 LCD/e-ink displays).
        // The smart variant is a capability handshake: older servers return
        // the source image, allowing firmware to detect and use its fallback.
        if matches!(format, Some("rgb565" | "rgb565-smart")) {
            // Use square dimensions when only one side specified (matches adapter behavior)
            let (target_w, target_h) = dimensions;

            match jpeg_to_rgb565_with_policy(&raw_image.data, target_w, target_h, resize_policy) {
                Ok(rgb565) => Ok(ImageData {
                    content_type: "application/octet-stream".to_string(),
                    data: rgb565.data,
                }),
                Err(_) => {
                    // Fall back to original on conversion error
                    Ok(raw_image)
                }
            }
        } else if format == Some("eink_acep6") {
            let (target_w, target_h) = dimensions;
            match image_to_eink_acep6_with_policy(
                &raw_image.data,
                target_w,
                target_h,
                resize_policy,
            ) {
                Ok(eink) => {
                    let Some(cache_key) = eink_cache_key else {
                        return Ok(raw_image);
                    };
                    let data = self.eink_artwork_cache.insert(cache_key, eink.data);
                    Ok(ImageData {
                        content_type: "application/octet-stream".to_string(),
                        data: data.as_ref().clone(),
                    })
                }
                Err(error) => {
                    tracing::warn!(
                        "Frame artwork conversion failed for {zone_id} at {target_w}x{target_h}: {error}"
                    );
                    Ok(raw_image)
                }
            }
        } else {
            Ok(raw_image)
        }
    }
}

async fn fetch_apple_music_artwork(
    image_key: &str,
    max_bytes: usize,
) -> anyhow::Result<crate::bus::ImageData> {
    let url = validate_apple_music_artwork_url(image_key)?;
    fetch_remote_artwork_url_bounded(url, max_bytes).await
}

/// Music Assistant's `image_url` (#549) is already an absolute URL the MA
/// server hands out for both now-playing and library items -- there is no
/// separate provider-side id/proxy to resolve the way Roon's `image_key` or
/// LMS's coverid are. Unlike Apple Music's CDN allowlist (MA art can come
/// from any of MA's own providers, not one known host), the only guard
/// worth enforcing here is "this is actually an http(s) URL", so a
/// malformed or unexpected scheme fails closed rather than being handed to
/// `reqwest` uninspected.
async fn fetch_musicassistant_artwork(
    image_key: &str,
    max_bytes: usize,
) -> anyhow::Result<crate::bus::ImageData> {
    let url = validate_musicassistant_artwork_url(image_key)?;
    fetch_remote_artwork_url_bounded(url, max_bytes).await
}

fn validate_musicassistant_artwork_url(image_key: &str) -> anyhow::Result<reqwest::Url> {
    let url = reqwest::Url::parse(image_key)
        .map_err(|_| anyhow::anyhow!("Music Assistant artwork URL is invalid"))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        anyhow::bail!("Music Assistant artwork URL must be http or https");
    }
    Ok(url)
}

fn validate_apple_music_artwork_url(image_key: &str) -> anyhow::Result<reqwest::Url> {
    let url = reqwest::Url::parse(image_key)
        .map_err(|_| anyhow::anyhow!("Apple Music artwork URL is invalid"))?;
    let url = if url.scheme().eq_ignore_ascii_case("musickit") {
        let fat = url
            .query_pairs()
            .find(|(key, _)| key == "fat")
            .map(|(_, value)| value.into_owned())
            .ok_or_else(|| anyhow::anyhow!("MusicKit artwork URL has no CDN URL"))?;
        reqwest::Url::parse(&fat)
            .map_err(|_| anyhow::anyhow!("MusicKit artwork CDN URL is invalid"))?
    } else {
        url
    };
    let host = url.host_str().unwrap_or_default();
    if url.scheme() != "https" || !(host == "mzstatic.com" || host.ends_with(".mzstatic.com")) {
        anyhow::bail!("Apple Music artwork URL is not an allowed Apple CDN URL");
    }

    Ok(url)
}

async fn fetch_remote_artwork_url_bounded(
    url: reqwest::Url,
    max_bytes: usize,
) -> anyhow::Result<crate::bus::ImageData> {
    let response = reqwest::get(url).await?.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        anyhow::bail!("remote artwork is too large");
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .filter(|value| value.starts_with("image/"))
        .ok_or_else(|| anyhow::anyhow!("remote artwork is not an image"))?
        .to_string();
    let mut response = response;
    let mut data = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(max_bytes as u64) as usize,
    );
    while let Some(chunk) = response.chunk().await? {
        if data.len().saturating_add(chunk.len()) > max_bytes {
            anyhow::bail!("remote artwork is too large");
        }
        data.extend_from_slice(&chunk);
    }
    Ok(crate::bus::ImageData { content_type, data })
}

/// Send legacy/bookmarked flash-page requests straight to the secure Web Serial origin.
pub async fn knob_flasher_redirect_handler() -> Redirect {
    Redirect::permanent(crate::app::KNOB_FLASHER_URL)
}

/// Route an LMS transport/volume action through the private reliable runtime without altering any
/// public HTTP or MCP payload.  The command becomes successful only after the LMS endpoint has
/// committed its exact-player readback to the aggregator.
pub(crate) async fn dispatch_lms_runtime_command(
    state: &AppState,
    zone_id: &str,
    command: Command,
) -> anyhow::Result<()> {
    dispatch_lms_runtime_command_via(state.reliable_commands.as_ref(), zone_id, command).await
}

/// Same routing and readback wait as [`dispatch_lms_runtime_command`], parameterized directly over
/// the reliable command gateway so callers without a full `AppState` - MQTT's inbound command
/// router (#529) - reuse the identical dispatch path HTTP/knob/MCP use.
pub(crate) async fn dispatch_lms_runtime_command_via(
    gateway: Option<&CommandGateway>,
    zone_id: &str,
    command: Command,
) -> anyhow::Result<()> {
    let target = PrefixedZoneId::lms(zone_id.strip_prefix("lms:").unwrap_or(zone_id));
    let Some(gateway) = gateway else {
        // Compatibility-only construction used by older embedders and contract fixtures. There is
        // no direct adapter fallback: preserve the established actionable error while refusing
        // native I/O outside the reliable runtime.
        return Err(anyhow::anyhow!("LMS host not configured"));
    };
    if !gateway.has_endpoint(&target) {
        return Err(anyhow::anyhow!("LMS command endpoint is not available"));
    }
    let now = tokio::time::Instant::now();
    let mut ticket = gateway
        .submit(CommandRequest {
            target,
            command: RuntimeCommand::Control(command),
            correlation_id: None,
            lane: CommandLane::Interactive,
            deadlines: CommandDeadlines {
                dispatch_by: now + Duration::from_secs(3),
                confirm_by: now + Duration::from_secs(15),
            },
        })
        .await
        .map_err(|error| anyhow::anyhow!("LMS command admission failed: {error:?}"))?;
    match ticket.wait_for_observable_result().await {
        CommandStatus::Confirmed { .. } => Ok(()),
        CommandStatus::Failed { detail } | CommandStatus::NotDispatched { detail } => {
            Err(anyhow::anyhow!(detail))
        }
        CommandStatus::Indeterminate => Err(anyhow::anyhow!(
            "LMS accepted the command but did not publish a verified readback in time"
        )),
        CommandStatus::Queued | CommandStatus::Dispatched | CommandStatus::AwaitingProjection => {
            Err(anyhow::anyhow!(
                "LMS command stopped without a terminal result"
            ))
        }
    }
}

/// Route an OpenHome transport/volume action through the reliable endpoint.
/// Public request and response shapes stay frozen; the command is successful
/// only after the endpoint commits an exact-device Zone readback.
pub(crate) async fn dispatch_openhome_runtime_command(
    state: &AppState,
    zone_id: &str,
    command: Command,
) -> anyhow::Result<()> {
    dispatch_openhome_runtime_command_via(state.reliable_commands.as_ref(), zone_id, command).await
}

/// Same routing as [`dispatch_openhome_runtime_command`], parameterized directly over the reliable
/// command gateway so callers without a full `AppState` - MQTT's inbound command router (#529) -
/// reuse the identical dispatch path HTTP/knob/MCP use.
pub(crate) async fn dispatch_openhome_runtime_command_via(
    gateway: Option<&CommandGateway>,
    zone_id: &str,
    command: Command,
) -> anyhow::Result<()> {
    if gateway.is_none() {
        let raw_id = zone_id.strip_prefix("openhome:").unwrap_or(zone_id);
        return Err(anyhow::anyhow!("Device not found: {raw_id}"));
    }
    dispatch_provider_runtime_command(
        gateway,
        PrefixedZoneId::openhome(zone_id.strip_prefix("openhome:").unwrap_or(zone_id)),
        command,
        "OpenHome",
    )
    .await
}

/// Route a UPnP transport/volume action through the reliable endpoint. Public
/// HTTP and MCP payloads stay unchanged; success means an exact SOAP readback
/// has committed into the aggregator, never merely that a SOAP write returned.
pub(crate) async fn dispatch_upnp_runtime_command(
    state: &AppState,
    zone_id: &str,
    command: Command,
) -> anyhow::Result<()> {
    dispatch_upnp_runtime_command_via(state.reliable_commands.as_ref(), zone_id, command).await
}

/// Same routing as [`dispatch_upnp_runtime_command`], parameterized directly over the reliable
/// command gateway so callers without a full `AppState` - MQTT's inbound command router (#529) -
/// reuse the identical dispatch path HTTP/knob/MCP use.
pub(crate) async fn dispatch_upnp_runtime_command_via(
    gateway: Option<&CommandGateway>,
    zone_id: &str,
    command: Command,
) -> anyhow::Result<()> {
    // Standalone API/MCP fixtures intentionally compose no runtime. Preserve
    // the frozen adapter-shaped refusal without doing native I/O outside the
    // composed server's reliable endpoint.
    if gateway.is_none() {
        let raw_id = zone_id.strip_prefix("upnp:").unwrap_or(zone_id);
        return Err(anyhow::anyhow!("Renderer not found: {raw_id}"));
    }
    dispatch_provider_runtime_command(
        gateway,
        PrefixedZoneId::upnp(zone_id.strip_prefix("upnp:").unwrap_or(zone_id)),
        command,
        "UPnP",
    )
    .await
}

/// Route a Roon transport/volume action through the reliable endpoint. Roon confirms through its
/// authoritative Core callback rather than a synthetic synchronous readback.
pub(crate) async fn dispatch_roon_runtime_command(
    state: &AppState,
    zone_id: &str,
    command: Command,
) -> anyhow::Result<()> {
    dispatch_roon_runtime_command_via(state.reliable_commands.as_ref(), zone_id, command).await
}

/// Same routing as [`dispatch_roon_runtime_command`], parameterized directly over the reliable
/// command gateway so callers without a full `AppState` - MQTT's inbound command router (#529) -
/// reuse the identical dispatch path HTTP/knob/MCP use.
pub(crate) async fn dispatch_roon_runtime_command_via(
    gateway: Option<&CommandGateway>,
    zone_id: &str,
    command: Command,
) -> anyhow::Result<()> {
    if gateway.is_none() {
        return Err(anyhow::anyhow!("Not connected to Roon"));
    }
    dispatch_provider_runtime_command(
        gateway,
        PrefixedZoneId::roon(zone_id.strip_prefix("roon:").unwrap_or(zone_id)),
        command,
        "Roon",
    )
    .await
}

async fn dispatch_provider_runtime_command(
    gateway: Option<&CommandGateway>,
    target: PrefixedZoneId,
    command: Command,
    provider: &str,
) -> anyhow::Result<()> {
    let Some(gateway) = gateway else {
        return Err(anyhow::anyhow!(
            "{provider} reliable command runtime is unavailable"
        ));
    };
    if !gateway.has_endpoint(&target) {
        return Err(anyhow::anyhow!(
            "{provider} command endpoint is not available"
        ));
    }
    let now = tokio::time::Instant::now();
    let mut ticket = gateway
        .submit(CommandRequest {
            target,
            command: RuntimeCommand::Control(command),
            correlation_id: None,
            lane: CommandLane::Interactive,
            deadlines: CommandDeadlines {
                dispatch_by: now + Duration::from_secs(3),
                confirm_by: now + Duration::from_secs(15),
            },
        })
        .await
        .map_err(|error| anyhow::anyhow!("{provider} command admission failed: {error:?}"))?;
    match ticket.wait_for_observable_result().await {
        CommandStatus::Confirmed { .. } => Ok(()),
        CommandStatus::Failed { detail } | CommandStatus::NotDispatched { detail } => {
            Err(anyhow::anyhow!(detail))
        }
        CommandStatus::Indeterminate => Err(anyhow::anyhow!(
            "{provider} accepted the command but did not publish a verified readback in time"
        )),
        CommandStatus::Queued | CommandStatus::Dispatched | CommandStatus::AwaitingProjection => {
            Err(anyhow::anyhow!(
                "{provider} command stopped without a terminal result"
            ))
        }
    }
}

/// Normalize OpenHome/UPnP's shared transport and integer volume vocabulary at
/// the surface boundary.  Provider-specific refusals still happen inside their
/// endpoint; this prevents a raw adapter action string crossing the bus seam.
pub(crate) fn renderer_runtime_command_from_action(
    action: &str,
    value: Option<i32>,
) -> anyhow::Result<Command> {
    match action {
        "play" => Ok(Command::Play),
        "pause" => Ok(Command::Pause),
        "play_pause" | "playpause" => Ok(Command::PlayPause),
        "stop" => Ok(Command::Stop),
        "next" => Ok(Command::Next),
        "previous" | "prev" => Ok(Command::Previous),
        "volume" | "vol_abs" => Ok(Command::VolumeAbsolute {
            value: value.unwrap_or(50) as f32,
            output_id: None,
        }),
        "vol_rel" => Ok(Command::VolumeRelative {
            delta: value.unwrap_or(0) as f32,
            output_id: None,
        }),
        _ => Err(anyhow::anyhow!("Unknown command: {action}")),
    }
}

/// Normalize the stable legacy LMS action vocabulary before it crosses the private runtime seam.
/// Kept here so HTTP, knob, and MCP surfaces cannot drift into subtly different native commands.
pub(crate) fn lms_runtime_command_from_action(
    action: &str,
    value: Option<f32>,
) -> anyhow::Result<Command> {
    match action {
        "play" => Ok(Command::Play),
        "pause" => Ok(Command::Pause),
        "play_pause" | "playpause" => Ok(Command::PlayPause),
        "stop" => Ok(Command::Stop),
        "next" => Ok(Command::Next),
        "previous" | "prev" => Ok(Command::Previous),
        "volume" | "vol_abs" => Ok(Command::VolumeAbsolute {
            value: value.unwrap_or(50.0),
            output_id: None,
        }),
        "vol_rel" => Ok(Command::VolumeRelative {
            delta: value.unwrap_or(0.0),
            output_id: None,
        }),
        _ => Err(anyhow::anyhow!("Unknown command: {action}")),
    }
}

/// Error response
#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

/// Generic zones response wrapper - clients expect {zones: [...]}
#[derive(Serialize)]
pub struct ZonesWrapper<T: Serialize> {
    pub zones: Vec<T>,
}

/// HQPlayer instances response wrapper - clients expect {instances: [...]}
#[derive(Serialize)]
pub struct InstancesWrapper<T: Serialize> {
    pub instances: Vec<T>,
}

/// LMS players response wrapper - clients expect {players: [...]}
#[derive(Serialize)]
pub struct PlayersWrapper<T: Serialize> {
    pub players: Vec<T>,
}

/// General status response
#[derive(Serialize)]
pub struct StatusResponse {
    pub service: &'static str,
    pub version: &'static str,
    pub git_sha: &'static str,
    pub uptime_secs: u64,
    pub roon_connected: bool,
    pub hqplayer_connected: bool,
    pub lms_connected: bool,
    pub openhome_devices: usize,
    pub upnp_devices: usize,
    pub bus_subscribers: usize,
}

/// GET /status - Service health check
pub async fn status_handler(State(state): State<AppState>) -> Json<StatusResponse> {
    let roon_status = state.roon.get_status().await;
    let hqp_status = state.hqplayer.get_status().await;
    let lms_status = state.lms.get_status().await;
    let openhome_status = state.openhome.get_status().await;
    let upnp_status = state.upnp.get_status().await;

    Json(StatusResponse {
        service: "unified-hifi-control",
        version: env!("UHC_VERSION"),
        git_sha: env!("UHC_GIT_SHA"),
        uptime_secs: state.start_time.elapsed().as_secs(),
        roon_connected: roon_status.connected,
        hqplayer_connected: hqp_status.connected,
        lms_connected: lms_status.connected,
        openhome_devices: openhome_status.device_count,
        upnp_devices: upnp_status.renderer_count,
        bus_subscribers: state.bus.subscriber_count(),
    })
}

// =============================================================================
// Roon handlers
// =============================================================================

/// GET /roon/status - Roon connection status
pub async fn roon_status_handler(
    State(state): State<AppState>,
) -> Json<crate::adapters::roon::RoonStatus> {
    Json(state.roon.get_status().await)
}

/// GET /roon/zones - List all Roon zones
pub async fn roon_zones_handler(
    State(state): State<AppState>,
) -> Json<ZonesWrapper<crate::adapters::roon::Zone>> {
    Json(ZonesWrapper {
        zones: state.roon.get_zones().await,
    })
}

/// GET /roon/zone/:zone_id - Get specific zone
pub async fn roon_zone_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
) -> impl IntoResponse {
    match state.roon.get_zone(&zone_id).await {
        Some(zone) => (StatusCode::OK, Json(zone)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Zone not found: {}", zone_id),
            }),
        )
            .into_response(),
    }
}

/// Control request body
#[derive(Deserialize)]
pub struct ControlRequest {
    pub zone_id: String,
    pub action: String,
}

/// POST /roon/control - Control playback
pub async fn roon_control_handler(
    State(state): State<AppState>,
    Json(req): Json<ControlRequest>,
) -> impl IntoResponse {
    let result = match renderer_runtime_command_from_action(&req.action, None) {
        Ok(command) => dispatch_roon_runtime_command(&state, &req.zone_id, command).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// Volume request body (f32 for fractional step support)
#[derive(Deserialize)]
pub struct VolumeRequest {
    /// Zone ID (also accepts output_id for backwards compatibility)
    #[serde(alias = "output_id")]
    pub zone_id: String,
    pub value: f32,
    #[serde(default)]
    pub relative: bool,
}

/// POST /roon/volume - Change volume
pub async fn roon_volume_handler(
    State(state): State<AppState>,
    Json(req): Json<VolumeRequest>,
) -> impl IntoResponse {
    let command = if req.relative {
        Command::VolumeRelative {
            delta: req.value,
            output_id: None,
        }
    } else {
        Command::VolumeAbsolute {
            value: req.value,
            output_id: None,
        }
    };
    match dispatch_roon_runtime_command(&state, &req.zone_id, command).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// Query params for image request
#[derive(Deserialize)]
pub struct ImageQuery {
    pub image_key: String,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

/// GET /roon/image - fetch album art
pub async fn roon_image_handler(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<ImageQuery>,
) -> impl IntoResponse {
    match state
        .roon
        .get_image(&params.image_key, params.width, params.height)
        .await
    {
        Ok(image_data) => {
            let headers = [(
                axum::http::header::CONTENT_TYPE,
                image_data
                    .content_type
                    .parse()
                    .unwrap_or(axum::http::HeaderValue::from_static("image/jpeg")),
            )];
            (StatusCode::OK, headers, image_data.data).into_response()
        }
        Err(e) => {
            tracing::warn!("Image fetch failed: {}", e);
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
                .into_response()
        }
    }
}

/// Query params for the collections image proxy (#549).
#[derive(Deserialize)]
pub struct CollectionImageQuery {
    /// Opaque token minted by `hifi_collections`/`/api/collections` on a
    /// browse row's `image` field. Never a provider-native image key.
    pub r#ref: String,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub format: Option<String>,
}

/// GET /api/collections/image -- fetch a `hifi_collections` item's artwork.
///
/// Resolves the opaque `ref` through `state.image_refs` to a
/// `(zone_id, image_key)` pair and re-enters `AppState::get_image`'s
/// existing per-provider dispatch, exactly like `/now_playing/image` and
/// `/roon/image` already do for now-playing art -- the only difference is
/// the image handle came from a browse row's ref, not the zone's live state.
/// An unknown, expired, or evicted ref reads as a plain 404, the same shape
/// as any other image fetch failure; nothing about a stale-vs-invalid ref is
/// distinguishable from here, deliberately (see `crate::mcp::refs`' module
/// docs on why that collapse is the point).
pub async fn collections_image_handler(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<CollectionImageQuery>,
) -> impl IntoResponse {
    let Some(target) = state.image_refs.resolve(&params.r#ref).await else {
        return (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "image ref is unknown or expired".to_string(),
            }),
        )
            .into_response();
    };
    match state
        .get_image(
            &target.zone_id,
            &target.image_key,
            params.width,
            params.height,
            params.format.as_deref(),
            None,
        )
        .await
    {
        Ok(image_data) => {
            let headers = [(
                axum::http::header::CONTENT_TYPE,
                image_data
                    .content_type
                    .parse()
                    .unwrap_or(axum::http::HeaderValue::from_static("image/jpeg")),
            )];
            (StatusCode::OK, headers, image_data.data).into_response()
        }
        Err(e) => {
            tracing::warn!("Collection image fetch failed: {}", e);
            (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
                .into_response()
        }
    }
}

// =============================================================================
// Roon Browse handlers
// =============================================================================

/// Query params for search request
#[derive(Deserialize)]
pub struct SearchQuery {
    pub q: String,
    #[serde(default)]
    pub zone_id: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    /// Search source: "library" (default), "tidal", or "qobuz"
    #[serde(default)]
    pub source: Option<String>,
}

/// Request body for play action
#[derive(Deserialize)]
pub struct PlayRequest {
    pub query: String,
    pub zone_id: String,
    /// Source: "library" (default), "tidal", or "qobuz"
    #[serde(default)]
    pub source: Option<String>,
    /// Action: "play" (default), "queue", or "radio"
    #[serde(default)]
    pub action: Option<String>,
}

/// Search result item (simplified from roon_api::browse::Item)
#[derive(Serialize)]
pub struct SearchResultItem {
    pub title: String,
    pub subtitle: Option<String>,
    pub item_key: Option<String>,
    pub hint: Option<String>,
}

impl From<roon_api::browse::Item> for SearchResultItem {
    fn from(item: roon_api::browse::Item) -> Self {
        use roon_api::browse::ItemHint;
        Self {
            title: item.title,
            subtitle: item.subtitle,
            item_key: item.item_key,
            hint: item.hint.map(|h| {
                match h {
                    ItemHint::None => "none",
                    ItemHint::Action => "action",
                    ItemHint::ActionList => "action_list",
                    ItemHint::List => "list",
                    ItemHint::Header => "header",
                }
                .to_string()
            }),
        }
    }
}

/// GET /roon/search - Search the Roon library, TIDAL, or Qobuz
pub async fn roon_search_handler(
    State(state): State<AppState>,
    Query(params): Query<SearchQuery>,
) -> impl IntoResponse {
    use crate::adapters::roon::SearchSource;

    if !state.roon.is_browse_connected().await {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "Roon Browse not connected".to_string(),
            }),
        )
            .into_response();
    }

    let source = match params.source.as_deref() {
        Some("tidal") => SearchSource::Tidal,
        Some("qobuz") => SearchSource::Qobuz,
        _ => SearchSource::Library,
    };

    match state
        .roon
        .search(&params.q, params.zone_id.as_deref(), params.limit, source)
        .await
    {
        Ok(items) => {
            let results: Vec<SearchResultItem> = items.into_iter().map(|i| i.into()).collect();
            (
                StatusCode::OK,
                Json(serde_json::json!({ "results": results })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// POST /roon/play - Search and play music
pub async fn roon_play_handler(
    State(state): State<AppState>,
    Json(req): Json<PlayRequest>,
) -> impl IntoResponse {
    use crate::adapters::roon::{PlayAction, SearchSource};

    if !state.roon.is_browse_connected().await {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "Roon Browse not connected".to_string(),
            }),
        )
            .into_response();
    }

    let source = match req.source.as_deref() {
        Some("tidal") => SearchSource::Tidal,
        Some("qobuz") => SearchSource::Qobuz,
        _ => SearchSource::Library,
    };

    let action = PlayAction::parse(req.action.as_deref().unwrap_or("play"));

    match state
        .roon
        .search_and_play(&req.query, &req.zone_id, source, action)
        .await
    {
        Ok(message) => (
            StatusCode::OK,
            Json(serde_json::json!({ "message": message })),
        )
            .into_response(),
        Err(e) => roon_browse_failure(&e, None),
    }
}

/// Turn a Roon browse/load failure into an HTTP response, keeping the failure
/// classes apart (issue #405).
///
/// A reference the Core refused is a client-fixable condition that now arrives
/// immediately, so it answers 404 with the Core's own explanation and the
/// recovery instruction. Everything else - a timed-out or unreachable Core, a
/// Core-side fault - keeps the 500 it has always had. Browse-not-connected is
/// still checked before the request is issued and still answers 503.
fn roon_browse_failure(err: &anyhow::Error, prefix: Option<&str>) -> axum::response::Response {
    use crate::adapters::roon::{RoonBrowseError, RoonBrowseErrorKind};

    let status = match RoonBrowseError::from_error(err).map(|rejection| rejection.kind) {
        Some(RoonBrowseErrorKind::InvalidItemKey | RoonBrowseErrorKind::ZoneNotFound) => {
            StatusCode::NOT_FOUND
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };

    let error = match prefix {
        Some(prefix) => format!("{}: {}", prefix, err),
        None => err.to_string(),
    };

    (status, Json(ErrorResponse { error })).into_response()
}

/// Play item request body
#[derive(Deserialize)]
pub struct PlayItemRequest {
    pub item_key: String,
    pub zone_id: String,
    #[serde(default)]
    pub action: Option<String>,
}

/// POST /roon/play_item - Play a specific item by its key
pub async fn roon_play_item_handler(
    State(state): State<AppState>,
    Json(req): Json<PlayItemRequest>,
) -> impl IntoResponse {
    use crate::adapters::roon::PlayAction;

    if !state.roon.is_browse_connected().await {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "Roon Browse not connected".to_string(),
            }),
        )
            .into_response();
    }

    let action = PlayAction::parse(req.action.as_deref().unwrap_or("play"));

    match state
        .roon
        .play_item(&req.item_key, &req.zone_id, action)
        .await
    {
        Ok(message) => (
            StatusCode::OK,
            Json(serde_json::json!({ "message": message })),
        )
            .into_response(),
        Err(e) => roon_browse_failure(&e, None),
    }
}

/// Browse request body
#[derive(Deserialize)]
pub struct BrowseRequest {
    #[serde(default)]
    pub item_key: Option<String>,
    #[serde(default)]
    pub zone_id: Option<String>,
    #[serde(default)]
    pub pop_all: bool,
    #[serde(default)]
    pub input: Option<String>,
    /// Session key for maintaining browse state across requests.
    ///
    /// See [`roon_browse_handler`] for what supplying one does and does not
    /// guarantee when two clients use the same value at the same time (#416).
    #[serde(default)]
    pub session_key: Option<String>,
}

/// Browse result converted to serializable format
#[derive(Serialize)]
pub struct BrowseResultResponse {
    pub action: String,
    pub list: Option<BrowseListInfo>,
    pub is_error: Option<bool>,
    pub message: Option<String>,
    /// Session key to use for subsequent browse calls
    pub session_key: String,
    /// Items at the current browse level
    pub items: Vec<BrowseItemResponse>,
}

#[derive(Serialize)]
pub struct BrowseListInfo {
    pub title: String,
    pub count: u32,
    pub level: u32,
    pub subtitle: Option<String>,
    pub image_key: Option<String>,
}

#[derive(Serialize)]
pub struct BrowseItemResponse {
    pub title: String,
    pub subtitle: Option<String>,
    pub item_key: Option<String>,
    pub hint: Option<String>,
    pub image_key: Option<String>,
}

/// POST /roon/browse - Browse the Roon library hierarchy
///
/// # What a caller-supplied `session_key` guarantees (#416)
///
/// It **is** the identity of a browse *position*: Roon keeps a stack of levels per
/// `multi_session_key`, so passing the same key back is how a client stays where it
/// was and how `pop_all` returns it to the root. Reusing it is the intended use.
///
/// It is **not** a lock, a lease, or a request identifier, and nothing here or in
/// the adapter rejects two clients that pick the same value:
///
/// * **Results are correlated per request, not per session.** Since #416 a browse or
///   load result is routed to the request that asked for it, by the MOO `request_id`
///   the Core echoes back. Two clients sharing a key can no longer be handed each
///   other's lists - which they silently could before, since the adapter resolved
///   whichever pending request in that session it found first.
/// * **The Core-side level stack is still shared.** Correlation makes each *answer*
///   honest; it cannot make two clients' navigation coherent. Concurrent browses
///   under one key all push onto the same stack, so a client's next `load` may page
///   the level the other client just entered, and a `pop_all` resets both. Pinned by
///   `a_shared_session_key_is_one_level_stack_however_well_results_correlate` in
///   `tests/roon_protocol.rs`.
/// * **Therefore: one navigating client, one session key.** Omit `session_key` and a
///   fresh one is minted for that request alone; the response reports it as
///   `session_key` so a client can choose to continue in it. Anything building a
///   multi-client navigation surface on this route - #399's MCP browse handle - must
///   mint one key per client session and keep concurrent use of a single key out of
///   reach, not merely undocumented. If a design genuinely needs two clients at one
///   position, the missing piece is serialising requests per key; that is a separate
///   change and it does not replace this one.
///
/// The parameter is kept rather than removed: it is the only way to walk a hierarchy
/// across requests, and removing it would be an API change requiring explicit
/// approval.
pub async fn roon_browse_handler(
    State(state): State<AppState>,
    Json(req): Json<BrowseRequest>,
) -> impl IntoResponse {
    use roon_api::browse::{BrowseOpts, ItemHint, LoadOpts};

    if !state.roon.is_browse_connected().await {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "Roon Browse not connected".to_string(),
            }),
        )
            .into_response();
    }

    // Use provided session_key or generate a new one
    let session_key = req.session_key.unwrap_or_else(|| {
        format!(
            "browse_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        )
    });

    let opts = BrowseOpts {
        item_key: req.item_key,
        zone_or_output_id: req.zone_id,
        pop_all: req.pop_all,
        input: req.input,
        multi_session_key: Some(session_key.clone()),
        ..Default::default()
    };

    // Browse to the level
    let browse_result = match state.roon.browse(opts).await {
        Ok(result) => result,
        Err(e) => return roon_browse_failure(&e, None),
    };

    // Load items at this level
    let items = if let Some(ref list) = browse_result.list {
        if list.count > 0 {
            let load_opts = LoadOpts {
                multi_session_key: Some(session_key.clone()),
                count: Some(50), // Load up to 50 items
                ..Default::default()
            };
            match state.roon.load(load_opts).await {
                Ok(load_result) => load_result
                    .items
                    .into_iter()
                    .map(|item| {
                        let hint_str = item.hint.map(|h| match h {
                            ItemHint::Action => "action",
                            ItemHint::ActionList => "action_list",
                            ItemHint::List => "list",
                            ItemHint::Header => "header",
                            ItemHint::None => "none",
                        });
                        BrowseItemResponse {
                            title: item.title,
                            subtitle: item.subtitle,
                            item_key: item.item_key,
                            hint: hint_str.map(|s| s.to_string()),
                            image_key: item.image_key,
                        }
                    })
                    .collect(),
                Err(e) => return roon_browse_failure(&e, Some("Browse load error")),
            }
        } else {
            vec![]
        }
    } else {
        vec![]
    };

    use roon_api::browse::Action;
    let action_str = match browse_result.action {
        Action::None => "none",
        Action::Message => "message",
        Action::List => "list",
        Action::ReplaceItem => "replace_item",
        Action::RemoveItem => "remove_item",
    };

    let response = BrowseResultResponse {
        action: action_str.to_string(),
        list: browse_result.list.map(|l| BrowseListInfo {
            title: l.title,
            count: l.count as u32,
            level: l.level,
            subtitle: l.subtitle,
            image_key: l.image_key,
        }),
        is_error: browse_result.is_error,
        message: browse_result.message,
        session_key,
        items,
    };
    (StatusCode::OK, Json(response)).into_response()
}

/// GET /roon/browse/status - Check if browse service is connected
pub async fn roon_browse_status_handler(State(state): State<AppState>) -> impl IntoResponse {
    let connected = state.roon.is_browse_connected().await;
    Json(serde_json::json!({
        "connected": connected
    }))
}

// =============================================================================
// HQPlayer handlers
// =============================================================================

pub(crate) async fn refresh_hqp_advanced_aggregate(
    state: &AppState,
    instance_name: &str,
) -> anyhow::Result<HqpAdvancedOptionsSnapshot> {
    crate::knobs::routes::dispatch_hqplayer_refresh(
        state,
        instance_name,
        HqpRuntimeCommand::RefreshAdvanced,
    )
    .await
    .map_err(|error| anyhow::anyhow!(error.message().to_string()))?;
    state
        .aggregator
        .get_hqplayer_snapshot(instance_name)
        .await
        .and_then(|snapshot| snapshot.advanced)
        .ok_or_else(|| {
            anyhow::anyhow!("HQPlayer advanced state was not retained by the aggregator")
        })
}

pub(crate) async fn refresh_hqp_profiles_aggregate(
    state: &AppState,
    instance_name: &str,
) -> anyhow::Result<Vec<HqpProfile>> {
    crate::knobs::routes::dispatch_hqplayer_refresh(
        state,
        instance_name,
        HqpRuntimeCommand::RefreshProfiles,
    )
    .await
    .map_err(|error| anyhow::anyhow!(error.message().to_string()))?;
    state
        .aggregator
        .get_hqplayer_snapshot(instance_name)
        .await
        .and_then(|snapshot| snapshot.profiles)
        .ok_or_else(|| anyhow::anyhow!("HQPlayer profiles were not retained by the aggregator"))
}

async fn hqp_default_pipeline_from_aggregator(
    state: &AppState,
) -> Option<crate::adapters::hqplayer::PipelineStatus> {
    state
        .aggregator
        .get_hqplayer_snapshot("default")
        .await
        .filter(|snapshot| snapshot.presence == HqpSnapshotPresence::Live)
        .map(|snapshot| snapshot.observation.pipeline)
}

/// GET /hqplayer/status - HQPlayer connection status
pub async fn hqp_status_handler(
    State(state): State<AppState>,
) -> Json<crate::adapters::hqplayer::HqpConnectionStatus> {
    if let Some(snapshot) = state.aggregator.get_hqplayer_snapshot("default").await {
        let mut connection = snapshot.observation.connection;
        connection.connected = snapshot.presence == HqpSnapshotPresence::Live;
        Json(connection)
    } else {
        // Configuration exists before the first native observation and remains useful while the
        // endpoint is unavailable; this fallback contains configuration, not playback state.
        Json(state.hqplayer.get_status().await)
    }
}

/// GET /hqplayer/pipeline - HQPlayer pipeline status
pub async fn hqp_pipeline_handler(State(state): State<AppState>) -> impl IntoResponse {
    match state.aggregator.get_hqplayer_snapshot("default").await {
        Some(snapshot) if snapshot.presence == HqpSnapshotPresence::Live => {
            (StatusCode::OK, Json(snapshot.observation.pipeline)).into_response()
        }
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "HQPlayer not connected".to_string(),
            }),
        )
            .into_response(),
    }
}

/// HQPlayer control request
#[derive(Deserialize)]
pub struct HqpControlRequest {
    pub action: String,
}

/// POST /hqplayer/control - Control HQPlayer playback
pub async fn hqp_control_handler(
    State(state): State<AppState>,
    Json(req): Json<HqpControlRequest>,
) -> impl IntoResponse {
    match crate::knobs::routes::dispatch_hqplayer_action(
        &state,
        "hqplayer:default",
        "default",
        &req.action,
        None,
    )
    .await
    {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.message().to_string(),
            }),
        )
            .into_response(),
    }
}

/// HQPlayer volume request
#[derive(Deserialize)]
pub struct HqpVolumeRequest {
    pub value: i32,
}

/// POST /hqplayer/volume - Change HQPlayer volume
pub async fn hqp_volume_handler(
    State(state): State<AppState>,
    Json(req): Json<HqpVolumeRequest>,
) -> impl IntoResponse {
    match crate::knobs::routes::dispatch_hqplayer_action(
        &state,
        "hqplayer:default",
        "default",
        "volume",
        Some(f64::from(req.value)),
    )
    .await
    {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.message().to_string(),
            }),
        )
            .into_response(),
    }
}

/// HQPlayer setting request (legacy - uses name/value with u32)
#[derive(Deserialize)]
pub struct HqpSettingRequest {
    pub name: String,
    pub value: u32,
}

/// Apply one legacy numeric setting, resolving the number to a name at this boundary.
///
/// Shared by `POST /hqplayer/setting` and the numeric arm of `POST /hqp/pipeline` so there is exactly
/// one place a list position is interpreted, and it is a place with the daemon's current enumeration
/// in hand. `samplerate` is the exception by contract: its number is **Hz**, not a position.
async fn hqp_apply_legacy_setting(
    state: &AppState,
    setting: &str,
    value: u32,
) -> anyhow::Result<()> {
    crate::knobs::routes::dispatch_hqplayer_reconfiguration(
        state,
        "default",
        HqpRuntimeCommand::LegacyPipelineIndex {
            setting: setting.to_string(),
            index: value,
        },
    )
    .await
    .map_err(|error| anyhow::anyhow!(error.message().to_string()))
}

/// Apply one setting given as a semantic name, which is the modern contract.
async fn hqp_apply_named_setting(
    state: &AppState,
    setting: &str,
    value: &str,
) -> anyhow::Result<()> {
    let normalized = match setting {
        "mode" | "filter" | "filter1x" | "filterNx" | "filternx" | "shaper" | "dither"
        | "junk_filter" => value.to_string(),
        "matrix_profile" if value.eq_ignore_ascii_case("[default]") => String::new(),
        "matrix_profile" => value.to_string(),
        "convolution" | "adaptive_volume" | "random" => parse_hqp_bool(value)?.to_string(),
        "repeat" => parse_hqp_repeat(value)?.to_string(),
        "samplerate" | "rate" => {
            let hz: u32 = value.parse().map_err(|_| {
                anyhow::anyhow!("Invalid rate value (expected Hz like 48000, 96000): {value}")
            })?;
            hz.to_string()
        }
        other => return Err(anyhow::anyhow!("Unknown setting: {}", other)),
    };
    crate::knobs::routes::dispatch_hqplayer_reconfiguration(
        state,
        "default",
        HqpRuntimeCommand::Pipeline {
            setting: setting.to_string(),
            value: normalized,
        },
    )
    .await
    .map_err(|error| anyhow::anyhow!(error.message().to_string()))
}

fn parse_hqp_bool(value: &str) -> anyhow::Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "on" | "yes" => Ok(true),
        "false" | "0" | "off" | "no" => Ok(false),
        _ => Err(anyhow::anyhow!(
            "Invalid boolean value {value:?}; expected true or false"
        )),
    }
}

fn parse_hqp_repeat(value: &str) -> anyhow::Result<u8> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" | "0" => Ok(0),
        "one" | "track" | "1" => Ok(1),
        "all" | "2" => Ok(2),
        _ => Err(anyhow::anyhow!(
            "Invalid repeat value {value:?}; expected off, one, or all"
        )),
    }
}

/// POST /hqplayer/setting - Change HQPlayer pipeline setting (legacy endpoint)
///
/// The request contract carries `value: u32` and is frozen, so this is the **compatibility boundary**:
/// the number is resolved into the semantic name the daemon's current enumeration gives that position,
/// and the name is what goes inward. Nothing downstream ever sees the number, and a position the
/// current list does not have is an error here rather than an index forwarded to the wire.
pub async fn hqp_setting_handler(
    State(state): State<AppState>,
    Json(req): Json<HqpSettingRequest>,
) -> impl IntoResponse {
    // This endpoint's accepted names, exactly as it has always accepted them. The numeric applier
    // below is shared with `POST /hqp/pipeline`, which additionally accepts `dither` — sharing the
    // applier must not quietly widen *this* route, so the gate is here and the error text is the one
    // it already answered with.
    const ACCEPTED: [&str; 8] = [
        "mode",
        "filter",
        "filter1x",
        "filterNx",
        "filternx",
        "shaper",
        "samplerate",
        "rate",
    ];
    if !ACCEPTED.contains(&req.name.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("Unknown setting: {}", req.name),
            }),
        )
            .into_response();
    }

    let result = hqp_apply_legacy_setting(&state, &req.name, req.value).await;

    match result {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// HQPlayer pipeline setting request - iOS/Node.js compatible format
#[derive(Deserialize)]
pub struct HqpPipelineRequest {
    pub setting: String,
    pub value: serde_json::Value, // Can be string or number
}

/// POST /hqp/pipeline - Change HQPlayer pipeline setting (iOS compatible)
pub async fn hqp_pipeline_update_handler(
    State(state): State<AppState>,
    Json(req): Json<HqpPipelineRequest>,
) -> impl IntoResponse {
    let valid_settings = [
        "mode",
        "samplerate",
        "filter1x",
        "filterNx",
        "shaper",
        "dither",
        "junk_filter",
        "convolution",
        "adaptive_volume",
        "repeat",
        "random",
        "matrix_profile",
    ];
    if !valid_settings.contains(&req.setting.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: format!("Invalid setting. Valid: {}", valid_settings.join(", ")),
            }),
        )
            .into_response();
    }

    // The request contract accepts a string or a number, and the two mean different things. A string
    // is a semantic name and travels inward unchanged. A **number** is a list position for every
    // family except `samplerate`, whose number is Hz — so it is resolved to a name here, at the
    // boundary, against the enumeration the daemon is serving now. Stringifying the number and
    // letting a resolver parse it back out was the fallback HQP-C-063 records: it made a stale or
    // guessed position select whatever now sits there.
    let result = match &req.value {
        serde_json::Value::Number(n) => match n.as_u64() {
            Some(v) if v <= u64::from(u32::MAX) => {
                hqp_apply_legacy_setting(&state, &req.setting, v as u32).await
            }
            _ => Err(anyhow::anyhow!(
                "Invalid numeric value for {}: {n}",
                req.setting
            )),
        },
        serde_json::Value::String(s) => hqp_apply_named_setting(&state, &req.setting, s).await,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: "Invalid value type".to_string(),
                }),
            )
                .into_response()
        }
    };

    match result {
        Ok(()) => match hqp_default_pipeline_from_aggregator(&state).await {
            Some(pipeline) => (StatusCode::OK, Json(pipeline)).into_response(),
            None => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse {
                    error: "HQPlayer accepted and verified the native setting, but its canonical pipeline readback is unavailable".to_string(),
                }),
            )
                .into_response(),
        },
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// GET /hqplayer/profiles - Get available profiles
pub async fn hqp_profiles_handler(State(state): State<AppState>) -> impl IntoResponse {
    match refresh_hqp_profiles_aggregate(&state, "default").await {
        Ok(profiles) => (StatusCode::OK, Json(profiles)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// HQPlayer profile request
#[derive(Deserialize)]
pub struct HqpProfileRequest {
    pub profile: String,
}

/// POST /hqplayer/profile - Load a profile
pub async fn hqp_load_profile_handler(
    State(state): State<AppState>,
    Json(req): Json<HqpProfileRequest>,
) -> impl IntoResponse {
    match crate::knobs::routes::dispatch_hqplayer_reconfiguration(
        &state,
        "default",
        HqpRuntimeCommand::LoadProfile {
            profile: req.profile,
        },
    )
    .await
    {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.message().to_string(),
            }),
        )
            .into_response(),
    }
}

/// GET /hqplayer/matrix/profiles - Get matrix profiles and current selection
pub async fn hqp_matrix_profiles_handler(State(state): State<AppState>) -> impl IntoResponse {
    if state
        .aggregator
        .get_hqplayer_snapshot("default")
        .await
        .is_none()
    {
        return (
            StatusCode::OK,
            Json(serde_json::json!({"profiles": [], "current": null})),
        )
            .into_response();
    }
    match refresh_hqp_advanced_aggregate(&state, "default").await {
        Ok(snapshot) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "profiles": snapshot.matrix_profiles,
                "current": snapshot.current_matrix_profile,
                "junk_filters": snapshot.junk_filters,
                "junk_filter": snapshot.state.filter_junk,
                "convolution": snapshot.state.convolution,
                "adaptive_volume": snapshot.state.adaptive,
                "repeat": snapshot.state.repeat,
                "random": snapshot.state.random,
                "native_state": snapshot.state,
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// Matrix profile request
#[derive(Deserialize)]
pub struct HqpMatrixProfileRequest {
    pub profile: u32,
}

/// POST /hqplayer/matrix/profile - Set matrix profile
pub async fn hqp_set_matrix_profile_handler(
    State(state): State<AppState>,
    Json(req): Json<HqpMatrixProfileRequest>,
) -> impl IntoResponse {
    let profile = match refresh_hqp_advanced_aggregate(&state, "default")
        .await
        .and_then(|snapshot| {
            snapshot
                .matrix_profiles
                .into_iter()
                .find(|profile| profile.index == req.profile)
                .ok_or_else(|| anyhow::anyhow!("Unknown matrix profile index: {}", req.profile))
        }) {
        Ok(profile) => profile,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
                .into_response();
        }
    };
    match crate::knobs::routes::dispatch_hqplayer_reconfiguration(
        &state,
        "default",
        HqpRuntimeCommand::Pipeline {
            setting: "matrix_profile".to_string(),
            value: profile.name,
        },
    )
    .await
    {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.message().to_string(),
            }),
        )
            .into_response(),
    }
}

// =============================================================================
// LMS handlers
// =============================================================================

/// GET /lms/status - LMS connection status
pub async fn lms_status_handler(
    State(state): State<AppState>,
) -> Json<crate::adapters::lms::LmsStatus> {
    Json(state.lms.get_status().await)
}

/// GET /lms/players - Get all players
pub async fn lms_players_handler(
    State(state): State<AppState>,
) -> Json<PlayersWrapper<crate::adapters::lms::LmsPlayer>> {
    Json(PlayersWrapper {
        players: state.lms.get_cached_players().await,
    })
}

/// GET /lms/player/:player_id - Get specific player
pub async fn lms_player_handler(
    State(state): State<AppState>,
    Path(player_id): Path<String>,
) -> impl IntoResponse {
    match state.lms.get_cached_player(&player_id).await {
        Some(player) => (StatusCode::OK, Json(player)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Player not found: {}", player_id),
            }),
        )
            .into_response(),
    }
}

/// LMS control request
#[derive(Deserialize)]
pub struct LmsControlRequest {
    pub player_id: String,
    pub action: String,
    #[serde(default)]
    pub value: Option<i32>,
}

/// POST /lms/control - Control LMS player
pub async fn lms_control_handler(
    State(state): State<AppState>,
    Json(req): Json<LmsControlRequest>,
) -> impl IntoResponse {
    let result =
        match lms_runtime_command_from_action(&req.action, req.value.map(|value| value as f32)) {
            Ok(command) => dispatch_lms_runtime_command(&state, &req.player_id, command).await,
            Err(error) => Err(error),
        };
    match result {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// LMS volume request
#[derive(Deserialize)]
pub struct LmsVolumeRequest {
    pub player_id: String,
    pub value: f32,
    #[serde(default)]
    pub relative: bool,
}

/// POST /lms/volume - Change LMS player volume
pub async fn lms_volume_handler(
    State(state): State<AppState>,
    Json(req): Json<LmsVolumeRequest>,
) -> impl IntoResponse {
    let command = if req.relative {
        Command::VolumeRelative {
            delta: req.value,
            output_id: None,
        }
    } else {
        Command::VolumeAbsolute {
            value: req.value,
            output_id: None,
        }
    };
    match dispatch_lms_runtime_command(&state, &req.player_id, command).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// LMS discovery request query params
#[derive(Deserialize)]
pub struct LmsDiscoverRequest {
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// GET /lms/discover - Discover LMS servers on the local network via UDP broadcast
pub async fn lms_discover_handler(Query(params): Query<LmsDiscoverRequest>) -> impl IntoResponse {
    use crate::adapters::discover_lms_servers;

    match discover_lms_servers(params.timeout_ms).await {
        Ok(servers) => (
            StatusCode::OK,
            Json(serde_json::json!({ "discovered": servers })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Discovery failed: {}", e),
            }),
        )
            .into_response(),
    }
}

// =============================================================================
// SSE Events
// =============================================================================

/// GET /events - Server-Sent Events stream
/// Guard that decrements SSE connection count on drop
struct SseConnectionGuard {
    counter: Arc<AtomicUsize>,
}

impl Drop for SseConnectionGuard {
    fn drop(&mut self) {
        let prev = self.counter.fetch_sub(1, Ordering::Relaxed);
        tracing::debug!("SSE connection closed ({} remaining)", prev - 1);
    }
}

/// Patches a serialized `BusEvent`'s embedded zone with the live
/// `browse_supported` derivation (#557).
///
/// `bus::events::Zone` -- the wire type embedded in `ZoneDiscovered` and
/// broadcast to every SSE subscriber -- predates #531's per-provider
/// `browse_supported` flag and has no field for it, unlike the REST
/// `/zones` and `/knob/zones` projections (`knobs::routes::ZoneInfo`),
/// which compute it correctly via
/// [`crate::mcp::tools::collections::zone_supports_hifi_collections`].
/// Every adapter's zone-discovery path would need a matching edit to add
/// a field that doesn't exist yet, so instead this patches the
/// already-serialized JSON at the one point a `ZoneDiscovered` event
/// leaves the process. The frontend's `crate::app::api::Zone` treats a
/// missing `browse_supported` as `false` (`#[serde(default)]`), so an
/// unpatched payload silently told every client -- Roon zones included --
/// that no zone could browse. A no-op for every other event: none of them
/// carry a `/payload/zone` pointer.
fn patch_zone_discovered_browse_supported(value: &mut serde_json::Value) {
    let Some(zone_id) = value
        .pointer("/payload/zone/zone_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return;
    };
    let supported = crate::mcp::tools::collections::zone_supports_hifi_collections(&zone_id);
    if let Some(zone) = value
        .pointer_mut("/payload/zone")
        .and_then(|v| v.as_object_mut())
    {
        zone.insert(
            "browse_supported".to_string(),
            serde_json::Value::Bool(supported),
        );
    }
}

pub async fn events_handler(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Track this connection
    let count = state.sse_connections.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::debug!("SSE connection opened ({} active)", count);

    let guard = SseConnectionGuard {
        counter: state.sse_connections.clone(),
    };
    let shutdown = state.shutdown.clone();
    let rx = state.bus.subscribe();

    // Create stream that terminates on shutdown
    // Use futures::StreamExt::take_until via UFCS (tokio_stream doesn't have it)
    let base_stream = BroadcastStream::new(rx);
    let with_shutdown =
        futures::StreamExt::take_until(base_stream, async move { shutdown.cancelled().await });

    let stream = with_shutdown
        .filter_map(|result| match result {
            Ok(event) => {
                // Serialize event to a Value first so ZoneDiscovered's embedded
                // zone can be patched with the live browse_supported derivation
                // (#557) before going out as JSON.
                match serde_json::to_value(&event) {
                    Ok(mut value) => {
                        patch_zone_discovered_browse_supported(&mut value);
                        match serde_json::to_string(&value) {
                            Ok(json) => Some(Ok(Event::default().data(json))),
                            Err(_) => None,
                        }
                    }
                    Err(_) => None,
                }
            }
            Err(_) => None, // Skip lagged messages
        })
        // Use map + flatten to attach guard lifetime to stream
        // When stream ends, guard is dropped (decrementing counter)
        .map(move |item| {
            let _ = &guard; // Keep guard alive while stream produces items
            item
        });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}

// =============================================================================
// OpenHome handlers
// =============================================================================

/// GET /openhome/status - OpenHome discovery status
pub async fn openhome_status_handler(
    State(state): State<AppState>,
) -> Json<crate::adapters::openhome::OpenHomeStatus> {
    Json(state.openhome.get_status().await)
}

/// GET /openhome/zones - List all discovered OpenHome devices
pub async fn openhome_zones_handler(
    State(state): State<AppState>,
) -> Json<ZonesWrapper<crate::adapters::openhome::OpenHomeZone>> {
    Json(ZonesWrapper {
        zones: state.openhome.get_zones().await,
    })
}

/// GET /openhome/zone/:zone_id/now_playing - Get now playing for zone
pub async fn openhome_now_playing_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
) -> impl IntoResponse {
    match state.openhome.get_now_playing(&zone_id).await {
        Some(np) => (StatusCode::OK, Json(np)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Zone not found: {}", zone_id),
            }),
        )
            .into_response(),
    }
}

/// OpenHome control request
#[derive(Deserialize)]
pub struct OpenHomeControlRequest {
    pub zone_id: String,
    pub action: String,
    #[serde(default)]
    pub value: Option<i32>,
}

/// POST /openhome/control - Control OpenHome device
pub async fn openhome_control_handler(
    State(state): State<AppState>,
    Json(req): Json<OpenHomeControlRequest>,
) -> impl IntoResponse {
    let result = match renderer_runtime_command_from_action(&req.action, req.value) {
        Ok(command) => dispatch_openhome_runtime_command(&state, &req.zone_id, command).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

// =============================================================================
// UPnP handlers
// =============================================================================

/// GET /upnp/status - UPnP discovery status
pub async fn upnp_status_handler(
    State(state): State<AppState>,
) -> Json<crate::adapters::upnp::UPnPStatus> {
    Json(state.upnp.get_status().await)
}

/// GET /upnp/zones - List all discovered UPnP renderers
pub async fn upnp_zones_handler(
    State(state): State<AppState>,
) -> Json<ZonesWrapper<crate::adapters::upnp::UPnPZone>> {
    Json(ZonesWrapper {
        zones: state.upnp.get_zones().await,
    })
}

/// GET /upnp/zone/:zone_id/now_playing - Get now playing for renderer
pub async fn upnp_now_playing_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
) -> impl IntoResponse {
    match state.upnp.get_now_playing(&zone_id).await {
        Some(np) => (StatusCode::OK, Json(np)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Renderer not found: {}", zone_id),
            }),
        )
            .into_response(),
    }
}

/// UPnP control request
#[derive(Deserialize)]
pub struct UPnPControlRequest {
    pub zone_id: String,
    pub action: String,
    #[serde(default)]
    pub value: Option<i32>,
}

/// POST /upnp/control - Control UPnP renderer
pub async fn upnp_control_handler(
    State(state): State<AppState>,
    Json(req): Json<UPnPControlRequest>,
) -> impl IntoResponse {
    let result = match renderer_runtime_command_from_action(&req.action, req.value) {
        Ok(command) => dispatch_upnp_runtime_command(&state, &req.zone_id, command).await,
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

// =============================================================================
// Configuration handlers
// =============================================================================

/// LMS configuration request
#[derive(Deserialize)]
pub struct LmsConfigRequest {
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// POST /lms/configure - Configure LMS connection
pub async fn lms_configure_handler(
    State(state): State<AppState>,
    Json(req): Json<LmsConfigRequest>,
) -> impl IntoResponse {
    // LMS has two concurrent observers of the same `lms:` projection.  Stop
    // both before retirement/reconfiguration so an old CLI callback cannot
    // republish zones from the former server after the projection is flushed.
    state
        .coordinator
        .stop_adapter_and_companions_then_flush(state.lms.as_ref(), "lms", "LMS reconfiguration")
        .await;

    // Configure new connection
    state
        .lms
        .configure(req.host.clone(), req.port, req.username, req.password)
        .await;

    // Start both observers together; the CLI companion is not an optional
    // feature of a manually configured LMS connection.
    match state
        .coordinator
        .start_adapter_and_companions(state.lms.as_ref())
        .await
    {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "ok": true,
                "host": req.host,
                "port": req.port.unwrap_or(9000)
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// HQPlayer configuration request
#[derive(Deserialize)]
pub struct HqpConfigRequest {
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub web_port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// POST /hqplayer/configure - Configure HQPlayer connection
pub async fn hqp_configure_handler(
    State(state): State<AppState>,
    Json(req): Json<HqpConfigRequest>,
) -> impl IntoResponse {
    // Configure the adapter
    state
        .hqplayer
        .configure(
            req.host.clone(),
            req.port,
            req.web_port,
            req.username,
            req.password,
        )
        .await;

    // Save to instance manager for persistence
    state.hqp_instances.save_to_config().await;

    // An enabled fresh installation may have skipped the HQPlayer lifecycle at process startup
    // because no endpoint existed yet. Start the idempotent manager after configuration rather than
    // requiring a process restart, while still honoring the adapter's explicit enabled setting.
    if state.coordinator.is_enabled("hqplayer").await {
        match state.hqp_instances.start().await {
            Ok(()) => state.coordinator.set_running("hqplayer", true).await,
            Err(error) => {
                tracing::warn!(
                    "HQPlayer managed lifecycle could not start after configuration: {error}"
                )
            }
        }
    }

    // Test connection by attempting to get pipeline status (this establishes connection)
    let connected = match state.hqplayer.get_pipeline_status().await {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("HQPlayer connection test failed: {}", e);
            false
        }
    };

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "host": req.host,
            "port": req.port.unwrap_or(4321),
            "web_port": req.web_port.unwrap_or(8088),
            "connected": connected
        })),
    )
        .into_response()
}

/// GET /lms/config - Get current LMS configuration
pub async fn lms_config_handler(State(state): State<AppState>) -> impl IntoResponse {
    let status = state.lms.get_status().await;
    Json(serde_json::json!({
        "configured": status.host.is_some(),
        "connected": status.connected,
        "host": status.host,
        "port": status.port,
        "cli_subscription_active": status.cli_subscription_active,
        "poll_interval_secs": status.poll_interval_secs
    }))
}

/// GET /hqplayer/config - Get current HQPlayer configuration
pub async fn hqp_config_handler(State(state): State<AppState>) -> impl IntoResponse {
    let status = state.hqplayer.get_status().await;
    let has_web_creds = state.hqplayer.has_web_credentials().await;
    Json(serde_json::json!({
        "configured": status.host.is_some(),
        "connected": status.connected,
        "host": status.host,
        "port": status.port,
        "web_port": status.web_port,
        "has_web_credentials": has_web_creds
    }))
}

/// HQPlayer detect request body
#[derive(Deserialize)]
pub struct HqpDetectRequest {
    pub host: String,
    #[serde(default = "default_hqp_port")]
    pub port: u16,
}

fn default_hqp_port() -> u16 {
    4321
}

/// POST /hqp/detect - Detect HQPlayer at a given host
pub async fn hqp_detect_handler(Json(req): Json<HqpDetectRequest>) -> impl IntoResponse {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::time::{timeout, Duration};

    // Try to connect to HQPlayer's native protocol port
    let addr = format!("{}:{}", req.host, req.port);

    let stream = match timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect(&addr),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(_)) | Err(_) => {
            return Json(serde_json::json!({
                "reachable": false,
                "error": "Cannot connect to HQPlayer at this address"
            }));
        }
    };

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // Read initial greeting
    let mut greeting = String::new();
    if timeout(Duration::from_secs(2), reader.read_line(&mut greeting))
        .await
        .is_err()
    {
        return Json(serde_json::json!({
            "reachable": false,
            "error": "No response from HQPlayer"
        }));
    }

    // Send INFO command
    if write_half
        .write_all(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><info/>\n")
        .await
        .is_err()
    {
        return Json(serde_json::json!({
            "reachable": false,
            "error": "Failed to send command to HQPlayer"
        }));
    }

    // Read INFO response
    let mut response = String::new();
    if timeout(Duration::from_secs(2), reader.read_line(&mut response))
        .await
        .is_err()
    {
        return Json(serde_json::json!({
            "reachable": false,
            "error": "No INFO response from HQPlayer"
        }));
    }

    // Parse XML response for product/version
    let product = extract_xml_attr(&response, "product");
    let version = extract_xml_attr(&response, "version");
    let is_embedded = product
        .as_ref()
        .map(|p| p.to_lowercase().contains("embedded"))
        .unwrap_or(false);

    Json(serde_json::json!({
        "reachable": true,
        "product": product,
        "version": version,
        "isEmbedded": is_embedded
    }))
}

/// Extract attribute value from XML string
fn extract_xml_attr(xml: &str, attr: &str) -> Option<String> {
    let pattern = format!("{}=\"", attr);
    if let Some(start) = xml.find(&pattern) {
        let value_start = start + pattern.len();
        if let Some(end) = xml[value_start..].find('"') {
            return Some(xml[value_start..value_start + end].to_string());
        }
    }
    None
}

// =============================================================================
// HQPlayer multi-instance handlers
// =============================================================================

/// GET /hqp/instances - List all HQPlayer instances
pub async fn hqp_instances_handler(State(state): State<AppState>) -> impl IntoResponse {
    let instances = state.hqp_instances.list_instances().await;
    Json(InstancesWrapper { instances })
}

/// HQPlayer add instance request
#[derive(Deserialize)]
pub struct HqpAddInstanceRequest {
    pub name: String,
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub web_port: Option<u16>,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// POST /hqp/instances - Add or update an HQPlayer instance
pub async fn hqp_add_instance_handler(
    State(state): State<AppState>,
    Json(req): Json<HqpAddInstanceRequest>,
) -> impl IntoResponse {
    if req.name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Instance name is required".to_string(),
            }),
        )
            .into_response();
    }

    if req.host.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "Host is required".to_string(),
            }),
        )
            .into_response();
    }

    let _adapter = state
        .hqp_instances
        .add_instance(
            req.name.clone(),
            req.host.clone(),
            req.port,
            req.web_port,
            req.username,
            req.password,
        )
        .await;

    if state.coordinator.is_enabled("hqplayer").await {
        match state.hqp_instances.start().await {
            Ok(()) => state.coordinator.set_running("hqplayer", true).await,
            Err(error) => tracing::warn!(
                "HQPlayer managed lifecycle could not start after adding an instance: {error}"
            ),
        }
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "name": req.name,
            "host": req.host,
            "port": req.port.unwrap_or(4321)
        })),
    )
        .into_response()
}

/// DELETE /hqp/instances/:name - Remove an HQPlayer instance
pub async fn hqp_remove_instance_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    // Remove zone links pointing to this instance first
    let _links_removed = state.hqp_zone_links.remove_links_for_instance(&name).await;

    if state.hqp_instances.remove_instance(&name).await {
        (
            StatusCode::OK,
            Json(serde_json::json!({"ok": true, "removed": name})),
        )
            .into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Instance not found: {}", name),
            }),
        )
            .into_response()
    }
}

/// GET /hqp/instances/:name/profiles - Get profiles for a specific HQPlayer instance
pub async fn hqp_instance_profiles_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    if state.hqp_instances.get(&name).await.is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Instance not found: {}", name),
            }),
        )
            .into_response();
    }

    match refresh_hqp_profiles_aggregate(&state, &name).await {
        Ok(profiles) => (StatusCode::OK, Json(profiles)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// POST /hqp/instances/:name/profile - Load a profile on a specific HQPlayer instance
pub async fn hqp_instance_load_profile_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<HqpProfileRequest>,
) -> impl IntoResponse {
    let profile = req.profile;
    match crate::knobs::routes::dispatch_hqplayer_reconfiguration(
        &state,
        &name,
        HqpRuntimeCommand::LoadProfile {
            profile: profile.clone(),
        },
    )
    .await
    {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({"ok": true, "instance": name, "profile": profile})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.message().to_string(),
            }),
        )
            .into_response(),
    }
}

/// GET /hqp/instances/:name/matrix/profiles - Get matrix profiles for a specific instance
pub async fn hqp_instance_matrix_profiles_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    if state.hqp_instances.get(&name).await.is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Instance not found: {}", name),
            }),
        )
            .into_response();
    }

    match refresh_hqp_advanced_aggregate(&state, &name).await {
        Ok(snapshot) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "instance": name,
                "profiles": snapshot.matrix_profiles,
                "current": snapshot.current_matrix_profile
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// Matrix profile request for instance
#[derive(Deserialize)]
pub struct HqpInstanceMatrixProfileRequest {
    pub value: u32,
}

/// POST /hqp/instances/:name/matrix/profile - Set matrix profile on a specific instance
pub async fn hqp_instance_set_matrix_profile_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<HqpInstanceMatrixProfileRequest>,
) -> impl IntoResponse {
    let profile = match refresh_hqp_advanced_aggregate(&state, &name)
        .await
        .and_then(|snapshot| {
            snapshot
                .matrix_profiles
                .into_iter()
                .find(|profile| profile.index == req.value)
                .ok_or_else(|| anyhow::anyhow!("Unknown matrix profile index: {}", req.value))
        }) {
        Ok(profile) => profile,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: e.to_string(),
                }),
            )
                .into_response();
        }
    };
    match crate::knobs::routes::dispatch_hqplayer_reconfiguration(
        &state,
        &name,
        HqpRuntimeCommand::Pipeline {
            setting: "matrix_profile".to_string(),
            value: profile.name,
        },
    )
    .await
    {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({"ok": true, "instance": name, "value": req.value})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.message().to_string(),
            }),
        )
            .into_response(),
    }
}

// =============================================================================
// HQPlayer zone linking handlers
// =============================================================================

/// GET /hqp/zones/links - Get all zone links
pub async fn hqp_zone_links_handler(State(state): State<AppState>) -> impl IntoResponse {
    let links = state.hqp_zone_links.get_links().await;
    Json(serde_json::json!({ "links": links }))
}

/// Zone link request
#[derive(Deserialize)]
pub struct ZoneLinkRequest {
    pub zone_id: String,
    pub instance: String,
}

/// POST /hqp/zones/link - Link a zone to an HQPlayer instance
pub async fn hqp_zone_link_handler(
    State(state): State<AppState>,
    Json(req): Json<ZoneLinkRequest>,
) -> impl IntoResponse {
    if req.zone_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "zone_id is required".to_string(),
            }),
        )
            .into_response();
    }

    if req.instance.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "instance is required".to_string(),
            }),
        )
            .into_response();
    }

    match state
        .hqp_zone_links
        .link_zone(req.zone_id.clone(), req.instance.clone())
        .await
    {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "ok": true,
                "zone_id": req.zone_id,
                "instance": req.instance
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: e.to_string(),
            }),
        )
            .into_response(),
    }
}

/// Zone unlink request
#[derive(Deserialize)]
pub struct ZoneUnlinkRequest {
    pub zone_id: String,
}

/// POST /hqp/zones/unlink - Unlink a zone from HQPlayer
pub async fn hqp_zone_unlink_handler(
    State(state): State<AppState>,
    Json(req): Json<ZoneUnlinkRequest>,
) -> impl IntoResponse {
    if req.zone_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "zone_id is required".to_string(),
            }),
        )
            .into_response();
    }

    let was_linked = state.hqp_zone_links.unlink_zone(&req.zone_id).await;

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "ok": true,
            "zone_id": req.zone_id,
            "was_linked": was_linked
        })),
    )
        .into_response()
}

/// GET /hqp/zones/:zone_id/pipeline - Get HQP pipeline for a linked zone
pub async fn hqp_zone_pipeline_handler(
    State(state): State<AppState>,
    Path(zone_id): Path<String>,
) -> impl IntoResponse {
    let instance = state.hqp_zone_links.get_instance_for_zone(&zone_id).await;
    let pipeline = match instance {
        Some(instance) => state
            .aggregator
            .get_hqplayer_snapshot(&instance)
            .await
            .filter(|snapshot| snapshot.presence == HqpSnapshotPresence::Live)
            .map(|snapshot| snapshot.observation.pipeline),
        None => None,
    };
    match pipeline {
        Some(pipeline) => (StatusCode::OK, Json(pipeline)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!(
                    "Zone {} not linked to HQPlayer or HQPlayer not configured",
                    zone_id
                ),
            }),
        )
            .into_response(),
    }
}

// =============================================================================
// HQPlayer discovery handler
// =============================================================================

/// HQP discovery request
#[derive(Deserialize)]
pub struct HqpDiscoverRequest {
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// GET /hqp/discover - Discover HQPlayer instances on the network via UDP multicast
pub async fn hqp_discover_handler(Query(params): Query<HqpDiscoverRequest>) -> impl IntoResponse {
    use crate::adapters::hqplayer::discover_hqplayers;

    match discover_hqplayers(params.timeout_ms).await {
        Ok(instances) => (
            StatusCode::OK,
            Json(serde_json::json!({ "discovered": instances })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("Discovery failed: {}", e),
            }),
        )
            .into_response(),
    }
}

// =============================================================================
// App settings handlers
// =============================================================================

/// App settings for UI preferences
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    // Support both snake_case (Rust) and camelCase (Node.js) for seamless migration
    #[serde(default, alias = "hideKnobsPage")]
    pub hide_knobs_page: bool,
    #[serde(default, alias = "hideHqpPage")]
    pub hide_hqp_page: bool,
    #[serde(default, alias = "hideLmsPage")]
    pub hide_lms_page: bool,
    #[serde(default)]
    pub adapters: AdapterSettings,
    /// Who turned `adapters.mqtt` on (#613).
    ///
    /// Not part of `AdapterSettings` because it is not an adapter toggle: it
    /// is provenance for one, and the only thing that reads it is the
    /// decision about whether to warn that Home Assistant is not consuming
    /// what UHC publishes. Under the add-on, publishing that nobody asked
    /// for must not produce a warning about nobody receiving it.
    ///
    /// `POST /api/settings` sets this itself rather than trusting the body,
    /// so the client never has to know the field exists.
    #[serde(default)]
    pub mqtt_enabled_by: crate::api::credentials::MqttEnableSource,
    /// Zone IDs the user has hidden from every zone list.
    ///
    /// `Option` rather than `Vec` because this type is both the stored shape *and* the request body
    /// of `POST /api/settings`, and that handler saves what it is given wholesale. The settings
    /// page builds an `AppSettings` from its own form fields, so a plain `#[serde(default)] Vec`
    /// would deserialise a body without the key as empty and silently erase the user's hide list
    /// every time they toggled an adapter. `None` means "the caller said nothing about hidden
    /// zones, keep what is stored"; `Some(vec![])` is an explicit clear. See
    /// `api_settings_post_handler`.
    #[serde(
        default,
        alias = "hiddenZones",
        skip_serializing_if = "Option::is_none"
    )]
    pub hidden_zones: Option<Vec<String>>,
    /// The user's explicit zone order, as zone IDs. Zones absent from it sort alphabetically after
    /// the ones present, so a newly discovered zone lands somewhere predictable rather than at a
    /// position that depends on `HashMap` iteration.
    ///
    /// `Option` for the same reason as `hidden_zones` — see that field.
    #[serde(default, alias = "zoneOrder", skip_serializing_if = "Option::is_none")]
    pub zone_order: Option<Vec<String>>,
    /// Per-zone display-name overrides, keyed by zone ID.
    ///
    /// Renaming is applied *before* sorting, which is what makes it useful beyond cosmetics: a user
    /// who prefixes several zones with `Basement - ` gets them grouped together in every list, with
    /// no grouping feature. That is the intended use, not a side effect.
    ///
    /// `Option` for the same reason as `hidden_zones` — see that field.
    #[serde(default, alias = "zoneNames", skip_serializing_if = "Option::is_none")]
    pub zone_names: Option<std::collections::BTreeMap<String, String>>,
}

impl AppSettings {
    /// The hide list, treating "unset" and "empty" alike — the distinction matters only to the
    /// settings write path, never to a reader.
    pub fn hidden_zone_ids(&self) -> &[String] {
        self.hidden_zones.as_deref().unwrap_or(&[])
    }

    /// The explicit zone order, empty when the user has never reordered anything.
    pub fn zone_order_ids(&self) -> &[String] {
        self.zone_order.as_deref().unwrap_or(&[])
    }

    /// The custom display name for a zone, if the user set one.
    pub fn custom_zone_name(&self, zone_id: &str) -> Option<&str> {
        self.zone_names
            .as_ref()
            .and_then(|names| names.get(zone_id))
            .map(String::as_str)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AdapterSettings {
    #[serde(default = "default_true")]
    pub roon: bool,
    #[serde(default)]
    pub upnp: bool,
    #[serde(default)]
    pub openhome: bool,
    #[serde(default)]
    pub lms: bool,
    #[serde(default)]
    pub hqplayer: bool,
    /// Spotify is an opt-in controller for existing Connect devices.
    #[serde(default)]
    pub spotify: bool,
    /// Apple Music is an opt-in bridge-backed playback adapter.
    #[serde(default)]
    pub applemusic: bool,
    /// Music Assistant is an opt-in remote playback adapter.
    #[serde(default)]
    pub musicassistant: bool,
    /// The MQTT/Home Assistant discovery publisher (#508) is opt-in and off
    /// by default: it needs a broker configured before it can do anything.
    #[serde(default)]
    pub mqtt: bool,
}

fn default_true() -> bool {
    true
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            hide_knobs_page: false,
            hide_hqp_page: false,
            hide_lms_page: false,
            // Nothing hidden by default. Roon exposes no private-zone flag to extensions, so any
            // default-on hiding would be us guessing which of the user's zones are personal — and
            // a wrong guess makes a zone vanish with no way for them to tell it was our choice.
            hidden_zones: None,
            // No explicit order until the user makes one; until then, alphabetical.
            zone_order: None,
            // Zones keep the names their provider reports until the user overrides one.
            zone_names: None,
            adapters: AdapterSettings {
                roon: true,
                upnp: false,
                openhome: false,
                lms: false,
                hqplayer: false,
                spotify: false,
                applemusic: false,
                musicassistant: false,
                mqtt: false,
            },
            mqtt_enabled_by: crate::api::credentials::MqttEnableSource::Automatic,
        }
    }
}

const APP_SETTINGS_FILE: &str = "app-settings.json";

fn settings_path() -> std::path::PathBuf {
    crate::config::get_config_file_path(APP_SETTINGS_FILE)
}

/// Load app settings from disk
/// Issue #76: Uses read_config_file for backwards-compatible fallback
pub fn load_app_settings() -> AppSettings {
    // read_config_file checks subdir first, falls back to root for legacy files
    let mut settings = match crate::config::read_config_file(APP_SETTINGS_FILE) {
        Some(content) => match serde_json::from_str(&content) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Failed to parse app settings: {}", e);
                AppSettings::default()
            }
        },
        None => AppSettings::default(),
    };

    // Auto-enable LMS adapter when started from LMS plugin
    // The LMS plugin sets LMS_UNIFIEDHIFI_STARTED=true when launching the bridge
    if crate::config::is_lms_plugin_started() && !settings.adapters.lms {
        tracing::info!("LMS plugin detected (LMS_UNIFIEDHIFI_STARTED), auto-enabling LMS adapter");
        settings.adapters.lms = true;
    }

    settings
}

/// Serialises every read-modify-write of `app-settings.json`.
///
/// The zone handlers each load the settings, change one field, and save the whole file. Without a
/// lock, two requests that overlap both read the same starting state and the second save discards
/// the first change — so hiding two zones in quick succession, or two browser tabs editing
/// different zones, can silently keep only one. Splitting the API into one-zone-per-request (rather
/// than whole-list writes) narrows that window but does not close it; this does.
static SETTINGS_WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Load the settings, apply `mutate`, and save — atomically with respect to other callers.
///
/// Every zone-settings mutation goes through here. The load happens *inside* the lock, which is the
/// point: a caller that loaded beforehand would be acting on state that another writer may already
/// have replaced.
fn mutate_app_settings(mutate: impl FnOnce(&mut AppSettings)) -> bool {
    let _guard = SETTINGS_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut settings = load_app_settings();
    mutate(&mut settings);
    save_app_settings_locked(&settings)
}

/// Turn the MQTT publisher on in persisted settings during the startup
/// environment bootstrap (#605).
///
/// Persisting matters as much as enabling does. Enabling only in memory would
/// leave `app-settings.json` saying `mqtt: false`, so the *next* boot could
/// not tell "the add-on switched this on for you" from "you switched it off",
/// and the user's later decision to turn it off would not survive a restart.
pub fn enable_mqtt_in_settings(source: credentials::MqttEnableSource) -> bool {
    mutate_app_settings(|settings| {
        settings.adapters.mqtt = true;
        // Who gets the blame, and therefore the warning (#613). Outside the
        // add-on, `UHC_MQTT_*` is the operator's own docker-compose or
        // systemd unit - a deliberate choice, so
        // [`credentials::MqttEnableSource::User`]. Under the add-on nothing
        // reaches here at all any more; the only records left carrying
        // `Automatic` are the ones an older add-on wrote before this
        // distinction existed, which is exactly whose warning must stay off.
        settings.mqtt_enabled_by = source;
    })
}

/// Overwrite the settings wholesale. Test-only on purpose.
///
/// Production writes go through [`mutate_app_settings`], which reads the current state inside the
/// lock. A blind overwrite is safe only when the caller knows nothing else is writing — true for a
/// test seeding a temp config directory, and not true anywhere else.
#[cfg(test)]
fn save_app_settings(settings: &AppSettings) -> bool {
    let _guard = SETTINGS_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    save_app_settings_locked(settings)
}

/// The actual write. Callers must already hold [`SETTINGS_WRITE_LOCK`].
fn save_app_settings_locked(settings: &AppSettings) -> bool {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let json = match serde_json::to_string_pretty(settings) {
        Ok(json) => json,
        Err(e) => {
            tracing::error!("Failed to serialize app settings: {}", e);
            return false;
        }
    };

    // Write to a sibling temp file, flush it to disk, then rename over the target. Writing in place
    // with `std::fs::write` truncates first, so a crash or a full disk between the truncate and the
    // last byte costs the user every adapter toggle, hide-list entry, and zone name at once.
    //
    // All three steps matter and none of them substitutes for another:
    //
    // - The temp file is a *sibling* so the rename stays within one filesystem. `rename(2)` across
    //   devices fails outright.
    // - `sync_all` before the rename is what makes the guarantee real. `rename` is atomic for the
    //   name only; it says nothing about whether the temp file's bytes have reached the disk. Skip
    //   the flush and a power loss can leave the renamed file empty or half-written on ext4,
    //   XFS, and APFS alike — the exact corruption the temp file was supposed to prevent.
    // - Cleaning up on rename failure keeps a stale `.json.tmp` from accumulating.
    //
    // Not covered: the directory entry created by the rename is itself unflushed, so a power loss
    // right after can lose the *new* settings. That is acceptable — the user sees their previous
    // configuration, not a damaged one, which is the property this is protecting.
    let temp = path.with_extension("json.tmp");
    let written = std::fs::File::create(&temp).and_then(|mut file| {
        use std::io::Write;
        file.write_all(json.as_bytes())?;
        file.sync_all()
    });
    if let Err(e) = written {
        tracing::error!("Failed to write app settings: {}", e);
        let _ = std::fs::remove_file(&temp);
        return false;
    }

    match std::fs::rename(&temp, &path) {
        Ok(()) => {
            tracing::info!("Saved app settings");
            true
        }
        Err(e) => {
            tracing::error!("Failed to replace app settings: {}", e);
            let _ = std::fs::remove_file(&temp);
            false
        }
    }
}

/// GET /api/settings - Get app settings
pub async fn api_settings_get_handler() -> impl IntoResponse {
    Json(load_app_settings())
}

#[derive(Debug, Serialize)]
pub struct SpotifyAccountResponse {
    pub account: Option<ProviderAccount>,
    /// Whether UHC has a persisted Spotify client configuration. This is
    /// deliberately non-secret and lets the UI distinguish configured from
    /// unconfigured even when Spotify profile lookup is unavailable.
    #[serde(default)]
    pub configured: bool,
    pub error: Option<String>,
}

/// GET /api/providers/spotify/account - Last account identity reported by the
/// Spotify adapter. Tokens and client credentials never leave the server.
pub async fn spotify_account_handler(
    State(state): State<AppState>,
) -> Json<SpotifyAccountResponse> {
    Json(SpotifyAccountResponse {
        account: state.aggregator.get_provider_account("spotify").await,
        configured: state.provider_auth.spotify_configured().await,
        error: state.aggregator.get_adapter_error("spotify").await,
    })
}

/// POST /api/settings - Update app settings with dynamic adapter enable/disable
pub async fn api_settings_post_handler(
    State(state): State<AppState>,
    Json(new_settings): Json<AppSettings>,
) -> impl IntoResponse {
    // Carry the zone fields forward when the body did not mention them. This endpoint saves its
    // request body wholesale, and its main caller -- the settings page -- builds an `AppSettings`
    // from its own form fields, which do not include the zone preferences. Without this, toggling
    // any adapter would unhide every zone the user had hidden. An explicit `"hidden_zones": []`
    // still clears.
    //
    // The merge happens inside the write lock, and reads the settings from inside it too. Doing the
    // load outside would reintroduce exactly the race the carry-forward exists to prevent: a zone
    // hidden between the load and the save would be carried forward from stale state and lost.
    let mut new_settings = new_settings;
    let mut old_settings = AppSettings::default();
    let saved = mutate_app_settings(|current| {
        old_settings = current.clone();
        if new_settings.hidden_zones.is_none() {
            new_settings.hidden_zones = current.hidden_zones.clone();
        }
        if new_settings.zone_order.is_none() {
            new_settings.zone_order = current.zone_order.clone();
        }
        if new_settings.zone_names.is_none() {
            new_settings.zone_names = current.zone_names.clone();
        }
        // Provenance for the MQTT toggle (#613), decided here rather than
        // taken from the body: a request that flips `mqtt` from off to on is
        // a person clicking a switch, which is exactly what makes the
        // "Home Assistant isn't receiving this" warning fair to show. Turning
        // it off clears the flag, so a later automatic enable does not
        // inherit a choice made about a different broker.
        new_settings.mqtt_enabled_by = if new_settings.adapters.mqtt {
            if current.adapters.mqtt {
                current.mqtt_enabled_by
            } else {
                credentials::MqttEnableSource::User
            }
        } else {
            credentials::MqttEnableSource::Automatic
        };
        *current = new_settings.clone();
    });

    if !saved {
        return Json(serde_json::json!({"ok": false, "error": "Failed to save settings"}));
    }
    let new_settings = new_settings;

    // Compare adapter enabled states and start/stop as needed
    let old_adapters = &old_settings.adapters;
    let new_adapters = &new_settings.adapters;

    // Helper to process adapter state changes
    let adapters_list = state.startable_adapters.clone();
    let coord = state.coordinator.clone();

    // Check each adapter for state changes
    let adapter_changes: Vec<(&str, bool)> = vec![
        ("roon", old_adapters.roon != new_adapters.roon),
        ("lms", old_adapters.lms != new_adapters.lms),
        ("openhome", old_adapters.openhome != new_adapters.openhome),
        ("upnp", old_adapters.upnp != new_adapters.upnp),
        ("hqplayer", old_adapters.hqplayer != new_adapters.hqplayer),
        ("spotify", old_adapters.spotify != new_adapters.spotify),
        (
            "applemusic",
            old_adapters.applemusic != new_adapters.applemusic,
        ),
        (
            "musicassistant",
            old_adapters.musicassistant != new_adapters.musicassistant,
        ),
        ("mqtt", old_adapters.mqtt != new_adapters.mqtt),
    ];

    let mut applied_transitions = Vec::new();
    for (name, changed) in &adapter_changes {
        if !changed {
            continue;
        }

        // Get the new enabled state
        let now_enabled = match *name {
            "roon" => new_adapters.roon,
            "lms" => new_adapters.lms,
            "openhome" => new_adapters.openhome,
            "upnp" => new_adapters.upnp,
            "hqplayer" => new_adapters.hqplayer,
            "spotify" => new_adapters.spotify,
            "applemusic" => new_adapters.applemusic,
            "musicassistant" => new_adapters.musicassistant,
            // MQTT is a bus consumer, not a `Startable` in `adapters_list` -
            // handled separately below via `state.mqtt` (the sanctioned
            // surface `tests/adapter_boundary_lint.rs` allows this module
            // to reach; it is not part of the legacy coordinator registry).
            "mqtt" => continue,
            _ => continue,
        };

        // Update coordinator state
        coord.set_enabled(name, now_enabled).await;
        applied_transitions.push(*name);

        // Find the adapter and start/stop it
        if let Some(adapter) = adapters_list.iter().find(|a| a.name() == *name) {
            if now_enabled {
                // An adapter that is not yet configured (no host, no
                // credentials, ...) is not a startup failure -- it is the
                // ordinary state of "enabled, waiting to be configured".
                // `start_all_enabled`/`start_enabled` already treat
                // `can_start() == false` as a skip rather than an error; this
                // toggle path used to reach `start_adapter_and_companions`
                // unconditionally, which raised "adapter cannot start" and
                // rolled the whole settings write back (see issue #544).
                // Mirror the skip here so enabling an unconfigured adapter
                // succeeds and leaves it idle, matching v3's behavior, and
                // only a genuine start failure of a *configured* adapter
                // triggers rollback.
                if !adapter.can_start().await {
                    tracing::info!(
                        "Adapter {} enabled but not yet configured; leaving idle until configured",
                        name
                    );
                } else {
                    tracing::info!("Dynamically enabling adapter: {}", name);
                    if let Err(error) = coord.start_adapter_and_companions(adapter.as_ref()).await {
                        tracing::warn!("Failed to start adapter {}: {}", name, error);
                        // Do not persist a feature as enabled when its runtime
                        // could not be started. The Settings client will restore
                        // its last confirmed switch position from this response.
                        coord
                            .rollback_adapter_transitions(
                                adapters_list.as_ref(),
                                old_adapters,
                                &applied_transitions,
                            )
                            .await;
                        // The new settings were already persisted (inside the write
                        // lock, for the hidden-zones carry-forward); put the old
                        // ones back so disk matches the rolled-back runtime.
                        let _ = mutate_app_settings(|current| *current = old_settings.clone());
                        return Json(serde_json::json!({
                            "ok": false,
                            "error": format!("Could not enable {name}: {error}"),
                        }));
                    }
                }
            } else {
                tracing::info!("Dynamically disabling adapter: {}", name);
                coord
                    .stop_adapter_and_companions_then_flush(
                        adapter.as_ref(),
                        name,
                        "settings disable",
                    )
                    .await;
            }
        }
    }

    // MQTT is a bus consumer with no zones/companions to flush, so it does
    // not go through the coordinator's start/stop-and-flush lifecycle - it
    // simply turns its background publisher task on or off.
    if old_adapters.mqtt != new_adapters.mqtt {
        state.mqtt.set_enabled(new_adapters.mqtt).await;
        // Keep the live publisher's copy of the provenance in step with the
        // one just persisted (#613), so the status endpoint stops or starts
        // qualifying for the "Home Assistant isn't receiving this" warning
        // in the same request that flipped the toggle.
        state
            .mqtt
            .set_enable_source(new_settings.mqtt_enabled_by)
            .await;
        applied_transitions.push("mqtt");
    }

    // Settings were persisted up front (inside the write lock, so the
    // hidden-zones carry-forward is race-free); every requested runtime
    // transition succeeded, so disk and runtime now agree.
    Json(serde_json::json!({"ok": true}))
}

/// One row of the zone management list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManagedZone {
    /// The name as displayed everywhere — the user's override when set, otherwise the provider's.
    pub zone_name: String,
    /// The name the provider reports. Shown alongside a renamed zone so the user can always map it
    /// back to what Roon or LMS calls it.
    pub provider_name: String,
    /// Whether `zone_name` is a user override rather than the provider's own name.
    pub renamed: bool,
    pub zone_id: String,
    pub source: String,
    pub hidden: bool,
}

/// `GET /api/zones/visibility` response.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ManagedZonesResponse {
    pub zones: Vec<ManagedZone>,
}

/// GET /api/zones/visibility - every zone the user can manage, hidden ones included.
///
/// Distinct from `/zones`, which is a zone *list* and therefore excludes hidden zones. This is the
/// management view, and it must show what is hidden or hiding would be a one-way door: the only
/// screen that can unhide a zone would be unable to see it.
pub async fn zone_visibility_get(State(state): State<AppState>) -> impl IntoResponse {
    let settings = load_app_settings();
    let hidden: std::collections::HashSet<&str> = settings
        .hidden_zone_ids()
        .iter()
        .map(String::as_str)
        .collect();

    // `manageable_zones` has already applied any rename, so `zone_name` here is the effective name.
    // The provider's own name comes from the aggregator, which never sees the override.
    let provider_names: std::collections::HashMap<String, String> = state
        .aggregator
        .get_zones()
        .await
        .into_iter()
        .map(|z| (z.zone_id, z.zone_name))
        .collect();

    let zones = crate::zone_list::manageable_zones(&state)
        .await
        .into_iter()
        .map(|z| {
            let provider_name = provider_names
                .get(&z.zone_id)
                .cloned()
                .unwrap_or_else(|| z.zone_name.clone());
            ManagedZone {
                hidden: hidden.contains(z.zone_id.as_str()),
                renamed: provider_name != z.zone_name,
                provider_name,
                zone_id: z.zone_id,
                zone_name: z.zone_name,
                source: z.source,
            }
        })
        .collect();

    Json(ManagedZonesResponse { zones })
}

// The zone request bodies are defined once, in `crate::app::api`, and shared with the client that
// sends them. `src/app` is not feature-gated, so the server compiles it too. Redefining them here
// would let a field name drift into a 422 the UI could not explain.
pub use crate::app::api::{ZoneNameRequest, ZoneOrderRequest, ZoneVisibilityRequest};

/// POST /api/zones/name - rename a zone, or clear the rename.
///
/// The override lives in UHC only; nothing is written back to Roon or LMS. Since the rename is
/// applied before sorting, prefixing several zones with a common string ("Basement - ") groups them
/// in every list, which is the main reason this exists.
pub async fn zone_name_post(Json(req): Json<ZoneNameRequest>) -> impl IntoResponse {
    let trimmed = req.name.as_deref().map(str::trim).unwrap_or("").to_string();

    // Validate before taking the write lock: a rejected name must not hold up other writers.
    if trimmed.chars().count() > MAX_ZONE_NAME_CHARS {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "ok": false,
                "error": format!("Zone names are limited to {MAX_ZONE_NAME_CHARS} characters.")
            })),
        );
    }

    let saved = mutate_app_settings(|settings| {
        let mut names = settings.zone_names.take().unwrap_or_default();
        if trimmed.is_empty() {
            names.remove(&req.zone_id);
        } else {
            names.insert(req.zone_id.clone(), trimmed.clone());
        }
        settings.zone_names = Some(names);
    });

    if !saved {
        // See `zone_order_post` for why this is a 500, not `200 {"ok": false}`.
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "ok": false,
                "error": "Could not save the zone name. Check that the config directory is writable."
            })),
        );
    }

    tracing::info!(zone_id = %req.zone_id, renamed = !trimmed.is_empty(), "Zone name updated");

    (StatusCode::OK, Json(serde_json::json!({"ok": true})))
}

/// Long enough for "Basement - Left Channel Monoblock", short enough that a name cannot break the
/// layout of a zone list or a knob's small display.
const MAX_ZONE_NAME_CHARS: usize = 60;

/// POST /api/zones/order - move one zone one place up or down.
///
/// Takes a zone and a direction rather than a whole ordered list. Two reasons: the client does not
/// have to reconstruct an order it only partly knows (hidden zones are in the order too), and two
/// browser tabs each nudging a different zone both land, where whole-list writes would have the
/// second silently discard the first.
pub async fn zone_order_post(
    State(state): State<AppState>,
    Json(req): Json<ZoneOrderRequest>,
) -> impl IntoResponse {
    // The order is computed over *manageable* zones, hidden ones included, so that hiding a zone
    // and later unhiding it puts it back where the user had placed it rather than at the end.
    let effective = crate::zone_list::manageable_zones(&state).await;
    let settings = load_app_settings();
    let hidden_ids: Vec<String> = settings.hidden_zone_ids().to_vec();
    let hidden: std::collections::HashSet<&str> = hidden_ids.iter().map(String::as_str).collect();

    // A drop names its destination exactly; a button step has to reason about hidden neighbours.
    let moved = match (&req.target_zone_id, req.direction) {
        (Some(target), _) => crate::zone_list::reorder_to(&effective, &req.zone_id, target),
        (None, Some(direction)) => {
            crate::zone_list::reorder(&effective, &req.zone_id, direction, &hidden)
        }
        (None, None) => None,
    };

    let Some(new_order) = moved else {
        // Already at the end it was moved toward, or no such zone. A no-op, not an error -- and the
        // UI reads `moved` to announce "already first" instead of claiming a move that never
        // happened.
        return (
            StatusCode::OK,
            Json(serde_json::json!({
                "ok": true,
                "moved": false,
                "zone_order": hidden_ids_order(&settings),
            })),
        );
    };

    let order_to_save = new_order.clone();
    if !mutate_app_settings(move |settings| settings.zone_order = Some(order_to_save)) {
        // A real 500. Returning 200 with `{"ok": false}` would be indistinguishable from success to
        // any client that checks the HTTP status -- and a settings page that silently discards
        // writes is worse than one that refuses them. A read-only bind-mounted config directory is
        // a common Docker misconfiguration, so this path is reachable in normal use.
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "ok": false,
                "error": "Could not save the zone order. Check that the config directory is writable."
            })),
        );
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({"ok": true, "moved": true, "zone_order": new_order})),
    )
}

fn hidden_ids_order(settings: &AppSettings) -> Vec<String> {
    settings.zone_order_ids().to_vec()
}

/// POST /api/zones/visibility - hide or unhide a single zone.
///
/// Deliberately not folded into `POST /api/settings`. That endpoint takes a whole `AppSettings`, so
/// a caller wanting only to hide a zone would have to send every other setting too — and
/// `AdapterSettings::default()` is all-false, so a body that omitted `adapters` would silently
/// disable Roon. A one-zone toggle also keeps the read-modify-write window as small as it can be:
/// two browser tabs hiding different zones both land, because each request states only its own
/// change and [`mutate_app_settings`] serialises the merge. Posting a whole list instead would have
/// the second write overwrite the first no matter how the server locked.
pub async fn zone_visibility_post(Json(req): Json<ZoneVisibilityRequest>) -> impl IntoResponse {
    let mut hidden = Vec::new();
    let saved = mutate_app_settings(|settings| {
        let mut list = settings.hidden_zones.take().unwrap_or_default();
        if req.hidden {
            if !list.contains(&req.zone_id) {
                list.push(req.zone_id.clone());
            }
        } else {
            list.retain(|id| *id != req.zone_id);
        }
        // Sorted so the persisted file has a stable shape regardless of the order zones were hidden
        // in.
        list.sort();
        hidden = list.clone();
        settings.hidden_zones = Some(list);
    });

    if !saved {
        // See `zone_order_post` for why this is a 500 rather than `200 {"ok": false}`.
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "ok": false,
                "error": "Could not save zone visibility. Check that the config directory is writable."
            })),
        );
    }

    tracing::info!(
        zone_id = %req.zone_id,
        hidden = req.hidden,
        "Zone visibility updated"
    );

    (
        StatusCode::OK,
        Json(serde_json::json!({"ok": true, "hidden_zones": hidden})),
    )
}

#[cfg(test)]
fn adapter_enabled(settings: &AdapterSettings, name: &str) -> Option<bool> {
    match name {
        "roon" => Some(settings.roon),
        "lms" => Some(settings.lms),
        "openhome" => Some(settings.openhome),
        "upnp" => Some(settings.upnp),
        "hqplayer" => Some(settings.hqplayer),
        "spotify" => Some(settings.spotify),
        "applemusic" => Some(settings.applemusic),
        "musicassistant" => Some(settings.musicassistant),
        "mqtt" => Some(settings.mqtt),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{create_bus, BusEvent, PlaybackState, Zone};
    use axum::response::IntoResponse;
    use http_body_util::BodyExt;
    use serial_test::serial;
    use std::env;
    use std::time::Duration;

    /// Pulls the JSON body out of a handler's `impl IntoResponse`, for tests
    /// that need to assert on `{"ok": ..., "error": ...}` rather than only
    /// on the settings file the handler wrote.
    async fn response_json(response: impl IntoResponse) -> serde_json::Value {
        let body = response.into_response().into_body();
        let bytes = body
            .collect()
            .await
            .expect("response body must be readable")
            .to_bytes();
        serde_json::from_slice(&bytes).expect("response body must be valid JSON")
    }

    /// Builds a minimal `bus::events::Zone` for a given zone id, for tests
    /// that only care about the id -> `browse_supported` derivation.
    fn test_zone(zone_id: &str) -> Zone {
        Zone {
            zone_id: zone_id.to_string(),
            zone_name: "Test Zone".to_string(),
            state: PlaybackState::Stopped,
            volume_control: None,
            now_playing: None,
            source: "test".to_string(),
            is_controllable: true,
            is_seekable: false,
            last_updated: 0,
            is_play_allowed: true,
            is_pause_allowed: true,
            is_next_allowed: true,
            is_previous_allowed: true,
        }
    }

    /// #557: a `ZoneDiscovered` event's embedded zone predates #531's
    /// `browse_supported` flag on the wire, so it must be patched into the
    /// serialized JSON rather than read off the struct (which has no such
    /// field). Every provider #531 wired -- Roon included -- must come out
    /// `true`; a provider it did not wire must come out `false`.
    #[test]
    fn zone_discovered_sse_payload_reports_browse_supported() {
        for (zone_id, expected) in [
            ("roon:zone-1", true),
            ("lms:zone-1", true),
            ("musicassistant:zone-1", true),
            ("openhome:zone-1", false),
            ("upnp:zone-1", false),
            ("hqplayer:zone-1", false),
        ] {
            let event = BusEvent::ZoneDiscovered {
                zone: test_zone(zone_id),
            };
            let mut value = serde_json::to_value(&event).expect("event serializes");
            patch_zone_discovered_browse_supported(&mut value);
            assert_eq!(
                value.pointer("/payload/zone/browse_supported"),
                Some(&serde_json::Value::Bool(expected)),
                "zone_id={zone_id}"
            );
        }
    }

    /// Non-`ZoneDiscovered` events have no `/payload/zone` pointer, so the
    /// patch must be a no-op rather than panicking or inserting a stray
    /// field.
    #[test]
    fn patch_is_noop_for_events_without_a_zone() {
        let event = BusEvent::RoonConnected {
            core_name: "Test Core".to_string(),
            version: "1.0".to_string(),
        };
        let mut value = serde_json::to_value(&event).expect("event serializes");
        let before = value.clone();
        patch_zone_discovered_browse_supported(&mut value);
        assert_eq!(value, before);
    }

    struct FakeAdapter(&'static str);

    #[async_trait::async_trait]
    impl Startable for FakeAdapter {
        fn name(&self) -> &'static str {
            self.0
        }
        async fn start(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop(&self) {}
    }

    /// Stands in for an adapter that has never been configured -- like a
    /// fresh HQPlayer instance with no host set. `can_start()` false is the
    /// normal state of "enabled but not yet set up", not a failure.
    struct UnconfiguredAdapter(&'static str);

    #[async_trait::async_trait]
    impl Startable for UnconfiguredAdapter {
        fn name(&self) -> &'static str {
            self.0
        }
        async fn start(&self) -> anyhow::Result<()> {
            panic!("start() must not be called when can_start() is false");
        }
        async fn stop(&self) {}
        async fn can_start(&self) -> bool {
            false
        }
    }

    /// Stands in for a *configured* adapter whose runtime genuinely fails to
    /// start (bad network, crashed process, ...) -- the one case where the
    /// settings write should still roll back.
    struct FailingAdapter(&'static str);

    #[async_trait::async_trait]
    impl Startable for FailingAdapter {
        fn name(&self) -> &'static str {
            self.0
        }
        async fn start(&self) -> anyhow::Result<()> {
            Err(anyhow::anyhow!("connection refused"))
        }
        async fn stop(&self) {}
    }

    fn fake_zone(zone_id: &str, source: &str) -> Zone {
        Zone {
            zone_id: zone_id.to_string(),
            zone_name: "Test Zone".to_string(),
            state: PlaybackState::Stopped,
            volume_control: None,
            now_playing: None,
            source: source.to_string(),
            is_controllable: true,
            is_seekable: false,
            last_updated: 0,
            is_play_allowed: true,
            is_pause_allowed: true,
            is_next_allowed: true,
            is_previous_allowed: true,
        }
    }

    /// Issue #429: disabling an adapter must remove its zones, not just stop
    /// it. This exercises the real path end to end -- a live `ZoneAggregator`
    /// consuming from a real bus -- rather than only asserting that an event
    /// was published, since `ZoneAggregator`'s own handling of
    /// `AdapterStopping` had no test anywhere in the codebase either.
    #[tokio::test]
    async fn stopping_an_adapter_flushes_its_zones_from_the_aggregator() {
        let bus = create_bus();
        let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
        let agg_for_task = aggregator.clone();
        tokio::spawn(async move { agg_for_task.run().await });

        // Give the aggregator's subscription a moment to attach before the
        // first publish -- otherwise this event can race the subscribe.
        tokio::time::sleep(Duration::from_millis(10)).await;

        bus.publish(BusEvent::ZoneDiscovered {
            zone: fake_zone("lms:aa:bb:cc:dd:ee:ff", "lms"),
        });
        bus.publish(BusEvent::ZoneDiscovered {
            zone: fake_zone("roon:untouched", "roon"),
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            aggregator.get_zones().await.len(),
            2,
            "both zones should be present before the adapter stops"
        );

        let adapter: Arc<dyn Startable> = Arc::new(FakeAdapter("lms"));
        let coordinator = AdapterCoordinator::new(bus.clone());
        coordinator.register("lms", true).await;
        coordinator
            .stop_adapter_and_flush(adapter.as_ref(), "test shutdown")
            .await;
        tokio::time::sleep(Duration::from_millis(20)).await;

        let remaining = aggregator.get_zones().await;
        assert_eq!(
            remaining.len(),
            1,
            "the lms zone should have been flushed, got {remaining:?}"
        );
        assert_eq!(remaining[0].zone_id, "roon:untouched");
    }

    async fn app_state_with_startable(startable: Arc<dyn Startable>) -> AppState {
        let bus = create_bus();
        let roon = Arc::new(RoonAdapter::new_disconnected(bus.clone()));
        let hqp_instances = Arc::new(HqpInstanceManager::new(bus.clone()));
        let hqplayer = hqp_instances.get_default().await;
        let hqp_zone_links = Arc::new(HqpZoneLinkService::new(hqp_instances.clone()));
        let lms = Arc::new(LmsAdapter::new(bus.clone()));
        let openhome = Arc::new(OpenHomeAdapter::new(bus.clone()));
        let upnp = Arc::new(UPnPAdapter::new(bus.clone()));
        let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
        let coordinator = Arc::new(AdapterCoordinator::new(bus.clone()));

        AppState::new(
            roon,
            hqplayer,
            hqp_instances,
            hqp_zone_links,
            lms,
            openhome,
            upnp,
            KnobStore::new(),
            bus,
            aggregator,
            coordinator,
            vec![startable],
            Instant::now(),
            CancellationToken::new(),
        )
    }

    #[test]
    fn apple_music_artwork_accepts_only_https_apple_cdn_urls() {
        assert!(validate_apple_music_artwork_url(
            "https://is1-ssl.mzstatic.com/image/thumb/example.jpg"
        )
        .is_ok());
        assert!(validate_apple_music_artwork_url(
            "musicKit://artwork/library/item/512x512?fat=https%3A%2F%2Fis1-ssl.mzstatic.com%2Fimage%2Fthumb%2Fexample.jpg"
        )
        .is_ok());
        for url in [
            "http://is1-ssl.mzstatic.com/image.jpg",
            "https://example.com/image.jpg",
            "file:///etc/passwd",
            "https://127.0.0.1/image.jpg",
        ] {
            assert!(
                validate_apple_music_artwork_url(url).is_err(),
                "accepted unsafe artwork URL: {url}"
            );
        }
    }

    #[tokio::test]
    async fn connector_artwork_stops_streaming_at_its_source_byte_limit() {
        use axum::{
            body::{Body, Bytes},
            http::header,
            routing::get,
            Router,
        };
        use futures::stream;

        let app = Router::new().route(
            "/art",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "image/png")],
                    Body::from_stream(stream::iter([
                        Ok::<_, std::convert::Infallible>(Bytes::from_static(b"123")),
                        Ok(Bytes::from_static(b"456")),
                    ])),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let url = reqwest::Url::parse(&format!("http://{address}/art")).unwrap();

        let error = fetch_remote_artwork_url_bounded(url, 4)
            .await
            .expect_err("the second chunk must be rejected before the body can grow past 4 bytes");
        assert!(error.to_string().contains("too large"));
        server.abort();
    }

    /// Issue #429, proving the wiring itself: calling the real
    /// `POST /api/settings` handler to disable an adapter must remove its
    /// zones. Unlike `stopping_an_adapter_flushes_its_zones_from_the_aggregator`
    /// above (which calls the coordinator lifecycle operation directly and would
    /// stay green even if the handler stopped calling it), this goes through
    /// `api_settings_post_handler` itself.
    #[tokio::test]
    #[serial]
    async fn disabling_an_adapter_via_the_real_endpoint_flushes_its_zones() {
        env::set_var("UHC_CONFIG_DIR", "/tmp/uhc-test-issue-429-adapter-stopping");

        // Seed "old" settings with the adapter already enabled, so the
        // handler's before/after comparison sees a disable, not a no-op.
        let mut seed = AppSettings::default();
        seed.adapters.lms = true;
        assert!(save_app_settings(&seed), "failed to seed test settings");

        let adapter: Arc<dyn Startable> = Arc::new(FakeAdapter("lms"));
        let state = app_state_with_startable(adapter).await;
        state.coordinator.register("lms", true).await;
        state.coordinator.set_running("lms", true).await;
        let agg_for_task = state.aggregator.clone();
        tokio::spawn(async move { agg_for_task.run().await });
        tokio::time::sleep(Duration::from_millis(10)).await;

        state.bus.publish(BusEvent::ZoneDiscovered {
            zone: fake_zone("lms:aa:bb:cc:dd:ee:ff", "lms"),
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            state.aggregator.get_zones().await.len(),
            1,
            "zone should be present before disabling"
        );

        let mut disabled = AppSettings::default();
        disabled.adapters.lms = false;
        let response = api_settings_post_handler(State(state.clone()), Json(disabled)).await;
        let _ = response; // the endpoint's own `{"ok": true}` body isn't the assertion here

        tokio::time::sleep(Duration::from_millis(20)).await;
        let remaining = state.aggregator.get_zones().await;
        assert_eq!(
            remaining.len(),
            0,
            "disabling lms via the real endpoint should flush its zone, got {remaining:?}"
        );
        assert!(
            !state.coordinator.is_running("lms").await,
            "disabling an adapter must also retire its coordinator running state"
        );

        env::remove_var("UHC_CONFIG_DIR");
    }

    #[test]
    #[serial]
    fn test_lms_auto_enabled_when_plugin_started() {
        // When started from LMS plugin, adapters.lms should be auto-enabled
        env::set_var("LMS_UNIFIEDHIFI_STARTED", "true");
        env::set_var("UHC_CONFIG_DIR", "/tmp/uhc-test-nonexistent-api");

        let settings = load_app_settings();

        env::remove_var("LMS_UNIFIEDHIFI_STARTED");
        env::remove_var("UHC_CONFIG_DIR");

        assert!(
            settings.adapters.lms,
            "adapters.lms should be true when LMS_UNIFIEDHIFI_STARTED=true"
        );
    }

    #[test]
    #[serial]
    fn test_lms_not_enabled_without_plugin_signal() {
        // Without LMS_UNIFIEDHIFI_STARTED, LMS should default to disabled
        env::remove_var("LMS_UNIFIEDHIFI_STARTED");
        env::set_var("UHC_CONFIG_DIR", "/tmp/uhc-test-nonexistent-api2");

        let settings = load_app_settings();

        env::remove_var("UHC_CONFIG_DIR");

        assert!(
            !settings.adapters.lms,
            "adapters.lms should be false without LMS_UNIFIEDHIFI_STARTED"
        );
    }

    #[test]
    fn provider_adapters_are_opt_in_by_default() {
        let settings = AppSettings::default();

        assert!(!settings.adapters.spotify);
        assert!(!settings.adapters.applemusic);
        assert!(!settings.adapters.musicassistant);
        assert!(!settings.adapters.mqtt);
    }

    #[test]
    fn adapter_setting_covers_each_transactional_adapter() {
        let settings = AdapterSettings {
            roon: true,
            lms: true,
            openhome: true,
            upnp: true,
            hqplayer: true,
            spotify: true,
            applemusic: true,
            musicassistant: true,
            mqtt: true,
        };
        for name in [
            "roon",
            "lms",
            "openhome",
            "upnp",
            "hqplayer",
            "spotify",
            "applemusic",
            "musicassistant",
            "mqtt",
        ] {
            assert_eq!(adapter_enabled(&settings, name), Some(true), "{name}");
        }
        assert_eq!(adapter_enabled(&settings, "not-an-adapter"), None);
    }

    /// The whole reason `hidden_zones` is an `Option`.
    ///
    /// `POST /api/settings` saves its request body wholesale, and the settings page builds its
    /// `AppSettings` from its own form fields -- which do not include hidden zones. With the
    /// obvious `#[serde(default)] Vec<String>`, a body without the key deserialises to an empty
    /// vec and wipes the list, so every adapter toggle would silently unhide every zone the user
    /// had hidden. That implementation passes any test that posts a *complete* settings body; it
    /// fails this one, which posts the partial body the real UI actually sends.
    #[tokio::test]
    #[serial]
    async fn posting_settings_without_hidden_zones_preserves_the_hide_list() {
        env::set_var("UHC_CONFIG_DIR", "/tmp/uhc-test-hidden-zones-carry-forward");

        let mut seeded = AppSettings::default();
        seeded.hidden_zones = Some(vec!["roon:phone".to_string()]);
        seeded.zone_order = Some(vec!["roon:kitchen".to_string()]);
        assert!(save_app_settings(&seeded), "failed to seed settings");

        // Exactly what the settings page sends: adapters and page flags, no zone fields at all.
        let from_the_ui: AppSettings = serde_json::from_value(serde_json::json!({
            "hide_knobs_page": false,
            "hide_hqp_page": false,
            "hide_lms_page": false,
            "adapters": { "roon": true, "upnp": false, "openhome": false, "lms": true, "hqplayer": false }
        }))
        .expect("the settings page's body must deserialise");
        assert!(
            from_the_ui.hidden_zones.is_none(),
            "sanity: a body without the key must arrive as None, not as an empty list"
        );

        let state = app_state_with_startable(Arc::new(FakeAdapter("lms"))).await;
        let _ = api_settings_post_handler(State(state), Json(from_the_ui)).await;

        let after = load_app_settings();
        env::remove_var("UHC_CONFIG_DIR");

        assert_eq!(
            after.hidden_zone_ids(),
            ["roon:phone".to_string()],
            "toggling an adapter must not erase the hide list"
        );
        assert_eq!(
            after.zone_order_ids(),
            ["roon:kitchen".to_string()],
            "toggling an adapter must not erase the zone order"
        );
        assert!(
            after.adapters.lms,
            "the change actually being made must land"
        );
    }

    /// The other half of the `Option`: an explicit empty list is a real instruction, not silence.
    #[tokio::test]
    #[serial]
    async fn posting_an_explicit_empty_hide_list_clears_it() {
        env::set_var(
            "UHC_CONFIG_DIR",
            "/tmp/uhc-test-hidden-zones-explicit-clear",
        );

        let mut seeded = AppSettings::default();
        seeded.hidden_zones = Some(vec!["roon:phone".to_string()]);
        assert!(save_app_settings(&seeded), "failed to seed settings");

        let mut clearing = AppSettings::default();
        clearing.hidden_zones = Some(vec![]);

        let state = app_state_with_startable(Arc::new(FakeAdapter("lms"))).await;
        let _ = api_settings_post_handler(State(state), Json(clearing)).await;

        let after = load_app_settings();
        env::remove_var("UHC_CONFIG_DIR");

        assert!(
            after.hidden_zone_ids().is_empty(),
            "an explicit empty list must clear the hide list, got {:?}",
            after.hidden_zone_ids()
        );
    }

    /// Issue #544: enabling an adapter that has never been configured (e.g. a
    /// fresh HQPlayer instance with no host set) must succeed and leave the
    /// adapter idle, exactly like v3's `if adapter.can_start().await { ... }`
    /// guard did. The integration merge routed this toggle through
    /// `start_adapter_and_companions`, which raises "adapter cannot start"
    /// for an unconfigured adapter and used to trip the settings rollback --
    /// surfacing a generic error to the user and undoing the whole write.
    #[tokio::test]
    #[serial]
    async fn enabling_an_unconfigured_adapter_succeeds_and_stays_idle() {
        env::set_var(
            "UHC_CONFIG_DIR",
            "/tmp/uhc-test-issue-544-unconfigured-enable",
        );

        let mut seeded = AppSettings::default();
        seeded.hidden_zones = Some(vec!["roon:phone".to_string()]);
        seeded.adapters.hqplayer = false;
        assert!(save_app_settings(&seeded), "failed to seed test settings");

        let state = app_state_with_startable(Arc::new(UnconfiguredAdapter("hqplayer"))).await;
        state.coordinator.register("hqplayer", false).await;

        let mut enabling = AppSettings::default();
        enabling.adapters.hqplayer = true;
        let response = api_settings_post_handler(State(state.clone()), Json(enabling)).await;
        let body = response_json(response).await;

        assert_eq!(
            body["ok"], true,
            "enabling an unconfigured adapter must not be reported as an error, got {body:?}"
        );

        let after = load_app_settings();
        env::remove_var("UHC_CONFIG_DIR");

        assert!(
            after.adapters.hqplayer,
            "the toggle itself must still take effect and persist"
        );
        assert_eq!(
            after.hidden_zone_ids(),
            ["roon:phone".to_string()],
            "the hide list must survive enabling an unconfigured adapter"
        );
        assert!(
            !state.coordinator.is_running("hqplayer").await,
            "an unconfigured adapter must stay idle, not be marked running"
        );
    }

    /// The other half of issue #544: a *configured* adapter whose runtime
    /// genuinely fails to start must still roll the settings write back --
    /// and that rollback must not take the user's hide list down with it.
    #[tokio::test]
    #[serial]
    async fn a_failed_adapter_start_rolls_back_without_erasing_the_hide_list() {
        env::set_var(
            "UHC_CONFIG_DIR",
            "/tmp/uhc-test-issue-544-failed-start-rollback",
        );

        let mut seeded = AppSettings::default();
        seeded.hidden_zones = Some(vec!["roon:phone".to_string()]);
        seeded.adapters.lms = false;
        assert!(save_app_settings(&seeded), "failed to seed test settings");

        let state = app_state_with_startable(Arc::new(FailingAdapter("lms"))).await;
        state.coordinator.register("lms", false).await;

        let mut enabling = AppSettings::default();
        enabling.adapters.lms = true;
        let response = api_settings_post_handler(State(state.clone()), Json(enabling)).await;
        let body = response_json(response).await;

        assert_eq!(
            body["ok"], false,
            "a genuine start failure of a configured adapter must still be reported"
        );

        let after = load_app_settings();
        env::remove_var("UHC_CONFIG_DIR");

        assert!(
            !after.adapters.lms,
            "a failed start must roll the toggle back to its previous state"
        );
        assert_eq!(
            after.hidden_zone_ids(),
            ["roon:phone".to_string()],
            "rolling back a failed adapter start must not erase the hide list"
        );
    }

    /// Hiding is idempotent and unhiding is exact -- a double-click must not stack duplicates, and
    /// unhiding one zone must not disturb the others.
    #[tokio::test]
    #[serial]
    async fn zone_visibility_toggling_is_idempotent() {
        env::set_var("UHC_CONFIG_DIR", "/tmp/uhc-test-zone-visibility-idempotent");
        assert!(
            save_app_settings(&AppSettings::default()),
            "failed to seed settings"
        );

        for _ in 0..3 {
            let _ = zone_visibility_post(Json(ZoneVisibilityRequest {
                zone_id: "roon:phone".to_string(),
                hidden: true,
            }))
            .await;
        }
        let _ = zone_visibility_post(Json(ZoneVisibilityRequest {
            zone_id: "lms:laptop".to_string(),
            hidden: true,
        }))
        .await;

        assert_eq!(
            load_app_settings().hidden_zone_ids(),
            ["lms:laptop".to_string(), "roon:phone".to_string()],
            "hiding twice must not duplicate the entry"
        );

        let _ = zone_visibility_post(Json(ZoneVisibilityRequest {
            zone_id: "roon:phone".to_string(),
            hidden: false,
        }))
        .await;

        let after = load_app_settings();
        env::remove_var("UHC_CONFIG_DIR");

        assert_eq!(
            after.hidden_zone_ids(),
            ["lms:laptop".to_string()],
            "unhiding one zone must leave the others hidden"
        );
    }

    /// Renaming with an empty name clears the override rather than storing an empty string.
    ///
    /// Storing `""` would leave a nameless row in every zone list with no way back short of hand
    /// editing JSON -- and "clear the field to reset" is the only reset affordance in the UI.
    #[tokio::test]
    #[serial]
    async fn clearing_a_zone_name_restores_the_provider_name() {
        env::set_var("UHC_CONFIG_DIR", "/tmp/uhc-test-zone-rename-clear");
        assert!(
            save_app_settings(&AppSettings::default()),
            "failed to seed settings"
        );

        let _ = zone_name_post(Json(ZoneNameRequest {
            zone_id: "roon:kitchen".to_string(),
            name: Some("Basement - Kitchen".to_string()),
        }))
        .await;
        assert_eq!(
            load_app_settings().custom_zone_name("roon:kitchen"),
            Some("Basement - Kitchen")
        );

        for cleared in [Some(String::new()), Some("   ".to_string()), None] {
            let _ = zone_name_post(Json(ZoneNameRequest {
                zone_id: "roon:kitchen".to_string(),
                name: cleared.clone(),
            }))
            .await;
            assert_eq!(
                load_app_settings().custom_zone_name("roon:kitchen"),
                None,
                "{cleared:?} must clear the override, not store it"
            );

            let _ = zone_name_post(Json(ZoneNameRequest {
                zone_id: "roon:kitchen".to_string(),
                name: Some("Basement - Kitchen".to_string()),
            }))
            .await;
        }

        env::remove_var("UHC_CONFIG_DIR");
    }

    /// A name is trimmed on the way in, so " Kitchen " and "Kitchen" cannot sort differently.
    #[tokio::test]
    #[serial]
    async fn zone_names_are_trimmed() {
        env::set_var("UHC_CONFIG_DIR", "/tmp/uhc-test-zone-rename-trim");
        assert!(
            save_app_settings(&AppSettings::default()),
            "failed to seed settings"
        );

        let _ = zone_name_post(Json(ZoneNameRequest {
            zone_id: "roon:kitchen".to_string(),
            name: Some("   Basement - Kitchen  ".to_string()),
        }))
        .await;

        let after = load_app_settings();
        env::remove_var("UHC_CONFIG_DIR");

        assert_eq!(
            after.custom_zone_name("roon:kitchen"),
            Some("Basement - Kitchen")
        );
    }

    /// An over-long name is refused rather than truncated, and nothing is written.
    #[tokio::test]
    #[serial]
    async fn an_over_long_zone_name_is_refused() {
        env::set_var("UHC_CONFIG_DIR", "/tmp/uhc-test-zone-rename-too-long");
        assert!(
            save_app_settings(&AppSettings::default()),
            "failed to seed settings"
        );

        let response = zone_name_post(Json(ZoneNameRequest {
            zone_id: "roon:kitchen".to_string(),
            name: Some("x".repeat(MAX_ZONE_NAME_CHARS + 1)),
        }))
        .await
        .into_response();

        let after = load_app_settings();
        env::remove_var("UHC_CONFIG_DIR");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            after.custom_zone_name("roon:kitchen"),
            None,
            "a refused rename must not be persisted"
        );
    }

    /// Concurrent hides must all survive.
    ///
    /// Each handler loads the settings, adds one zone, and saves the whole file. Without a lock
    /// spanning that read-modify-write, two overlapping requests both read the same starting list
    /// and the second save discards the first zone — so hiding several zones quickly, or from two
    /// browser tabs, silently keeps only some of them. One-zone-per-request narrows the window; it
    /// does not close it, and this test fails any implementation that relies on the narrowing.
    ///
    /// `multi_thread` is load-bearing. The handlers' read-modify-write contains no `.await`, so on
    /// the default current-thread runtime the tasks would run to completion one at a time and this
    /// would pass against the racy implementation it exists to catch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    #[serial]
    async fn concurrent_hides_do_not_overwrite_each_other() {
        env::set_var("UHC_CONFIG_DIR", "/tmp/uhc-test-zone-visibility-concurrent");
        assert!(
            save_app_settings(&AppSettings::default()),
            "failed to seed settings"
        );

        let zone_ids: Vec<String> = (0..12).map(|i| format!("roon:zone-{i:02}")).collect();
        let mut handles = Vec::new();
        for zone_id in zone_ids.clone() {
            handles.push(tokio::spawn(async move {
                zone_visibility_post(Json(ZoneVisibilityRequest {
                    zone_id,
                    hidden: true,
                }))
                .await;
            }));
        }
        for handle in handles {
            handle.await.expect("hide task panicked");
        }

        let after = load_app_settings();
        env::remove_var("UHC_CONFIG_DIR");

        let mut expected = zone_ids;
        expected.sort();
        assert_eq!(
            after.hidden_zone_ids(),
            expected.as_slice(),
            "every concurrent hide must survive; lost entries mean the read-modify-write raced"
        );
    }

    /// A successful save leaves no `.json.tmp` behind, and the file it wrote is complete.
    ///
    /// The durable-replace path writes a sibling temp file, flushes it, and renames over the
    /// target. A rename that silently failed to move the temp file — or a code path that wrote the
    /// temp and forgot the rename — would leave the config unchanged while reporting success, with
    /// a stray temp file as the only evidence.
    #[tokio::test]
    #[serial]
    async fn saving_settings_leaves_no_temp_file_behind() {
        let dir = std::env::temp_dir().join("uhc-test-settings-durable-replace");
        let _ = std::fs::remove_dir_all(&dir);
        env::set_var("UHC_CONFIG_DIR", dir.to_string_lossy().as_ref());

        let _ = zone_visibility_post(Json(ZoneVisibilityRequest {
            zone_id: "roon:phone".to_string(),
            hidden: true,
        }))
        .await;

        let written = load_app_settings();
        let strays: Vec<_> = walk_config_files(&dir)
            .into_iter()
            .filter(|name| name.ends_with(".json.tmp"))
            .collect();
        env::remove_var("UHC_CONFIG_DIR");
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            written.hidden_zone_ids(),
            ["roon:phone".to_string()],
            "the rename must have published the new contents"
        );
        assert!(strays.is_empty(), "temp files left behind: {strays:?}");
    }

    fn walk_config_files(dir: &std::path::Path) -> Vec<String> {
        let mut found = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return found;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(walk_config_files(&path));
            } else {
                found.push(path.to_string_lossy().into_owned());
            }
        }
        found
    }

    /// A failed save must be an HTTP error, not `200 {"ok": false}`.
    ///
    /// The client checks the HTTP status; a 200 with a falsy body is indistinguishable from success
    /// to it, which is how a settings page ends up silently discarding writes. Exercised through a
    /// config directory that cannot be created, standing in for the read-only bind mount that makes
    /// this reachable in normal Docker use.
    #[tokio::test]
    #[serial]
    async fn an_unwritable_config_directory_produces_a_server_error() {
        // A path under a *file* can never be created as a directory.
        let blocker = std::env::temp_dir().join("uhc-test-unwritable-config-blocker");
        std::fs::write(&blocker, b"not a directory").expect("write blocker file");
        env::set_var(
            "UHC_CONFIG_DIR",
            blocker.join("nested").to_string_lossy().as_ref(),
        );

        let response = zone_visibility_post(Json(ZoneVisibilityRequest {
            zone_id: "roon:phone".to_string(),
            hidden: true,
        }))
        .await
        .into_response();

        env::remove_var("UHC_CONFIG_DIR");
        let _ = std::fs::remove_file(&blocker);

        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "a save failure must surface as an HTTP error the client can detect"
        );
    }

    /// Issue #548: a tester upgraded a v3 release install (config dir preserved) to the
    /// integration/streaming-ha-alpha build and found `hidden_zones` emptied while `zone_order`
    /// stayed intact -- with no explicit edit in between. This seeds a v3-shaped `app-settings.json`
    /// verbatim (the exact bytes a v3 release writes, not a value round-tripped through this build's
    /// own `AppSettings`, so a struct-shape or attribute mismatch would actually show up) and drives
    /// every early write path a freshly-upgraded server hits, asserting all three zone lists survive
    /// each one:
    ///
    /// - the load itself (`load_app_settings`)
    /// - `POST /api/settings` with the settings page's real partial body (adapter toggle, no zone
    ///   fields -- see `posting_settings_without_hidden_zones_preserves_the_hide_list`)
    /// - `POST /api/zones/visibility` (hiding a *different* zone)
    /// - `POST /api/zones/order` (moving a zone)
    /// - `POST /api/zones/name` (renaming a zone)
    ///
    /// This is a *regression* test: as of this commit it passes, because `mutate_app_settings` reads
    /// and merges every field through the same `Option<Vec<_>>` / `Option<BTreeMap<_, _>>` shape on
    /// both sides of the merge, and every handler that is not `POST /api/settings` only ever mutates
    /// the one field it owns. It exists to catch a *future* regression -- e.g. a merge that
    /// re-introduces a non-`Option` zone field, drops a `#[serde(default)]`, or adds a write path
    /// that reconstructs `AppSettings` from scratch instead of mutating the loaded value.
    #[tokio::test]
    #[serial]
    async fn upgrading_a_v3_settings_file_survives_every_early_write_path() {
        env::set_var(
            "UHC_CONFIG_DIR",
            "/tmp/uhc-test-issue-548-upgrade-simulation",
        );

        // The exact shape a v3 release build persists: snake_case keys, no unknown fields, three
        // populated zone lists. Written as raw bytes -- not through this build's own `AppSettings` --
        // so a field-shape mismatch introduced here would actually be exercised by deserialization.
        let v3_settings_json = serde_json::json!({
            "hide_knobs_page": false,
            "hide_hqp_page": false,
            "hide_lms_page": false,
            "adapters": {
                "roon": true,
                "upnp": false,
                "openhome": false,
                "lms": false,
                "hqplayer": false
            },
            "hidden_zones": ["roon:phone"],
            "zone_order": ["roon:kitchen", "roon:phone", "roon:office"],
            "zone_names": {"roon:phone": "Upstairs Phone"}
        });
        let settings_file = settings_path();
        std::fs::create_dir_all(settings_file.parent().unwrap())
            .expect("create config subdirectory");
        std::fs::write(
            &settings_file,
            serde_json::to_string_pretty(&v3_settings_json).unwrap(),
        )
        .expect("seed v3-shaped settings.json");

        let assert_zone_lists_intact = |settings: &AppSettings, step: &str| {
            assert_eq!(
                settings.hidden_zone_ids(),
                ["roon:phone".to_string()],
                "hidden_zones lost after {step}"
            );
            assert_eq!(
                settings.zone_order_ids(),
                [
                    "roon:kitchen".to_string(),
                    "roon:phone".to_string(),
                    "roon:office".to_string()
                ],
                "zone_order lost after {step}"
            );
            assert_eq!(
                settings.custom_zone_name("roon:phone"),
                Some("Upstairs Phone"),
                "zone_names lost after {step}"
            );
        };

        // Step 1: the load itself, exactly as `load_app_settings()` does at every request and at
        // startup (`main.rs` calls it to build the adapter coordinator's initial registry).
        let loaded = load_app_settings();
        assert_zone_lists_intact(&loaded, "load_app_settings()");

        // Step 2: `POST /api/settings` with the real settings-page body -- an adapter toggle, no
        // zone fields at all (the client's own `AppSettings` type, in `src/app/api.rs`, does not
        // declare them). This is the write every "flip a switch" interaction sends.
        let bus = create_bus();
        let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
        let agg_for_task = aggregator.clone();
        tokio::spawn(async move { agg_for_task.run().await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        // Publish the zones referenced above so `manageable_zones` (used by the order/name steps
        // below) has something to reorder and rename.
        for (id, source) in [
            ("roon:kitchen", "roon"),
            ("roon:phone", "roon"),
            ("roon:office", "roon"),
        ] {
            bus.publish(BusEvent::ZoneDiscovered {
                zone: fake_zone(id, source),
            });
        }
        tokio::time::sleep(Duration::from_millis(20)).await;

        let state = app_state_with_startable(Arc::new(FakeAdapter("lms"))).await;
        // Reuse the live aggregator with the published zones instead of the empty one
        // `app_state_with_startable` constructs.
        let state = AppState {
            aggregator: aggregator.clone(),
            ..state
        };

        let toggle_body: AppSettings = serde_json::from_value(serde_json::json!({
            "hide_knobs_page": false,
            "hide_hqp_page": false,
            "hide_lms_page": false,
            "adapters": { "roon": true, "upnp": false, "openhome": false, "lms": true, "hqplayer": false }
        }))
        .expect("the settings page's own partial body must deserialise");
        let _ = api_settings_post_handler(State(state.clone()), Json(toggle_body)).await;
        assert_zone_lists_intact(&load_app_settings(), "POST /api/settings (adapter toggle)");

        // Step 3: `POST /api/zones/visibility`, hiding a zone *other* than the one already hidden.
        let visibility_response = zone_visibility_post(Json(ZoneVisibilityRequest {
            zone_id: "roon:office".to_string(),
            hidden: true,
        }))
        .await
        .into_response();
        assert_eq!(visibility_response.status(), StatusCode::OK);
        let after_hide = load_app_settings();
        // `zone_visibility_post` sorts the hide list before persisting, for a stable on-disk shape.
        assert_eq!(
            after_hide.hidden_zone_ids(),
            ["roon:office".to_string(), "roon:phone".to_string()],
            "hiding a new zone must add to, not replace, the carried-forward hide list"
        );
        assert_eq!(
            after_hide.zone_order_ids(),
            [
                "roon:kitchen".to_string(),
                "roon:phone".to_string(),
                "roon:office".to_string()
            ],
            "zone_order lost after POST /api/zones/visibility"
        );
        assert_eq!(
            after_hide.custom_zone_name("roon:phone"),
            Some("Upstairs Phone"),
            "zone_names lost after POST /api/zones/visibility"
        );

        // Step 4: `POST /api/zones/order`, moving a zone via the real handler (exercises
        // `manageable_zones` and the write-lock merge together).
        let order_response = zone_order_post(
            State(state.clone()),
            Json(ZoneOrderRequest {
                zone_id: "roon:kitchen".to_string(),
                direction: Some(crate::app::api::MoveDirection::Down),
                target_zone_id: None,
            }),
        )
        .await
        .into_response();
        assert_eq!(order_response.status(), StatusCode::OK);
        let after_order = load_app_settings();
        assert_eq!(
            after_order.hidden_zone_ids(),
            ["roon:office".to_string(), "roon:phone".to_string()],
            "hidden_zones lost after POST /api/zones/order"
        );
        assert_eq!(
            after_order.custom_zone_name("roon:phone"),
            Some("Upstairs Phone"),
            "zone_names lost after POST /api/zones/order"
        );

        // Step 5: `POST /api/zones/name`, renaming a different zone.
        let name_response = zone_name_post(Json(ZoneNameRequest {
            zone_id: "roon:kitchen".to_string(),
            name: Some("Kitchen Speaker".to_string()),
        }))
        .await
        .into_response();
        assert_eq!(name_response.status(), StatusCode::OK);
        let final_settings = load_app_settings();
        env::remove_var("UHC_CONFIG_DIR");

        assert_eq!(
            final_settings.hidden_zone_ids(),
            ["roon:office".to_string(), "roon:phone".to_string()],
            "hidden_zones lost after POST /api/zones/name"
        );
        assert_eq!(
            final_settings.custom_zone_name("roon:phone"),
            Some("Upstairs Phone"),
            "the original v3-set zone name must survive every step"
        );
        assert_eq!(
            final_settings.custom_zone_name("roon:kitchen"),
            Some("Kitchen Speaker"),
            "the new rename made during this simulation must also land"
        );
    }
}
