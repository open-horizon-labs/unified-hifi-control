//! Extended event types for the unified event bus architecture.
//!
//! This module defines comprehensive event types that abstract across
//! different audio sources (Roon, LMS, HQPlayer, etc.) into a unified
//! zone-based model.

use serde::{Deserialize, Serialize};
use std::fmt;

// =============================================================================
// PrefixedZoneId - Type-safe zone identifier with source prefix
// =============================================================================

/// A zone identifier that enforces the `source:raw_id` format at compile time.
///
/// This prevents bugs where adapters emit bus events with raw IDs instead of
/// prefixed IDs, which would cause the aggregator to silently drop updates.
///
/// # Examples
/// ```ignore
/// let zone_id = PrefixedZoneId::roon("1601bb42ed14351b99c2926214f6cbb80724");
/// assert_eq!(zone_id.as_str(), "roon:1601bb42ed14351b99c2926214f6cbb80724");
/// assert_eq!(zone_id.source(), "roon");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PrefixedZoneId(String);

impl PrefixedZoneId {
    /// Create a Roon zone ID
    pub fn roon(raw_id: impl AsRef<str>) -> Self {
        Self(format!("roon:{}", raw_id.as_ref()))
    }

    /// Create an LMS zone ID
    pub fn lms(raw_id: impl AsRef<str>) -> Self {
        Self(format!("lms:{}", raw_id.as_ref()))
    }

    /// Create an OpenHome zone ID
    pub fn openhome(raw_id: impl AsRef<str>) -> Self {
        Self(format!("openhome:{}", raw_id.as_ref()))
    }

    /// Create a UPnP zone ID
    pub fn upnp(raw_id: impl AsRef<str>) -> Self {
        Self(format!("upnp:{}", raw_id.as_ref()))
    }

    /// Create a HQPlayer zone ID
    pub fn hqplayer(raw_id: impl AsRef<str>) -> Self {
        Self(format!("hqplayer:{}", raw_id.as_ref()))
    }

    /// Parse a prefixed zone ID from a string.
    /// Returns None if the string doesn't contain a valid prefix.
    pub fn parse(s: impl AsRef<str>) -> Option<Self> {
        let s = s.as_ref();
        let valid_prefixes = ["roon:", "lms:", "openhome:", "upnp:", "hqplayer:"];
        if valid_prefixes.iter().any(|p| s.starts_with(p)) {
            Some(Self(s.to_string()))
        } else {
            None
        }
    }

    /// Get the full prefixed zone ID as a string slice
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Get the source prefix (e.g., "roon", "lms")
    pub fn source(&self) -> &str {
        self.0.split(':').next().unwrap_or("")
    }

    /// Get the raw ID without the prefix
    pub fn raw_id(&self) -> &str {
        self.0.split(':').nth(1).unwrap_or(&self.0)
    }
}

impl fmt::Display for PrefixedZoneId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl AsRef<str> for PrefixedZoneId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<PrefixedZoneId> for String {
    fn from(id: PrefixedZoneId) -> Self {
        id.0
    }
}

// =============================================================================
// Core Data Structures
// =============================================================================

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
    /// Track title
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

/// Image data returned from adapters
#[derive(Debug, Clone)]
pub struct ImageData {
    /// MIME content type (e.g., "image/jpeg", "image/png")
    pub content_type: String,

    /// Raw image bytes
    pub data: Vec<u8>,
}

/// Zone update payload for partial updates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ZoneUpdate {
    /// Zone identifier
    pub zone_id: String,

    /// Updated playback state (if changed)
    pub state: Option<PlaybackState>,

    /// Updated volume (if changed)
    pub volume: Option<f32>,

    /// Updated mute state (if changed)
    pub is_muted: Option<bool>,

    /// Updated seek position (if changed)
    pub seek_position: Option<f64>,
}

// =============================================================================
// Commands
// =============================================================================

/// Playback and control commands that can be sent to zones.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "action", content = "params")]
pub enum Command {
    /// Start or resume playback
    Play,

    /// Pause playback
    Pause,

