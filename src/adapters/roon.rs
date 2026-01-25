//! Roon adapter using rust-roon-api
//!
//! Connects to Roon Core via SOOD discovery and WebSocket protocol.

use anyhow::Result;
use async_trait::async_trait;
use roon_api::{
    image::{Args as ImageArgs, Format as ImageFormat, Image, Scale, Scaling},
    status::{self, Status},
    transport::{self, volume, Control, Transport, Zone as RoonZone},
    CoreEvent, Info, Parsed, RoonApi, Services, Svc,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, RwLock};
use tokio_util::sync::CancellationToken;

use crate::adapters::handle::{AdapterHandle, RetryConfig};
use crate::adapters::traits::{
    AdapterCommand, AdapterCommandResponse, AdapterContext, AdapterLogic,
};
use crate::bus::{
    BusEvent, NowPlaying as BusNowPlaying, PlaybackState, PrefixedZoneId, SharedBus,
    VolumeControl as BusVolumeControl, Zone as BusZone,
};
use crate::config::get_config_file_path;
use crate::knobs::KnobStore;

const ROON_STATE_FILE: &str = "roon_state.json";

/// Pending image request - stores the oneshot sender to deliver the result
type ImageRequest = oneshot::Sender<Option<ImageData>>;

/// Image data returned from Roon
#[derive(Debug, Clone)]
pub struct ImageData {
    pub content_type: String,
    pub data: Vec<u8>,
}

/// Get the Roon state file path in the config subdirectory
/// Issue #76: Organize config files into unified-hifi/ subdirectory
fn get_roon_state_path() -> PathBuf {
    get_config_file_path(ROON_STATE_FILE)
}

/// Maximum relative volume step per call (prevents wild jumps)
const MAX_RELATIVE_STEP: f32 = 10.0;

/// Default volume range when output info unavailable
const DEFAULT_VOLUME_MIN: f32 = 0.0;
const DEFAULT_VOLUME_MAX: f32 = 100.0;

// =============================================================================
// SAFETY CRITICAL: Volume range handling
// =============================================================================
//
// Bug (catastrophe): Hardcoded 0-100 range causes dB values like -12 to be
// clamped to 0 (maximum volume), risking equipment damage.
//
// Fix: Use zone's actual volume range (e.g., -64 to 0 dB).
// See tests/volume_safety.rs for regression protection.

/// Clamp value to range (f32 for fractional step support)
#[inline]
pub fn clamp(value: f32, min: f32, max: f32) -> f32 {
    value.max(min).min(max)
}

/// Get volume range from output, with safe defaults
///
/// Returns (min, max) tuple. For dB zones this might be (-64, 0),
/// for percentage zones (0, 100).
pub fn get_volume_range(output: Option<&Output>) -> (f32, f32) {
    let Some(output) = output else {
        return (DEFAULT_VOLUME_MIN, DEFAULT_VOLUME_MAX);
    };

    let Some(ref vol) = output.volume else {
        return (DEFAULT_VOLUME_MIN, DEFAULT_VOLUME_MAX);
    };

    let min = vol.min.unwrap_or(DEFAULT_VOLUME_MIN);
    let max = vol.max.unwrap_or(DEFAULT_VOLUME_MAX);

    (min, max)
}

/// Zone information exposed via API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Zone {
    pub zone_id: String,
    pub display_name: String,
    pub state: String,
    pub is_next_allowed: bool,
    pub is_previous_allowed: bool,
    pub is_pause_allowed: bool,
    pub is_play_allowed: bool,
    pub now_playing: Option<NowPlaying>,
    pub outputs: Vec<Output>,
}

/// Output information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Output {
    pub output_id: String,
    pub display_name: String,
    pub volume: Option<VolumeInfo>,
}

/// Volume information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeInfo {
    pub value: Option<f32>,
    pub min: Option<f32>,
    pub max: Option<f32>,
    pub is_muted: Option<bool>,
    /// Volume step size from Roon API (varies per zone)
    pub step: Option<f32>,
}

/// Now playing information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub image_key: Option<String>,
    pub seek_position: Option<i64>,
    pub length: Option<u32>,
}

/// Roon connection status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoonStatus {
    pub connected: bool,
    pub core_name: Option<String>,
    pub core_version: Option<String>,
    pub zone_count: usize,
}

/// Internal state
#[derive(Default)]
struct RoonState {
    connected: bool,
    core_name: Option<String>,
    core_version: Option<String>,
    zones: HashMap<String, Zone>,
    transport: Option<Transport>,
    image: Option<Image>,
    /// Pending image requests: request_id -> (image_key, oneshot sender)
    pending_images: HashMap<usize, (String, ImageRequest)>,
}

