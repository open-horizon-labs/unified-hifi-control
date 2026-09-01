//! Home Assistant MQTT discovery payloads for one UHC control device
//! ("knob") (#523).
//!
//! Every entity here (`sensor`, `binary_sensor`, `select`, `number`) is
//! natively supported by HA MQTT discovery, unlike the zone `media_player`
//! composition in [`crate::mqtt::discovery`] - so one knob is one small HA
//! device grouping: a battery sensor, a charging binary_sensor, a
//! connectivity binary_sensor, a firmware version diagnostic sensor, a
//! current-zone sensor, a zone-reassignment select, and a volume-step
//! number. Display rotation and power-mode curves stay UHC-only; they are
//! not exposed here.
//!
//! **Naming/id stability (user decision, #523):** every `unique_id` and
//! topic component is derived from the knob id, which never changes. Only
//! `device.name` (and the zone select's friendly labels, where used) comes
//! from the knob's user-assigned display name, so a rename updates what HA
//! shows without ever re-creating an entity.

use serde::Serialize;

use crate::knobs::store::Knob;
use crate::mqtt::topics;

/// Non-secret runtime settings the discovery payloads are built from.
#[derive(Debug, Clone)]
pub struct KnobDiscoverySettings<'a> {
    pub base_topic: &'a str,
    pub discovery_prefix: &'a str,
    pub availability_topic: &'a str,
}

#[derive(Debug, Clone, Serialize)]
struct Device {
    identifiers: [String; 1],
    name: String,
    manufacturer: &'static str,
    model: &'static str,
    via_device: &'static str,
}

/// A knob with an empty (never-configured) display name still needs a
/// readable device name; the knob id itself is stable and unique so it is
/// the only safe fallback.
fn display_name(knob_id: &str, knob: &Knob) -> String {
    if knob.name.trim().is_empty() {
        format!("Knob {knob_id}")
    } else {
        knob.name.clone()
    }
}

fn device_for(knob_id: &str, knob: &Knob) -> Device {
    Device {
        identifiers: [format!("uhc_knob_{}", topics::zone_slug(knob_id))],
        name: display_name(knob_id, knob),
        manufacturer: "Unified Hi-Fi Control",
        model: "S3 Knob",
        via_device: "unified_hifi_control",
    }
}

/// One `(discovery_topic, retained_json_payload)` pair.
pub type DiscoveryEntry = (String, serde_json::Value);

/// Build every HA discovery entry for one knob.
///
/// `zone_ids` is the current live zone list (ids only), used as the
/// zone-reassignment `select`'s options - refreshed on every call so the
/// list tracks zones as they appear or disappear.
pub fn discovery_entries(
    knob_id: &str,
    knob: &Knob,
    zone_ids: &[String],
    settings: &KnobDiscoverySettings<'_>,
) -> Vec<DiscoveryEntry> {
    let device = device_for(knob_id, knob);
    let state_topic = topics::knob_state_topic(settings.base_topic, knob_id);
    let slug = topics::zone_slug(knob_id);
    let mut entries = Vec::new();

    entries.push((
        topics::discovery_topic(settings.discovery_prefix, "sensor", knob_id, "battery"),
        serde_json::json!({
            "name": "Battery",
            "unique_id": format!("uhc_knob_{slug}_battery"),
            "device_class": "battery",
            "unit_of_measurement": "%",
            "state_class": "measurement",
            "entity_category": "diagnostic",
            "state_topic": state_topic,
            "value_template": "{{ value_json.battery_level }}",
            "availability_topic": settings.availability_topic,
            "device": device,
        }),
    ));

    entries.push((
        topics::discovery_topic(
            settings.discovery_prefix,
            "binary_sensor",
            knob_id,
            "charging",
        ),
        serde_json::json!({
            "name": "Charging",
            "unique_id": format!("uhc_knob_{slug}_charging"),
            "device_class": "battery_charging",
            "entity_category": "diagnostic",
            "state_topic": state_topic,
            "value_template": "{{ 'ON' if value_json.battery_charging else 'OFF' }}",
            "availability_topic": settings.availability_topic,
            "device": device,
        }),
    ));

    entries.push((
        topics::discovery_topic(
            settings.discovery_prefix,
            "binary_sensor",
            knob_id,
            "connectivity",
        ),
        serde_json::json!({
            "name": "Connectivity",
            "unique_id": format!("uhc_knob_{slug}_connectivity"),
            "device_class": "connectivity",
            "entity_category": "diagnostic",
            "state_topic": state_topic,
            "value_template": "{{ 'ON' if value_json.online else 'OFF' }}",
            "availability_topic": settings.availability_topic,
            "device": device,
        }),
    ));

    entries.push((
        topics::discovery_topic(settings.discovery_prefix, "sensor", knob_id, "firmware"),
        serde_json::json!({
            "name": "Firmware Version",
            "unique_id": format!("uhc_knob_{slug}_firmware"),
            "entity_category": "diagnostic",
            "state_topic": state_topic,
            "value_template": "{{ value_json.firmware_version }}",
            "availability_topic": settings.availability_topic,
            "device": device,
        }),
    ));

    entries.push((
        topics::discovery_topic(settings.discovery_prefix, "sensor", knob_id, "zone"),
        serde_json::json!({
            "name": "Zone",
            "unique_id": format!("uhc_knob_{slug}_zone"),
            "state_topic": state_topic,
            "value_template": "{{ value_json.zone_id }}",
            "availability_topic": settings.availability_topic,
            "device": device,
        }),
    ));

    entries.push((
        topics::discovery_topic(settings.discovery_prefix, "select", knob_id, "zone_select"),
        serde_json::json!({
            "name": "Zone",
            "unique_id": format!("uhc_knob_{slug}_zone_select"),
            "entity_category": "config",
            "options": zone_ids,
            "state_topic": state_topic,
            "value_template": "{{ value_json.assigned_zone_id }}",
            "command_topic": topics::knob_command_topic(settings.base_topic, knob_id, "zone"),
            "availability_topic": settings.availability_topic,
            "device": device,
        }),
    ));

    entries.push((
        topics::discovery_topic(settings.discovery_prefix, "number", knob_id, "volume_step"),
        serde_json::json!({
            "name": "Volume Step",
            "unique_id": format!("uhc_knob_{slug}_volume_step"),
            "entity_category": "config",
            "min": 0,
            "max": 20,
            "step": 0.5,
            "mode": "box",
            "state_topic": state_topic,
            "value_template": "{{ value_json.volume_step }}",
            "command_topic": topics::knob_command_topic(settings.base_topic, knob_id, "volume_step"),
            "availability_topic": settings.availability_topic,
            "device": device,
        }),
    ));

    entries
}

