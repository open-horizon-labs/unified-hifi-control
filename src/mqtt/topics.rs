//! MQTT topic naming for the Home Assistant publisher (#508).
//!
//! One zone maps to one HA "device" grouping several entities (state sensor,
//! album art image, volume number, mute switch, transport buttons). Topics
//! are namespaced under a configurable `base_topic` (state/command) and a
//! separate `discovery_prefix` (HA's `homeassistant` convention), matching
//! the split UHC settings expose.

/// Turn a UHC zone id (e.g. `roon:1234`) into an MQTT/HA-safe slug.
///
/// HA object ids and MQTT topic levels both reject `:`, so the prefix
/// separator becomes `_`. Collisions are not possible: no adapter mints a
/// zone id containing `_` where another's `:`-joined form would collide,
/// because `PrefixedZoneId` enforces exactly one `source:raw_id` split.
pub fn zone_slug(zone_id: &str) -> String {
    zone_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Bridge-wide availability topic (Last Will + online announcement).
pub fn availability_topic(base_topic: &str) -> String {
    format!("{base_topic}/bridge/status")
}

/// Retained JSON state topic for one zone.
pub fn state_topic(base_topic: &str, zone_id: &str) -> String {
    format!("{base_topic}/media_player/{}/state", zone_slug(zone_id))
}

/// Command topic for one transport/control action on one zone.
pub fn command_topic(base_topic: &str, zone_id: &str, action: &str) -> String {
    format!(
        "{base_topic}/media_player/{}/{action}/set",
        zone_slug(zone_id)
    )
}

/// HA MQTT discovery config topic for one entity of one zone.
///
/// `<discovery_prefix>/<component>/<node_id>/<object_id>/config`, the
/// single-entity discovery form documented at
/// <https://www.home-assistant.io/integrations/mqtt/#discovery-topic>.
pub fn discovery_topic(
    discovery_prefix: &str,
    component: &str,
    zone_id: &str,
    entity_suffix: &str,
) -> String {
    format!(
        "{discovery_prefix}/{component}/unified_hifi_control/{}_{entity_suffix}/config",
        zone_slug(zone_id)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_replace_non_alphanumeric_with_underscore() {
        assert_eq!(zone_slug("roon:1234abCD"), "roon_1234abCD");
        assert_eq!(zone_slug("lms:00:11:22:33:44:55"), "lms_00_11_22_33_44_55");
    }

    #[test]
    fn topics_are_namespaced_under_base_and_discovery_prefix() {
        assert_eq!(
            availability_topic("unified-hifi"),
            "unified-hifi/bridge/status"
        );
        assert_eq!(
            state_topic("unified-hifi", "roon:abc"),
            "unified-hifi/media_player/roon_abc/state"
        );
        assert_eq!(
            command_topic("unified-hifi", "roon:abc", "volume"),
            "unified-hifi/media_player/roon_abc/volume/set"
        );
        assert_eq!(
            discovery_topic("homeassistant", "sensor", "roon:abc", "state"),
            "homeassistant/sensor/unified_hifi_control/roon_abc_state/config"
        );
    }
}
