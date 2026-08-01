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

use crate::api::AppState;
use crate::mcp::routing::{TransportRoute, VolumeRoute, ZoneTarget};
use crate::mcp::types::{error_result, now_playing_from_zone, text_result};
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
            return error_result("volume_set requires a value (0-100)".into());
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
        // the refusal comes from whoever actually knows the action set.
        other => other,
    };

    let result = match ZoneTarget::classify(&args.zone_id).for_transport() {
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

    match result {
        Ok(()) => {
            // Report the observed state back, so the model does not have to
            // follow every command with hifi_now_playing.
            if let Some(zone) = state.aggregator.get_zone(&args.zone_id).await {
                let np = now_playing_from_zone(zone);
                let json = serde_json::to_string_pretty(&np).unwrap_or_else(|_| "{}".to_string());
                Ok(text_result(format!(
                    "Action '{}' executed.\n\nCurrent state:\n{}",
                    args.action, json
                )))
            } else {
                Ok(text_result(format!("Action '{}' executed.", args.action)))
            }
        }
        Err(e) => error_result(format!("Control error: {}", e)),
    }
}

/// Volume, absolute or relative.
///
/// Refuses zone types with no volume control rather than defaulting them to
/// Roon the way transport does.
async fn set_volume(
    state: &AppState,
    zone_id: &str,
    value: f64,
    relative: bool,
) -> Result<CallToolResult, CallToolError> {
    let result = match ZoneTarget::classify(zone_id).for_volume() {
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
            return error_result("Volume control not supported for this zone type".into());
        }
    };

    match result {
        Ok(()) => Ok(text_result(format!(
            "Volume {}",
            if relative { "adjusted" } else { "set" }
        ))),
        Err(e) => error_result(format!("Volume error: {}", e)),
    }
}
