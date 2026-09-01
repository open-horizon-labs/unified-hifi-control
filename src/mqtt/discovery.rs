//! Home Assistant MQTT discovery payloads for one UHC zone (#508).
//!
//! Home Assistant's MQTT integration has no `media_player` platform - the
//! discoverable component list
//! (<https://www.home-assistant.io/integrations/mqtt/>) tops out at
//! `sensor`, `image`, `number`, `switch`, `button` and friends, and
//! `homeassistant/components/mqtt/media_player.py` does not exist in HA
//! core. This publisher composes the same primitives every third-party
//! "MQTT media player" project (bkbilly/mqtt_media_player,
//! TroyFernandes/hass-mqtt-mediaplayer, etc.) uses instead: one `sensor`
//! carrying state and now-playing attributes, one `image` for album art,
//! one `number` for volume, one `switch` for mute, and four `button`
//! entities for transport - all grouped under a single HA device per zone
//! via a shared `device.identifiers`.

use serde::Serialize;

use crate::bus::Zone;
use crate::mqtt::topics;

/// Non-secret runtime settings the discovery payloads are built from.
#[derive(Debug, Clone)]
pub struct DiscoverySettings<'a> {
    pub base_topic: &'a str,
    pub discovery_prefix: &'a str,
    pub availability_topic: &'a str,
}

#[derive(Debug, Clone, Serialize)]
struct Device {
    identifiers: [String; 1],
    name: String,
    manufacturer: &'static str,
    model: String,
    via_device: &'static str,
}

fn device_for(zone: &Zone) -> Device {
    Device {
        identifiers: [format!("uhc_{}", topics::zone_slug(&zone.zone_id))],
        name: zone.zone_name.clone(),
        manufacturer: "Unified Hi-Fi Control",
        model: zone.source.clone(),
        via_device: "unified_hifi_control",
    }
}

/// One `(discovery_topic, retained_json_payload)` pair, or `None` for an
/// entity this zone does not support (its transport buttons are still
/// published unconditionally - see the module doc on graceful ignore).
pub type DiscoveryEntry = (String, serde_json::Value);

/// Build every HA discovery entry for one zone.
///
/// Entities always published: state `sensor`, album-art `image`, volume
/// `number`, mute `switch`. Transport `button`s are published only for
/// actions the zone actually allows (`Zone.is_play_allowed` etc.), so HA
/// never shows a button that would just log a graceful refusal.
pub fn discovery_entries(zone: &Zone, settings: &DiscoverySettings<'_>) -> Vec<DiscoveryEntry> {
    let device = device_for(zone);
    let state_topic = topics::state_topic(settings.base_topic, &zone.zone_id);
    let mut entries = Vec::new();

    entries.push((
        topics::discovery_topic(settings.discovery_prefix, "sensor", &zone.zone_id, "state"),
        serde_json::json!({
            "name": "State",
            "unique_id": format!("uhc_{}_state", topics::zone_slug(&zone.zone_id)),
            "state_topic": state_topic,
            "value_template": "{{ value_json.state }}",
            "json_attributes_topic": state_topic,
            "availability_topic": settings.availability_topic,
            "device": device,
        }),
    ));

    entries.push((
        topics::discovery_topic(settings.discovery_prefix, "image", &zone.zone_id, "art"),
        serde_json::json!({
            "name": "Album Art",
            "unique_id": format!("uhc_{}_art", topics::zone_slug(&zone.zone_id)),
            "url_topic": state_topic,
            "url_template": "{{ value_json.picture }}",
            "availability_topic": settings.availability_topic,
            "device": device,
        }),
    ));

    if zone.volume_control.is_some() {
        entries.push((
            topics::discovery_topic(settings.discovery_prefix, "number", &zone.zone_id, "volume"),
            serde_json::json!({
                "name": "Volume",
                "unique_id": format!("uhc_{}_volume", topics::zone_slug(&zone.zone_id)),
                "state_topic": state_topic,
                "value_template": "{{ value_json.volume }}",
                "command_topic": topics::command_topic(settings.base_topic, &zone.zone_id, "volume"),
                "min": 0,
                "max": 100,
                "step": 1,
                "mode": "slider",
                "unit_of_measurement": "%",
                "availability_topic": settings.availability_topic,
                "device": device,
            }),
        ));

        entries.push((
            topics::discovery_topic(settings.discovery_prefix, "switch", &zone.zone_id, "mute"),
            serde_json::json!({
                "name": "Mute",
                "unique_id": format!("uhc_{}_mute", topics::zone_slug(&zone.zone_id)),
                "state_topic": state_topic,
                "value_template": "{{ 'ON' if value_json.muted else 'OFF' }}",
                "command_topic": topics::command_topic(settings.base_topic, &zone.zone_id, "mute"),
                "availability_topic": settings.availability_topic,
                "device": device,
            }),
        ));
    }

    let transport_buttons: [(&str, &str, bool); 4] = [
        ("play", "Play", zone.is_play_allowed),
        ("pause", "Pause", zone.is_pause_allowed),
        ("next", "Next", zone.is_next_allowed),
        ("previous", "Previous", zone.is_previous_allowed),
    ];
    for (action, label, allowed) in transport_buttons {
        if !allowed {
            continue;
        }
        entries.push((
            topics::discovery_topic(settings.discovery_prefix, "button", &zone.zone_id, action),
            serde_json::json!({
                "name": label,
                "unique_id": format!("uhc_{}_{action}", topics::zone_slug(&zone.zone_id)),
                "command_topic": topics::command_topic(settings.base_topic, &zone.zone_id, action),
                "payload_press": "PRESS",
                "availability_topic": settings.availability_topic,
                "device": device,
            }),
        ));
    }

    entries
}

