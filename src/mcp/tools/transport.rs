//! Transport control: play, pause, skip, volume.
//!
//! # Two lookup tables live here, and neither is visible in `tools/list`
//!
//! 1. The MCP action -> backend action map. `playpause` becomes `play_pause`,
//!    and `prev` is accepted as a synonym for `previous`. Getting either wrong
//!    breaks the tool while leaving every snapshot green, so
//!    `tests/mcp_contract.rs::lms_round_trip_pins_the_action_map_and_the_volume_sign`
//!    asserts the command the backend actually receives.
//! 2. Volume handling: `volume_set` requires a value, `volume_up`/`volume_down`
//!    default the delta to 5, and `volume_down` negates it. The sign is the kind
//!    of thing a careless edit inverts silently.
//!
//! # #398: the action set is closed, and nothing is defaulted to Roon
//!
//! Three things changed here, and all three are behavior changes:
//!
//! - **The action match no longer ends in `other => other`.** It forwarded any
//!   string to an adapter, so a typo surfaced as whatever that backend said — or,
//!   with the device offline, as a device-lookup failure that never mentioned the
//!   action at all. An action outside
//!   [`CONTROL_ACTIONS`](crate::mcp::routing::CONTROL_ACTIONS) is now refused with
//!   that list, before the zone is even classified: the action check is free and
//!   independent of the zone, so it names the fault a client can fix first.
//! - **Volume reaches OpenHome and UPnP.** Both adapters implement
//!   `vol_abs`/`vol_rel` and `POST /{openhome,upnp}/control` has exposed them all
//!   along; only this path declined. The refusal that stood here —
//!   `"Volume control not supported for this zone type"` — was a false claim about
//!   two providers, and #395's envelope already flagged it `not_implemented`
//!   tracked by #398. It is now simply implemented.
//! - **An unplaceable zone id is refused instead of routed to Roon**, for both
//!   transport and volume, and an `hqplayer:` zone is refused *as HQPlayer* with
//!   #328 attached instead of being sent to Roon.
//!
//! `operation` still reports the *normalized* action, so `prev` surfaces as
//! `previous` and `playpause` as `play_pause`; `params.value` still reports the
//! *resolved* delta, so `volume_down` with no value surfaces as `-5.0`. On the two
//! providers #398 wired it also reports the **integer** that was sent, because
//! their `control` takes one — see [`set_volume`].
//!
//! Volume still uses a *different* routing rule from transport — see
//! [`crate::mcp::routing`]. The asymmetry has moved rather than gone: it used to
//! be OpenHome/UPnP, and now it is only that the library rule is narrower than
//! both.
//!
//! # Where a refusal's classification comes from
//!
//! Not from here. [`crate::mcp::capabilities`] owns whether a gap is the
//! provider's limit or UHC's, and this module asks it — so `hifi_control`'s
//! refusal and `hifi_capabilities`' report cannot describe the same gap two
//! different ways.

use crate::adapters::AdapterCommand;
use crate::api::AppState;
use crate::knobs::routes::{dispatch_hqplayer_action, HqpDispatchError};
use crate::mcp::capabilities::{support, Capability};
use crate::mcp::envelope::{Envelope, Observed, Refusal, Scope};
use crate::mcp::envelope::Provider;
use crate::mcp::routing::{
    unplaceable_zone_refusal, unplaceable_zone_text, TransportRoute, VolumeRoute, ZoneTarget,
    CONTROL_ACTIONS, TRANSPORT_ACTIONS,
};
use rust_mcp_sdk::{
    macros::{mcp_tool, JsonSchema},
    schema::{schema_utils::CallToolError, CallToolResult},
};
use serde::{Deserialize, Serialize};

