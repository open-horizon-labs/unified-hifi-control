//! Mock Roon Core for testing
//!
//! Provides a simplified mock that exposes Roon-like state via HTTP endpoints.
//! Note: The real Roon adapter uses the roon_api crate which handles the actual
//! WebSocket/SOOD protocol. This mock is useful for testing zone state management
//! and the higher-level adapter logic.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Mock zone state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MockZone {
    pub zone_id: String,
    pub display_name: String,
    pub state: String, // playing, paused, stopped
    pub is_next_allowed: bool,
    pub is_previous_allowed: bool,
    pub is_pause_allowed: bool,
    pub is_play_allowed: bool,
    pub now_playing: Option<MockNowPlaying>,
    pub outputs: Vec<MockOutput>,
}

/// Mock now playing info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MockNowPlaying {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub image_key: Option<String>,
    pub seek_position: Option<i64>,
    pub length: Option<u32>,
}

/// Mock output info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MockOutput {
    pub output_id: String,
    pub display_name: String,
    pub volume: Option<MockVolume>,
}

/// Mock volume info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MockVolume {
    pub value: f32,
    pub min: f32,
    pub max: f32,
    pub is_muted: bool,
}

impl MockZone {
    pub fn new(zone_id: &str, display_name: &str) -> Self {
        Self {
            zone_id: zone_id.to_string(),
            display_name: display_name.to_string(),
            state: "stopped".to_string(),
            is_next_allowed: true,
            is_previous_allowed: true,
            is_pause_allowed: false,
            is_play_allowed: true,
            now_playing: None,
            outputs: vec![MockOutput {
                output_id: format!("{}_output", zone_id),
                display_name: display_name.to_string(),
                volume: Some(MockVolume {
                    value: 50.0,
                    min: 0.0,
                    max: 100.0,
                    is_muted: false,
                }),
            }],
        }
    }
}

/// Mock Roon Core state
pub struct MockRoonState {
    pub core_name: String,
    pub core_version: String,
    pub zones: HashMap<String, MockZone>,
}

impl Default for MockRoonState {
    fn default() -> Self {
        Self {
            core_name: "Mock Roon Core".to_string(),
            core_version: "2.0.0".to_string(),
            zones: HashMap::new(),
        }
    }
}

/// Mock Roon Core
///
/// Unlike other mock servers, this doesn't run an HTTP server because the
/// Roon adapter uses the roon_api crate which handles the proprietary
/// WebSocket/SOOD protocol internally.
///
/// Instead, this mock provides a controllable state object that can be
/// used to inject zones and test the adapter's behavior with known data.
pub struct MockRoonCore {
    state: Arc<RwLock<MockRoonState>>,
}

