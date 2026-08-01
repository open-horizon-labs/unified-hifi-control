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
//!   transport and volume, instead of being sent to Roon. An `hqplayer:` zone was
//!   refused this way too, tracked by #328 — #328 itself now wires it instead,
//!   through [`handle_hqplayer_control`], its own dispatch below.
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
use crate::bus::PlaybackState;
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
    /// For volume actions: 0-100 for normalized providers, or decimal dB for HQPlayer zones; relative amounts use the provider's corresponding scale.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
}

/// Default step for `volume_up` / `volume_down` when no value is supplied.
const DEFAULT_VOLUME_DELTA: f64 = 5.0;

pub async fn handle_control(
    state: &AppState,
    args: HifiControlTool,
) -> Result<CallToolResult, CallToolError> {
    // A direct HQPlayer zone is resolved through `HqpInstanceManager`, not the
    // single shared adapter the other four providers use, and it accepts a wider
    // action vocabulary (stop/seek/mute, #328) than `CONTROL_ACTIONS` — so it is
    // its own dispatch, branched before the closed-set check below rather than
    // folded into it. `ZoneTarget::classify` is pure and does no I/O, so checking
    // it first costs nothing on the other four providers' path.
    if ZoneTarget::classify(&args.zone_id) == ZoneTarget::HqPlayer {
        return handle_hqplayer_control(state, args).await;
    }

    // The action is checked first, and before the zone is classified. It is a
    // closed set with no I/O behind it, so this is the cheapest fault to name —
    // and naming it does not depend on the zone id being right.
    if !CONTROL_ACTIONS.contains(&args.action.as_str()) {
        return unknown_action(state, &args.zone_id, &args.action, CONTROL_ACTIONS).await;
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
                        &["0-100", "HQPlayer dB"],
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
        _ => return unknown_action(state, &args.zone_id, &args.action, CONTROL_ACTIONS).await,
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
        // `handle_control` returns through `handle_hqplayer_control` for every
        // `ZoneTarget::HqPlayer` zone id before this match is ever reached.
        TransportRoute::HqPlayer => unreachable_refused(),
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
///
/// A superset of [`CONTROL_ACTIONS`]. `stop`, `seek` and `mute` are additions
/// #328 wires for HQPlayer specifically: `HqpAdapter` has a working call for
/// each (`stop`, `seek`, `volume_mute` — `src/adapters/hqplayer.rs`), and no
/// other adapter's `hifi_control` path exposes seek at all, so adding it to the
/// shared list would offer four providers an action that does nothing.
const HQPLAYER_CONTROL_ACTIONS: &[&str] = &[
    "play",
    "pause",
    "playpause",
    "next",
    "previous",
    "prev",
    "stop",
    "seek",
    "mute",
    "volume_set",
    "volume_up",
    "volume_down",
];

/// `hifi_control` for a direct HQPlayer zone (`hqplayer:<instance_name>`).
///
/// Its own dispatch rather than a branch inside the shared match above, for two
/// reasons #328 could not fold into it:
///
/// - The zone id names an `HqpInstanceManager` instance, not one of the four
///   providers' single shared adapter — resolution is a name lookup, not a
///   route.
/// - The action vocabulary is wider ([`HQPLAYER_CONTROL_ACTIONS`]), and volume
///   is decimal-dB clamped to the zone's own observed range rather than an
///   integer clamped to 0-100.
///
/// # Never reports a pre-command poll as current state
///
/// A direct HQPlayer zone is refreshed only by the producer's status poll (2 s
/// by default); every write below is fire-and-forget. Reading
/// `state.aggregator` immediately afterwards and calling it "Current state"
/// would tell a client the daemon they just told to pause is still playing —
/// the poll from before the command, presented as if it were after. So, unlike
/// [`handle_control`] above, this never calls [`Observed::from_aggregator`]
/// after a write and never renders a "Current state" block; #397 already
/// established the same constraint for the `hifi_hqplayer_*` tools
/// (`src/mcp/tools/hqplayer.rs`), and this follows it for the same reason. It
/// does read `state.aggregator` *before* dispatching `playpause`/`volume_up`/
/// `volume_down` — as the input those actions resolve against, never as a claim
/// about what happened afterwards.
pub(crate) async fn handle_hqplayer_control(
    state: &AppState,
    args: HifiControlTool,
) -> Result<CallToolResult, CallToolError> {
    if !HQPLAYER_CONTROL_ACTIONS.contains(&args.action.as_str()) {
        return unknown_action(state, &args.zone_id, &args.action, HQPLAYER_CONTROL_ACTIONS).await;
    }

    let env = Envelope::write("hifi_control", hqplayer_operation(&args.action))
        .param("zone_id", &*args.zone_id)
        .param("action", &*args.action)
        .scope(Scope::for_zone(state, &args.zone_id, Provider::HqPlayer).await);

    let instance_name = args
        .zone_id
        .strip_prefix("hqplayer:")
        .unwrap_or(&args.zone_id);
    let Some(adapter) = state.hqp_instances.get(instance_name).await else {
        return env.failed(format!(
            "HQPlayer instance '{instance_name}' is not configured"
        ));
    };

    match args.action.as_str() {
        "play" => hqplayer_result(env, &args.action, adapter.play().await),
        "pause" => hqplayer_result(env, &args.action, adapter.pause().await),
        "stop" => hqplayer_result(env, &args.action, adapter.stop().await),
        "next" => hqplayer_result(env, &args.action, adapter.next().await),
        "previous" | "prev" => hqplayer_result(env, &args.action, adapter.previous().await),
        "mute" => hqplayer_result(env, &args.action, adapter.volume_mute().await),
        // No native toggle on the wire (only discrete Play/Pause/Stop), so the
        // toggle is resolved here against the last state the aggregator
        // published — the same "most recently observed" state every other
        // adapter's own `play_pause` resolves against internally.
        "playpause" => {
            let playing = state
                .aggregator
                .get_zone(&args.zone_id)
                .await
                .map(|zone| zone.state == PlaybackState::Playing)
                .unwrap_or(false);
            let result = if playing {
                adapter.pause().await
            } else {
                adapter.play().await
            };
            hqplayer_result(env, &args.action, result)
        }
        "seek" => {
            let Some(value) = args.value else {
                return env.refused(
                    "seek requires a value (position in seconds)",
                    Refusal::invalid_parameter(
                        "value",
                        &["a position in seconds, e.g. 30"],
                        "action='seek' has no default position and none may be invented.",
                    ),
                );
            };
            let env = env.param("value", value);
            // The wire position is an unsigned integer; a negative or fractional
            // request is rounded rather than rejected, since a client computing
            // "10 seconds back" from track start can legitimately land on 0.
            let position = value.max(0.0).round() as u32;
            hqplayer_result(env, &args.action, adapter.seek(position).await)
        }
        "volume_set" => {
            let Some(value) = args.value else {
                return env.refused(
                    "volume_set requires a value (a level in dB)",
                    Refusal::invalid_parameter(
                        "value",
                        &["a decimal dB level, e.g. -20.0"],
                        "action='volume_set' sets an absolute level and has no default. Use \
                         volume_up or volume_down for a relative change.",
                    ),
                );
            };
            let sent = clamp_to_observed_range(state, &args.zone_id, value).await;
            let env = env.param("value", sent);
            hqplayer_result(env, &args.action, adapter.set_volume_db(sent).await)
        }
        "volume_up" | "volume_down" => {
            let Some(control) = state
                .aggregator
                .get_zone(&args.zone_id)
                .await
                .and_then(|zone| zone.volume_control)
            else {
                return env.failed(format!(
                    "Cannot adjust volume for '{}': no level has been observed yet for this zone",
                    args.zone_id
                ));
            };
            // HqpAdapter's own volume_up/volume_down send a fixed device-side
            // step with no value ("VolumeUp"/"VolumeDown" on the wire) — not the
            // arbitrary decimal delta this action documents. So a relative move
            // is computed here instead: last observed level plus the resolved
            // delta, clamped to the zone's own range, sent as one `set_volume_db`
            // — decimal-dB throughout, unlike OpenHome/UPnP's integer rounding.
            let magnitude = args.value.unwrap_or(DEFAULT_VOLUME_DELTA);
            let delta = if args.action == "volume_up" {
                magnitude
            } else {
                -magnitude
            };
            let env = env.param("value", delta);
            let sent = (f64::from(control.value) + delta)
                .clamp(f64::from(control.min), f64::from(control.max));
            hqplayer_result(env, &args.action, adapter.set_volume_db(sent).await)
        }
        // Unreachable: HQPLAYER_CONTROL_ACTIONS gates this whole match, one arm
        // per entry.
        _ => unknown_action(state, &args.zone_id, &args.action, HQPLAYER_CONTROL_ACTIONS).await,
    }
}

/// The envelope's `operation` for a direct HQPlayer action — the normalized name
/// of what was asked for, same convention as `handle_control`'s `backend_action`.
fn hqplayer_operation(action: &str) -> &'static str {
    match action {
        "play" => "play",
        "pause" => "pause",
        "stop" => "stop",
        "next" => "next",
        "previous" | "prev" => "previous",
        "playpause" => "play_pause",
        "seek" => "seek",
        "mute" => "mute",
        "volume_set" => "volume_absolute",
        "volume_up" | "volume_down" => "volume_relative",
        // Unreachable: every `HQPLAYER_CONTROL_ACTIONS` entry has an arm above.
        _ => "unknown_action",
    }
}

/// Finish a direct HQPlayer command: `accepted` on success, a backend error
/// naming the daemon's own failure otherwise. Never `observed` — see
/// [`handle_hqplayer_control`]'s doc comment.
fn hqplayer_result(
    env: Envelope,
    action: &str,
    result: anyhow::Result<()>,
) -> Result<CallToolResult, CallToolError> {
    match result {
        Ok(()) => Ok(env.text_result(format!("Action '{action}' executed."))),
        Err(e) => env.failed(format!("Control error: {}", e)),
    }
}

/// Clamp a requested absolute dB level to the zone's own observed range, or
/// pass it through unclamped if the aggregator holds no range yet — an absent
/// range is not evidence the daemon has none, only that nothing has been
/// observed to report.
async fn clamp_to_observed_range(state: &AppState, zone_id: &str, value: f64) -> f64 {
    match state
        .aggregator
        .get_zone(zone_id)
        .await
        .and_then(|zone| zone.volume_control)
    {
        Some(control) => value.clamp(f64::from(control.min), f64::from(control.max)),
        None => value,
    }
}

/// Refuse an action outside the accepted set.
///
/// `operation` is the literal `"unknown_action"` rather than the string the client
/// sent. Before #398, `other => other` put the unrecognised action straight into
/// `operation`, which made that field an open set and let an envelope report a verb
/// UHC does not have as though it were one.
///
/// `accepted` is a parameter rather than always [`CONTROL_ACTIONS`] because a
/// direct HQPlayer zone accepts a wider vocabulary (`stop`/`seek`/`mute`, #328) —
/// see [`HQPLAYER_CONTROL_ACTIONS`] — and the refusal should name the set that
/// actually applies to this zone, not the other four providers' set.
async fn unknown_action(
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

/// Volume, absolute or relative, for the four zone-controlling adapters #398
/// wired here. A direct HQPlayer zone's volume goes through
/// [`handle_hqplayer_control`] instead — decimal-dB and clamped to the zone's
/// observed range, neither of which this integer-rounding path does.
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
        VolumeRoute::Lms | VolumeRoute::Roon | VolumeRoute::Refused(_) | VolumeRoute::HqPlayer => {
            env.param("value", value)
        }
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
        // `handle_control` returns through `handle_hqplayer_control` for every
        // `ZoneTarget::HqPlayer` zone id before this function is ever called.
        VolumeRoute::HqPlayer => unreachable_refused(),
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
