//! SSE wire protocol events for the Muse ecosystem.
//!
//! `MuseEvent` is the subset of UHC's internal `BusEvent` that crosses
//! the wire via SSE. Consumers (Memex, etc.) depend on this crate
//! instead of duplicating types.

use crate::zone::{NowPlaying, Zone, ZoneState};
use serde::{Deserialize, Serialize};

/// Events that cross the wire via SSE.
///
/// This is the subset of UHC's internal `BusEvent` that external consumers
/// need to handle. UHC converts from `BusEvent` to `MuseEvent` at the SSE
/// boundary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "payload")]
pub enum MuseEvent {
    // =========================================================================
    // Zone Lifecycle Events
    // =========================================================================
    /// A new zone was discovered by an adapter
    ZoneDiscovered {
        /// Full zone information
        zone: Zone,
    },

    /// Zone information was updated
    ZoneUpdated(ZoneState),

    /// A zone was removed (went offline, adapter disconnected, etc.)
    ZoneRemoved {
        /// Zone identifier (prefixed, e.g., "roon:xxx")
        zone_id: String,
    },

    // =========================================================================
    // Now Playing Events
    // =========================================================================
    /// Now playing information changed for a zone
    NowPlayingChanged {
        /// Zone identifier (prefixed, e.g., "roon:xxx")
        zone_id: String,
        /// Human-readable zone name (enriched from aggregator, None over SSE)
        #[serde(default)]
        zone_name: Option<String>,
        /// Source adapter (e.g., "roon", "lms") (enriched from aggregator, None over SSE)
        #[serde(default)]
        source: Option<String>,
        /// Updated now playing info
        now_playing: Option<NowPlaying>,
    },

    /// Seek position changed (for progress updates)
    SeekPositionChanged {
        /// Zone identifier (prefixed, e.g., "roon:xxx")
        zone_id: String,
        /// Current position in milliseconds
        position: i64,
    },

    /// Volume changed
    VolumeChanged {
        /// Output ID
        output_id: String,
        /// Current volume value
        value: f32,
        /// Whether the output is muted
        is_muted: bool,
    },

    // =========================================================================
    // Adapter Lifecycle Events
    // =========================================================================
    /// An adapter connected to its backend
    AdapterConnected {
        /// Adapter identifier (e.g., "roon", "lms", "hqplayer")
        adapter: String,
        /// Connection details
        details: Option<String>,
    },

    /// An adapter disconnected from its backend
    AdapterDisconnected {
        /// Adapter identifier
        adapter: String,
        /// Reason for disconnection
        reason: Option<String>,
    },

    // =========================================================================
    // HQPlayer Events
    // =========================================================================
    /// HQPlayer pipeline changed
    HqpPipelineChanged {
        /// HQPlayer host
        host: String,
        /// Filter setting
        filter: Option<String>,
        /// Shaper setting
        shaper: Option<String>,
        /// Sample rate
        rate: Option<String>,
    },
}

impl MuseEvent {
    /// Get the event type as a string (for logging/filtering)
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::ZoneDiscovered { .. } => "zone_discovered",
            Self::ZoneUpdated { .. } => "zone_updated",
            Self::ZoneRemoved { .. } => "zone_removed",
            Self::NowPlayingChanged { .. } => "now_playing_changed",
            Self::SeekPositionChanged { .. } => "seek_position_changed",
            Self::VolumeChanged { .. } => "volume_changed",
            Self::AdapterConnected { .. } => "adapter_connected",
            Self::AdapterDisconnected { .. } => "adapter_disconnected",
            Self::HqpPipelineChanged { .. } => "hqp_pipeline_changed",
        }
    }

    /// Check if this is a zone-related event
    pub fn is_zone_event(&self) -> bool {
        matches!(
            self,
            Self::ZoneDiscovered { .. } | Self::ZoneUpdated { .. } | Self::ZoneRemoved { .. }
        )
    }

    /// Check if this is a playback-related event
    pub fn is_playback_event(&self) -> bool {
        matches!(
            self,
            Self::NowPlayingChanged { .. }
                | Self::SeekPositionChanged { .. }
                | Self::VolumeChanged { .. }
        )
    }

    /// Check if this is an adapter lifecycle event
    pub fn is_adapter_event(&self) -> bool {
        matches!(
            self,
            Self::AdapterConnected { .. } | Self::AdapterDisconnected { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zone::PlaybackState;

    #[test]
    fn test_muse_event_serialization() {
        let event = MuseEvent::ZoneUpdated(ZoneState {
            zone_id: "roon:123".to_string(),
            display_name: "Living Room".to_string(),
            state: PlaybackState::Playing,
        });

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("ZoneUpdated"));
        assert!(json.contains("roon:123"));

        let deserialized: MuseEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(event, deserialized);
    }

    #[test]
    fn test_zone_discovered_serialization() {
        let zone = Zone {
            zone_id: "lms:00:11:22:33:44:55".to_string(),
            zone_name: "Kitchen".to_string(),
            state: PlaybackState::Stopped,
            volume_control: None,
            now_playing: None,
            source: "lms".to_string(),
            is_controllable: true,
            is_seekable: true,
            last_updated: 1234567890,
            is_play_allowed: true,
            is_pause_allowed: false,
            is_next_allowed: true,
            is_previous_allowed: true,
        };

        let event = MuseEvent::ZoneDiscovered { zone: zone.clone() };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: MuseEvent = serde_json::from_str(&json).unwrap();

        match deserialized {
            MuseEvent::ZoneDiscovered { zone: z } => assert_eq!(z, zone),
            _ => panic!("Wrong event type"),
        }
    }

    #[test]
    fn test_event_type_methods() {
        let event = MuseEvent::ZoneDiscovered {
            zone: Zone {
                zone_id: "test:1".to_string(),
                zone_name: "Test".to_string(),
                state: PlaybackState::Unknown,
                volume_control: None,
                now_playing: None,
                source: "test".to_string(),
                is_controllable: false,
                is_seekable: false,
                last_updated: 0,
                is_play_allowed: false,
                is_pause_allowed: false,
                is_next_allowed: false,
                is_previous_allowed: false,
            },
        };

        assert_eq!(event.event_type(), "zone_discovered");
        assert!(event.is_zone_event());
        assert!(!event.is_playback_event());
        assert!(!event.is_adapter_event());
    }
}
