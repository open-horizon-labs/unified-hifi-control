//! Inbound HA -> UHC command routing for knob entities (#523).
//!
//! The zone-reassignment `select` and volume-step `number` both route
//! through [`crate::knobs::store::KnobStore::update_config`] - the exact
//! same knob config path `src/knobs/routes.rs` uses for the web UI/device
//! sync flow - rather than a new adapter surface.

use crate::knobs::store::{KnobConfigUpdate, KnobStore};

/// One config action parsed from a knob command topic.
#[derive(Debug, Clone, PartialEq)]
pub enum ParsedKnobAction {
    /// Reassign the knob's controlled zone. An empty string clears the
    /// override, letting the device choose its own zone again.
    Zone(String),
    /// Volume-step override in the same units `KnobConfig::volume_step_override`
    /// uses; 0 or negative clears it.
    VolumeStep(f64),
}

/// Split a command topic into `(knob_slug, action_name)`, or `None` if it
/// does not match `<base_topic>/knob/<slug>/<action>/set`.
pub fn parse_command_topic<'a>(base_topic: &str, topic: &'a str) -> Option<(&'a str, &'a str)> {
    let rest = topic.strip_prefix(base_topic)?;
    let rest = rest.strip_prefix("/knob/")?;
    let rest = rest.strip_suffix("/set")?;
    let (slug, action) = rest.rsplit_once('/')?;
    if slug.is_empty() || action.is_empty() {
        return None;
    }
    Some((slug, action))
}

/// Interpret an action name and its raw MQTT payload as one knob config
/// action. Unrecognized actions or malformed payloads return `None`.
pub fn parse_action(action: &str, payload: &str) -> Option<ParsedKnobAction> {
    let payload = payload.trim();
    match action {
        "zone" => Some(ParsedKnobAction::Zone(payload.to_string())),
        "volume_step" => payload
            .parse::<f64>()
            .ok()
            .map(ParsedKnobAction::VolumeStep),
        _ => None,
    }
}

/// Outcome of routing one command, for logging/testing.
#[derive(Debug, PartialEq)]
pub enum DispatchOutcome {
    Applied,
    KnobNotFound,
}

/// Apply one parsed action to `knob_id` through `KnobStore::update_config`.
pub async fn dispatch(
    knobs: &KnobStore,
    knob_id: &str,
    action: ParsedKnobAction,
) -> DispatchOutcome {
    let update = match action {
        ParsedKnobAction::Zone(zone_id) => KnobConfigUpdate {
            assigned_zone_id: Some(zone_id),
            ..Default::default()
        },
        ParsedKnobAction::VolumeStep(step) => KnobConfigUpdate {
            volume_step_override: Some(step),
            ..Default::default()
        },
    };
    match knobs.update_config(knob_id, update).await {
        Some(_) => DispatchOutcome::Applied,
        None => DispatchOutcome::KnobNotFound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_command_topics() {
        assert_eq!(
            parse_command_topic("unified-hifi", "unified-hifi/knob/aabbcc/zone/set"),
            Some(("aabbcc", "zone"))
        );
        assert_eq!(
            parse_command_topic("unified-hifi", "unified-hifi/knob/aabbcc/volume_step/set"),
            Some(("aabbcc", "volume_step"))
        );
    }

    #[test]
    fn rejects_topics_outside_the_base_or_missing_suffix() {
        assert_eq!(
            parse_command_topic("unified-hifi", "other-base/knob/aabbcc/zone/set"),
            None
        );
        assert_eq!(
            parse_command_topic("unified-hifi", "unified-hifi/knob/aabbcc/zone"),
            None
        );
    }

    #[test]
    fn parses_zone_and_volume_step_payloads() {
        assert_eq!(
            parse_action("zone", "roon:abc"),
            Some(ParsedKnobAction::Zone("roon:abc".to_string()))
        );
        assert_eq!(
            parse_action("zone", ""),
            Some(ParsedKnobAction::Zone(String::new())),
            "empty payload clears the override"
        );
        assert_eq!(
            parse_action("volume_step", "2.5"),
            Some(ParsedKnobAction::VolumeStep(2.5))
        );
    }

    #[test]
    fn unrecognized_actions_and_payloads_are_ignored() {
        assert_eq!(parse_action("rotation", "180"), None);
        assert_eq!(parse_action("volume_step", "not-a-number"), None);
    }

    #[tokio::test]
    async fn dispatch_reports_missing_knob() {
        let knobs = KnobStore::default();
        let outcome = dispatch(&knobs, "does-not-exist", ParsedKnobAction::Zone("roon:abc".to_string()))
            .await;
        assert_eq!(outcome, DispatchOutcome::KnobNotFound);
    }
}
