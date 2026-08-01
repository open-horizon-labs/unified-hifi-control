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
//! Volume also uses a *different* routing rule from transport — see
//! [`crate::mcp::routing`]. `sonos:x` is refused for volume but routed to Roon
//! for transport. That asymmetry is deliberate today and load-bearing for #398.
//!
//! # The envelope makes both lookup tables observable, and corrects the prose
//!
//! `operation` reports the *normalized* action, so `prev` surfaces as `previous`
//! and `playpause` as `play_pause`; `params.value` reports the *resolved* delta,
//! so `volume_down` with no value surfaces as `-5.0`. Neither fact was visible to
//! a client before.
//!
//! Volume refusals are where the envelope deliberately says something the frozen
//! text does not. One string — `"Volume control not supported for this zone
//! type"` — covers two unrelated situations, and #395 requires them to be
//! distinguishable:
//!
//! | zone id | envelope | why |
//! |---|---|---|
//! | `openhome:x`, `upnp:x` | `unsupported` / `not_implemented` | the adapter implements volume and `POST /openhome/control` exposes it; only this MCP path declines to call it. A UHC gap, not a provider limit. |
//! | `sonos:x` | `invalid` / `invalid_parameter(zone_id)` | UHC never identified a provider, so it cannot claim one lacks volume. The client's zone id is the problem. |
//!
//! The second row is the one place in this issue where the envelope's `outcome`
//! contradicts the accompanying prose. That is intentional: #395 freezes the
//! prose and #398 corrects it, and until then teaching the client the right
//! lesson beats matching a misleading sentence.

use crate::api::AppState;
use crate::mcp::envelope::{Envelope, Observed, Refusal, Scope};
use crate::mcp::routing::{
    TransportRoute, VolumeRoute, ZoneTarget, ACCEPTED_ZONE_PREFIXES, TRANSPORT_ACTIONS,
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
        // Anything unrecognised is passed through for the adapter to reject, so
        // the refusal comes from whoever actually knows the action set. This is
        // why `operation` is not a closed set; #398 closes it.
        other => other,
    };

    let target = ZoneTarget::classify(&args.zone_id);
    let route = target.for_transport();
    // `operation` is the normalized backend action, so `prev` reports as
    // `previous` and `playpause` as `play_pause`.
    let env = Envelope::write("hifi_control", backend_action)
        .param("zone_id", &*args.zone_id)
        .param("action", &*args.action);

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
    };

    // One aggregator snapshot serves both `scope.zone_name` and `observed`, so
    // the two cannot describe different moments. It is taken after the command,
    // which is what makes it a read-back rather than a guess — `set_volume` does
    // the same for the same reason, at the cost of a second read there because
    // its refusal path needs a scope before any command runs.
    let observed = Observed::from_aggregator(state, &args.zone_id).await;
    let scope = Scope {
        // Identification, not route: `sonos:x` reports `unknown` even though the
        // call above went to Roon. See ZoneTarget::provider.
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

/// Volume, absolute or relative.
///
/// Refuses zone types with no volume control rather than defaulting them to
/// Roon the way transport does. See this module's docs for why the two refusal
/// cases get different envelopes behind one frozen string.
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

    // `value` is the *resolved* level or delta: the defaulted 5 and the negation
    // applied by `volume_down` are both visible here and nowhere else.
    let env = Envelope::write("hifi_control", operation)
        .param("zone_id", zone_id)
        .param("value", value);

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
        VolumeRoute::Unsupported => {
            let env = env.scope(Scope::for_zone(state, zone_id, target.provider()).await);
            let text = "Volume control not supported for this zone type";

            return match target {
                // The adapter implements volume and HTTP exposes it; only this
                // path does not call it. Saying "the provider cannot" here would
                // be exactly the mislabelling #392 rule 3 forbids.
                ZoneTarget::OpenHome | ZoneTarget::Upnp => env.refused(
                    text,
                    Refusal::NotImplemented {
                        operation: operation.to_string(),
                        tracked_by: "#398",
                        alternatives: TRANSPORT_ACTIONS
                            .iter()
                            .map(|a| format!("hifi_control action={a}"))
                            .collect(),
                        detail: format!(
                            "UHC's MCP volume path does not call the {} adapter. The adapter \
                             itself implements volume and POST /{}/control exposes it, so this \
                             is a UHC gap, not a provider limitation.",
                            provider_label(target),
                            provider_label(target),
                        ),
                    },
                ),
                // No provider was ever identified, so no claim about a provider
                // can be made. The zone id is the problem.
                _ => env.refused(
                    text,
                    Refusal::invalid_parameter(
                        "zone_id",
                        ACCEPTED_ZONE_PREFIXES,
                        "This zone id's prefix names no adapter, so UHC cannot say whether \
                         volume is available. Call hifi_zones for valid ids.",
                    ),
                ),
            };
        }
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

/// The lowercase provider label used in refusal detail and HTTP route names.
fn provider_label(target: ZoneTarget) -> &'static str {
    match target {
        ZoneTarget::OpenHome => "openhome",
        ZoneTarget::Upnp => "upnp",
        ZoneTarget::Lms => "lms",
        ZoneTarget::Roon => "roon",
        ZoneTarget::Unknown => "unknown",
    }
}
