//! HQPlayer pipeline inspection and control.
//!
//! These four tools are advertised only when the HQPlayer adapter is enabled in
//! settings — see [`crate::mcp::tools::list_tools`].
//!
//! # The setting alias table is contract
//!
//! [`handle_set_pipeline`] accepts several spellings per setting
//! (`filter1x`/`filter_1x`, `filterNx`/`filter_nx`/`filternx`, `shaper`/`dither`,
//! `rate`/`samplerate`). None of that appears in `tools/list`, so dropping an
//! alias would turn a documented input into "Unknown setting" with a green
//! snapshot suite. `tests/mcp_contract.rs::hqplayer_set_pipeline_alias_table_is_pinned`
//! checks every alias individually.

use crate::api::AppState;
use crate::mcp::types::{error_result, json_result, text_result, McpHqpStatus, McpPipelineStatus};
use rust_mcp_sdk::{
    macros::{mcp_tool, JsonSchema},
    schema::{schema_utils::CallToolError, CallToolResult},
};
use serde::{Deserialize, Serialize};

/// Get HQPlayer status
#[mcp_tool(
    name = "hifi_hqplayer_status",
    description = "Get HQPlayer Embedded status and current pipeline settings",
    read_only_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiHqplayerStatusTool {}

/// List HQPlayer profiles
#[mcp_tool(
    name = "hifi_hqplayer_profiles",
    description = "List available HQPlayer Embedded configurations",
    read_only_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiHqplayerProfilesTool {}

/// Load an HQPlayer profile
#[mcp_tool(
    name = "hifi_hqplayer_load_profile",
    description = "Load an HQPlayer Embedded configuration (will restart HQPlayer)",
    destructive_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiHqplayerLoadProfileTool {
    /// Configuration name to load (get from hifi_hqplayer_profiles)
    pub profile: String,
}

/// Change an HQPlayer pipeline setting
#[mcp_tool(
    name = "hifi_hqplayer_set_pipeline",
    description = "Change an HQPlayer pipeline setting (mode, samplerate, filter1x, filterNx, shaper, dither)"
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiHqplayerSetPipelineTool {
    /// Setting to change: mode, samplerate, filter1x, filterNx, shaper, dither
    pub setting: String,
    /// New value for the setting
    pub value: String,
}

/// The setting names this tool advertises, used in the refusal message.
///
/// Kept next to the alias match below so the two stay in step.
const VALID_SETTINGS: &str = "mode, samplerate, filter1x, filterNx, shaper, dither";

pub async fn handle_status(state: &AppState) -> Result<CallToolResult, CallToolError> {
    let status = state.hqplayer.get_status().await;
    let pipeline = state.hqplayer.get_pipeline_status().await.ok();

    let mcp_status = McpHqpStatus {
        connected: status.connected,
        host: status.host,
        pipeline: pipeline.map(|p| McpPipelineStatus {
            state: p.status.state,
            filter: p.status.active_filter,
            shaper: p.status.active_shaper,
            rate: p.status.active_rate,
        }),
    };
    Ok(json_result(&mcp_status))
}

pub async fn handle_profiles(state: &AppState) -> Result<CallToolResult, CallToolError> {
    let profiles = state.hqplayer.get_cached_profiles().await;
    let profile_names: Vec<String> = profiles.into_iter().map(|p| p.title).collect();
    Ok(json_result(&profile_names))
}

pub async fn handle_load_profile(
    state: &AppState,
    args: HifiHqplayerLoadProfileTool,
) -> Result<CallToolResult, CallToolError> {
    match state.hqplayer.load_profile(&args.profile).await {
        Ok(()) => Ok(text_result(format!("Loaded profile: {}", args.profile))),
        Err(e) => error_result(format!("Failed to load profile: {}", e)),
    }
}

pub async fn handle_set_pipeline(
    state: &AppState,
    args: HifiHqplayerSetPipelineTool,
) -> Result<CallToolResult, CallToolError> {
    // All settings use name-based lookups; the adapter handles the conversion.
    // Only samplerate needs numeric parsing (an Hz value).
    let result = match args.setting.as_str() {
        // Accepts a name like "PCM", "DSD", "[source]".
        "mode" => state.hqplayer.set_mode(&args.value).await,
        "filter1x" | "filter_1x" => state.hqplayer.set_filter_1x(&args.value).await,
        "filterNx" | "filter_nx" | "filternx" => state.hqplayer.set_filter_nx(&args.value).await,
        "shaper" | "dither" => state.hqplayer.set_shaper(&args.value).await,
        "rate" | "samplerate" => {
            // Samplerate uses an Hz value, e.g. "48000", "96000".
            if let Ok(v) = args.value.parse::<u32>() {
                state.hqplayer.set_rate(v).await
            } else {
                return error_result("Invalid rate value (expected Hz like 48000, 96000)".into());
            }
        }
        _ => {
            return error_result(format!(
                "Unknown setting: {}. Valid: {}",
                args.setting, VALID_SETTINGS
            ));
        }
    };

    match result {
        Ok(()) => Ok(text_result(format!(
            "Set {} to {}",
            args.setting, args.value
        ))),
        Err(e) => error_result(format!("Failed to set {}: {}", args.setting, e)),
    }
}
