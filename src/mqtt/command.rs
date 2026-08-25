//! Inbound HA -> UHC command routing for the MQTT publisher (#508, #529).
//!
//! Command topics carry a UHC zone id via a slug -> zone id map the
//! publisher fills in as it (re)publishes discovery/state (see
//! [`crate::mqtt::topics::zone_slug`]). Commands route through one of two
//! sanctioned surfaces, chosen by zone prefix:
//!
//! - Providers migrated onto `AdapterLogic` (Music Assistant, Spotify, Apple
//!   Music) route through `AppState::adapter_registry`, exactly like
//!   `src/mcp/tools/transport.rs` and `src/knobs/routes.rs` use it.
//! - Legacy zones (Roon, LMS, HQPlayer, OpenHome, UPnP) route through the
//!   reliable command gateway (`crate::bus::runtime::CommandGateway`) via
//!   the same `dispatch_*_runtime_command` entry points HTTP, knob, and MCP
//!   surfaces use - specifically their `_via` variants, which take the
//!   gateway (and, for HQPlayer, the aggregator) directly rather than a full
//!   `AppState`, since this module never holds one
//!   (`tests/adapter_boundary_lint.rs` forbids it from reaching a concrete
//!   adapter field directly).
//!
//! HQPlayer's volume is native decibels, not the 0-100 percentage every HA
//! `number` entity sends; consistent with `crate::mqtt::state`'s decision to
//! never publish a normalized volume for a decibel-scale zone, an inbound
//! volume/mute command for an HQPlayer zone is refused rather than
//! misapplied against the wrong scale.

use std::sync::Arc;

use crate::adapters::{AdapterCommand, AdapterCommandResponse};
use crate::aggregator::ZoneAggregator;
use crate::api::{
    dispatch_lms_runtime_command_via, dispatch_openhome_runtime_command_via,
    dispatch_roon_runtime_command_via, dispatch_upnp_runtime_command_via, AdapterRegistry,
};
use crate::bus::runtime::CommandGateway;
use crate::bus::Command;
use crate::knobs::routes::dispatch_hqplayer_action_via;

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

    /// Native transport/volume `Command` for the reliable command gateway's
    /// percentage-scale legacy providers (Roon, LMS, OpenHome, UPnP). Not
    /// used for HQPlayer, whose decibel scale needs its own zone-aware
    /// translation - see [`ParsedAction::into_hqplayer_action`].
    fn into_gateway_command(self) -> Command {
        match self {
            ParsedAction::Play => Command::Play,
            ParsedAction::Pause => Command::Pause,
            ParsedAction::Next => Command::Next,
            ParsedAction::Previous => Command::Previous,
            ParsedAction::Volume(value) => Command::VolumeAbsolute {
                value: value as f32,
                output_id: None,
            },
            ParsedAction::Mute(muted) => Command::Mute {
                muted,
                output_id: None,
            },
        }
    }

    /// The action name and optional numeric value `dispatch_hqplayer_action_via` expects, or
    /// `None` for a verb HQPlayer's decibel-scale volume cannot honor from a 0-100 MQTT payload.
    fn into_hqplayer_action(self) -> Option<(&'static str, Option<f64>)> {
        match self {
            ParsedAction::Play => Some(("play", None)),
            ParsedAction::Pause => Some(("pause", None)),
            ParsedAction::Next => Some(("next", None)),
            ParsedAction::Previous => Some(("previous", None)),
            ParsedAction::Volume(_) | ParsedAction::Mute(_) => None,
        }
    }
}

/// Outcome of routing one command, for logging/testing.
#[derive(Debug, PartialEq)]
pub enum DispatchOutcome {
    Sent,
    /// The owning provider (adapter or gateway endpoint) rejected the command.
    Refused(String),
    /// The command was not attempted: the zone's prefix has no bridged provider, or the verb is
    /// not supported for that provider (e.g. HQPlayer volume/mute over MQTT's percentage scale).
    Unsupported(String),
}