/// Control playback
#[mcp_tool(
    name = "hifi_control",
    description = "Control playback: play, pause, playpause (toggle), next, previous, repeat_off/repeat_context/repeat_track, shuffle_on/shuffle_off, or adjust volume"
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiControlTool {
    /// The zone ID to control
    pub zone_id: String,
    /// Action: play, pause, playpause, next, previous, repeat_off, repeat_context, repeat_track, shuffle_on, shuffle_off, volume_set, volume_up, volume_down
    pub action: String,
    /// For volume actions: the level (0-100 for volume_set) or amount to change
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
}

/// Default step for `volume_up` / `volume_down` when no value is supplied.
const DEFAULT_VOLUME_DELTA: f64 = 5.0;

pub async fn handle_control(
    state: &AppState,
    args: HifiControlTool,
) -> Result<CallToolResult, CallToolError> {
    // HQPlayer has a provider-specific action vocabulary and resolves through
    // HqpInstanceManager rather than the shared adapter path.
    if ZoneTarget::classify(&args.zone_id) == ZoneTarget::HqPlayer {
        return handle_hqplayer_control(state, args).await;
    }

    // The action is checked first, and before the zone is classified. It is a
    // closed set with no I/O behind it, so this is the cheapest fault to name —
    // and naming it does not depend on the zone id being right.
    if !CONTROL_ACTIONS.contains(&args.action.as_str()) {
        return unknown_action(state, &args.zone_id, &args.action).await;
    }

    // Map MCP actions to backend actions. Volume actions divert to the volume
    // path, which routes differently from transport.
    let backend_action = match args.action.as_str() {
        "play" => "play",
        "pause" => "pause",
        "playpause" => "play_pause",
        "next" => "next",
        "previous" | "prev" => "previous",
        "volume_set" => {
            if let Some(v) = args.value {
                return set_volume(state, &args.zone_id, v, false).await;
            }
            // `value` is optional in the schema because volume_up/down default
            // it; volume_set cannot, so this is a per-action requirement the
            // input schema cannot express. The envelope names the parameter and
            // its range, which the prose already did — now machine-readably.
            return Envelope::write("hifi_control", "volume_absolute")
                .param("zone_id", &*args.zone_id)
                .param("action", "volume_set")
                .scope(
                    Scope::for_zone(
                        state,
                        &args.zone_id,
                        ZoneTarget::classify(&args.zone_id).provider(),
                    )
                    .await,
                )
                .refused(
                    "volume_set requires a value (0-100, or dB for HQPlayer)",
                    Refusal::invalid_parameter(
                        "value",
                        &["0-100"],
                        "action='volume_set' sets an absolute level and has no default. \
                         Use volume_up or volume_down for a relative change.",
                    ),
                );
        }
        "volume_up" => {
            let delta = args.value.unwrap_or(DEFAULT_VOLUME_DELTA);
            return set_volume(state, &args.zone_id, delta, true).await;
        }
        "volume_down" => {
            let delta = args.value.unwrap_or(DEFAULT_VOLUME_DELTA);
            // Negated: the same relative call, opposite direction.
            return set_volume(state, &args.zone_id, -delta, true).await;
        }
        "repeat_off" | "repeat_context" | "repeat_track" | "shuffle_on" | "shuffle_off" => {
            args.action.as_str()
        }
        // Unreachable: CONTROL_ACTIONS gates this whole match, and
        // `routing::tests::control_actions_cover_the_documented_set` proves the
        // two lists agree. Refused rather than forwarded, because forwarding is
        // exactly what #398 removed.
        _ => return unknown_action(state, &args.zone_id, &args.action).await,
    };

    let target = ZoneTarget::classify(&args.zone_id);
    // `operation` is the normalized backend action, so `prev` reports as
    // `previous` and `playpause` as `play_pause`.
    let env = Envelope::write("hifi_control", backend_action)
        .param("zone_id", &*args.zone_id)
        .param("action", &*args.action);

    if let Some(capability) = mode_capability(backend_action) {
        if !matches!(
            support(target, capability),
            crate::mcp::capabilities::Support::Supported
        ) {
            return refuse_zone(state, env, &args.zone_id, target, capability).await;
        }
    }

    let route = target.for_transport();
    if let TransportRoute::Refused(refused) = route {
        let capability = if backend_action == "next" || backend_action == "previous" {
            Capability::TransportSkip
        } else {
            Capability::Transport
        };
        return refuse_zone(state, env, &args.zone_id, refused, capability).await;
    }

    let result = match route {
        TransportRoute::Lms => state.lms.control(&args.zone_id, backend_action, None).await,
        TransportRoute::OpenHome => {
            state
                .openhome
                .control(&args.zone_id, backend_action, None)
                .await
        }
        TransportRoute::Upnp => {
            state
                .upnp
                .control(&args.zone_id, backend_action, None)
                .await
        }
        TransportRoute::Roon => state.roon.control(&args.zone_id, backend_action).await,
        TransportRoute::AppleMusic | TransportRoute::Spotify | TransportRoute::MusicAssistant => {
            dispatch_adapter_command(
                state,
                target.label(),
                &args.zone_id,
                transport_command(backend_action),
            )
            .await
        }
        // Handled before routing above; retained for exhaustiveness.
        TransportRoute::HqPlayer => unreachable_refused(),
        // Handled above; kept exhaustive rather than caught by a wildcard so a
        // new route variant fails to compile instead of falling through.
        TransportRoute::Refused(_) => unreachable_refused(),
    };

    // One aggregator snapshot serves both `scope.zone_name` and `observed`, so
    // the two cannot describe different moments. It is taken after the command,
    // which is what makes it a read-back rather than a guess — `set_volume` does
    // the same for the same reason, at the cost of a second read there because
    // its refusal path needs a scope before any command runs.
    let observed = Observed::from_aggregator(state, &args.zone_id).await;
    let scope = Scope {
        // Identification, not route. Since #398 the two agree for every id that
        // reaches an adapter, because nothing unplaceable does any more.
        provider: target.provider(),
        zone_id: Some(args.zone_id.clone()),
        zone_name: observed.as_ref().map(|o| o.zone.zone_name.clone()),
    };
    let env = env.scope(scope);

    match result {
        Ok(()) => {
            // Report the observed state back, so the model does not have to
            // follow every command with hifi_now_playing.
            match observed {
                Some(observed) => {
                    // The text renders the payload struct, so its key order is
                    // declaration order; `observed.zone` goes through `to_value`
                    // and is alphabetical. Equal as JSON, different bytes — see
                    // the envelope module docs.
                    let json = serde_json::to_string_pretty(&observed.zone)
                        .unwrap_or_else(|_| "{}".to_string());
                    let text = format!(
                        "Action '{}' executed.\n\nCurrent state:\n{}",
                        args.action, json
                    );
                    Ok(env.observed(Some(observed)).text_result(text))
                }
                None => Ok(env.text_result(format!("Action '{}' executed.", args.action))),
            }
        }
        Err(e) => env.failed(format!("Control error: {}", e)),
    }
}

