//! Pure translation between Home Assistant's REST/WebSocket payloads and UHC devices (#790).
//!
//! Sources: Home Assistant developer docs — media_player entity
//! (https://developers.home-assistant.io/docs/core/entity/media-player/), REST API
//! (https://developers.home-assistant.io/docs/api/rest/), WebSocket API
//! (https://developers.home-assistant.io/docs/api/websocket/), and
//! `homeassistant/components/media_player/const.py` for the feature bits.

use serde_json::Value;

use crate::devices::{
    CapabilityEvidence, DeviceCapabilities, DeviceCapability, DeviceInput, DevicePower,
};

/// The integration domain UHC's own Home Assistant export registers under. Entities on this
/// platform are UHC's zones reflected back; linking a zone to one would loop.
pub const UHC_PLATFORM: &str = "unified_hifi_control";

// `MediaPlayerEntityFeature` bits (media_player/const.py).
pub const FEATURE_VOLUME_SET: u64 = 4;
pub const FEATURE_VOLUME_MUTE: u64 = 8;
pub const FEATURE_TURN_ON: u64 = 128;
pub const FEATURE_TURN_OFF: u64 = 256;
pub const FEATURE_SELECT_SOURCE: u64 = 2048;

/// REST base (`…/api`) and WebSocket URL derived from the configured core-API URL.
///
/// The configured value is the `/api/config` endpoint UHC already probes
/// (`crate::mqtt::consumer::core_api_credentials`). Under the Supervisor the WebSocket lives at
/// `/core/websocket`, not `/core/api/websocket`.
pub fn endpoints(config_url: &str) -> Option<(String, String)> {
    let base = config_url.trim_end_matches('/').strip_suffix("/config")?;
    let ws_scheme = |url: &str| {
        url.replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1)
    };
    let websocket = match base.strip_suffix("/core/api") {
        Some(origin) => format!("{}/core/websocket", ws_scheme(origin)),
        None => format!("{}/websocket", ws_scheme(base)),
    };
    Some((base.to_string(), websocket))
}

/// Entity ids UHC must never offer: those registered by UHC's own integration.
///
/// Returns `None` when the registry payload is not a list, so the caller fails closed (offers
/// nothing) rather than offering an unfiltered set.
pub fn excluded_entities(registry: &Value) -> Option<std::collections::HashSet<String>> {
    let entries = registry.as_array()?;
    Some(
        entries
            .iter()
            .filter(|entry| entry.get("platform").and_then(Value::as_str) == Some(UHC_PLATFORM))
            .filter_map(|entry| entry.get("entity_id").and_then(Value::as_str))
            .map(str::to_string)
            .collect(),
    )
}

/// Registry `unique_id` per entity id: the only stable identity (entity ids are user-renamable).
pub fn unique_ids(registry: &Value) -> std::collections::HashMap<String, String> {
    registry
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            Some((
                entry.get("entity_id")?.as_str()?.to_string(),
                entry.get("unique_id")?.as_str()?.to_string(),
            ))
        })
        .collect()
}

/// What one `media_player` state object says.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityState {
    pub entity_id: String,
    pub name: String,
    pub power: DevicePower,
    pub volume: Option<f32>,
    pub muted: Option<bool>,
    pub source: Option<String>,
    pub sources: Vec<String>,
    pub features: u64,
}

/// Parse a state object; `None` for anything that is not a `media_player`.
pub fn parse_state(state: &Value) -> Option<EntityState> {
    let entity_id = state.get("entity_id")?.as_str()?;
    if !entity_id.starts_with("media_player.") {
        return None;
    }
    let attributes = state.get("attributes").cloned().unwrap_or(Value::Null);
    let text = |key: &str| attributes.get(key).and_then(Value::as_str);
    Some(EntityState {
        entity_id: entity_id.to_string(),
        name: text("friendly_name").unwrap_or(entity_id).to_string(),
        power: power_from_state(state.get("state").and_then(Value::as_str).unwrap_or("")),
        volume: attributes
            .get("volume_level")
            .and_then(Value::as_f64)
            .map(|v| v as f32)
            .filter(|v| v.is_finite()),
        muted: attributes.get("is_volume_muted").and_then(Value::as_bool),
        source: text("source").map(str::to_string),
        sources: attributes
            .get("source_list")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        features: attributes
            .get("supported_features")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    })
}

/// Power from a media_player state. `standby` is deprecated in Home Assistant (removal
/// 2026.8) and treated as `off`; `unavailable`/`unknown` are never rounded to off.
pub fn power_from_state(state: &str) -> DevicePower {
    match state {
        "off" | "standby" => DevicePower::Standby,
        "on" | "idle" | "playing" | "paused" | "buffering" => DevicePower::On,
        _ => DevicePower::Unknown,
    }
}

