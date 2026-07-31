//! Zone discovery and now-playing.
//!
//! Read-only. Both tools read from the aggregator, never from an adapter, per
//! AGENTS.md ("Adapters are dumb, Aggregator owns state").

use crate::api::AppState;
use crate::mcp::types::{error_result, json_result, now_playing_from_zone, McpZone};
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
    Ok(json_result(&mcp_zones))
}

pub async fn handle_now_playing(
    state: &AppState,
    args: HifiNowPlayingTool,
) -> Result<CallToolResult, CallToolError> {
    match state.aggregator.get_zone(&args.zone_id).await {
        Some(zone) => Ok(json_result(&now_playing_from_zone(zone))),
        // The id is echoed back so a client can tell a typo from an absent zone.
        None => error_result(format!("Zone not found: {}", args.zone_id)),
    }
}