/// Roon adapter
#[derive(Clone)]
pub struct RoonAdapter {
    state: Arc<RwLock<RoonState>>,
    bus: SharedBus,
    /// Wrapped in RwLock to allow creating fresh token on restart
    shutdown: Arc<RwLock<CancellationToken>>,
    /// Base URL for Roon extension display (e.g., "http://hostname:3000")
    base_url: Arc<RwLock<Option<String>>>,
    /// Whether the adapter has been started
    started: Arc<std::sync::atomic::AtomicBool>,
    /// Knob store for displaying controller count in Roon extension
    knob_store: Option<KnobStore>,
}

impl RoonAdapter {
    /// Create a disconnected Roon adapter (stub, used when disabled)
    pub fn new_disconnected(bus: SharedBus) -> Self {
        Self {
            state: Arc::new(RwLock::new(RoonState::default())),
            bus,
            shutdown: Arc::new(RwLock::new(CancellationToken::new())),
            base_url: Arc::new(RwLock::new(None)),
            started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            knob_store: None,
        }
    }

    /// Create Roon adapter ready to start
    ///
    /// `base_url` is shown in Roon Settings → Extensions (e.g., "http://hostname:3000")
    /// `knob_store` is used to display controller count in Roon extension status
    pub fn new_configured(bus: SharedBus, base_url: String, knob_store: KnobStore) -> Self {
        Self {
            state: Arc::new(RwLock::new(RoonState::default())),
            bus,
            shutdown: Arc::new(RwLock::new(CancellationToken::new())),
            base_url: Arc::new(RwLock::new(Some(base_url))),
            started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            knob_store: Some(knob_store),
        }
    }

    /// Create and immediately start Roon adapter (legacy API for compatibility)
    pub async fn new(bus: SharedBus, base_url: String, knob_store: KnobStore) -> Result<Self> {
        let adapter = Self::new_configured(bus, base_url, knob_store);
        adapter.start_internal().await?;
        Ok(adapter)
    }

    /// Start the Roon event loop (internal - use Startable trait)
    async fn start_internal(&self) -> Result<()> {
        use std::sync::atomic::Ordering;

        // Check if already started
        if self.started.swap(true, Ordering::SeqCst) {
            return Ok(()); // Already started
        }

        // Verify configuration
        {
            let url = self.base_url.read().await;
            if url.is_none() {
                self.started.store(false, Ordering::SeqCst);
                return Err(anyhow::anyhow!("Roon base_url not configured"));
            }
        }

        // Create fresh cancellation token for this run (previous token may be cancelled)
        let shutdown = {
            let mut token = self.shutdown.write().await;
            *token = CancellationToken::new();
            token.clone()
        };

        // Create AdapterHandle and spawn run_with_retry
        let handle = AdapterHandle::new(self.clone(), self.bus.clone(), shutdown);
        let config = RetryConfig::new(Duration::from_secs(1), Duration::from_secs(60));

        tokio::spawn(async move {
            if let Err(e) = handle.run_with_retry(config).await {
                tracing::error!("Roon adapter exited with error: {}", e);
            }
        });

        Ok(())
    }

    /// Stop the Roon adapter (internal - use Startable trait)
    async fn stop_internal(&self) {
        use std::sync::atomic::Ordering;

        // Cancel background tasks
        self.shutdown.read().await.cancel();

        // Reset started flag so we can restart later
        self.started.store(false, Ordering::SeqCst);

        tracing::info!("Roon adapter stopped");
    }

    /// Check if adapter is configured (has base_url)
    async fn is_configured(&self) -> bool {
        self.base_url.read().await.is_some()
    }

    /// Get connection status
    pub async fn get_status(&self) -> RoonStatus {
        let state = self.state.read().await;
        RoonStatus {
            connected: state.connected,
            core_name: state.core_name.clone(),
            core_version: state.core_version.clone(),
            zone_count: state.zones.len(),
        }
    }

    /// Get all zones
    pub async fn get_zones(&self) -> Vec<Zone> {
        let state = self.state.read().await;
        state.zones.values().cloned().collect()
    }

    /// Get specific zone
    pub async fn get_zone(&self, zone_id: &str) -> Option<Zone> {
        let state = self.state.read().await;
        state.zones.get(zone_id).cloned()
    }

    /// Control playback
    pub async fn control(&self, zone_id: &str, action: &str) -> Result<()> {
        // Clone transport while holding lock, then release before await
        let transport = {
            let state = self.state.read().await;
            state
                .transport
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Not connected to Roon"))?
        };

        let control = match action {
            "play" => Control::Play,
            "pause" => Control::Pause,
            "play_pause" => Control::PlayPause,
            "stop" => Control::Stop,
            "previous" => Control::Previous,
            "next" => Control::Next,
            _ => return Err(anyhow::anyhow!("Unknown action: {}", action)),
        };

        transport.control(zone_id, &control).await;
        Ok(())
    }

