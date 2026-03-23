//! EventReporter - Forward bus events to the Memex muse-ingest proxy
//!
//! Issue #49: When a Memex license is configured, forward bus events to
//! the Muse ingest proxy. The proxy decides what to persist and how to
//! embed - UHC just ships raw events.
//!
//! Features:
//! - License-gated: no license -> no forwarding, zero side effects
//! - Fire-and-forget: network errors logged, never block bus processing
//! - Debounce: skip duplicate events within 5s window
//! - Batch: buffer up to 10 events or 5s, then POST as array

use crate::aggregator::ZoneAggregator;
use crate::bus::{BusEvent, SharedBus};
use muse_events::ingest::{IngestEvent, IngestRequest};
use muse_events::{MuseEvent, NowPlaying};
use reqwest::Client;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Default ingest endpoint
const DEFAULT_INGEST_URL: &str = "https://muse-ingest.ohlabs.ai/ingest";

/// Debounce window for duplicate events
const DEBOUNCE_WINDOW_SECS: u64 = 5;

/// Maximum batch size before flushing
const MAX_BATCH_SIZE: usize = 10;

/// Maximum time to buffer events before flushing
const BATCH_FLUSH_INTERVAL_SECS: u64 = 5;

/// EventReporter forwards bus events to the Memex muse-ingest proxy.
pub struct EventReporter {
    /// HTTP client for sending events
    client: Client,
    /// Ingest proxy URL
    ingest_url: String,
    /// License JWT (None = disabled)
    license: Arc<RwLock<Option<String>>>,
    /// Debounce tracking: key -> last seen time
    debounce_cache: Arc<RwLock<HashMap<String, Instant>>>,
    /// Pending events to batch
    pending_events: Arc<RwLock<Vec<IngestEvent>>>,
    /// Zone aggregator for enriching NowPlayingChanged events
    aggregator: Arc<ZoneAggregator>,
    /// Shutdown signal
    shutdown: CancellationToken,
}

// IngestEvent and IngestRequest are now imported from muse_events::ingest

