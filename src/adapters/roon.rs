//! Roon adapter using rust-roon-api
//!
//! Connects to Roon Core via SOOD discovery and WebSocket protocol.

use anyhow::Result;
use roon_api::{
    info, CoreEvent, Info, Parsed, RoonApi, Services, Svc,
    transport::{self, Control, Transport, Zone as RoonZone, volume},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::bus::{BusEvent, SharedBus};

const CONFIG_PATH: &str = "roon_state.json";

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
struct RoonState {
    connected: bool,
    core_name: Option<String>,
    core_version: Option<String>,
    zones: HashMap<String, Zone>,
    transport: Option<Transport>,
}

impl Default for RoonState {
    fn default() -> Self {
        Self {
            connected: false,
            core_name: None,
            core_version: None,
            zones: HashMap::new(),
            transport: None,
        }
    }
}

/// Roon adapter
pub struct RoonAdapter {
    state: Arc<RwLock<RoonState>>,
    bus: SharedBus,
}

impl RoonAdapter {
    /// Create and start Roon adapter
    pub async fn new(bus: SharedBus) -> Result<Self> {
        let state = Arc::new(RwLock::new(RoonState::default()));
        let state_clone = state.clone();
        let bus_clone = bus.clone();

        // Spawn Roon event loop
        tokio::spawn(async move {
            if let Err(e) = run_roon_loop(state_clone, bus_clone).await {
                tracing::error!("Roon event loop error: {}", e);
            }
        });

        Ok(Self { state, bus })
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
        let transport = state.transport.as_ref()
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
        let transport = state.transport.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Not connected to Roon"))?;

        if relative {
            // Relative volume changes - clamp step size to prevent wild jumps
            let clamped_step = clamp(value, -MAX_RELATIVE_STEP, MAX_RELATIVE_STEP);
            transport.change_volume(output_id, &volume::ChangeMode::Relative, clamped_step).await;
        } else {
            // Absolute volume - MUST use output's actual range
            let output = self.find_output(&state, output_id);
            let (min, max) = get_volume_range(output.as_ref());
            let clamped_value = clamp(value, min, max);

            tracing::debug!(
                "Volume change: output={}, requested={}, clamped={}, range={}..{}",
                output_id, value, clamped_value, min, max
            );

            transport.change_volume(output_id, &volume::ChangeMode::Absolute, clamped_value).await;
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
        let transport = state.transport.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Not connected to Roon"))?;

        let how = if mute { volume::Mute::Mute } else { volume::Mute::Unmute };
        transport.mute(output_id, &how).await;
        Ok(())
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

    let outputs = roon_zone.outputs.iter().map(|o| Output {
        output_id: o.output_id.clone(),
        display_name: o.display_name.clone(),
        volume: o.volume.as_ref().map(|v| VolumeInfo {
            value: v.value,
            min: v.min,
            max: v.max,
            is_muted: v.is_muted,
        }),
    }).collect();

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

/// Main Roon event loop
async fn run_roon_loop(state: Arc<RwLock<RoonState>>, bus: SharedBus) -> Result<()> {
    tracing::info!("Starting Roon discovery...");

    // Extension info - uses CARGO_PKG_* from Cargo.toml
    let info = info!("com.open-horizon-labs", "Unified Hi-Fi Control");

    // Create API instance
    let mut roon = RoonApi::new(info);

    // Services we want from Roon Core
    let services = vec![Services::Transport(Transport::new())];

    // Empty provided services (we don't provide any)
    let provided: HashMap<String, Svc> = HashMap::new();

    // State persistence callback
    let get_roon_state = || RoonApi::load_roon_state(CONFIG_PATH);

    // Start discovery
    let (mut handles, mut core_rx) = roon
        .start_discovery(Box::new(get_roon_state), provided, Some(services))
        .await
        .ok_or_else(|| anyhow::anyhow!("Failed to start Roon discovery"))?;

    tracing::info!("Roon discovery started, waiting for core...");

    // Event processing task
    let state_for_events = state.clone();
    let bus_for_events = bus.clone();
    handles.spawn(async move {
        loop {
            if let Some((event, msg)) = core_rx.recv().await {
                match event {
                    CoreEvent::Found(mut core) => {
                        tracing::info!(
                            "Roon Core found: {} (version {})",
                            core.display_name,
                            core.display_version
                        );

                        let mut s = state_for_events.write().await;
                        s.connected = true;
                        s.core_name = Some(core.display_name.clone());
                        s.core_version = Some(core.display_version.clone());

                        // Publish connected event
                        bus_for_events.publish(BusEvent::RoonConnected {
                            core_name: core.display_name.clone(),
                            version: core.display_version.clone(),
                        });

                        // Get transport service and subscribe to zones
                        if let Some(transport) = core.get_transport().cloned() {
                            transport.subscribe_zones().await;
                            s.transport = Some(transport);
                        }
                    }
                    CoreEvent::Lost(core) => {
                        tracing::warn!(
                            "Roon Core lost: {} (version {})",
                            core.display_name,
                            core.display_version
                        );

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
                            // Persist pairing state
                            if let Err(e) = RoonApi::save_roon_state(CONFIG_PATH, roon_state) {
                                tracing::warn!("Failed to save Roon state: {}", e);
                            }
                        }
                        Parsed::Zones(zones) => {
                            let mut s = state_for_events.write().await;
                            for zone in zones {
                                tracing::debug!("Zone update: {} ({})", zone.display_name, zone.zone_id);
                                let converted = convert_zone(&zone);

                                // Publish zone updated event
                                bus_for_events.publish(BusEvent::ZoneUpdated {
                                    zone_id: converted.zone_id.clone(),
                                    display_name: converted.display_name.clone(),
                                    state: converted.state.clone(),
                                });

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
                        _ => {}
                    }
                }
            }
        }
    });

    // Wait for all handles (runs forever unless error)
    while handles.join_next().await.is_some() {}

    Ok(())
}