/// The action vocabulary a direct HQPlayer zone accepts through `hifi_control`.
const HQPLAYER_CONTROL_ACTIONS: &[&str] = &[
    "play", "pause", "playpause", "next", "previous", "prev", "stop", "seek", "mute",
    "volume_set", "volume_up", "volume_down",
];

pub(crate) async fn handle_hqplayer_control(
    state: &AppState,
    args: HifiControlTool,
) -> Result<CallToolResult, CallToolError> {
    if !HQPLAYER_CONTROL_ACTIONS.contains(&args.action.as_str()) {
        return unknown_action_with_accepted(state, &args.zone_id, &args.action, HQPLAYER_CONTROL_ACTIONS).await;
    }
    let env = Envelope::write("hifi_control", hqplayer_operation(&args.action))
        .param("zone_id", &*args.zone_id)
        .param("action", &*args.action)
        .scope(Scope::for_zone(state, &args.zone_id, Provider::HqPlayer).await);
    let instance_name = args.zone_id.strip_prefix("hqplayer:").unwrap_or(&args.zone_id);
    let dispatch_action = if args.action == "volume_set" { "volume" } else { args.action.as_str() };
    let resolved_value = match args.action.as_str() {
        "volume_up" | "volume_down" => Some(args.value.unwrap_or(DEFAULT_VOLUME_DELTA)),
        _ => args.value,
    };
    let env = match resolved_value { Some(value) => env.param("value", value), None => env };
    match dispatch_hqplayer_action(state, &args.zone_id, instance_name, dispatch_action, resolved_value).await {
        Ok(()) => {
            let observed = Observed::from_aggregator(state, &args.zone_id).await;
            Ok(env.observed(observed).text_result(format!("Action '{}' executed and verified state was published.", args.action)))
        }
        Err(HqpDispatchError::NotFound(message)) => env.failed(message),
        Err(HqpDispatchError::BadRequest { message, .. }) => env.refused(message, Refusal::invalid_parameter(
            "value", &["a value valid for the selected HQPlayer action"],
            "The action was rejected against the zone state published by the aggregator.",
        )),
        Err(HqpDispatchError::Backend(message)) => env.failed(message),
    }
}

