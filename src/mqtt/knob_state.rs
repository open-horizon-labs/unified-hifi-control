//! Retained per-knob state payload for the MQTT publisher (#523).
//!
//! One JSON object per knob carries everything the composed HA entities
//! read via `value_template`: battery level/charging, derived online
//! connectivity, the zone the knob is currently controlling, the desired
//! zone override, firmware version, and the volume-step override.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::knobs::store::Knob;

/// A knob with no `now_playing`/status poll in this long is considered
/// offline. Chosen to comfortably exceed the knob firmware's normal poll
/// interval (seconds) while still surfacing a real disconnect promptly.
pub const OFFLINE_AFTER_SECS: i64 = 300;

/// Retained payload published to `<base_topic>/knob/<knob_id>/state`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct KnobStatePayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery_level: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery_charging: Option<bool>,
    /// Derived from `last_seen` staleness, not a field UHC stores directly.
    pub online: bool,
    /// The zone this knob is currently controlling, as last reported by the
    /// device itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone_id: Option<String>,
    /// The user-requested zone override (Home Assistant `select`), distinct
    /// from `zone_id` above.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assigned_zone_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firmware_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_step: Option<f64>,
}

/// Build the retained state payload for one knob at `now`.
pub fn build_state_payload(knob: &Knob, now: DateTime<Utc>) -> KnobStatePayload {
    let online = (now - knob.last_seen).num_seconds() < OFFLINE_AFTER_SECS;
    KnobStatePayload {
        battery_level: knob.status.battery_level,
        battery_charging: knob.status.battery_charging,
        online,
        zone_id: knob.status.zone_id.clone(),
        assigned_zone_id: knob.config.assigned_zone_id.clone(),
        firmware_version: knob.version.clone(),
        volume_step: knob.config.volume_step_override,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::knobs::store::{KnobConfig, KnobStatus};
    use chrono::Duration;

    fn knob_fixture(last_seen: DateTime<Utc>) -> Knob {
        Knob {
            name: "Kitchen Knob".to_string(),
            last_seen,
            version: Some("1.2.3".to_string()),
            config: KnobConfig {
                volume_step_override: Some(2.5),
                assigned_zone_id: Some("roon:abc".to_string()),
                ..KnobConfig::default()
            },
            config_sha: "deadbeef".to_string(),
            status: KnobStatus {
                battery_level: Some(80),
                battery_charging: Some(true),
                zone_id: Some("roon:def".to_string()),
                ip: None,
            },
        }
    }

    #[test]
    fn recently_seen_knob_is_online() {
        let payload = build_state_payload(&knob_fixture(Utc::now()), Utc::now());
        assert!(payload.online);
        assert_eq!(payload.battery_level, Some(80));
        assert_eq!(payload.battery_charging, Some(true));
        assert_eq!(payload.zone_id.as_deref(), Some("roon:def"));
        assert_eq!(payload.assigned_zone_id.as_deref(), Some("roon:abc"));
        assert_eq!(payload.firmware_version.as_deref(), Some("1.2.3"));
        assert_eq!(payload.volume_step, Some(2.5));
    }

    #[test]
    fn stale_last_seen_reports_offline() {
        let now = Utc::now();
        let stale = now - Duration::seconds(OFFLINE_AFTER_SECS + 60);
        let payload = build_state_payload(&knob_fixture(stale), now);
        assert!(!payload.online);
    }

    #[test]
    fn missing_optional_fields_are_omitted() {
        let mut knob = knob_fixture(Utc::now());
        knob.status.battery_level = None;
        knob.status.battery_charging = None;
        knob.config.volume_step_override = None;
        knob.config.assigned_zone_id = None;
        knob.version = None;
        let payload = build_state_payload(&knob, Utc::now());
        let json = serde_json::to_value(&payload).expect("serializes");
        assert!(json.get("battery_level").is_none());
        assert!(json.get("battery_charging").is_none());
        assert!(json.get("assigned_zone_id").is_none());
        assert!(json.get("firmware_version").is_none());
        assert!(json.get("volume_step").is_none());
        // `online` and `zone_id` policy differs: online always present.
        assert!(json.get("online").is_some());
    }
}
