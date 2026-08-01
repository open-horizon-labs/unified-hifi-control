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

use crate::api::AppState;
use crate::mcp::capabilities::{support, Capability};
use crate::mcp::envelope::{Envelope, Observed, Provider, Refusal, Scope};
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
    description = "Control playback: play, pause, playpause (toggle), next, previous, or adjust volume"
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiControlTool {
    /// The zone ID to control
    pub zone_id: String,
    /// Action: play, pause, playpause, next, previous, volume_set, volume_up, volume_down
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
    // A direct HQPlayer zone is dispatched through its own function
    // (`crate::knobs::routes::dispatch_hqplayer_action`, the same core the
    // `/knob` HTTP surface uses) *before* the generic action gate below, not
    // through [`TransportRoute`]/[`VolumeRoute`]. Its action vocabulary (stop,
    // seek, mute), its decimal-dB volume, and its zone-state-dependent refusals
    // (next/previous/play_pause resolved against the last published state) do
    // not fit the generic 0-100/four-adapter grid those routes model — and that
    // grid stays [`TransportRoute::Refused`]/[`VolumeRoute::Refused`] for
    // HQPlayer on purpose, so it still reports the honest "not this path" for
    // every other caller. #401/#406.
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
                    "volume_set requires a value (0-100)",
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

/// Direct HQPlayer transport, skip and volume, dispatched through
/// [`crate::knobs::routes::dispatch_hqplayer_action`] rather than reimplemented
/// here — the same capability checks, clamps and command core the `/knob`
/// HTTP surface uses. #401 found that MCP had its own zone-id switch,
/// independent of `knob_control_handler`, with the same "falls through to
/// Roon" defect #391 fixed there; #406 closed it by routing here instead of
/// widening [`TransportRoute`]/[`VolumeRoute`], whose vocabulary (0-100,
/// integer, four fixed adapters) HQPlayer does not share.
///
/// Every action the dispatch core accepts is forwarded verbatim except
/// `volume_set`, which is MCP's own spelling for an absolute write; the
/// translation happens at this boundary rather than widening the shared
/// core's vocabulary. An action the core does not recognise is refused by the
/// core itself (`HqpDispatchError::BadRequest` with `code: "UNKNOWN_ACTION"`),
/// not pre-filtered here — mirroring #406's own shape rather than duplicating
/// its validation in a second list that could drift from the first.
async fn handle_hqplayer_control(
    state: &AppState,
    args: HifiControlTool,
) -> Result<CallToolResult, CallToolError> {
    let instance = args.zone_id.trim_start_matches("hqplayer:");
    let dispatch_action = match args.action.as_str() {
        "volume_set" => "volume",
        other => other,
    };
    let operation = match args.action.as_str() {
        "volume_set" => "volume_absolute",
        "volume_up" | "volume_down" => "volume_relative",
        "playpause" => "play_pause",
        "prev" => "previous",
        other => other,
    };

    let env = Envelope::write("hifi_control", operation)
        .param("zone_id", &*args.zone_id)
        .param("action", &*args.action);
    let env = env.param_opt("value", args.value);

    match crate::knobs::routes::dispatch_hqplayer_action(
        state,
        &args.zone_id,
        instance,
        dispatch_action,
        args.value,
    )
    .await
    {
        Ok(()) => {
            // Read back from the aggregator, exactly like every other zone's
            // write — but never called "Current state": the adapter's
            // transport commands are fire-and-forget and the aggregator is
            // refreshed only by the producer's status poll (2s by default), so
            // this read is the poll from *before* the command, every time.
            // Calling it current told an assistant its own pause had not taken
            // effect. The gap cannot be closed here — a surface may not read
            // the adapter for state (`docs/ARCHITECTURE.md`,
            // `tests/architecture_lint.rs`) — so the claim is dropped rather
            // than a fresher observation invented.
            let observed = Observed::from_aggregator(state, &args.zone_id).await;
            let scope = Scope {
                provider: Provider::HqPlayer,
                zone_id: Some(args.zone_id.clone()),
                zone_name: observed.as_ref().map(|o| o.zone.zone_name.clone()),
            };
            let env = env.scope(scope);
            match observed {
                Some(observed) => {
                    let json = serde_json::to_string_pretty(&observed.zone)
                        .unwrap_or_else(|_| "{}".to_string());
                    let text = format!(
                        "Action '{}' executed.\n\nZone state as last observed *before* this \
                         command (a direct HQPlayer zone refreshes on its poll interval; call \
                         hifi_now_playing for the state after it):\n{}",
                        args.action, json
                    );
                    Ok(env.observed(Some(observed)).text_result(text))
                }
                None => Ok(env.text_result(format!("Action '{}' executed.", args.action))),
            }
        }
        Err(err) => {
            let env = env.scope(Scope::for_zone(state, &args.zone_id, Provider::HqPlayer).await);
            match err {
                // The instance name, or the zone the aggregator currently
                // publishes for it, does not exist right now — the client's
                // remedy is to re-discover, not to resend the same id.
                crate::knobs::routes::HqpDispatchError::NotFound(message) => env.refused(
                    message.clone(),
                    Refusal::UnknownTarget {
                        parameter: "zone_id",
                        discover_with: "hifi_zones",
                        detail: message,
                    },
                ),
                crate::knobs::routes::HqpDispatchError::BadRequest { message, code } => env
                    .refused(
                        message.clone(),
                        Refusal::InvalidParameter {
                            parameter: hqplayer_bad_request_parameter(code),
                            accepted: vec![],
                            detail: message,
                        },
                    ),
                crate::knobs::routes::HqpDispatchError::Backend(message) => {
                    env.failed(format!("Control error: {}", message))
                }
            }
        }
    }
}

/// Which `hifi_control` parameter a [`crate::knobs::routes::HqpDispatchError::BadRequest`]
/// code is about, for the refusal's `parameter` field.
fn hqplayer_bad_request_parameter(code: &'static str) -> &'static str {
    match code {
        "UNKNOWN_ACTION" | "ACTION_NOT_ALLOWED" | "STATE_UNKNOWN" => "action",
        _ => "value",
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
    Envelope::write("hifi_control", "unknown_action")
        .param("zone_id", zone_id)
        .param("action", action)
        .scope(Scope::for_zone(state, zone_id, ZoneTarget::classify(zone_id).provider()).await)
        .refused(
            format!(
                "Unknown action '{action}'. Valid actions: {}.",
                CONTROL_ACTIONS.join(", ")
            ),
            Refusal::invalid_parameter(
                "action",
                CONTROL_ACTIONS,
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
        VolumeRoute::Lms | VolumeRoute::Roon | VolumeRoute::Refused(_) => env.param("value", value),
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
