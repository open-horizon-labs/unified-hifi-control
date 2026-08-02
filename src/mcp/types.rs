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
//! # Result construction moved to [`crate::mcp::envelope`] (#395)
//!
//! `json_result` and `error_result` used to live here. Every tool now builds an
//! [`Envelope`](crate::mcp::envelope::Envelope) and finishes with
//! `Envelope::json_result` / `text_result` / `refused` / `failed`, which produce
//! the same text *and* attach the structured payload. [`text_result`] below
//! remains as the primitive those call.
//!
//! The declaration-order warning above is the reason `Envelope::json_result`
//! renders its text from the payload struct rather than from the `serde_json::Value`
//! it puts in `data`: a `Value::Object` is a `BTreeMap` and would serialize these
//! structs alphabetically, changing the bytes clients receive.

use rust_mcp_sdk::schema::{CallToolResult, TextContent};
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
/// `ref` (#396) is additive and optional: it is `Some` exactly when the result
/// has a durable-enough handle to mint a ref for (a Roon `item_key`, or an LMS
/// `Library`/`Url` target — never an LMS `GlobalSearchItem`, a positional
/// breadcrumb that can silently mis-resolve; see
/// `crate::adapters::lms::LmsPlayTarget`). An absent `ref` is honest: some
/// results genuinely have no safe way to be addressed later, and minting one
/// anyway would trade "no ref" for "a ref that might play the wrong thing".
/// `title`/`subtitle` are unchanged from before this issue.
#[derive(Debug, Serialize)]
pub struct McpSearchResult {
    pub title: String,
    pub subtitle: Option<String>,
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub r#ref: Option<String>,
}

/// `hifi_play`'s structured payload: the adapter's own message about what it
/// matched and started.
///
/// A named type rather than a `json!` literal so the field inventory in
/// `tests/mcp_contract.rs` can reach it by serialization. The success path needs a
/// live music library, which no mock provides, so a hand-written field list would
/// have a hole exactly where #396 is about to edit.
///
/// This is prose, not parsed data. It exists because until #396 mints opaque
/// refs, the adapter's sentence is the only record of *which* item matched — so a
/// client reading only `structuredContent` would otherwise learn nothing about it.
#[derive(Debug, Serialize)]
pub struct McpPlayResult {
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct McpHqpStatus {
    pub connected: bool,
    pub host: Option<String>,
    pub pipeline: Option<McpPipelineStatus>,
    pub options: Option<McpHqpOptions>,
    pub options_unavailable_reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct McpPipelineStatus {
    pub state: String,
    pub filter: String,
    pub shaper: String,
    pub rate: u32,
}

#[derive(Debug, Serialize)]
pub struct McpHqpSelection {
    pub current: String,
    pub choices: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct McpHqpOptions {
    pub mode: McpHqpSelection,
    pub samplerate: McpHqpSelection,
    pub filter1x: McpHqpSelection,
    #[serde(rename = "filterNx")]
    pub filter_nx: McpHqpSelection,
    pub shaper: McpHqpSelection,
    pub junk_filter: McpHqpSelection,
    pub matrix_profile: McpHqpSelection,
    pub convolution: bool,
    pub adaptive_volume: bool,
    pub repeat: String,
    pub random: bool,
}

// =============================================================================
// Result constructors
// =============================================================================

/// Plain text content: exactly one text block.
///
/// One block, always. `tests/mcp_contract.rs::result_text` treats the text a
/// client reads as the concatenation of every text block, so a second block would
/// change the response — which is why #395's structured payload rides
/// `structuredContent` instead. `every_tool_result_has_exactly_one_content_block`
/// asserts it.
///
/// A failure is still reported *to the model* as readable text rather than as a
/// protocol error, so it can read the reason and retry. The `Error: ` prefix and
/// the wording after it are contract, and
/// `Envelope::refused`/`Envelope::failed` are the only things that add it.
pub fn text_result(text: String) -> CallToolResult {
    CallToolResult::text_content(vec![TextContent::from(text)])
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