/// Inputs from `source_list`. The source string is both the id (what `select_source` takes)
/// and the name.
pub fn inputs(state: &EntityState) -> Vec<DeviceInput> {
    state
        .sources
        .iter()
        .map(|source| DeviceInput {
            id: source.clone(),
            name: source.clone(),
            available: true,
        })
        .collect()
}

/// Capability claims from `supported_features`: Home Assistant's own claim for the entity, so
/// the evidence is implementation-derived and readback is what confirms each command.
pub fn capabilities(features: u64) -> DeviceCapabilities {
    let claim = |bit: u64, path: &str| DeviceCapability {
        supported: features & bit != 0,
        evidence: CapabilityEvidence::ImplementationDerived,
        detail: if features & bit != 0 {
            format!("{path} via Home Assistant; confirmed by state readback")
        } else {
            "the entity does not advertise this feature".to_string()
        },
    };
    DeviceCapabilities {
        observe: DeviceCapability {
            supported: true,
            evidence: CapabilityEvidence::ImplementationDerived,
            detail: "media_player state from the Home Assistant REST API".to_string(),
        },
        volume: DeviceCapability {
            detail: if features & FEATURE_VOLUME_SET != 0 {
                "media_player.volume_set (0–1), bounded by any user ceiling; confirmed by state \
                 readback"
                    .to_string()
            } else {
                "the entity does not advertise volume_set".to_string()
            },
            ..claim(FEATURE_VOLUME_SET, "media_player.volume_set")
        },
        mute: claim(FEATURE_VOLUME_MUTE, "media_player.volume_mute"),
        power: DeviceCapability {
            supported: features & (FEATURE_TURN_ON | FEATURE_TURN_OFF) != 0,
            ..claim(
                FEATURE_TURN_ON | FEATURE_TURN_OFF,
                "media_player.turn_on/turn_off",
            )
        },
        input_select: claim(FEATURE_SELECT_SOURCE, "media_player.select_source"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn endpoints_follow_the_supervisor_and_standalone_layouts() {
        assert_eq!(
            endpoints("http://supervisor/core/api/config").unwrap(),
            (
                "http://supervisor/core/api".to_string(),
                "ws://supervisor/core/websocket".to_string()
            )
        );
        assert_eq!(
            endpoints("https://ha.local:8123/api/config").unwrap(),
            (
                "https://ha.local:8123/api".to_string(),
                "wss://ha.local:8123/api/websocket".to_string()
            )
        );
        assert!(endpoints("http://ha.local:8123/").is_none());
    }

    /// CRITICAL: UHC's own exported zones are never offered as devices, and an unreadable
    /// registry offers nothing rather than everything.
    #[test]
    fn critical_uhc_entities_are_excluded_and_bad_registry_fails_closed() {
        let registry = json!([
            {"entity_id": "media_player.living_room", "platform": "unified_hifi_control", "unique_id": "roon:1"},
            {"entity_id": "media_player.marantz", "platform": "denonavr", "unique_id": "0006781E0AB8"}
        ]);
        let excluded = excluded_entities(&registry).unwrap();
        assert!(excluded.contains("media_player.living_room"));
        assert!(!excluded.contains("media_player.marantz"));
        assert!(excluded_entities(&json!({"error": "unauthorized"})).is_none());
    }

    #[test]
    fn media_player_state_parses_and_other_domains_are_ignored() {
        let state = parse_state(&json!({
            "entity_id": "media_player.marantz",
            "state": "on",
            "attributes": {
                "friendly_name": "Marantz SR6008",
                "volume_level": 0.405, "is_volume_muted": false,
                "source": "TV Audio", "source_list": ["CD", "TV Audio"],
                "supported_features": 4 | 8 | 128 | 256 | 2048
            }
        }))
        .unwrap();
        assert_eq!(state.power, DevicePower::On);
        assert_eq!(state.volume, Some(0.405));
        assert_eq!(state.source.as_deref(), Some("TV Audio"));
        assert_eq!(inputs(&state).len(), 2);
        let claims = capabilities(state.features);
        assert!(claims.volume.supported && claims.power.supported && claims.input_select.supported);
        assert!(parse_state(&json!({"entity_id": "light.kitchen", "state": "on"})).is_none());
    }

    #[test]
    fn power_never_rounds_unknown_to_off() {
        assert_eq!(power_from_state("off"), DevicePower::Standby);
        assert_eq!(power_from_state("standby"), DevicePower::Standby);
        assert_eq!(power_from_state("idle"), DevicePower::On);
        assert_eq!(power_from_state("unavailable"), DevicePower::Unknown);
        assert_eq!(power_from_state("unknown"), DevicePower::Unknown);
    }

    #[test]
    fn missing_features_are_claimed_unsupported() {
        let claims = capabilities(0);
        assert!(!claims.volume.supported);
        assert!(!claims.power.supported);
        assert!(!claims.input_select.supported);
    }
}
