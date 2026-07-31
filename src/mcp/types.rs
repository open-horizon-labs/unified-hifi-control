//! MCP response shapes and result constructors.
//!
//! # Field order is part of the contract
//!
//! These structs serialize in **declaration order**, and their serialized form
//! is text an MCP client reads. Reordering a field changes the payload. Add new
//! fields at the end, and add them to `FIELD_ROLES` in
//! `tests/mcp_contract.rs` — AGENTS.md forbids returning data no tool can use,
//! and that test enforces it.
//!
//! `hifi_status` deliberately does *not* have a struct here. It is built with a
//! `serde_json::json!` literal, and `serde_json` without `preserve_order`
//! serializes maps as `BTreeMap` — alphabetically. Giving it a struct would
//! switch it to declaration order and reorder the keys clients receive.
//! `tests/mcp_contract.rs::hifi_status_shape_is_pinned` asserts the ordering.
//!
//! This module is #395's (result envelope) primary target.

use rust_mcp_sdk::schema::{schema_utils::CallToolError, CallToolResult, TextContent};
use serde::Serialize;

// =============================================================================
// Response types
// =============================================================================

#[derive(Debug, Serialize)]
pub struct McpZone {
    pub zone_id: String,
    pub zone_name: String,
    pub state: String,
    pub volume: Option<f64>,
    pub is_muted: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct McpNowPlaying {
    pub zone_id: String,
    pub zone_name: String,
    pub state: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub volume: Option<f64>,
    pub is_muted: Option<bool>,
}

/// A search hit.
///
/// Note what is missing: any stable identifier. A client that finds something
/// here can only hand the title back to `hifi_play`, which re-searches and takes
/// the first match. That is #392's keystone finding and #396's job; it is
/// recorded as a known defect in `FIELD_ROLES`, not endorsed here.
#[derive(Debug, Serialize)]
pub struct McpSearchResult {
    pub title: String,
    pub subtitle: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct McpHqpStatus {
    pub connected: bool,
    pub host: Option<String>,
    pub pipeline: Option<McpPipelineStatus>,
}

#[derive(Debug, Serialize)]
pub struct McpPipelineStatus {
    pub state: String,
    pub filter: String,
    pub shaper: String,
    pub rate: u32,
}

// =============================================================================
// Result constructors
// =============================================================================

/// Plain text content.
pub fn text_result(text: String) -> CallToolResult {
    CallToolResult::text_content(vec![TextContent::from(text)])
}

/// A failure reported *to the model* as readable text rather than as a protocol
/// error, so it can read the reason and retry.
///
/// The `Error: ` prefix and the wording after it are contract: a model decides
/// what to do next by reading this string. `tests/mcp_contract.rs` pins the
/// exact text of every refusal.
pub fn error_result(msg: String) -> Result<CallToolResult, CallToolError> {
    Ok(text_result(format!("Error: {}", msg)))
}

/// Pretty-printed JSON content.
pub fn json_result<T: Serialize>(data: &T) -> CallToolResult {
    let json = serde_json::to_string_pretty(data).unwrap_or_else(|_| "{}".to_string());
    text_result(json)
}

/// Build the now-playing payload from an aggregator zone.
///
/// Shared by `hifi_now_playing` and by `hifi_control`'s post-command state
/// block, which is why it lives here rather than in either tool module.
pub fn now_playing_from_zone(zone: crate::bus::Zone) -> McpNowPlaying {
    McpNowPlaying {
        zone_id: zone.zone_id,
        zone_name: zone.zone_name,
        state: zone.state.to_string(),
        title: zone.now_playing.as_ref().map(|n| n.title.clone()),
        artist: zone.now_playing.as_ref().map(|n| n.artist.clone()),
        album: zone.now_playing.as_ref().map(|n| n.album.clone()),
        volume: zone.volume_control.as_ref().map(|v| v.value as f64),
        is_muted: zone.volume_control.as_ref().map(|v| v.is_muted),
    }
}