/// Discovery topics for a zone that no longer exists, to retract every
/// retained config this publisher could have written for it. Includes every
/// possible entity regardless of what capabilities the zone last reported,
/// since a retraction after removal has no `Zone` to consult.
pub fn discovery_topics_for_removal(discovery_prefix: &str, zone_id: &str) -> Vec<String> {
    let mut topics = vec![
        topics::discovery_topic(discovery_prefix, "sensor", zone_id, "state"),
        topics::discovery_topic(discovery_prefix, "image", zone_id, "art"),
        topics::discovery_topic(discovery_prefix, "number", zone_id, "volume"),
        topics::discovery_topic(discovery_prefix, "switch", zone_id, "mute"),
    ];
    for action in ["play", "pause", "next", "previous"] {
        topics.push(topics::discovery_topic(
            discovery_prefix,
            "button",
            zone_id,
            action,
        ));
    }
    topics
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{PlaybackState, VolumeControl, VolumeScale};

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
            now_playing: None,
            source: "roon".to_string(),
            is_controllable: true,
            is_seekable: true,
            last_updated: 0,
            is_play_allowed: true,
            is_pause_allowed: true,
            is_next_allowed: true,
            is_previous_allowed: false,
        }
    }

    fn settings() -> DiscoverySettings<'static> {
        DiscoverySettings {
            base_topic: "unified-hifi",
            discovery_prefix: "homeassistant",
            availability_topic: "unified-hifi/bridge/status",
        }
    }

    #[test]
    fn publishes_state_art_volume_mute_and_allowed_buttons_only() {
        let zone = zone_fixture();
        let entries = discovery_entries(&zone, &settings());
        let topics: Vec<&str> = entries.iter().map(|(topic, _)| topic.as_str()).collect();

        assert!(topics
            .iter()
            .any(|t| t.contains("/sensor/") && t.ends_with("_state/config")));
        assert!(topics
            .iter()
            .any(|t| t.contains("/image/") && t.ends_with("_art/config")));
        assert!(topics
            .iter()
            .any(|t| t.contains("/number/") && t.ends_with("_volume/config")));
        assert!(topics
            .iter()
            .any(|t| t.contains("/switch/") && t.ends_with("_mute/config")));
        assert!(topics.iter().any(|t| t.ends_with("_play/config")));
        assert!(topics.iter().any(|t| t.ends_with("_pause/config")));
        assert!(topics.iter().any(|t| t.ends_with("_next/config")));
        // Zone does not allow "previous" - no button published for it.
        assert!(!topics.iter().any(|t| t.ends_with("_previous/config")));
    }

    #[test]
    fn entities_share_one_device_identifier() {
        let zone = zone_fixture();
        let entries = discovery_entries(&zone, &settings());
        for (_, payload) in &entries {
            assert_eq!(
                payload["device"]["identifiers"][0],
                serde_json::json!("uhc_roon_abc")
            );
        }
    }

    #[test]
    fn no_volume_control_skips_volume_and_mute_entities() {
        let mut zone = zone_fixture();
        zone.volume_control = None;
        let entries = discovery_entries(&zone, &settings());
        let topics: Vec<&str> = entries.iter().map(|(topic, _)| topic.as_str()).collect();
        assert!(!topics.iter().any(|t| t.contains("/number/")));
        assert!(!topics.iter().any(|t| t.contains("/switch/")));
    }

    #[test]
    fn removal_retracts_every_possible_entity() {
        let topics = discovery_topics_for_removal("homeassistant", "roon:abc");
        assert_eq!(topics.len(), 8);
    }
}
