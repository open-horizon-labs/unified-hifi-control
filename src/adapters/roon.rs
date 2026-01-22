//! Roon adapter using rust-roon-api
//!
//! Connects to Roon Core via SOOD discovery and WebSocket protocol.

use anyhow::Result;
use roon_api::{
    image::{Args as ImageArgs, Format as ImageFormat, Image, Scale, Scaling},
    info,
    status::{self, Status},
    transport::{self, volume, Control, Transport, Zone as RoonZone},
    CoreEvent, Info, Parsed, RoonApi, Services, Svc,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{oneshot, RwLock};
use tokio_util::sync::CancellationToken;

use crate::bus::{
    BusEvent, NowPlaying as BusNowPlaying, PlaybackState, SharedBus,
    VolumeControl as BusVolumeControl, Zone as BusZone,
};
use crate::config::get_config_file_path;

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
const MAX_RELATIVE_STEP: i32 = 10;

/// Default volume range when output info unavailable
const DEFAULT_VOLUME_MIN: i32 = 0;
const DEFAULT_VOLUME_MAX: i32 = 100;

// =============================================================================
// SAFETY CRITICAL: Volume range handling
// =============================================================================
//
// Bug (catastrophe): Hardcoded 0-100 range causes dB values like -12 to be
// clamped to 0 (maximum volume), risking equipment damage.
//
// Fix: Use zone's actual volume range (e.g., -64 to 0 dB).
// See tests/volume_safety.rs for regression protection.

/// Clamp value to range, handling NaN by returning min
#[inline]
pub fn clamp(value: i32, min: i32, max: i32) -> i32 {
    value.max(min).min(max)
}

/// Get volume range from output, with safe defaults
///
/// Returns (min, max) tuple. For dB zones this might be (-64, 0),
/// for percentage zones (0, 100).
pub fn get_volume_range(output: Option<&Output>) -> (i32, i32) {
    let Some(output) = output else {
        return (DEFAULT_VOLUME_MIN, DEFAULT_VOLUME_MAX);
    };

    let Some(ref vol) = output.volume else {
        return (DEFAULT_VOLUME_MIN, DEFAULT_VOLUME_MAX);
    };

    let min = vol.min.map(|v| v as i32).unwrap_or(DEFAULT_VOLUME_MIN);
    let max = vol.max.map(|v| v as i32).unwrap_or(DEFAULT_VOLUME_MAX);

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
}

/// Initial reconnection delay
const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(1);
/// Maximum reconnection delay
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

use std::time::Duration;

impl RoonAdapter {
    /// Create a disconnected Roon adapter (stub, used when disabled)
    pub fn new_disconnected(bus: SharedBus) -> Self {
        Self {
            state: Arc::new(RwLock::new(RoonState::default())),
            bus,
            shutdown: Arc::new(RwLock::new(CancellationToken::new())),
            base_url: Arc::new(RwLock::new(None)),
            started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Create Roon adapter ready to start
    ///
    /// `base_url` is shown in Roon Settings → Extensions (e.g., "http://hostname:3000")
    pub fn new_configured(bus: SharedBus, base_url: String) -> Self {
        Self {
            state: Arc::new(RwLock::new(RoonState::default())),
            bus,
            shutdown: Arc::new(RwLock::new(CancellationToken::new())),
            base_url: Arc::new(RwLock::new(Some(base_url))),
            started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Create and immediately start Roon adapter (legacy API for compatibility)
    pub async fn new(bus: SharedBus, base_url: String) -> Result<Self> {
        let adapter = Self::new_configured(bus, base_url);
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

        let base_url = {
            let url = self.base_url.read().await;
            url.clone()
                .ok_or_else(|| anyhow::anyhow!("Roon base_url not configured"))?
        };

        // Create fresh cancellation token for this run (previous token may be cancelled)
        let shutdown_clone = {
            let mut token = self.shutdown.write().await;
            *token = CancellationToken::new();
            token.clone()
        };

        let state_clone = self.state.clone();
        let bus_clone = self.bus.clone();

        // Spawn Roon event loop with reconnection logic
        tokio::spawn(async move {
            let mut retry_delay = INITIAL_RETRY_DELAY;

            loop {
                tracing::info!("Starting Roon discovery loop...");

                // Wrap run_roon_loop in select! to allow cancellation during execution
                let loop_result = tokio::select! {
                    _ = shutdown_clone.cancelled() => {
                        tracing::info!("Roon adapter shutdown requested during discovery");
                        break;
                    }
                    result = run_roon_loop(state_clone.clone(), bus_clone.clone(), base_url.clone(), shutdown_clone.clone()) => {
                        result
                    }
                };

                match loop_result {
                    Ok(()) => {
                        tracing::info!("Roon event loop ended normally");
                    }
                    Err(e) => {
                        tracing::error!("Roon event loop error: {}", e);
                    }
                }

                // Clear state on exit
                {
                    let mut s = state_clone.write().await;
                    s.connected = false;
                    s.transport = None;
                    s.zones.clear();
                }

                tracing::info!("Roon loop exited, reconnecting in {:?}...", retry_delay);

                // Check for shutdown before sleeping
                tokio::select! {
                    _ = shutdown_clone.cancelled() => {
                        tracing::info!("Roon adapter shutdown requested");
                        break;
                    }
                    _ = tokio::time::sleep(retry_delay) => {
                        // Exponential backoff up to max
                        retry_delay = (retry_delay * 2).min(MAX_RETRY_DELAY);
                    }
                }
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
        let state = self.state.read().await;
        let transport = state
            .transport
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Not connected to Roon"))?;

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
    pub async fn change_volume(&self, output_id: &str, value: i32, relative: bool) -> Result<()> {
        let state = self.state.read().await;
        let transport = state
            .transport
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Not connected to Roon"))?;

        if relative {
            // Relative volume changes - clamp step size to prevent wild jumps
            let clamped_step = clamp(value, -MAX_RELATIVE_STEP, MAX_RELATIVE_STEP);
            transport
                .change_volume(output_id, &volume::ChangeMode::Relative, clamped_step)
                .await;
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

            transport
                .change_volume(output_id, &volume::ChangeMode::Absolute, clamped_value)
                .await;
        }

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
        let state = self.state.read().await;
        let transport = state
            .transport
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Not connected to Roon"))?;

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

        // Request the image
        let req_id = {
            let mut state = self.state.write().await;
            let image = state
                .image
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Image service not available"))?;

            // Build scaling args
            let scaling = match (width, height) {
                (Some(w), Some(h)) => Some(Scaling::new(Scale::Fit, w, h)),
                (Some(w), None) => Some(Scaling::new(Scale::Fit, w, w)),
                (None, Some(h)) => Some(Scaling::new(Scale::Fit, h, h)),
                (None, None) => Some(Scaling::new(Scale::Fit, 300, 300)),
            };

            let args = ImageArgs::new(scaling, Some(ImageFormat::Jpeg));
            let req_id = image.get_image(image_key, args).await;

            if let Some(req_id) = req_id {
                state
                    .pending_images
                    .insert(req_id, (image_key.to_string(), tx));
                req_id
            } else {
                return Err(anyhow::anyhow!("Failed to request image"));
            }
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
    let volume_control = zone.outputs.first().and_then(|o| {
        o.volume.as_ref().map(|v| BusVolumeControl {
            value: v.value.unwrap_or(50.0),
            min: v.min.unwrap_or(-64.0),
            max: v.max.unwrap_or(0.0),
            step: 1.0,
            is_muted: v.is_muted.unwrap_or(false),
            scale: crate::bus::VolumeScale::Decibel,
            output_id: Some(o.output_id.clone()),
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
    }
}

/// Main Roon event loop
async fn run_roon_loop(
    state: Arc<RwLock<RoonState>>,
    bus: SharedBus,
    base_url: String,
    shutdown: CancellationToken,
) -> Result<()> {
    tracing::info!("Starting Roon discovery...");

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

    // Extension info - uses CARGO_PKG_* from Cargo.toml
    // Use same extension ID as Node.js for seamless migration
    // Note: info! macro appends CARGO_PKG_NAME to the prefix, so:
    // "com.muness" + "." + "unified-hifi-control" = "com.muness.unified-hifi-control"
    let info = info!("com.muness", "Unified Hi-Fi Control");

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
                break;
            };

            match event {
                CoreEvent::Found(mut core) => {
                    let core_name = core.display_name.clone();
                    let core_version = core.display_version.clone();

                    tracing::info!("Roon Core found: {} (version {})", core_name, core_version);

                    // Update status shown in Roon Settings → Extensions
                    if let Some(status) = core.get_status() {
                        let message = format!("Connected • {}", base_url_for_events);
                        status.set_status(message, false).await;
                    }

                    let mut s = state_for_events.write().await;
                    s.connected = true;
                    s.core_name = Some(core_name.clone());
                    s.core_version = Some(core_version.clone());

                    // Publish connected event
                    bus_for_events.publish(BusEvent::RoonConnected {
                        core_name: core_name.clone(),
                        version: core_version.clone(),
                    });

                    // Get transport service and subscribe to zones
                    if let Some(transport) = core.get_transport().cloned() {
                        transport.subscribe_zones().await;
                        s.transport = Some(transport);
                    }

                    // Get image service for album art
                    if let Some(image) = core.get_image().cloned() {
                        s.image = Some(image);
                        tracing::info!("Roon Image service available");
                    }
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

                    let mut s = state_for_events.write().await;
                    s.connected = false;
                    s.core_name = None;
                    s.core_version = None;
                    s.zones.clear();
                    s.transport = None;

                    // Publish disconnected event
                    bus_for_events.publish(BusEvent::RoonDisconnected);
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
                                "Zone update: {} ({})",
                                zone.display_name,
                                zone.zone_id
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
                                bus_for_events.publish(BusEvent::ZoneUpdated {
                                    zone_id: converted.zone_id.clone(),
                                    display_name: converted.display_name.clone(),
                                    state: converted.state.clone(),
                                });
                            }

                            // Publish now playing changed if present
                            if let Some(ref np) = converted.now_playing {
                                bus_for_events.publish(BusEvent::NowPlayingChanged {
                                    zone_id: converted.zone_id.clone(),
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

                                    if vol_changed {
                                        bus_for_events.publish(BusEvent::VolumeChanged {
                                            output_id: output.output_id.clone(),
                                            value: vol.value.unwrap_or(0.0),
                                            is_muted: vol.is_muted.unwrap_or(false),
                                        });
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
                                    if let Some(pos) = seek.seek_position {
                                        bus_for_events.publish(BusEvent::SeekPositionChanged {
                                            zone_id: seek.zone_id.clone(),
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
                            bus_for_events.publish(BusEvent::ZoneRemoved {
                                zone_id: zone_id.clone(),
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
                                let _ = sender.send(Some(ImageData {
                                    content_type: "image/jpeg".to_string(),
                                    data,
                                }));
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
                                let _ = sender.send(Some(ImageData {
                                    content_type: "image/png".to_string(),
                                    data,
                                }));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    });

    // Wait for all handles (runs forever unless error)
    while handles.join_next().await.is_some() {}

    Ok(())
}

// Startable trait implementation via macro
crate::impl_startable!(RoonAdapter, "roon", is_configured);
