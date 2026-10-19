//! MQTT topic and payload decoding. Semantic routing lives in zone_control.

pub use crate::zone_control::{dispatch, Action as ParsedAction, DispatchOutcome};

/// Split a command topic into `(zone_slug, action_name)`, or `None` if it
/// does not match `<base_topic>/media_player/<slug>/<action>/set`.
pub fn parse_command_topic<'a>(base_topic: &str, topic: &'a str) -> Option<(&'a str, &'a str)> {
    let rest = topic.strip_prefix(base_topic)?;
    let rest = rest.strip_prefix("/media_player/")?;
    let rest = rest.strip_suffix("/set")?;
    let (slug, action) = rest.rsplit_once('/')?;
    if slug.is_empty() || action.is_empty() {
        return None;
    }
    Some((slug, action))
}

/// Interpret an action name and its raw MQTT payload as one control action.
/// Unrecognized actions or malformed payloads return `None` - the caller
/// logs and drops these rather than treating them as adapter errors.
pub fn parse_action(action: &str, payload: &str) -> Option<ParsedAction> {
    let payload = payload.trim();
    match action {
        "play" => Some(ParsedAction::Play),
        "pause" => Some(ParsedAction::Pause),
        "next" => Some(ParsedAction::Next),
        "previous" => Some(ParsedAction::Previous),
        "volume" => payload
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite())
            .map(|value| ParsedAction::Volume(value.clamp(0.0, 100.0))),
        "mute" => match payload.to_ascii_uppercase().as_str() {
            "ON" | "TRUE" | "1" => Some(ParsedAction::Mute(true)),
            "OFF" | "FALSE" | "0" => Some(ParsedAction::Mute(false)),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{aggregator::ZoneAggregator, api::AdapterRegistry};
    use std::sync::Arc;

    #[test]
    fn parses_well_formed_command_topics() {
        assert_eq!(
            parse_command_topic(
                "unified-hifi",
                "unified-hifi/media_player/roon_abc/play/set"
            ),
            Some(("roon_abc", "play"))
        );
        assert_eq!(
            parse_command_topic(
                "unified-hifi",
                "unified-hifi/media_player/roon_abc/volume/set"
            ),
            Some(("roon_abc", "volume"))
        );
    }

    #[test]
    fn rejects_topics_outside_the_base_or_missing_suffix() {
        assert_eq!(
            parse_command_topic("unified-hifi", "other-base/media_player/roon_abc/play/set"),
            None
        );
        assert_eq!(
            parse_command_topic("unified-hifi", "unified-hifi/media_player/roon_abc/play"),
            None
        );
    }

    #[test]
    fn parses_transport_and_volume_and_mute_payloads() {
        assert_eq!(parse_action("play", "PRESS"), Some(ParsedAction::Play));
        assert_eq!(
            parse_action("volume", "42"),
            Some(ParsedAction::Volume(42.0))
        );
        assert_eq!(
            parse_action("volume", "150"),
            Some(ParsedAction::Volume(100.0)),
            "volume payloads are clamped to 0-100"
        );
        assert_eq!(parse_action("mute", "ON"), Some(ParsedAction::Mute(true)));
        assert_eq!(parse_action("mute", "off"), Some(ParsedAction::Mute(false)));
    }

    #[test]
    fn unrecognized_actions_and_payloads_are_ignored() {
        assert_eq!(parse_action("seek", "10"), None);
        assert_eq!(parse_action("volume", "not-a-number"), None);
        for value in ["NaN", "inf", "-inf"] {
            assert_eq!(
                parse_action("volume", value),
                None,
                "non-finite volume must never become an adapter level"
            );
        }
        assert_eq!(parse_action("mute", "maybe"), None);
    }

    #[tokio::test]
    async fn dispatch_reports_unsupported_for_a_prefix_with_no_bridged_provider() {
        let registry = Arc::new(AdapterRegistry::default());
        let bus = crate::bus::create_bus();
        let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
        let directory = tempfile::tempdir().unwrap();
        let devices = crate::devices::service::DeviceService::new(
            aggregator.clone(),
            bus,
            crate::devices::links::DeviceLinkStore::open(directory.path().join("links.json")),
        );
        let outcome = dispatch(
            &registry,
            &aggregator,
            &devices,
            None,
            "spotify:abc",
            ParsedAction::Play,
        )
        .await;
        assert_eq!(
            outcome,
            DispatchOutcome::Unsupported("no bridged provider for 'spotify' zones".to_string())
        );
    }

    #[tokio::test]
    async fn dispatch_refuses_a_legacy_zone_with_no_reliable_command_gateway() {
        let registry = Arc::new(AdapterRegistry::default());
        let bus = crate::bus::create_bus();
        let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
        let directory = tempfile::tempdir().unwrap();
        let devices = crate::devices::service::DeviceService::new(
            aggregator.clone(),
            bus,
            crate::devices::links::DeviceLinkStore::open(directory.path().join("links.json")),
        );
        let outcome = dispatch(
            &registry,
            &aggregator,
            &devices,
            None,
            "roon:abc",
            ParsedAction::Play,
        )
        .await;
        assert_eq!(
            outcome,
            DispatchOutcome::Refused("Not connected to Roon".to_string())
        );
    }

    #[tokio::test]
    async fn dispatch_refuses_hqplayer_volume_over_mqtts_percentage_scale() {
        let registry = Arc::new(AdapterRegistry::default());
        let bus = crate::bus::create_bus();
        let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
        let directory = tempfile::tempdir().unwrap();
        let devices = crate::devices::service::DeviceService::new(
            aggregator.clone(),
            bus,
            crate::devices::links::DeviceLinkStore::open(directory.path().join("links.json")),
        );
        let outcome = dispatch(
            &registry,
            &aggregator,
            &devices,
            None,
            "hqplayer:instance1",
            ParsedAction::Volume(50.0),
        )
        .await;
        assert!(matches!(outcome, DispatchOutcome::Unsupported(_)));
    }
}