    /// Change volume
    ///
    /// SAFETY CRITICAL: For absolute volume, we must clamp to the output's actual
    /// volume range. dB-based zones (like HQPlayer) use ranges like -64 to 0.
    /// Naively clamping to 0-100 would send -12 dB → 0 (MAX VOLUME), risking
    /// equipment damage. See tests/volume_safety.rs for regression protection.
    pub async fn change_volume(&self, output_id: &str, value: f32, relative: bool) -> Result<()> {
        // Clone transport and gather volume info while holding lock, then release before await
        let (transport, mode, final_value) = {
            let state = self.state.read().await;
            let transport = state
                .transport
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Not connected to Roon"))?;

            if relative {
                // Relative volume changes - clamp step size to prevent wild jumps
                let clamped_step = clamp(value, -MAX_RELATIVE_STEP, MAX_RELATIVE_STEP);
                (transport, volume::ChangeMode::Relative, clamped_step)
            } else {
                // Absolute volume - MUST use output's actual range
                let output = self.find_output(&state, output_id);
                let (min, max) = get_volume_range(output.as_ref());
                let clamped_value = clamp(value, min, max);

                tracing::debug!(
                    "Volume change: output={}, requested={}, clamped={}, range={}..{}",
                    output_id,
                    value,
                    clamped_value,
                    min,
                    max
                );

                (transport, volume::ChangeMode::Absolute, clamped_value)
            }
        };

        // Roon transport API now takes f64 to support fractional dB steps
        transport
            .change_volume(output_id, &mode, final_value as f64)
            .await;
        Ok(())
    }

    /// Find output by ID across all zones
    fn find_output(&self, state: &RoonState, output_id: &str) -> Option<Output> {
        for zone in state.zones.values() {
            for output in &zone.outputs {
                if output.output_id == output_id {
                    return Some(output.clone());
                }
            }
        }
        None
    }

    /// Mute/unmute
    pub async fn mute(&self, output_id: &str, mute: bool) -> Result<()> {
        // Clone transport while holding lock, then release before await
        let transport = {
            let state = self.state.read().await;
            state
                .transport
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Not connected to Roon"))?
        };

        let how = if mute {
            volume::Mute::Mute
        } else {
            volume::Mute::Unmute
        };
        transport.mute(output_id, &how).await;
        Ok(())
    }

    /// Get album art image
    pub async fn get_image(
        &self,
        image_key: &str,
        width: Option<u32>,
        height: Option<u32>,
    ) -> Result<ImageData> {
        let (tx, rx) = oneshot::channel();

        // Clone image service while holding lock, then release before await
        let image = {
            let state = self.state.read().await;
            state
                .image
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Image service not available"))?
        };

        // Build scaling args
        let scaling = match (width, height) {
            (Some(w), Some(h)) => Some(Scaling::new(Scale::Fit, w, h)),
            (Some(w), None) => Some(Scaling::new(Scale::Fit, w, w)),
            (None, Some(h)) => Some(Scaling::new(Scale::Fit, h, h)),
            (None, None) => Some(Scaling::new(Scale::Fit, 300, 300)),
        };

        // Request the image (lock not held)
        let args = ImageArgs::new(scaling, Some(ImageFormat::Jpeg));
        let req_id = image.get_image(image_key, args).await;

        let req_id = match req_id {
            Some(id) => {
                // Re-acquire lock to insert pending request
                let mut state = self.state.write().await;
                state.pending_images.insert(id, (image_key.to_string(), tx));
                id
            }
            None => return Err(anyhow::anyhow!("Failed to request image")),
        };

        tracing::debug!("Requested image {} with req_id {}", image_key, req_id);

        // Wait for response with timeout
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), rx).await;

        // Clean up pending request on timeout or cancellation
        if !matches!(result, Ok(Ok(Some(_)))) {
            let mut state = self.state.write().await;
            state.pending_images.remove(&req_id);
        }

        match result {
            Ok(Ok(Some(data))) => Ok(data),
            Ok(Ok(None)) => Err(anyhow::anyhow!("Image not found")),
            Ok(Err(_)) => Err(anyhow::anyhow!("Image request cancelled")),
            Err(_) => Err(anyhow::anyhow!("Image request timed out")),
        }
    }
}