fn hqplayer_operation(action: &str) -> &'static str {
    match action {
        "play" => "play", "pause" => "pause", "stop" => "stop", "next" => "next",
        "previous" | "prev" => "previous", "playpause" => "play_pause", "seek" => "seek",
        "mute" => "mute", "volume_set" => "volume_absolute",
        "volume_up" | "volume_down" => "volume_relative", _ => "unknown_action",
    }
}

/// Refuse an action outside [`CONTROL_ACTIONS`].
///
/// `operation` is the literal `"unknown_action"` rather than the string the client
/// sent. Before #398, `other => other` put the unrecognised action straight into
/// `operation`, which made that field an open set and let an envelope report a verb
/// UHC does not have as though it were one.
async fn unknown_action(
    state: &AppState,
    zone_id: &str,
    action: &str,
) -> Result<CallToolResult, CallToolError> {
    unknown_action_with_accepted(state, zone_id, action, CONTROL_ACTIONS).await
}

async fn unknown_action_with_accepted(
    state: &AppState,
    zone_id: &str,
    action: &str,
    accepted: &'static [&'static str],
) -> Result<CallToolResult, CallToolError> {
    Envelope::write("hifi_control", "unknown_action")
        .param("zone_id", zone_id)
        .param("action", action)
        .scope(Scope::for_zone(state, zone_id, ZoneTarget::classify(zone_id).provider()).await)
        .refused(
            format!(
                "Unknown action '{action}'. Valid actions: {}.",
                accepted.join(", ")
            ),
            Refusal::invalid_parameter(
                "action",
                accepted,
                "hifi_control's action set is closed. Until #398 an unrecognised action was \
                 forwarded to the backend, which then answered about whatever it thought you \
                 meant; now it is refused here, with the whole set.",
            ),
        )
}

