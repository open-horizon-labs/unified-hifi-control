//! Zone discovery and now-playing.
//!
//! Read-only. Both tools read from the aggregator, never from an adapter, per
//! AGENTS.md ("Adapters are dumb, Aggregator owns state").
//!
//! Both report [`Outcome::Ok`](crate::mcp::envelope::Outcome::Ok) on success and
//! carry no `observed` block: for a read, the payload *is* the observation, and
//! duplicating it into `observed` would give a client two places to look that
//! could disagree.
//!
//! # `outcome: ok` on an empty zone list is a weaker claim than it looks
//!
//! With no adapter connected, `hifi_zones` returns `[]` and the envelope says
//! `ok`. A model may read that as "confirmed: this system has no zones" when the
//! truth is "nothing is connected yet". Today's bare `[]` is equally ambiguous,
//! so this is not a regression — but the envelope turns silence into an explicit
//! positive claim. `hifi_status` is the tool that answers "is anything
//! connected", and #398 has the per-zone capability data to qualify this
//! properly. Recorded rather than papered over with an invented field.

use crate::api::AppState;
use crate::mcp::envelope::{Envelope, Refusal, Scope};
use crate::mcp::types::{now_playing_from_zone, McpZone};
use rust_mcp_sdk::{
    macros::{mcp_tool, JsonSchema},
    schema::{schema_utils::CallToolError, CallToolResult},
};
use serde::{Deserialize, Serialize};

/// List all available playback zones
#[mcp_tool(
    name = "hifi_zones",
    description = "List all available playback zones (Roon, LMS, OpenHome, UPnP)",
    read_only_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiZonesTool {}

/// Get current playback state for a zone
#[mcp_tool(
    name = "hifi_now_playing",
    description = "Get current playback state for a zone (track, artist, album, play state, volume)",
    read_only_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiNowPlayingTool {
    /// The zone ID to query (get from hifi_zones)
    pub zone_id: String,
}

pub async fn handle_zones(state: &AppState) -> Result<CallToolResult, CallToolError> {
    let zones = state.aggregator.get_zones().await;
    let mcp_zones: Vec<McpZone> = zones
        .into_iter()
        .map(|z| McpZone {
            zone_id: z.zone_id,
            zone_name: z.zone_name,
            state: z.state.to_string(),
            volume: z.volume_control.as_ref().map(|v| v.value as f64),
            is_muted: z.volume_control.as_ref().map(|v| v.is_muted),
        })
        .collect();

    // No scope: this tool spans every provider, so naming one would be a lie.
    Ok(Envelope::read("hifi_zones", "list_zones").json_result(&mcp_zones))
}

pub async fn handle_now_playing(
    state: &AppState,
    args: HifiNowPlayingTool,
) -> Result<CallToolResult, CallToolError> {
    let target = crate::mcp::routing::ZoneTarget::classify(&args.zone_id);
    let env = Envelope::read("hifi_now_playing", "get_now_playing").param("zone_id", &*args.zone_id);

    match state.aggregator.get_zone(&args.zone_id).await {
        Some(zone) => {
            let scope = Scope {
                provider: target.provider(),
                zone_id: Some(args.zone_id.clone()),
                zone_name: Some(zone.zone_name.clone()),
            };
            Ok(env
                .scope(scope)
                .json_result(&now_playing_from_zone(zone)))
        }
        // The id is echoed back so a client can tell a typo from an absent zone.
        // The envelope goes further and names the tool that lists valid ids,
        // which is the difference between a model correcting itself and retrying.
        None => {
            let detail = format!(
                "No zone with id {:?} is known to the aggregator. Call hifi_zones \
                 for the current list.",
                args.zone_id
            );
            env.scope(Scope {
                provider: target.provider(),
                zone_id: Some(args.zone_id.clone()),
                zone_name: None,
            })
            .refused(
                format!("Zone not found: {}", args.zone_id),
                Refusal::UnknownTarget {
                    parameter: "zone_id",
                    discover_with: "hifi_zones",
                    detail,
                },
            )
        }
    }
}