    /// Toggle play/pause
    PlayPause,

    /// Stop playback
    Stop,

    /// Skip to next track
    Next,

    /// Skip to previous track
    Previous,

    /// Set absolute volume
    VolumeAbsolute {
        /// Target volume value
        value: f32,
        /// Specific output ID (for multi-output zones)
        output_id: Option<String>,
    },

    /// Adjust volume relatively
    VolumeRelative {
        /// Volume adjustment (positive = louder, negative = quieter)
        delta: f32,
        /// Specific output ID (for multi-output zones)
        output_id: Option<String>,
    },

    /// Set mute state
    Mute {
        /// True to mute, false to unmute
        muted: bool,
        /// Specific output ID (for multi-output zones)
        output_id: Option<String>,
    },

    /// Toggle mute state
    MuteToggle {
        /// Specific output ID (for multi-output zones)
        output_id: Option<String>,
    },

    /// Seek to position
    Seek {
        /// Target position in seconds
        position: f64,
    },

    /// Seek relative to current position
    SeekRelative {
        /// Seek offset in seconds (positive = forward, negative = backward)
        offset: f64,
    },

    /// Set shuffle mode
    Shuffle { enabled: bool },

    /// Set repeat mode
    Repeat { mode: RepeatMode },
}

/// Repeat mode options
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RepeatMode {
    Off,
    One,
    All,
}

/// Result of a command execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommandResponse {
    /// Zone ID the command was sent to
    pub zone_id: String,

    /// The command that was executed
    pub command: Command,

    /// Whether the command succeeded
    pub success: bool,

    /// Error message if command failed
    pub error: Option<String>,

    /// Timestamp of execution
    pub timestamp: u64,
}

// =============================================================================
// Bus Events
// =============================================================================

/// All events that can be published on the event bus.
///
/// Events are organized into categories:
/// - Zone lifecycle: Discovery, updates, removal
/// - Now playing: Track changes, seek updates
/// - Commands: Incoming commands and their results
/// - Adapter lifecycle: Adapter start/stop, cleanup
/// - System: Shutdown, health checks
/// - Legacy: Backward-compatible events for existing integrations
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
#[allow(clippy::large_enum_variant)] // Zone is intentionally large for full state
pub enum BusEvent {
    // =========================================================================
    // Zone Lifecycle Events
    // =========================================================================
    /// A new zone was discovered by an adapter
    ZoneDiscovered {
        /// Full zone information
        zone: Zone,
    },

    /// Zone information was updated
    ZoneUpdated {
        /// Zone identifier (must be prefixed, e.g., "roon:xxx")
        zone_id: PrefixedZoneId,
        /// Display name
        display_name: String,
        /// Current state
        state: String,
    },

    /// A zone was removed (went offline, adapter disconnected, etc.)
    ZoneRemoved {
        /// Zone identifier (must be prefixed, e.g., "roon:xxx")
        zone_id: PrefixedZoneId,
    },

    // =========================================================================
    // Now Playing Events
    // =========================================================================
    /// Now playing information changed for a zone
    NowPlayingChanged {
        /// Zone identifier (must be prefixed, e.g., "roon:xxx")
        zone_id: PrefixedZoneId,
        /// Track title
        title: Option<String>,
        /// Artist name
        artist: Option<String>,
        /// Album name
        album: Option<String>,
        /// Image key for album art
        image_key: Option<String>,
    },

    /// Seek position changed (for progress updates)
    SeekPositionChanged {
        /// Zone identifier (must be prefixed, e.g., "roon:xxx")
        zone_id: PrefixedZoneId,
        position: i64,
    },

    /// Volume changed
    VolumeChanged {
        output_id: String,
        value: f32,
        is_muted: bool,
    },

    // =========================================================================
    // Command Events
    // =========================================================================
    /// A command was received for a zone
    CommandReceived {
        /// Target zone
        zone_id: String,
        /// The command to execute
        command: Command,
        /// Optional request ID for correlation
        request_id: Option<String>,
    },