impl MockRoonCore {
    /// Create a new mock Roon Core
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(MockRoonState::default())),
        }
    }

    /// Get the core name
    pub async fn core_name(&self) -> String {
        self.state.read().await.core_name.clone()
    }

    /// Get the core version
    pub async fn core_version(&self) -> String {
        self.state.read().await.core_version.clone()
    }

    /// Add a zone
    pub async fn add_zone(&self, zone_id: &str, display_name: &str) {
        let mut state = self.state.write().await;
        state
            .zones
            .insert(zone_id.to_string(), MockZone::new(zone_id, display_name));
    }

    /// Get all zones
    pub async fn get_zones(&self) -> Vec<MockZone> {
        self.state.read().await.zones.values().cloned().collect()
    }

    /// Get a specific zone
    pub async fn get_zone(&self, zone_id: &str) -> Option<MockZone> {
        self.state.read().await.zones.get(zone_id).cloned()
    }

    /// Set zone state (playing, paused, stopped)
    pub async fn set_zone_state(&self, zone_id: &str, zone_state: &str) {
        let mut state = self.state.write().await;
        if let Some(zone) = state.zones.get_mut(zone_id) {
            zone.state = zone_state.to_string();
            zone.is_pause_allowed = zone_state == "playing";
            zone.is_play_allowed = zone_state != "playing";
        }
    }

    /// Set zone volume
    pub async fn set_zone_volume(&self, zone_id: &str, volume: f32) {
        let mut state = self.state.write().await;
        if let Some(zone) = state.zones.get_mut(zone_id) {
            if let Some(output) = zone.outputs.first_mut() {
                if let Some(ref mut vol) = output.volume {
                    vol.value = volume.clamp(vol.min, vol.max);
                }
            }
        }
    }

    /// Set zone mute
    pub async fn set_zone_muted(&self, zone_id: &str, muted: bool) {
        let mut state = self.state.write().await;
        if let Some(zone) = state.zones.get_mut(zone_id) {
            if let Some(output) = zone.outputs.first_mut() {
                if let Some(ref mut vol) = output.volume {
                    vol.is_muted = muted;
                }
            }
        }
    }

    /// Set zone now playing
    pub async fn set_now_playing(&self, zone_id: &str, title: &str, artist: &str, album: &str) {
        let mut state = self.state.write().await;
        if let Some(zone) = state.zones.get_mut(zone_id) {
            zone.now_playing = Some(MockNowPlaying {
                title: title.to_string(),
                artist: artist.to_string(),
                album: album.to_string(),
                image_key: Some(format!("image_{}", zone_id)),
                seek_position: Some(0),
                length: Some(300),
            });
        }
    }

    /// Clear zone now playing
    pub async fn clear_now_playing(&self, zone_id: &str) {
        let mut state = self.state.write().await;
        if let Some(zone) = state.zones.get_mut(zone_id) {
            zone.now_playing = None;
        }
    }
}

impl Default for MockRoonCore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_roon_creates_default() {
        let mock = MockRoonCore::new();
        assert_eq!(mock.core_name().await, "Mock Roon Core");
        assert_eq!(mock.core_version().await, "2.0.0");
    }

    #[tokio::test]
    async fn mock_roon_adds_zones() {
        let mock = MockRoonCore::new();
        mock.add_zone("zone-1", "Living Room").await;
        mock.add_zone("zone-2", "Kitchen").await;

        let zones = mock.get_zones().await;
        assert_eq!(zones.len(), 2);

        let zone1 = mock.get_zone("zone-1").await.unwrap();
        assert_eq!(zone1.display_name, "Living Room");
    }

    #[tokio::test]
    async fn mock_roon_sets_zone_state() {
        let mock = MockRoonCore::new();
        mock.add_zone("zone-1", "Living Room").await;
        mock.set_zone_state("zone-1", "playing").await;

        let zone = mock.get_zone("zone-1").await.unwrap();
        assert_eq!(zone.state, "playing");
        assert!(zone.is_pause_allowed);
        assert!(!zone.is_play_allowed);
    }

    #[tokio::test]
    async fn mock_roon_sets_volume() {
        let mock = MockRoonCore::new();
        mock.add_zone("zone-1", "Living Room").await;
        mock.set_zone_volume("zone-1", 75.0).await;

        let zone = mock.get_zone("zone-1").await.unwrap();
        let vol = zone.outputs[0].volume.as_ref().unwrap();
        assert_eq!(vol.value, 75.0);
    }

    #[tokio::test]
    async fn mock_roon_sets_now_playing() {
        let mock = MockRoonCore::new();
        mock.add_zone("zone-1", "Living Room").await;
        mock.set_now_playing("zone-1", "Test Song", "Test Artist", "Test Album")
            .await;

        let zone = mock.get_zone("zone-1").await.unwrap();
        let np = zone.now_playing.as_ref().unwrap();
        assert_eq!(np.title, "Test Song");
        assert_eq!(np.artist, "Test Artist");
        assert_eq!(np.album, "Test Album");
    }

    #[tokio::test]
    async fn mock_roon_clears_now_playing() {
        let mock = MockRoonCore::new();
        mock.add_zone("zone-1", "Living Room").await;
        mock.set_now_playing("zone-1", "Test Song", "Test Artist", "Test Album")
            .await;
        mock.clear_now_playing("zone-1").await;

        let zone = mock.get_zone("zone-1").await.unwrap();
        assert!(zone.now_playing.is_none());
    }
}