/// Discovery topics for a knob that no longer exists, to retract every
/// retained config this publisher could have written for it.
pub fn discovery_topics_for_removal(discovery_prefix: &str, knob_id: &str) -> Vec<String> {
    vec![
        topics::discovery_topic(discovery_prefix, "sensor", knob_id, "battery"),
        topics::discovery_topic(discovery_prefix, "binary_sensor", knob_id, "charging"),
        topics::discovery_topic(discovery_prefix, "binary_sensor", knob_id, "connectivity"),
        topics::discovery_topic(discovery_prefix, "sensor", knob_id, "firmware"),
        topics::discovery_topic(discovery_prefix, "sensor", knob_id, "zone"),
        topics::discovery_topic(discovery_prefix, "select", knob_id, "zone_select"),
        topics::discovery_topic(discovery_prefix, "number", knob_id, "volume_step"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knobs::store::{KnobConfig, KnobStatus};
    use chrono::Utc;

    fn knob_fixture(name: &str) -> Knob {
        Knob {
            name: name.to_string(),
            last_seen: Utc::now(),
            version: Some("1.0.0".to_string()),
            config: KnobConfig::default(),
            config_sha: "abc12345".to_string(),
            status: KnobStatus::default(),
        }
    }

    fn settings() -> KnobDiscoverySettings<'static> {
        KnobDiscoverySettings {
            base_topic: "unified-hifi",
            discovery_prefix: "homeassistant",
            availability_topic: "unified-hifi/bridge/status",
        }
    }

    #[test]
    fn publishes_every_entity_grouped_under_one_device() {
        let knob = knob_fixture("Kitchen Knob");
        let zones = vec!["roon:abc".to_string()];
        let entries = discovery_entries("aabbcc", &knob, &zones, &settings());
        assert_eq!(entries.len(), 7);
        for (_, payload) in &entries {
            assert_eq!(
                payload["device"]["identifiers"][0],
                serde_json::json!("uhc_knob_aabbcc")
            );
            assert_eq!(payload["device"]["name"], serde_json::json!("Kitchen Knob"));
        }
    }

    #[test]
    fn unique_ids_and_topics_derive_from_knob_id_not_name() {
        let knob_a = knob_fixture("Living Room Knob");
        let knob_b = knob_fixture("Renamed To Something Else");
        let zones: Vec<String> = vec![];
        let entries_a = discovery_entries("aabbcc", &knob_a, &zones, &settings());
        let entries_b = discovery_entries("aabbcc", &knob_b, &zones, &settings());

        // Same knob id -> identical topics and unique_ids regardless of name.
        for ((topic_a, payload_a), (topic_b, payload_b)) in entries_a.iter().zip(entries_b.iter()) {
            assert_eq!(topic_a, topic_b);
            assert_eq!(payload_a["unique_id"], payload_b["unique_id"]);
            // Only the display name should differ.
            assert_ne!(payload_a["device"]["name"], payload_b["device"]["name"]);
        }
    }

    #[test]
    fn empty_display_name_falls_back_to_knob_id() {
        let knob = knob_fixture("");
        let zones: Vec<String> = vec![];
        let entries = discovery_entries("aabbcc", &knob, &zones, &settings());
        assert_eq!(
            entries[0].1["device"]["name"],
            serde_json::json!("Knob aabbcc")
        );
    }

    #[test]
    fn zone_select_options_track_the_live_zone_list() {
        let knob = knob_fixture("Kitchen Knob");
        let zones = vec!["roon:abc".to_string(), "lms:def".to_string()];
        let entries = discovery_entries("aabbcc", &knob, &zones, &settings());
        let (_, select) = entries
            .iter()
            .find(|(topic, _)| topic.contains("/select/"))
            .expect("select entry present");
        assert_eq!(
            select["options"],
            serde_json::json!(["roon:abc", "lms:def"])
        );
    }

    #[test]
    fn removal_retracts_every_entity() {
        let topics = discovery_topics_for_removal("homeassistant", "aabbcc");
        assert_eq!(topics.len(), 7);
    }
}