#[async_trait]
impl AdapterLogic for RoonAdapter {
    fn prefix(&self) -> &'static str {
        "roon"
    }

    async fn run(&self, ctx: AdapterContext) -> Result<()> {
        let base_url = {
            let url = self.base_url.read().await;
            url.clone()
                .ok_or_else(|| anyhow::anyhow!("Roon base_url not configured"))?
        };

        run_roon_loop(
            self.state.clone(),
            ctx.bus,
            base_url,
            ctx.shutdown,
            self.knob_store.clone(),
        )
        .await
    }

    async fn handle_command(
        &self,
        zone_id: &str,
        command: AdapterCommand,
    ) -> Result<AdapterCommandResponse> {
        // Strip "roon:" prefix if present (bus/aggregator uses prefixed IDs)
        let zone_id = zone_id.strip_prefix("roon:").unwrap_or(zone_id);

        let result = match command {
            AdapterCommand::Play => self.control(zone_id, "play").await,
            AdapterCommand::Pause => self.control(zone_id, "pause").await,
            AdapterCommand::PlayPause => self.control(zone_id, "play_pause").await,
            AdapterCommand::Stop => self.control(zone_id, "stop").await,
            AdapterCommand::Next => self.control(zone_id, "next").await,
            AdapterCommand::Previous => self.control(zone_id, "previous").await,
            AdapterCommand::VolumeAbsolute(value) => {
                // Get the first output for this zone
                if let Some(zone) = self.get_zone(zone_id).await {
                    if let Some(output) = zone.outputs.first() {
                        self.change_volume(&output.output_id, value as f32, false)
                            .await
                    } else {
                        Err(anyhow::anyhow!("Zone has no outputs"))
                    }
                } else {
                    Err(anyhow::anyhow!("Zone not found"))
                }
            }
            AdapterCommand::VolumeRelative(delta) => {
                // Get the first output for this zone
                if let Some(zone) = self.get_zone(zone_id).await {
                    if let Some(output) = zone.outputs.first() {
                        self.change_volume(&output.output_id, delta as f32, true)
                            .await
                    } else {
                        Err(anyhow::anyhow!("Zone has no outputs"))
                    }
                } else {
                    Err(anyhow::anyhow!("Zone not found"))
                }
            }
            AdapterCommand::Mute(mute) => {
                // Get the first output for this zone
                if let Some(zone) = self.get_zone(zone_id).await {
                    if let Some(output) = zone.outputs.first() {
                        self.mute(&output.output_id, mute).await
                    } else {
                        Err(anyhow::anyhow!("Zone has no outputs"))
                    }
                } else {
                    Err(anyhow::anyhow!("Zone not found"))
                }
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

/// Convert Roon zone to our Zone struct
fn convert_zone(roon_zone: &RoonZone) -> Zone {
    let now_playing = roon_zone.now_playing.as_ref().map(|np| NowPlaying {
        title: np.three_line.line1.clone(),
        artist: np.three_line.line2.clone(),
        album: np.three_line.line3.clone(),
        image_key: np.image_key.clone(),
        seek_position: np.seek_position,
        length: np.length,
    });

    // Log output volume info for debugging
    for o in &roon_zone.outputs {
        tracing::debug!(
            "Zone '{}' output '{}': volume={:?}",
            roon_zone.display_name,
            o.display_name,
            o.volume
                .as_ref()
                .map(|v| format!("value={:?} min={:?} max={:?}", v.value, v.min, v.max))
        );
    }

    let outputs = roon_zone
        .outputs
        .iter()
        .map(|o| Output {
            output_id: o.output_id.clone(),
            display_name: o.display_name.clone(),
            volume: o.volume.as_ref().map(|v| VolumeInfo {
                value: v.value,
                min: v.min,
                max: v.max,
                is_muted: v.is_muted,
                step: v.step,
            }),
        })
        .collect();

    let state_str = match roon_zone.state {
        transport::State::Playing => "playing",
        transport::State::Paused => "paused",
        transport::State::Loading => "loading",
        transport::State::Stopped => "stopped",
    };

    Zone {
        zone_id: roon_zone.zone_id.clone(),
        display_name: roon_zone.display_name.clone(),
        state: state_str.to_string(),
        is_next_allowed: roon_zone.is_next_allowed,
        is_previous_allowed: roon_zone.is_previous_allowed,
        is_pause_allowed: roon_zone.is_pause_allowed,
        is_play_allowed: roon_zone.is_play_allowed,
        now_playing,
        outputs,
    }
}

/// Convert local Zone to bus Zone for ZoneDiscovered event
fn roon_zone_to_bus_zone(zone: &Zone) -> BusZone {
    // Get volume from first output (if available)
    // Use prefixed output_id for consistent aggregator matching
    let volume_control = zone.outputs.first().and_then(|o| {
        o.volume.as_ref().map(|v| {
            // Use get_volume_range for consistent defaults with change_volume
            let (default_min, default_max) = get_volume_range(Some(o));
            let min = v.min.unwrap_or(default_min);
            let max = v.max.unwrap_or(default_max);
            // Default to min (safest - for dB zones 0=max, for percent zones 0=min)
            let value = v.value.unwrap_or(min);
            // Infer scale from range: if max <= 0, it's dB; otherwise percentage
            let scale = if max <= 0.0 {
                crate::bus::VolumeScale::Decibel
            } else {
                crate::bus::VolumeScale::Percentage
            };
            BusVolumeControl {
                value,
                min,
                max,
                step: v.step.unwrap_or(1.0),
                is_muted: v.is_muted.unwrap_or(false),
                scale,
                output_id: Some(format!("roon:{}", o.output_id)),
            }
        })
    });

    let now_playing = zone.now_playing.as_ref().map(|np| BusNowPlaying {
        title: np.title.clone(),
        artist: np.artist.clone(),
        album: np.album.clone(),
        image_key: np.image_key.clone(),
        seek_position: np.seek_position.map(|p| p as f64),
        duration: np.length.map(|l| l as f64),
        metadata: None,
    });

    BusZone {
        zone_id: format!("roon:{}", zone.zone_id),
        zone_name: zone.display_name.clone(),
        state: PlaybackState::from(zone.state.as_str()),
        volume_control,
        now_playing,
        source: "roon".to_string(),
        is_controllable: true,
        is_seekable: zone.now_playing.is_some(),
        last_updated: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        is_play_allowed: zone.is_play_allowed,
        is_pause_allowed: zone.is_pause_allowed,
        is_next_allowed: zone.is_next_allowed,
        is_previous_allowed: zone.is_previous_allowed,
    }
}

/// Main Roon event loop
async fn run_roon_loop(
    state: Arc<RwLock<RoonState>>,
    bus: SharedBus,
    base_url: String,
    shutdown: CancellationToken,
    knob_store: Option<KnobStore>,
) -> Result<()> {
    tracing::info!("Starting Roon discovery...");

    // Flag to signal that the loop needs to restart (e.g., core lost, channel closed)
    let restart_needed = Arc::new(AtomicBool::new(false));

    // Ensure config subdirectory exists for state persistence
    // Issue #76: State files now go into unified-hifi/ subdirectory
    let state_path = get_roon_state_path();
    if let Some(parent) = state_path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
            tracing::info!("Created config subdirectory: {:?}", parent);
        }
    }
    let state_path_str = state_path.to_string_lossy().to_string();
    tracing::info!("Roon state file: {}", state_path_str);

    // Extension info - Issue #169: Use UHC_VERSION for consistent version display
    // Use same extension ID as Node.js for seamless migration
    let info = Info::new(
        "com.muness.unified-hifi-control".to_string(),
        "Unified Hi-Fi Control",
        env!("UHC_VERSION"),
        Some("Muness Castle"),
        "",
        Some(env!("CARGO_PKG_REPOSITORY")),
    );

    // Create API instance
    let mut roon = RoonApi::new(info);

    // Create Status service - this is what makes extension visible in Roon Settings
    let (svc, status) = Status::new(&roon);

    // Services we want from Roon Core
    let services = vec![
        Services::Transport(Transport::new()),
        Services::Image(Image::new()),
        Services::Status(status),
    ];

    // Register Status as a provided service (this enables the pairing UI)
    let mut provided: HashMap<String, Svc> = HashMap::new();
    provided.insert(status::SVCNAME.to_owned(), svc);

    // State persistence callback - use proper path
    let state_path_clone = state_path_str.clone();
    let get_roon_state = move || RoonApi::load_roon_state(&state_path_clone);

    // Start discovery
    let (mut handles, mut core_rx) = roon
        .start_discovery(Box::new(get_roon_state), provided, Some(services))
        .await
        .ok_or_else(|| anyhow::anyhow!("Failed to start Roon discovery"))?;

    tracing::info!(
        "Roon discovery started, waiting for core (authorize in Roon → Settings → Extensions)..."
    );

    // Event processing task
    let state_for_events = state.clone();
    let bus_for_events = bus.clone();
    let state_path_for_events = state_path_str.clone();
    let base_url_for_events = base_url;
    let shutdown_for_events = shutdown.clone();
    let restart_needed_for_events = restart_needed.clone();
    let knob_store_for_events = knob_store;
    handles.spawn(async move {
        loop {
            // Use select! to allow cancellation and handle channel close
            // Issue #128: Without this, the loop would spin on channel close
            // causing high CPU and preventing graceful shutdown
            let event_result = tokio::select! {
                _ = shutdown_for_events.cancelled() => {
                    tracing::info!("Roon event handler shutdown requested");
                    break;
                }
                result = core_rx.recv() => result
            };

            let Some((event, msg)) = event_result else {
                // Channel closed - exit gracefully to allow reconnection
                tracing::info!("Roon event channel closed, exiting handler");
                restart_needed_for_events.store(true, std::sync::atomic::Ordering::SeqCst);
                break;
            };

            match event {
                CoreEvent::Registered(mut core, _token) => {
                    let core_name = core.display_name.clone();
                    let core_version = core.display_version.clone();

                    tracing::info!("Roon Core found: {} (version {})", core_name, core_version);

                    // Update status shown in Roon Settings → Extensions
                    // Issue #169: Show version and controller count
                    if let Some(status) = core.get_status() {
                        let knob_count = if let Some(ref store) = knob_store_for_events {
                            store.list().await.len()
                        } else {
                            0
                        };
                        let message = if knob_count > 0 {
                            format!(
                                "v{} • {} controller{} • {}",
                                env!("UHC_VERSION"),
                                knob_count,
                                if knob_count == 1 { "" } else { "s" },
                                base_url_for_events
                            )
                        } else {
                            format!("v{} • {}", env!("UHC_VERSION"), base_url_for_events)
                        };
                        status.set_status(message, false).await;
                    }

                    // Get transport and image services BEFORE acquiring lock
                    let transport = core.get_transport().cloned();
                    let image = core.get_image().cloned();

                    // Subscribe to zones BEFORE acquiring lock (async operation)
                    if let Some(ref t) = transport {
                        t.subscribe_zones().await;
                    }

                    // Now acquire lock and update state synchronously
                    {
                        let mut s = state_for_events.write().await;
                        s.connected = true;
                        s.core_name = Some(core_name.clone());
                        s.core_version = Some(core_version.clone());
                        s.transport = transport;
                        s.image = image.clone();
                    }

                    if image.is_some() {
                        tracing::info!("Roon Image service available");
                    }

                    // Publish connected event (after lock released)
                    bus_for_events.publish(BusEvent::RoonConnected {
                        core_name: core_name.clone(),
                        version: core_version.clone(),
                    });
                }
                CoreEvent::Lost(mut core) => {
                    let lost_core_name = core.display_name.clone();
                    let lost_core_version = core.display_version.clone();

                    tracing::warn!(
                        "Roon Core lost: {} (version {})",
                        lost_core_name,
                        lost_core_version
                    );

                    // Update status shown in Roon Settings → Extensions
                    if let Some(status) = core.get_status() {
                        status
                            .set_status("Disconnected - searching...".to_string(), true)
                            .await;
                    }

                    {
                        let mut s = state_for_events.write().await;
                        s.connected = false;
                        s.core_name = None;
                        s.core_version = None;
                        s.zones.clear();
                        s.transport = None;
                        s.image = None;
                        s.pending_images.clear();
                    }

                    // Publish disconnected event
                    bus_for_events.publish(BusEvent::RoonDisconnected);

                    // Signal restart needed and break
                    restart_needed_for_events.store(true, std::sync::atomic::Ordering::SeqCst);
                    break;
                }
                _ => {}
            }

            // Handle parsed messages
            if let Some((_, parsed)) = msg {
                match parsed {
                    Parsed::RoonState(roon_state) => {
                        // Persist pairing state to data directory
                        if let Err(e) = RoonApi::save_roon_state(&state_path_for_events, roon_state)
                        {
                            tracing::warn!("Failed to save Roon state: {}", e);
                        } else {
                            tracing::debug!("Roon state saved to {}", state_path_for_events);
                        }
                    }
                    Parsed::Zones(zones) => {
                        let mut s = state_for_events.write().await;
                        for zone in zones {
                            tracing::debug!(
                                "Zone update: {} ({}) - now_playing: {:?}",
                                zone.display_name,
                                zone.zone_id,
                                zone.now_playing.as_ref().map(|np| &np.three_line.line1)
                            );
                            let converted = convert_zone(&zone);
                            let is_new = !s.zones.contains_key(&zone.zone_id);
                            let old_zone = s.zones.get(&zone.zone_id).cloned();

                            if is_new {
                                // New zone - emit ZoneDiscovered
                                let bus_zone = roon_zone_to_bus_zone(&converted);
                                bus_for_events.publish(BusEvent::ZoneDiscovered { zone: bus_zone });
                            } else {
                                // Existing zone - emit ZoneUpdated
                                // Use prefixed zone_id to match ZoneDiscovered format
                                let prefixed_zone_id = PrefixedZoneId::roon(&converted.zone_id);
                                bus_for_events.publish(BusEvent::ZoneUpdated {
                                    zone_id: prefixed_zone_id.clone(),
                                    display_name: converted.display_name.clone(),
                                    state: converted.state.clone(),
                                });
                            }

                            // Publish now playing changed if present
                            // Use prefixed zone_id to match aggregator's stored format
                            let prefixed_zone_id = PrefixedZoneId::roon(&converted.zone_id);
                            if let Some(ref np) = converted.now_playing {
                                bus_for_events.publish(BusEvent::NowPlayingChanged {
                                    zone_id: prefixed_zone_id.clone(),
                                    title: Some(np.title.clone()),
                                    artist: Some(np.artist.clone()),
                                    album: Some(np.album.clone()),
                                    image_key: np.image_key.clone(),
                                });
                            }

                            // Publish volume changed for each output with changed volume
                            for output in &converted.outputs {
                                if let Some(ref vol) = output.volume {
                                    let old_vol = old_zone.as_ref().and_then(|oz| {
                                        oz.outputs
                                            .iter()
                                            .find(|o| o.output_id == output.output_id)
                                            .and_then(|o| o.volume.as_ref())
                                    });

                                    // Emit if volume changed or this is a new zone
                                    let vol_changed = old_vol
                                        .map(|ov| {
                                            ov.value != vol.value || ov.is_muted != vol.is_muted
                                        })
                                        .unwrap_or(true);

                                    // Handle VolumeChanged emission safely:
                                    // - vol.value can be None transiently
                                    // - Using unwrap_or(0.0) would set dB zones to max volume, risking damage
                                    // - But we still want to emit mute changes using last known value
                                    if vol_changed {
                                        // Try current value, then last known value from old_vol
                                        let value_to_use =
                                            vol.value.or_else(|| old_vol.and_then(|ov| ov.value));

                                        if let Some(value) = value_to_use {
                                            bus_for_events.publish(BusEvent::VolumeChanged {
                                                output_id: format!("roon:{}", output.output_id),
                                                value,
                                                is_muted: vol.is_muted.unwrap_or(false),
                                            });
                                        }
                                    }
                                }
                            }

                            s.zones.insert(zone.zone_id.clone(), converted);
                        }
                    }
                    Parsed::ZonesSeek(zones_seek) => {
                        let mut s = state_for_events.write().await;
                        for seek in zones_seek {
                            if let Some(zone) = s.zones.get_mut(&seek.zone_id) {
                                if let Some(np) = &mut zone.now_playing {
                                    np.seek_position = seek.seek_position;

                                    // Publish seek position changed
                                    // Use prefixed zone_id to match aggregator's stored format
                                    if let Some(pos) = seek.seek_position {
                                        bus_for_events.publish(BusEvent::SeekPositionChanged {
                                            zone_id: PrefixedZoneId::roon(&seek.zone_id),
                                            position: pos,
                                        });
                                    }
                                }
                            }
                        }
                    }
                    Parsed::ZonesRemoved(zone_ids) => {
                        let mut s = state_for_events.write().await;
                        for zone_id in zone_ids {
                            tracing::debug!("Zone removed: {}", zone_id);
                            s.zones.remove(&zone_id);

                            // Publish zone removed event
                            // Use prefixed zone_id to match aggregator's stored format
                            bus_for_events.publish(BusEvent::ZoneRemoved {
                                zone_id: PrefixedZoneId::roon(&zone_id),
                            });
                        }
                    }
                    Parsed::Jpeg((image_key, data)) => {
                        tracing::debug!(
                            "Received JPEG image: {} ({} bytes)",
                            image_key,
                            data.len()
                        );
                        let mut s = state_for_events.write().await;
                        // Find pending request by matching image_key
                        if let Some(req_id) = s
                            .pending_images
                            .iter()
                            .find(|(_, (key, _))| key == &image_key)
                            .map(|(k, _)| *k)
                        {
                            if let Some((_key, sender)) = s.pending_images.remove(&req_id) {
                                if sender
                                    .send(Some(ImageData {
                                        content_type: "image/jpeg".to_string(),
                                        data,
                                    }))
                                    .is_err()
                                {
                                    tracing::debug!(
                                        "Image request cancelled (receiver dropped): {}",
                                        image_key
                                    );
                                }
                            }
                        }
                    }
                    Parsed::Png((image_key, data)) => {
                        tracing::debug!("Received PNG image: {} ({} bytes)", image_key, data.len());
                        let mut s = state_for_events.write().await;
                        // Find pending request by matching image_key
                        if let Some(req_id) = s
                            .pending_images
                            .iter()
                            .find(|(_, (key, _))| key == &image_key)
                            .map(|(k, _)| *k)
                        {
                            if let Some((_key, sender)) = s.pending_images.remove(&req_id) {
                                if sender
                                    .send(Some(ImageData {
                                        content_type: "image/png".to_string(),
                                        data,
                                    }))
                                    .is_err()
                                {
                                    tracing::debug!(
                                        "Image request cancelled (receiver dropped): {}",
                                        image_key
                                    );
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    });

    // Wait for handles - abort all when one signals restart needed
    while handles.join_next().await.is_some() {
        // Check if any task signaled restart needed
        if restart_needed.load(std::sync::atomic::Ordering::SeqCst) {
            tracing::info!("Restart signaled, aborting remaining Roon tasks");
            handles.abort_all();
            break;
        }
    }

    // Clear state before returning
    {
        let mut s = state.write().await;
        s.connected = false;
        s.transport = None;
        s.image = None;
        s.zones.clear();
        s.pending_images.clear();
    }

    // Check if restart is needed
    if restart_needed.load(std::sync::atomic::Ordering::SeqCst) {
        Err(anyhow::anyhow!("Roon core lost, restart needed"))
    } else {
        Ok(())
    }
}

// Startable trait implementation via macro
crate::impl_startable!(RoonAdapter, "roon", is_configured);

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a test zone with specified volume parameters
    fn make_test_zone(
        output_id: &str,
        volume_value: Option<f32>,
        volume_min: Option<f32>,
        volume_max: Option<f32>,
    ) -> Zone {
        Zone {
            zone_id: "test-zone".to_string(),
            display_name: "Test Zone".to_string(),
            state: "stopped".to_string(),
            is_next_allowed: true,
            is_previous_allowed: true,
            is_pause_allowed: false,
            is_play_allowed: true,
            now_playing: None,
            outputs: vec![Output {
                output_id: output_id.to_string(),
                display_name: "Test Output".to_string(),
                volume: Some(VolumeInfo {
                    value: volume_value,
                    min: volume_min,
                    max: volume_max,
                    is_muted: None,
                    step: None,
                }),
            }],
        }
    }

    #[test]
    fn roon_zone_to_bus_zone_db_scale_value_none_defaults_to_min() {
        // dB zone: max <= 0 means dB scale
        let zone = make_test_zone("output-1", None, Some(-80.0), Some(0.0));
        let bus_zone = roon_zone_to_bus_zone(&zone);

        let vc = bus_zone.volume_control.expect("should have volume_control");
        assert_eq!(vc.value, -80.0, "value should default to min for dB zones");
        assert_eq!(vc.min, -80.0);
        assert_eq!(vc.max, 0.0);
        assert_eq!(vc.scale, crate::bus::VolumeScale::Decibel);
        assert_eq!(vc.step, 1.0, "step should default to 1.0");
        assert!(!vc.is_muted, "is_muted should default to false");
        assert_eq!(vc.output_id, Some("roon:output-1".to_string()));
    }

    #[test]
    fn roon_zone_to_bus_zone_percent_scale_value_none_defaults_to_min() {
        // Percentage zone: max > 0 means percentage scale
        let zone = make_test_zone("output-2", None, Some(0.0), Some(100.0));
        let bus_zone = roon_zone_to_bus_zone(&zone);

        let vc = bus_zone.volume_control.expect("should have volume_control");
        assert_eq!(
            vc.value, 0.0,
            "value should default to min for percentage zones"
        );
        assert_eq!(vc.min, 0.0);
        assert_eq!(vc.max, 100.0);
        assert_eq!(vc.scale, crate::bus::VolumeScale::Percentage);
        assert_eq!(vc.step, 1.0, "step should default to 1.0");
        assert!(!vc.is_muted, "is_muted should default to false");
        assert_eq!(vc.output_id, Some("roon:output-2".to_string()));
    }

    #[test]
    fn roon_zone_to_bus_zone_preserves_actual_value() {
        // When value is Some, it should be preserved
        let zone = make_test_zone("output-3", Some(-30.0), Some(-80.0), Some(0.0));
        let bus_zone = roon_zone_to_bus_zone(&zone);

        let vc = bus_zone.volume_control.expect("should have volume_control");
        assert_eq!(vc.value, -30.0, "actual value should be preserved");
    }

    #[test]
    fn roon_zone_to_bus_zone_no_volume_returns_none() {
        // Zone with output but no volume should have volume_control = None
        let zone = Zone {
            zone_id: "test-zone".to_string(),
            display_name: "Test Zone".to_string(),
            state: "stopped".to_string(),
            is_next_allowed: true,
            is_previous_allowed: true,
            is_pause_allowed: false,
            is_play_allowed: true,
            now_playing: None,
            outputs: vec![Output {
                output_id: "output-no-vol".to_string(),
                display_name: "No Volume Output".to_string(),
                volume: None,
            }],
        };
        let bus_zone = roon_zone_to_bus_zone(&zone);

        assert!(
            bus_zone.volume_control.is_none(),
            "should be None when output has no volume"
        );
    }
}
