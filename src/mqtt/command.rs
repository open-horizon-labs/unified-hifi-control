//! Inbound HA -> UHC command routing for the MQTT publisher (#508).
//!
//! Command topics carry a UHC zone id via a slug -> zone id map the
//! publisher fills in as it (re)publishes discovery/state (see
//! [`crate::mqtt::topics::zone_slug`]). Commands route to the owning
//! adapter through `AppState::adapter_registry`, the same sanctioned
//! surface `src/mcp/tools/transport.rs` and `src/knobs/routes.rs` use
//! (`tests/adapter_boundary_lint.rs` forbids this module from reaching a
//! concrete adapter field directly).
//!
//! **Known limitation:** `AdapterRegistry` only holds providers migrated to
//! the `AdapterLogic` trait (Music Assistant, Spotify, Apple Music today).
//! Roon, LMS, HQPlayer, OpenHome and UPnP zones are legacy `AppState`
//! fields the architecture boundary lint (#436) does not yet let a new
//! surface reach. Commands for those zones are refused with a clear log
//! line rather than bypassing the lint; once #436 migrates a legacy
//! adapter onto `AdapterLogic`, its zones gain MQTT command support for
//! free through this same path.

use std::sync::Arc;

use crate::adapters::{AdapterCommand, AdapterCommandResponse};
use crate::api::AdapterRegistry;

/// One transport/control action parsed from a command topic.
#[derive(Debug, Clone, PartialEq)]
pub enum ParsedAction {
    Play,
    Pause,
    Next,
    Previous,
    /// Percent volume, already clamped to 0-100.
    Volume(f64),
    Mute(bool),
}

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
            .map(|value| ParsedAction::Volume(value.clamp(0.0, 100.0))),
        "mute" => match payload.to_ascii_uppercase().as_str() {
            "ON" | "TRUE" | "1" => Some(ParsedAction::Mute(true)),
            "OFF" | "FALSE" | "0" => Some(ParsedAction::Mute(false)),
            _ => None,
        },
        _ => None,
    }
}

impl ParsedAction {
    fn into_adapter_command(self) -> AdapterCommand {
        match self {
            ParsedAction::Play => AdapterCommand::Play,
            ParsedAction::Pause => AdapterCommand::Pause,
            ParsedAction::Next => AdapterCommand::Next,
            ParsedAction::Previous => AdapterCommand::Previous,
            // The adapter registry's volume scale is each provider's native
            // 0-100 percentage convention (see `AdapterCommand::VolumeAbsolute`
            // call sites in `src/knobs/routes.rs`).
            ParsedAction::Volume(value) => AdapterCommand::VolumeAbsolute(value.round() as i32),
            ParsedAction::Mute(muted) => AdapterCommand::Mute(muted),
        }
    }
}

/// Outcome of routing one command, for logging/testing.
#[derive(Debug, PartialEq)]
pub enum DispatchOutcome {
    Sent,
    AdapterRefused(String),
    /// The zone's prefix has no registry-backed adapter - see the module
    /// doc's known limitation.
    ProviderNotBridged,
}

/// Route one parsed action to the adapter that owns `zone_id`.
pub async fn dispatch(
    adapter_registry: &Arc<AdapterRegistry>,
    zone_id: &str,
    action: ParsedAction,
) -> DispatchOutcome {
    let Some(prefix) = zone_id.split(':').next().filter(|p| !p.is_empty()) else {
        return DispatchOutcome::ProviderNotBridged;
    };
    if !adapter_registry.has_adapter(prefix).await {
        return DispatchOutcome::ProviderNotBridged;
    }
    let command = action.into_adapter_command();
    match adapter_registry.command(prefix, zone_id, command).await {
        Ok(AdapterCommandResponse {
            success: true,
            ..
        }) => DispatchOutcome::Sent,
        Ok(AdapterCommandResponse { error, .. }) => {
            DispatchOutcome::AdapterRefused(error.unwrap_or_else(|| "command refused".to_string()))
        }
        Err(error) => DispatchOutcome::AdapterRefused(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_well_formed_command_topics() {
        assert_eq!(
            parse_command_topic("unified-hifi", "unified-hifi/media_player/roon_abc/play/set"),
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
        assert_eq!(
            parse_action("mute", "off"),
            Some(ParsedAction::Mute(false))
        );
    }

    #[test]
    fn unrecognized_actions_and_payloads_are_ignored() {
        assert_eq!(parse_action("seek", "10"), None);
        assert_eq!(parse_action("volume", "not-a-number"), None);
        assert_eq!(parse_action("mute", "maybe"), None);
    }

    #[tokio::test]
    async fn dispatch_reports_unbridged_provider_for_legacy_zones() {
        let registry = Arc::new(AdapterRegistry::default());
        let outcome = dispatch(&registry, "roon:abc", ParsedAction::Play).await;
        assert_eq!(outcome, DispatchOutcome::ProviderNotBridged);
    }
}