impl EventReporter {
    /// Create a new EventReporter
    ///
    /// If `license` is None or empty, the reporter is created but disabled.
    /// Call `set_license` later to enable forwarding.
    pub fn new(
        license: Option<String>,
        aggregator: Arc<ZoneAggregator>,
        shutdown: CancellationToken,
    ) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|e| {
                warn!(
                    "Failed to build HTTP client with custom config: {}. Using default.",
                    e
                );
                Client::default()
            });

        // Filter out empty license strings
        let license = license.filter(|l| !l.is_empty());

        Self {
            client,
            ingest_url: DEFAULT_INGEST_URL.to_string(),
            license: Arc::new(RwLock::new(license)),
            debounce_cache: Arc::new(RwLock::new(HashMap::new())),
            pending_events: Arc::new(RwLock::new(Vec::new())),
            aggregator,
            shutdown,
        }
    }

    /// Check if the reporter is enabled (has a valid license)
    pub async fn is_enabled(&self) -> bool {
        self.license.read().await.is_some()
    }

    /// Set or update the license
    ///
    /// Pass `None` or empty string to disable the reporter.
    pub async fn set_license(&self, license: Option<String>) {
        let license = license.filter(|l| !l.is_empty());
        let was_enabled = self.is_enabled().await;
        *self.license.write().await = license.clone();

        // Clear buffered state when disabling to avoid leaking stale events
        if was_enabled && license.is_none() {
            self.pending_events.write().await.clear();
            self.debounce_cache.write().await.clear();
        }

        match (was_enabled, license.is_some()) {
            (false, true) => info!("EventReporter enabled with Memex license"),
            (true, false) => info!("EventReporter disabled (license removed)"),
            _ => {}
        }
    }

    /// Get the current license (for API response)
    pub async fn get_license(&self) -> Option<String> {
        self.license.read().await.clone()
    }

    /// Start the event processing loop
    ///
    /// Spawns background tasks for:
    /// 1. Bus subscription - receiving and processing events
    /// 2. Batch flusher - periodically flushing pending events
    /// 3. Debounce cleaner - cleaning up old debounce entries
    pub async fn run(&self, bus: SharedBus) {
        // Clone Arc references for spawned tasks
        let license = self.license.clone();
        let pending = self.pending_events.clone();
        let debounce = self.debounce_cache.clone();
        let client = self.client.clone();
        let ingest_url = self.ingest_url.clone();
        let aggregator = self.aggregator.clone();
        let shutdown = self.shutdown.clone();

        // Start batch flusher task
        let flush_license = license.clone();
        let flush_pending = pending.clone();
        let flush_client = client.clone();
        let flush_url = ingest_url.clone();
        let flush_shutdown = shutdown.clone();
        tokio::spawn(async move {
            Self::batch_flusher(
                flush_license,
                flush_pending,
                flush_client,
                flush_url,
                flush_shutdown,
            )
            .await;
        });

        // Start debounce cleaner task
        let clean_debounce = debounce.clone();
        let clean_shutdown = shutdown.clone();
        tokio::spawn(async move {
            Self::debounce_cleaner(clean_debounce, clean_shutdown).await;
        });

        // Main event processing loop
        let mut rx = bus.subscribe();
        info!(
            "EventReporter started (license: {})",
            license.read().await.is_some()
        );

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    info!("EventReporter shutting down");
                    // Flush any remaining events
                    Self::flush_events(
                        license.clone(),
                        pending.clone(),
                        client.clone(),
                        ingest_url.clone(),
                    ).await;
                    break;
                }
                result = rx.recv() => {
                    match result {
                        Ok(event) => {
                            // Skip if no license
                            if license.read().await.is_none() {
                                continue;
                            }

                            // Convert and possibly enrich the event
                            if let Some(ingest_event) = self.convert_event(&event, &aggregator).await {
                                // Check debounce
                                let key = Self::debounce_key(&ingest_event);
                                let should_process = {
                                    let mut cache = debounce.write().await;
                                    let now = Instant::now();
                                    if let Some(last_seen) = cache.get(&key) {
                                        if now.duration_since(*last_seen) < Duration::from_secs(DEBOUNCE_WINDOW_SECS) {
                                            debug!("Debounced event: {}", ingest_event.event.event_type());
                                            false
                                        } else {
                                            cache.insert(key, now);
                                            true
                                        }
                                    } else {
                                        cache.insert(key, now);
                                        true
                                    }
                                };

                                if should_process {
                                    let mut events = pending.write().await;
                                    events.push(ingest_event);

                                    // Flush if batch is full
                                    if events.len() >= MAX_BATCH_SIZE {
                                        drop(events); // Release lock before flush
                                        Self::flush_events(
                                            license.clone(),
                                            pending.clone(),
                                            client.clone(),
                                            ingest_url.clone(),
                                        ).await;
                                    }
                                }
                            }
                        }
                        Err(_) => {
                            // Channel lagged, just continue
                            debug!("EventReporter channel lagged, continuing");
                        }
                    }
                }
            }
        }

        info!("EventReporter stopped");
    }

    /// Convert a BusEvent to an IngestEvent.
    ///
    /// Only forwards NowPlayingChanged and HqpPipelineChanged — these are the only
    /// events with listening memory value. Everything else returns None.
    async fn convert_event(
        &self,
        event: &BusEvent,
        aggregator: &ZoneAggregator,
    ) -> Option<IngestEvent> {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs();
        let muse_event = match event {
            BusEvent::NowPlayingChanged {
                zone_id,
                title,
                artist,
                album,
                image_key,
            } => {
                // Enrich with zone metadata from aggregator
                let zone = aggregator.get_zone(zone_id.as_str()).await;
                let (zone_name, source) = zone
                    .as_ref()
                    .map(|z| (Some(z.zone_name.clone()), Some(z.source.clone())))
                    .unwrap_or((None, None));

                let now_playing = title.as_ref().map(|t| {
                    let (duration, metadata) = zone
                        .as_ref()
                        .and_then(|z| z.now_playing.as_ref())
                        .map(|np| (np.duration, np.metadata.clone()))
                        .unwrap_or((None, None));

                    NowPlaying {
                        title: t.clone(),
                        artist: artist.clone().unwrap_or_default(),
                        album: album.clone().unwrap_or_default(),
                        image_key: image_key.clone(),
                        seek_position: None,
                        duration,
                        metadata,
                    }
                });

                MuseEvent::NowPlayingChanged {
                    zone_id: zone_id.as_str().to_string(),
                    zone_name,
                    source,
                    now_playing,
                }
            }
            BusEvent::HqpPipelineChanged {
                host,
                filter,
                shaper,
                rate,
            } => MuseEvent::HqpPipelineChanged {
                host: host.clone(),
                filter: filter.clone(),
                shaper: shaper.clone(),
                rate: rate.clone(),
            },

            // All other events have no listening memory value
            _ => return None,
        };
        Some(IngestEvent {
            event: muse_event,
            timestamp,
        })
    }

    /// Generate a debounce key for an event.
    ///
    /// Uses the event's serde serialization for content-based dedup.
    fn debounce_key(event: &IngestEvent) -> String {
        let content = serde_json::to_string(&event.event).unwrap_or_default();
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        format!("{:x}", hasher.finish())
    }

    /// Flush pending events to the ingest proxy
    async fn flush_events(
        license: Arc<RwLock<Option<String>>>,
        pending: Arc<RwLock<Vec<IngestEvent>>>,
        client: Client,
        ingest_url: String,
    ) {
        let license = license.read().await.clone();
        let Some(jwt) = license else {
            return;
        };

        let events: Vec<IngestEvent> = {
            let mut pending = pending.write().await;
            std::mem::take(&mut *pending)
        };

        if events.is_empty() {
            return;
        }

        let event_count = events.len();
        debug!("Flushing {} events to ingest proxy", event_count);

        let request = IngestRequest { events };

        // Fire-and-forget: spawn a task so we don't block
        tokio::spawn(async move {
            match client
                .post(&ingest_url)
                .header("Authorization", format!("Bearer {}", jwt))
                .header("Content-Type", "application/json")
                .json(&request)
                .send()
                .await
            {
                Ok(response) => {
                    if response.status().is_success() {
                        debug!("Successfully sent {} events to ingest proxy", event_count);
                    } else {
                        warn!(
                            "Ingest proxy returned error: {} {}",
                            response.status(),
                            response.text().await.unwrap_or_default()
                        );
                    }
                }
                Err(e) => {
                    warn!("Failed to send events to ingest proxy: {}", e);
                }
            }
        });
    }

    /// Background task that periodically flushes pending events
    async fn batch_flusher(
        license: Arc<RwLock<Option<String>>>,
        pending: Arc<RwLock<Vec<IngestEvent>>>,
        client: Client,
        ingest_url: String,
        shutdown: CancellationToken,
    ) {
        let mut ticker = interval(Duration::from_secs(BATCH_FLUSH_INTERVAL_SECS));

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    break;
                }
                _ = ticker.tick() => {
                    if !pending.read().await.is_empty() {
                        Self::flush_events(
                            license.clone(),
                            pending.clone(),
                            client.clone(),
                            ingest_url.clone(),
                        ).await;
                    }
                }
            }
        }
    }

    /// Background task that cleans up old debounce entries
    async fn debounce_cleaner(
        debounce: Arc<RwLock<HashMap<String, Instant>>>,
        shutdown: CancellationToken,
    ) {
        let mut ticker = interval(Duration::from_secs(30));

        loop {
            tokio::select! {
                _ = shutdown.cancelled() => {
                    break;
                }
                _ = ticker.tick() => {
                    let mut cache = debounce.write().await;
                    let now = Instant::now();
                    let expiry = Duration::from_secs(DEBOUNCE_WINDOW_SECS * 2);
                    cache.retain(|_, last_seen| now.duration_since(*last_seen) < expiry);
                }
            }
        }
    }
}