/// Volume, absolute or relative.
///
/// Since #398 this reaches all four zone-controlling adapters. What it refuses is a
/// zone id it cannot place, and an `hqplayer:` zone — the latter named as
/// HQPlayer's gap rather than as a nonexistent provider's limit.
async fn set_volume(
    state: &AppState,
    zone_id: &str,
    value: f64,
    relative: bool,
) -> Result<CallToolResult, CallToolError> {
    let target = ZoneTarget::classify(zone_id);
    let route = target.for_volume();
    let operation = if relative {
        "volume_relative"
    } else {
        "volume_absolute"
    };

    // OpenHome and UPnP `control` take an integer, so the value UHC sends them is
    // rounded — and `params.value` reports what was **sent**, not what was asked
    // for. That is what #395's `params` contract means ("the parameters as the
    // server resolved them"), and it is the only place a client can see that its
    // fractional value was changed.
    //
    // `round`, not the `as i32` truncation this path shipped with for one commit:
    // truncation turns `volume_up value=0.5` into a silent no-op, which is
    // precisely the class of quiet lie this issue exists to remove. Rounding
    // still loses a delta below 0.5 in absolute terms — stated rather than hidden,
    // because `params.value` will report the `0` that was sent.
    let integer_volume = value.round() as i32;
    let env = Envelope::write("hifi_control", operation).param("zone_id", zone_id);
    let env = match route {
        VolumeRoute::OpenHome | VolumeRoute::Upnp => env.param("value", integer_volume),
        // Nothing is sent on a refusal, so the client's resolved request is the
        // honest value to report.
        VolumeRoute::AppleMusic
        | VolumeRoute::Spotify
        | VolumeRoute::MusicAssistant
        | VolumeRoute::Lms
        | VolumeRoute::Roon
        | VolumeRoute::HqPlayer
        | VolumeRoute::Refused(_) => env.param("value", value),
    };

    if let VolumeRoute::Refused(refused) = route {
        return refuse_zone(state, env, zone_id, refused, Capability::Volume).await;
    }

    let result = match route {
        VolumeRoute::Lms => {
            state
                .lms
                .change_volume(zone_id, value as f32, relative)
                .await
        }
        VolumeRoute::Roon => {
            state
                .roon
                .change_volume(zone_id, value as f32, relative)
                .await
        }
        // #398. Both adapters clamp to 0-100 themselves — `vol_abs` clamps the
        // level, `vol_rel` clamps the sum — and both take an integer, which is
        // what the HTTP path has always passed them (`POST /openhome/control`
        // deserializes `value` as an integer). So this reaches the adapter
        // identically to how the web UI does.
        VolumeRoute::OpenHome => {
            state
                .openhome
                .control(zone_id, volume_action(relative), Some(integer_volume))
                .await
        }
        VolumeRoute::Upnp => {
            state
                .upnp
                .control(zone_id, volume_action(relative), Some(integer_volume))
                .await
        }
        VolumeRoute::AppleMusic | VolumeRoute::Spotify | VolumeRoute::MusicAssistant => {
            dispatch_adapter_command(
                state,
                target.label(),
                zone_id,
                if relative {
                    AdapterCommand::VolumeRelative(integer_volume)
                } else {
                    AdapterCommand::VolumeAbsolute(integer_volume)
                },
            )
            .await
        }
        VolumeRoute::HqPlayer => unreachable_refused(),
        VolumeRoute::Refused(_) => unreachable_refused(),
    };

    let env = env.scope(Scope::for_zone(state, zone_id, target.provider()).await);

    match result {
        Ok(()) => {
            // Volume is a write with a read-back, same as transport: `accepted`,
            // never `ok`. The frozen text says only "adjusted"/"set", which is
            // exactly the ambiguity #221 reported; `observed.zone.volume` is the
            // level the aggregator now holds.
            let observed = Observed::from_aggregator(state, zone_id).await;
            Ok(env.observed(observed).text_result(format!(
                "Volume {}",
                if relative { "adjusted" } else { "set" }
            )))
        }
        Err(e) => env.failed(format!("Volume error: {}", e)),
    }
}

/// The OpenHome/UPnP `control` action name for a volume write.
///
/// Both adapters spell absolute volume `vol_abs` (with `volume` as a synonym) and
/// relative `vol_rel`.
fn volume_action(relative: bool) -> &'static str {
    if relative {
        "vol_rel"
    } else {
        "vol_abs"
    }
}