    /// Result of a command execution
    CommandResult {
        /// The command response
        response: CommandResponse,
        /// Request ID for correlation (if provided in CommandReceived)
        request_id: Option<String>,
    },

    // =========================================================================
    // Adapter Lifecycle Events
    // =========================================================================
    /// An adapter is stopping (zones will be flushed)
    AdapterStopping {
        /// Adapter identifier (e.g., "roon", "lms", "hqplayer")
        adapter: String,
        /// Reason for stopping
        reason: Option<String>,
    },

    /// An adapter has fully stopped
    AdapterStopped {
        /// Adapter identifier
        adapter: String,
    },

    /// All zones from an adapter were flushed
    ZonesFlushed {
        /// Adapter identifier
        adapter: String,
        /// Zone IDs that were removed
        zone_ids: Vec<String>,
    },

    /// An adapter connected to its backend
    AdapterConnected {
        /// Adapter identifier
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
    // System Events
    // =========================================================================
    /// System is shutting down
    ShuttingDown {
        /// Reason for shutdown
        reason: Option<String>,
    },

    /// Health check event (can be used for monitoring)
    HealthCheck {
        /// Timestamp
        timestamp: u64,
    },

    // =========================================================================
    // Legacy Events (for backward compatibility)
    // =========================================================================
    /// Roon Core connected (legacy)
    RoonConnected { core_name: String, version: String },

    /// Roon Core disconnected (legacy)
    RoonDisconnected,

    /// HQPlayer connected (legacy)
    HqpConnected { host: String },

    /// HQPlayer disconnected (legacy)
    HqpDisconnected { host: String },

    /// HQPlayer state changed (legacy)
    HqpStateChanged { host: String, state: String },

    /// HQPlayer pipeline changed (legacy)
    HqpPipelineChanged {
        host: String,
        filter: Option<String>,
        shaper: Option<String>,
        rate: Option<String>,
    },

    /// LMS connected (legacy)
    LmsConnected { host: String },

    /// LMS disconnected (legacy)
    LmsDisconnected { host: String },

    /// LMS player state changed (legacy)
    LmsPlayerStateChanged { player_id: String, state: String },

    /// Control command from external source (legacy, for MQTT/HA)
    ControlCommand {
        zone_id: String,
        action: String,
        value: Option<serde_json::Value>,
    },
}

impl BusEvent {
    /// Get the event type as a string (for logging/filtering)
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::ZoneDiscovered { .. } => "zone_discovered",
            Self::ZoneUpdated { .. } => "zone_updated",
            Self::ZoneRemoved { .. } => "zone_removed",
            Self::NowPlayingChanged { .. } => "now_playing_changed",
            Self::SeekPositionChanged { .. } => "seek_position_changed",
            Self::VolumeChanged { .. } => "volume_changed",
            Self::CommandReceived { .. } => "command_received",
            Self::CommandResult { .. } => "command_result",
            Self::AdapterStopping { .. } => "adapter_stopping",
            Self::AdapterStopped { .. } => "adapter_stopped",
            Self::ZonesFlushed { .. } => "zones_flushed",
            Self::AdapterConnected { .. } => "adapter_connected",
            Self::AdapterDisconnected { .. } => "adapter_disconnected",
            Self::ShuttingDown { .. } => "shutting_down",
            Self::HealthCheck { .. } => "health_check",
            Self::RoonConnected { .. } => "roon_connected",
            Self::RoonDisconnected => "roon_disconnected",
            Self::HqpConnected { .. } => "hqp_connected",
            Self::HqpDisconnected { .. } => "hqp_disconnected",
            Self::HqpStateChanged { .. } => "hqp_state_changed",
            Self::HqpPipelineChanged { .. } => "hqp_pipeline_changed",
            Self::LmsConnected { .. } => "lms_connected",
            Self::LmsDisconnected { .. } => "lms_disconnected",
            Self::LmsPlayerStateChanged { .. } => "lms_player_state_changed",
            Self::ControlCommand { .. } => "control_command",
        }
    }

    /// Check if this is a zone-related event
    pub fn is_zone_event(&self) -> bool {
        matches!(
            self,
            Self::ZoneDiscovered { .. }
                | Self::ZoneUpdated { .. }
                | Self::ZoneRemoved { .. }
                | Self::ZonesFlushed { .. }
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

    /// Check if this is a command-related event
    pub fn is_command_event(&self) -> bool {
        matches!(
            self,
            Self::CommandReceived { .. } | Self::CommandResult { .. } | Self::ControlCommand { .. }
        )
    }

    /// Check if this is an adapter lifecycle event
    pub fn is_adapter_event(&self) -> bool {
        matches!(
            self,
            Self::AdapterStopping { .. }
                | Self::AdapterStopped { .. }
                | Self::AdapterConnected { .. }
                | Self::AdapterDisconnected { .. }
        )
    }

    /// Check if this is a legacy event
    pub fn is_legacy_event(&self) -> bool {
        matches!(
            self,
            Self::RoonConnected { .. }
                | Self::RoonDisconnected
                | Self::HqpConnected { .. }
                | Self::HqpDisconnected { .. }
                | Self::HqpStateChanged { .. }
                | Self::HqpPipelineChanged { .. }
                | Self::LmsConnected { .. }
                | Self::LmsDisconnected { .. }
                | Self::LmsPlayerStateChanged { .. }
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
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
    fn test_event_type() {
        let event = BusEvent::ZoneDiscovered {
            zone: Zone {
                zone_id: "test:1".to_string(),
                zone_name: "Test Zone".to_string(),
                state: PlaybackState::Stopped,
                volume_control: None,
                now_playing: None,
                source: "test".to_string(),
                is_controllable: true,
                is_seekable: true,
                last_updated: 0,
                is_play_allowed: true,
                is_pause_allowed: false,
                is_next_allowed: true,
                is_previous_allowed: true,
            },
        };
        assert_eq!(event.event_type(), "zone_discovered");
        assert!(event.is_zone_event());
        assert!(!event.is_legacy_event());
    }

    #[test]
    fn test_command_serialization() {
        let cmd = Command::VolumeAbsolute {
            value: 50.0,
            output_id: Some("out1".to_string()),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("VolumeAbsolute"));
        assert!(json.contains("50.0"));
    }

    #[test]
    fn test_bus_event_serialization() {
        let event = BusEvent::NowPlayingChanged {
            zone_id: PrefixedZoneId::roon("123"),
            title: Some("Test Song".to_string()),
            artist: Some("Test Artist".to_string()),
            album: Some("Test Album".to_string()),
            image_key: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("now_playing_changed") || json.contains("NowPlayingChanged"));
    }

    #[test]
    fn test_prefixed_zone_id_constructors() {
        let roon = PrefixedZoneId::roon("abc123");
        assert_eq!(roon.as_str(), "roon:abc123");
        assert_eq!(roon.source(), "roon");
        assert_eq!(roon.raw_id(), "abc123");

        let lms = PrefixedZoneId::lms("00:11:22:33:44:55");
        assert_eq!(lms.as_str(), "lms:00:11:22:33:44:55");

        let openhome = PrefixedZoneId::openhome("uuid-here");
        assert_eq!(openhome.as_str(), "openhome:uuid-here");

        let upnp = PrefixedZoneId::upnp("device-id");
        assert_eq!(upnp.as_str(), "upnp:device-id");

        let hqp = PrefixedZoneId::hqplayer("instance");
        assert_eq!(hqp.as_str(), "hqplayer:instance");
    }

    #[test]
    fn test_prefixed_zone_id_parse() {
        // Valid prefixes
        assert!(PrefixedZoneId::parse("roon:abc").is_some());
        assert!(PrefixedZoneId::parse("lms:abc").is_some());
        assert!(PrefixedZoneId::parse("openhome:abc").is_some());
        assert!(PrefixedZoneId::parse("upnp:abc").is_some());
        assert!(PrefixedZoneId::parse("hqplayer:abc").is_some());

        // Invalid - no prefix
        assert!(PrefixedZoneId::parse("abc123").is_none());
        assert!(PrefixedZoneId::parse("unknown:abc").is_none());
    }
}