/// Route one parsed action to the adapter or gateway endpoint that owns `zone_id`.
pub async fn dispatch(
    adapter_registry: &Arc<AdapterRegistry>,
    aggregator: &ZoneAggregator,
    reliable_commands: Option<&CommandGateway>,
    zone_id: &str,
    action: ParsedAction,
) -> DispatchOutcome {
    let Some(prefix) = zone_id.split(':').next().filter(|p| !p.is_empty()) else {
        return DispatchOutcome::Unsupported("zone id has no provider prefix".to_string());
    };

    if adapter_registry.has_adapter(prefix).await {
        let command = action.into_adapter_command();
        return match adapter_registry.command(prefix, zone_id, command).await {
            Ok(AdapterCommandResponse { success: true, .. }) => DispatchOutcome::Sent,
            Ok(AdapterCommandResponse { error, .. }) => {
                DispatchOutcome::Refused(error.unwrap_or_else(|| "command refused".to_string()))
            }
            Err(error) => DispatchOutcome::Refused(error.to_string()),
        };
    }

    match prefix {
        "roon" => gateway_outcome(
            dispatch_roon_runtime_command_via(reliable_commands, zone_id, action.into_gateway_command())
                .await,
        ),
        "lms" => gateway_outcome(
            dispatch_lms_runtime_command_via(reliable_commands, zone_id, action.into_gateway_command())
                .await,
        ),
        "openhome" => gateway_outcome(
            dispatch_openhome_runtime_command_via(
                reliable_commands,
                zone_id,
                action.into_gateway_command(),
            )
            .await,
        ),
        "upnp" => gateway_outcome(
            dispatch_upnp_runtime_command_via(reliable_commands, zone_id, action.into_gateway_command())
                .await,
        ),
        "hqplayer" => {
            let Some((hqp_action, value)) = action.into_hqplayer_action() else {
                return DispatchOutcome::Unsupported(
                    "HQPlayer volume/mute is not available over MQTT's 0-100 percentage scale"
                        .to_string(),
                );
            };
            let instance = zone_id.strip_prefix("hqplayer:").unwrap_or(zone_id);
            match dispatch_hqplayer_action_via(
                aggregator,
                reliable_commands,
                zone_id,
                instance,
                hqp_action,
                value,
            )
            .await
            {
                Ok(()) => DispatchOutcome::Sent,
                Err(error) => DispatchOutcome::Refused(error.message().to_string()),
            }
        }
        _ => DispatchOutcome::Unsupported(format!("no bridged provider for '{prefix}' zones")),
    }
}

/// Fold one of the reliable-gateway dispatch functions' `anyhow::Result<()>` into the shared
/// outcome type this module's callers log against.
fn gateway_outcome(result: anyhow::Result<()>) -> DispatchOutcome {
    match result {
        Ok(()) => DispatchOutcome::Sent,
        Err(error) => DispatchOutcome::Refused(error.to_string()),
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
    async fn dispatch_reports_unsupported_for_a_prefix_with_no_bridged_provider() {
        let registry = Arc::new(AdapterRegistry::default());
        let aggregator = ZoneAggregator::new(crate::bus::create_bus());
        let outcome = dispatch(&registry, &aggregator, None, "spotify:abc", ParsedAction::Play)
            .await;
        assert_eq!(
            outcome,
            DispatchOutcome::Unsupported("no bridged provider for 'spotify' zones".to_string())
        );
    }

    #[tokio::test]
    async fn dispatch_refuses_a_legacy_zone_with_no_reliable_command_gateway() {
        let registry = Arc::new(AdapterRegistry::default());
        let aggregator = ZoneAggregator::new(crate::bus::create_bus());
        let outcome = dispatch(&registry, &aggregator, None, "roon:abc", ParsedAction::Play).await;
        assert_eq!(
            outcome,
            DispatchOutcome::Refused("Not connected to Roon".to_string())
        );
    }

    #[tokio::test]
    async fn dispatch_refuses_hqplayer_volume_over_mqtts_percentage_scale() {
        let registry = Arc::new(AdapterRegistry::default());
        let aggregator = ZoneAggregator::new(crate::bus::create_bus());
        let outcome = dispatch(
            &registry,
            &aggregator,
            None,
            "hqplayer:instance1",
            ParsedAction::Volume(50.0),
        )
        .await;
        assert!(matches!(outcome, DispatchOutcome::Unsupported(_)));
    }
}