fn transport_command(action: &str) -> AdapterCommand {
    match action {
        "play" => AdapterCommand::Play,
        "pause" => AdapterCommand::Pause,
        "play_pause" => AdapterCommand::PlayPause,
        "stop" => AdapterCommand::Stop,
        "next" => AdapterCommand::Next,
        "previous" => AdapterCommand::Previous,
        "repeat_off" => AdapterCommand::SetRepeat(crate::bus::RepeatMode::Off),
        "repeat_context" => AdapterCommand::SetRepeat(crate::bus::RepeatMode::All),
        "repeat_track" => AdapterCommand::SetRepeat(crate::bus::RepeatMode::One),
        "shuffle_on" => AdapterCommand::SetShuffle(true),
        "shuffle_off" => AdapterCommand::SetShuffle(false),
        _ => unreachable!("validated transport action: {action}"),
    }
}

fn mode_capability(action: &str) -> Option<Capability> {
    match action {
        "repeat_off" | "repeat_context" | "repeat_track" => Some(Capability::RepeatMode),
        "shuffle_on" | "shuffle_off" => Some(Capability::ShuffleMode),
        _ => None,
    }
}

async fn dispatch_adapter_command(
    state: &AppState,
    prefix: &str,
    zone_id: &str,
    command: AdapterCommand,
) -> anyhow::Result<()> {
    let response = state
        .adapter_registry
        .command(prefix, zone_id, command)
        .await?;
    if response.success {
        Ok(())
    } else {
        anyhow::bail!(response
            .error
            .unwrap_or_else(|| format!("{prefix} adapter rejected command")))
    }
}

/// Refuse a zone id that has no path for this capability.
///
/// Two cases, and the difference is the whole point of #398:
///
/// - **A recognised provider with nothing wired** (`hqplayer:`) is `unsupported`
///   with the classification [`crate::mcp::capabilities`] gives it, so the refusal
///   and the capability report agree by construction.
/// - **An id UHC cannot place** is `invalid`: no provider was identified, so no
///   claim about a provider can be made, and the client's zone id is what needs
///   fixing.
async fn refuse_zone(
    state: &AppState,
    env: Envelope,
    zone_id: &str,
    target: ZoneTarget,
    capability: Capability,
) -> Result<CallToolResult, CallToolError> {
    let env = env.scope(Scope::for_zone(state, zone_id, target.provider()).await);

    match target.prefix() {
        // A real provider. Ask the capability model, never restate it here.
        Some(_) => {
            let state_of = support(target, capability);
            let alternatives = TRANSPORT_ACTIONS
                .iter()
                .map(|a| format!("hifi_control action={a}"))
                .collect();
            match state_of.refusal(capability, alternatives) {
                Some(refusal) => {
                    let detail = state_of.evidence().unwrap_or_default();
                    env.refused(
                        format!(
                            "{} zones are not controllable from MCP yet: {detail} \
                             hifi_capabilities reports what each provider supports.",
                            target.label()
                        ),
                        refusal,
                    )
                }
                // Unreachable: a route only refuses where the capability model
                // records a gap, and
                // `every_supported_capability_reaches_that_providers_own_adapter`
                // proves the converse. Reported as a backend error rather than
                // invented as a provider limit.
                None => env.failed(format!(
                    "No control path for {zone_id}, though {} reports this capability as \
                     supported. This is a UHC routing bug.",
                    target.label()
                )),
            }
        }
        // Not placeable at all.
        None => env.refused(
            unplaceable_zone_text(zone_id, target),
            unplaceable_zone_refusal(target),
        ),
    }
}

/// The `Refused` arms are handled before the dispatch match, and the compiler
/// cannot see that. Returning an error rather than panicking keeps `src/lib.rs`'s
/// crate-wide `deny(clippy::panic)` intact and, if the invariant ever breaks,
/// produces a diagnosable message instead of a crashed request.
fn unreachable_refused() -> anyhow::Result<()> {
    anyhow::bail!(
        "internal routing error: a refused zone reached command dispatch. This is a UHC bug; \
         hifi_capabilities reports the intended routing."
    )
}
