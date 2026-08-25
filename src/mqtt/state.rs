//! Retained per-zone state payload for the MQTT publisher (#508).
//!
//! One JSON object per zone carries everything the composed HA entities
//! read via `value_template`/`json_attributes_topic`: playback state,
//! volume, mute, and now-playing metadata including an `entity_picture`
//! URL built from UHC's existing art proxy.

use serde::Serialize;

use crate::bus::Zone;

/// Retained payload published to `<base_topic>/media_player/<zone>/state`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ZoneStatePayload {
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// Volume normalized to a 0-100 percentage regardless of the source
    /// adapter's native scale, so one HA `number` entity works for every
    /// provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<f64>,
    pub muted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    /// Absolute `entity_picture` URL via UHC's existing `/now_playing/image`
    /// proxy, present only when the zone has known art.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
}

/// Normalize a volume control's value to a 0-100 percentage.
///
/// Decibel and unknown scales are not linear percentages; rather than
/// publish a misleading number, only percentage and linear scales are
/// converted. Other scales report no volume, and the HA `number` entity
/// simply keeps its last retained value.
fn normalized_volume(zone: &Zone) -> Option<f64> {
    let volume = zone.volume_control.as_ref()?;
    use crate::bus::VolumeScale;
    match volume.scale {
        VolumeScale::Percentage => Some(volume.value as f64),
        VolumeScale::Linear => {
            let span = (volume.max - volume.min).max(f32::EPSILON);
            Some((((volume.value - volume.min) / span) * 100.0) as f64)
        }
        VolumeScale::Decibel | VolumeScale::Unknown => None,
    }
}

/// Build the retained state payload for one zone.
///
/// `picture_base_url` is the absolute base URL of UHC's HTTP server (e.g.
/// `http://uhc.local:8088`); when the zone has a `now_playing.image_key`,
/// the payload's `picture` points at UHC's own art proxy rather than the
/// adapter's raw image key, so it works for every provider (Roon browse
/// keys, remote HTTPS URLs, MusicKit CDN references, etc.) without HA ever
/// needing provider-specific credentials.
pub fn build_state_payload(zone: &Zone, picture_base_url: &str) -> ZoneStatePayload {
    let now_playing = zone.now_playing.as_ref();
    let picture = now_playing
        .and_then(|np| np.image_key.as_ref())
        .map(|_| {
            format!(
                "{picture_base_url}/now_playing/image?zone_id={}",
                urlencoding::encode(&zone.zone_id)
            )
        });

    ZoneStatePayload {
        state: zone.state.to_string(),
        title: now_playing.map(|np| np.title.clone()),
        artist: now_playing.map(|np| np.artist.clone()),
        album: now_playing.map(|np| np.album.clone()),
        volume: normalized_volume(zone),
        muted: zone
            .volume_control
            .as_ref()
            .map(|v| v.is_muted)
            .unwrap_or(false),
        position: now_playing.and_then(|np| np.seek_position),
        duration: now_playing.and_then(|np| np.duration),
        picture,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{NowPlaying, PlaybackState, VolumeControl, VolumeScale};

    fn zone_fixture() -> Zone {
        Zone {
            zone_id: "roon:abc".to_string(),
            zone_name: "Living Room".to_string(),
            state: PlaybackState::Playing,
            volume_control: Some(VolumeControl {
                value: 50.0,
                min: 0.0,
                max: 100.0,
                step: 1.0,
                is_muted: false,
                scale: VolumeScale::Percentage,
                output_id: None,
            }),
            now_playing: Some(NowPlaying {
                title: "Song".to_string(),
                artist: "Artist".to_string(),
                album: "Album".to_string(),
                image_key: Some("key123".to_string()),
                seek_position: Some(12.5),
                duration: Some(200.0),
                metadata: None,
                repeat_mode: None,
                shuffle: None,
            }),
            source: "roon".to_string(),
            is_controllable: true,
            is_seekable: true,
            last_updated: 0,
            is_play_allowed: true,
            is_pause_allowed: true,
            is_next_allowed: true,
            is_previous_allowed: true,
        }
    }

    #[test]
    fn builds_full_payload_with_picture_url() {
        let zone = zone_fixture();
        let payload = build_state_payload(&zone, "http://uhc.local:8088");
        assert_eq!(payload.state, "playing");
        assert_eq!(payload.title.as_deref(), Some("Song"));
        assert_eq!(payload.volume, Some(50.0));
        assert!(!payload.muted);
        assert_eq!(
            payload.picture.as_deref(),
            Some("http://uhc.local:8088/now_playing/image?zone_id=roon%3Aabc")
        );
    }

    #[test]
    fn missing_now_playing_omits_metadata_and_picture() {
        let mut zone = zone_fixture();
        zone.now_playing = None;
        let payload = build_state_payload(&zone, "http://uhc.local:8088");
        assert!(payload.title.is_none());
        assert!(payload.picture.is_none());
    }

    #[test]
    fn decibel_scale_reports_no_normalized_volume() {
        let mut zone = zone_fixture();
        zone.volume_control.as_mut().unwrap().scale = VolumeScale::Decibel;
        let payload = build_state_payload(&zone, "http://uhc.local:8088");
        assert_eq!(payload.volume, None);
    }

    #[test]
    fn linear_scale_normalizes_to_percentage() {
        let mut zone = zone_fixture();
        {
            let volume = zone.volume_control.as_mut().unwrap();
            volume.scale = VolumeScale::Linear;
            volume.min = 0.0;
            volume.max = 1.0;
            volume.value = 0.25;
        }
        let payload = build_state_payload(&zone, "http://uhc.local:8088");
        assert_eq!(payload.volume, Some(25.0));
    }

    #[test]
    fn muted_flag_reflects_volume_control() {
        let mut zone = zone_fixture();
        zone.volume_control.as_mut().unwrap().is_muted = true;
        let payload = build_state_payload(&zone, "http://uhc.local:8088");
        assert!(payload.muted);
    }
}
