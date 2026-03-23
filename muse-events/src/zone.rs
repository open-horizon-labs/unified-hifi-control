//! Zone and playback types shared across the Muse ecosystem.
//!
//! These types represent the domain model for zones, playback state,
//! and now playing information.

use serde::{Deserialize, Serialize};

/// Unified zone representation across all adapters.
///
/// A zone represents a logical playback destination (Roon zone, LMS player,
/// HQPlayer instance, etc.) with a consistent interface regardless of source.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Zone {
    /// Unique zone identifier (e.g., "roon:1234", "lms:00:11:22:33:44:55")
    pub zone_id: String,

    /// Human-readable zone name
    pub zone_name: String,

    /// Current playback state
    pub state: PlaybackState,

    /// Volume control information (if available)
    pub volume_control: Option<VolumeControl>,

    /// Currently playing track (if any)
    pub now_playing: Option<NowPlaying>,

    /// Source adapter identifier (e.g., "roon", "lms", "hqplayer")
    pub source: String,

    /// Whether playback controls are available
    pub is_controllable: bool,

    /// Whether the zone supports seeking
    pub is_seekable: bool,

    /// Last update timestamp (milliseconds since epoch)
    pub last_updated: u64,

    /// Whether play command is currently allowed
    pub is_play_allowed: bool,

    /// Whether pause command is currently allowed
    pub is_pause_allowed: bool,

    /// Whether next track command is allowed
    pub is_next_allowed: bool,

    /// Whether previous track command is allowed
    pub is_previous_allowed: bool,
}

/// Simplified zone state for ZoneUpdated events.
///
/// Contains the essential state that changes during zone updates,
/// without the full Zone struct overhead.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ZoneState {
    /// Zone identifier
    pub zone_id: String,

    /// Display name
    pub display_name: String,

    /// Current playback state
    pub state: PlaybackState,
}

/// Playback state enumeration
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum PlaybackState {
    Playing,
    Paused,
    Stopped,
    Loading,
    /// Buffering (used by streaming sources)
    Buffering,
    /// Unknown/unavailable state
    #[default]
    Unknown,
}

impl std::fmt::Display for PlaybackState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Playing => write!(f, "playing"),
            Self::Paused => write!(f, "paused"),
            Self::Stopped => write!(f, "stopped"),
            Self::Loading => write!(f, "loading"),
            Self::Buffering => write!(f, "buffering"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

impl From<&str> for PlaybackState {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "playing" | "play" => Self::Playing,
            "paused" | "pause" => Self::Paused,
            "stopped" | "stop" => Self::Stopped,
            "loading" => Self::Loading,
            "buffering" => Self::Buffering,
            _ => Self::Unknown,
        }
    }
}

/// Volume control information for a zone or output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VolumeControl {
    /// Current volume value (in the scale defined by min/max)
    pub value: f32,

    /// Minimum volume value (e.g., -64 for dB, 0 for percentage)
    pub min: f32,

    /// Maximum volume value (e.g., 0 for dB, 100 for percentage)
    pub max: f32,

    /// Volume step size (for relative adjustments)
    pub step: f32,

    /// Whether volume is currently muted
    pub is_muted: bool,

    /// Volume scale type
    pub scale: VolumeScale,

    /// Output ID for this volume control (for multi-output zones)
    pub output_id: Option<String>,
}

/// Volume scale type
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum VolumeScale {
    /// Decibels (typically -64 to 0)
    Decibel,
    /// Percentage (0 to 100)
    Percentage,
    /// Linear (0.0 to 1.0)
    Linear,
    /// Unknown/unspecified
    #[default]
    Unknown,
}

/// Now playing track information.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NowPlaying {
    /// Track title (always present, may be empty string)
    pub title: String,

    /// Artist name
    pub artist: String,

    /// Album name
    pub album: String,

    /// Image key or URL for album art
    pub image_key: Option<String>,

    /// Current seek position in seconds
    pub seek_position: Option<f64>,

    /// Total track duration in seconds
    pub duration: Option<f64>,

    /// Additional metadata (format, bitrate, etc.)
    pub metadata: Option<TrackMetadata>,
}

/// Additional track metadata
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TrackMetadata {
    /// Audio format (e.g., "FLAC", "DSD", "MQA")
    pub format: Option<String>,

    /// Sample rate in Hz (e.g., 44100, 192000)
    pub sample_rate: Option<u32>,

    /// Bit depth (e.g., 16, 24, 32)
    pub bit_depth: Option<u8>,

    /// Bitrate in kbps
    pub bitrate: Option<u32>,

    /// Genre
    pub genre: Option<String>,

    /// Composer
    pub composer: Option<String>,

    /// Track number
    pub track_number: Option<u32>,

    /// Disc number
    pub disc_number: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_playback_state_from_str() {
        assert_eq!(PlaybackState::from("playing"), PlaybackState::Playing);
        assert_eq!(PlaybackState::from("PAUSED"), PlaybackState::Paused);
        assert_eq!(PlaybackState::from("stop"), PlaybackState::Stopped);
        assert_eq!(PlaybackState::from("unknown_state"), PlaybackState::Unknown);
    }

    #[test]
    fn test_playback_state_display() {
        assert_eq!(PlaybackState::Playing.to_string(), "playing");
        assert_eq!(PlaybackState::Paused.to_string(), "paused");
    }

    #[test]
    fn test_zone_serialization() {
        let zone = Zone {
            zone_id: "roon:123".to_string(),
            zone_name: "Living Room".to_string(),
            state: PlaybackState::Playing,
            volume_control: Some(VolumeControl {
                value: -20.0,
                min: -64.0,
                max: 0.0,
                step: 1.0,
                is_muted: false,
                scale: VolumeScale::Decibel,
                output_id: None,
            }),
            now_playing: None,
            source: "roon".to_string(),
            is_controllable: true,
            is_seekable: true,
            last_updated: 1234567890,
            is_play_allowed: false,
            is_pause_allowed: true,
            is_next_allowed: true,
            is_previous_allowed: true,
        };

        let json = serde_json::to_string(&zone).unwrap();
        let deserialized: Zone = serde_json::from_str(&json).unwrap();
        assert_eq!(zone, deserialized);
    }

    #[test]
    fn test_volume_scale_serialization() {
        assert_eq!(
            serde_json::to_string(&VolumeScale::Decibel).unwrap(),
            "\"decibel\""
        );
        assert_eq!(
            serde_json::to_string(&VolumeScale::Linear).unwrap(),
            "\"linear\""
        );
    }
}
