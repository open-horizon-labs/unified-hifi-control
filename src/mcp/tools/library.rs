//! Library search and playback.
//!
//! Both tools route on `lms:` only; everything else, including an absent
//! `zone_id`, goes to Roon (see [`crate::mcp::routing`]).
//!
//! This module is the primary target for #396 (opaque content references) and
//! #399 (hierarchical browse). Note what `McpSearchResult` lacks: any stable
//! identifier. A client that finds something here can only hand the title back
//! to `hifi_play`, which re-searches and takes the first match. That is #392's
//! keystone finding — recorded in `FIELD_ROLES` in `tests/mcp_contract.rs` as a
//! known defect, and out of scope for #394.

use crate::api::AppState;
use crate::mcp::routing::{LibraryRoute, ZoneTarget};
use crate::mcp::types::{error_result, json_result, text_result, McpSearchResult};
use rust_mcp_sdk::{
    macros::{mcp_tool, JsonSchema},
    schema::{schema_utils::CallToolError, CallToolResult},
};
use serde::{Deserialize, Serialize};

/// How many results to request from LMS.
///
/// Kept separate from [`ROON_SEARCH_LIMIT`] even though both are 10 today. They
/// were two independent literals before #394, and a refactor mandated to change
/// no behavior should not quietly couple them: the two backends page and rank
/// differently, so a future tuning of one is not a decision about the other.
const LMS_SEARCH_LIMIT: usize = 10;

/// How many results to request from Roon.
const ROON_SEARCH_LIMIT: usize = 10;

/// Search for music
#[mcp_tool(
    name = "hifi_search",
    description = "Search for tracks, albums, or artists. Roon: searches Library, TIDAL, or Qobuz (use source param). LMS: searches all installed providers including streaming plugins (zone_id recommended as different players may have different sources configured).",
    read_only_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiSearchTool {
    /// Search query (e.g., "Hotel California", "Eagles", "jazz piano")
    pub query: String,
    /// Zone ID for context-aware results. Recommended for LMS (different players may have different sources).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone_id: Option<String>,
    /// Where to search: "library" (default), "tidal", or "qobuz". Roon only; LMS searches all providers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Search and play music in one command
#[mcp_tool(
    name = "hifi_play",
    description = "Search and play music. Searches and plays, queues, or starts radio from the first matching result. Use action='queue' to add to queue. action='radio' and source param are Roon-only; LMS searches all providers."
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiPlayTool {
    /// What to play (e.g., "early Michael Jackson", "Dark Side of the Moon")
    pub query: String,
    /// Zone ID to play on (get from hifi_zones)
    pub zone_id: String,
    /// Where to search: "library" (default), "tidal", or "qobuz". Roon only; LMS searches all providers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// What to do: "play" (default), "queue", or "radio". radio is Roon-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

/// Parse the `source` parameter into Roon's search source.
///
/// Anything unrecognised falls back to Library, matching the documented default.
fn roon_search_source(source: Option<&str>) -> crate::adapters::roon::SearchSource {
    use crate::adapters::roon::SearchSource;
    match source {
        Some("tidal") => SearchSource::Tidal,
        Some("qobuz") => SearchSource::Qobuz,
        _ => SearchSource::Library,
    }
}

pub async fn handle_search(
    state: &AppState,
    args: HifiSearchTool,
) -> Result<CallToolResult, CallToolError> {
    match ZoneTarget::classify_opt(args.zone_id.as_deref()).for_library() {
        LibraryRoute::Lms => {
            // LMS uses globalsearch, which covers every installed provider
            // (library, TIDAL, Qobuz, ...), so `source` does not apply.
            match state
                .lms
                .search(&args.query, args.zone_id.as_deref(), Some(LMS_SEARCH_LIMIT))
                .await
            {
                Ok(results) => {
                    let mcp_results: Vec<McpSearchResult> = results
                        .into_iter()
                        .map(|item| McpSearchResult {
                            subtitle: lms_subtitle(&item),
                            title: item.title,
                        })
                        .collect();
                    Ok(json_result(&mcp_results))
                }
                Err(e) => error_result(format!("Search error: {}", e)),
            }
        }
        LibraryRoute::Roon => {
            match state
                .roon
                .search(
                    &args.query,
                    args.zone_id.as_deref(),
                    Some(ROON_SEARCH_LIMIT),
                    roon_search_source(args.source.as_deref()),
                )
                .await
            {
                Ok(results) => {
                    let mcp_results: Vec<McpSearchResult> = results
                        .into_iter()
                        .map(|item| McpSearchResult {
                            title: item.title,
                            subtitle: item.subtitle,
                        })
                        .collect();
                    Ok(json_result(&mcp_results))
                }
                Err(e) => error_result(format!("Search error: {}", e)),
            }
        }
    }
}

/// Render an LMS search hit's subtitle.
///
/// LMS returns typed results with separate artist/album fields; the subtitle is
/// the human-readable rendering of that type.
fn lms_subtitle(item: &crate::adapters::lms::LmsSearchResult) -> Option<String> {
    use crate::adapters::lms::LmsSearchResultType;
    match item.result_type {
        LmsSearchResultType::Album => item.artist.as_ref().map(|a| format!("Album by {}", a)),
        LmsSearchResultType::Artist => Some("Artist".to_string()),
        LmsSearchResultType::Track => match (&item.artist, &item.album) {
            (Some(a), Some(al)) => Some(format!("{} - {}", a, al)),
            (Some(a), None) => Some(a.clone()),
            _ => None,
        },
    }
}

pub async fn handle_play(
    state: &AppState,
    args: HifiPlayTool,
) -> Result<CallToolResult, CallToolError> {
    match ZoneTarget::classify(&args.zone_id).for_library() {
        LibraryRoute::Lms => {
            use crate::adapters::lms::LmsPlayAction;

            // LMS ignores `source` (library only) and has no radio mode. The
            // refusal names the supported actions so the model can retry.
            if args.action.as_deref() == Some("radio") {
                return error_result(
                    "Radio mode not supported for LMS. Use 'play' or 'queue'.".into(),
                );
            }

            let action = LmsPlayAction::parse(args.action.as_deref());
            match state
                .lms
                .search_and_play(&args.query, &args.zone_id, action)
                .await
            {
                Ok(message) => Ok(text_result(message)),
                Err(e) => error_result(format!("Play error: {}", e)),
            }
        }
        LibraryRoute::Roon => {
            use crate::adapters::roon::PlayAction;

            let action = PlayAction::parse(args.action.as_deref().unwrap_or("play"));
            match state
                .roon
                .search_and_play(
                    &args.query,
                    &args.zone_id,
                    roon_search_source(args.source.as_deref()),
                    action,
                )
                .await
            {
                Ok(message) => Ok(text_result(message)),
                Err(e) => error_result(format!("Play error: {}", e)),
            }
        }
    }
}
