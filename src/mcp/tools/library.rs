//! Library search and playback.
//!
//! Both tools route on `roon:` and `lms:`. Everything else is refused since #398,
//! where it used to reach Roon (see [`crate::mcp::routing`]).
//!
//! # What #398 changed, and the one default it kept
//!
//! `openhome:`, `upnp:` and `hqplayer:` zones were sent to Roon's library, which
//! searched a library those zones cannot play from and then failed inside Roon —
//! so the model learned that Roon was broken rather than that the zone has no
//! library. They are now refused with the reason, sourced from
//! [`crate::mcp::capabilities`]. Unplaceable ids are refused with the accepted
//! prefixes.
//!
//! **An absent `zone_id` on `hifi_search` still means Roon.** `None` is not a
//! malformed zone id: there is nothing to route by, LMS `globalsearch` requires a
//! player id, and this tool's own description documents Roon as the default. It is
//! reported as `scope.provider: "roon"`, so it is visible rather than silent, and
//! `tests/mcp_contract.rs::an_absent_zone_id_still_routes_search_to_roon` pins it.
//!
//! This module is the primary target for #396 (opaque content references) and
//! #399 (hierarchical browse). Note what `McpSearchResult` lacks: any stable
//! identifier. A client that finds something here can only hand the title back
//! to `hifi_play`, which re-searches and takes the first match. That is #392's
//! keystone finding — recorded in `FIELD_ROLES` in `tests/mcp_contract.rs` as a
//! known defect, and out of scope for #394.

use crate::api::AppState;
use crate::mcp::capabilities::{support, Capability};
use crate::mcp::envelope::{Envelope, Observed, Refusal, Scope};
use crate::mcp::routing::{
    unplaceable_zone_refusal, unplaceable_zone_text, LibraryRoute, ZoneTarget,
};
use crate::mcp::types::{McpPlayResult, McpSearchResult};
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
    let target = ZoneTarget::classify_opt(args.zone_id.as_deref());
    let route = target.for_library();

    // A search is a read, so `ok` on success. `zone_id` is optional here, and an
    // absent one is omitted rather than reported as null — "no zone context" and
    // "zone context of null" are different requests.
    let mut env = Envelope::read("hifi_search", "search")
        .param("query", &*args.query)
        .param_opt("zone_id", args.zone_id.as_deref());

    env = match route {
        // LMS ignores `source` entirely, so echoing it back as understood would
        // claim the server honored something it discarded. A refused zone
        // reaches no backend at all, so the same reasoning applies.
        LibraryRoute::Lms | LibraryRoute::Refused(_) => env,
        LibraryRoute::Roon => env.param(
            "source",
            roon_source_name(roon_search_source(args.source.as_deref())),
        ),
    };

    let env = match args.zone_id.as_deref() {
        Some(zone_id) => env.scope(Scope::for_zone(state, zone_id, target.provider()).await),
        None => env.scope(Scope::provider_only(target.provider())),
    };

    if let LibraryRoute::Refused(refused) = route {
        // `zone_id` is present whenever the route refuses: an absent one
        // classifies as Roon and never lands here.
        let zone_id = args.zone_id.as_deref().unwrap_or_default();
        return refuse_library_zone(env, zone_id, refused, Capability::Search);
    }

    match route {
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
                    Ok(env.json_result(&mcp_results))
                }
                Err(e) => env.failed(format!("Search error: {}", e)),
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
                    Ok(env.json_result(&mcp_results))
                }
                Err(e) => env.failed(format!("Search error: {}", e)),
            }
        }
        // Handled above; exhaustive so a new route variant fails to compile
        // rather than silently taking one of the two library paths.
        LibraryRoute::Refused(_) => env.failed(
            "internal routing error: a refused zone reached library dispatch. This is a UHC bug.",
        ),
    }
}

/// Refuse a zone id with no library path.
///
/// The classification comes from [`crate::mcp::capabilities`], so this refusal and
/// `hifi_capabilities`' report cannot disagree about whether a gap belongs to the
/// provider or to UHC — the distinction #398 exists to keep straight.
fn refuse_library_zone(
    env: Envelope,
    zone_id: &str,
    target: ZoneTarget,
    capability: Capability,
) -> Result<CallToolResult, CallToolError> {
    match target.prefix() {
        Some(_) => {
            let state_of = support(target, capability);
            // Transport is what an OpenHome or UPnP zone *can* do: it plays
            // whatever a control point has already put on it.
            let alternatives = vec![
                "hifi_control action=play".to_string(),
                "hifi_capabilities".to_string(),
            ];
            let detail = state_of.evidence().unwrap_or_default();
            match state_of.refusal(capability, alternatives) {
                Some(refusal) => env.refused(
                    format!(
                        "{} zones have no library path from MCP: {detail} \
                         hifi_capabilities reports what each provider supports.",
                        target.label()
                    ),
                    refusal,
                ),
                None => env.failed(format!(
                    "No library path for {zone_id}, though {} reports this capability as \
                     supported. This is a UHC routing bug.",
                    target.label()
                )),
            }
        }
        None => env.refused(
            unplaceable_zone_text(zone_id, target),
            unplaceable_zone_refusal(target),
        ),
    }
}

/// The `source` name the server resolved to, for the envelope's `params`.
///
/// `roon_search_source` silently falls back to Library for anything
/// unrecognised, so reporting the resolved name is the only way a client learns
/// that its `source: "spotify"` was quietly read as `library`.
fn roon_source_name(source: crate::adapters::roon::SearchSource) -> &'static str {
    use crate::adapters::roon::SearchSource;
    match source {
        SearchSource::Tidal => "tidal",
        SearchSource::Qobuz => "qobuz",
        SearchSource::Library => "library",
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
    let target = ZoneTarget::classify(&args.zone_id);
    let route = target.for_library();

    if let LibraryRoute::Refused(refused) = route {
        let env = Envelope::write("hifi_play", "play")
            .param("query", &*args.query)
            .param("zone_id", &*args.zone_id)
            .scope(Scope::for_zone(state, &args.zone_id, target.provider()).await);
        return refuse_library_zone(env, &args.zone_id, refused, Capability::PlayByQuery);
    }

    match route {
        LibraryRoute::Lms => {
            use crate::adapters::lms::LmsPlayAction;

            // LMS ignores `source` (library only) and has no radio mode. The
            // refusal names the supported actions so the model can retry.
            if args.action.as_deref() == Some("radio") {
                return Envelope::write("hifi_play", "radio")
                    .param("query", &*args.query)
                    .param("zone_id", &*args.zone_id)
                    .param("action", "radio")
                    .scope(Scope::for_zone(state, &args.zone_id, target.provider()).await)
                    .refused(
                        "Radio mode not supported for LMS. Use 'play' or 'queue'.",
                        Refusal::ProviderLimitation {
                            operation: "radio".to_string(),
                            alternatives: vec![
                                "hifi_play action=play".to_string(),
                                "hifi_play action=queue".to_string(),
                            ],
                            detail: "LMS's core protocol has no equivalent of Roon Radio. \
                                     Plugin approximations exist (e.g. Don't Stop The Music) \
                                     but UHC drives none of them."
                                .to_string(),
                        },
                    );
            }

            let action = LmsPlayAction::parse(args.action.as_deref());
            // `operation` is the *resolved* action, so an absent `action` reports
            // whatever LmsPlayAction::parse defaulted to rather than nothing.
            let env = Envelope::write("hifi_play", lms_action_name(action))
                .param("query", &*args.query)
                .param("zone_id", &*args.zone_id)
                .param("action", lms_action_name(action))
                .scope(Scope::for_zone(state, &args.zone_id, target.provider()).await);

            match state
                .lms
                .search_and_play(&args.query, &args.zone_id, action)
                .await
            {
                Ok(message) => Ok(play_success(state, env, message).await),
                Err(e) => env.failed(format!("Play error: {}", e)),
            }
        }
        LibraryRoute::Roon => {
            use crate::adapters::roon::PlayAction;

            let source = roon_search_source(args.source.as_deref());
            let action = PlayAction::parse(args.action.as_deref().unwrap_or("play"));
            let env = Envelope::write("hifi_play", roon_action_name(action))
                .param("query", &*args.query)
                .param("zone_id", &*args.zone_id)
                .param("action", roon_action_name(action))
                .param("source", roon_source_name(source))
                .scope(Scope::for_zone(state, &args.zone_id, target.provider()).await);

            match state
                .roon
                .search_and_play(&args.query, &args.zone_id, source, action)
                .await
            {
                Ok(message) => Ok(play_success(state, env, message).await),
                Err(e) => env.failed(format!("Play error: {}", e)),
            }
        }
        // Handled above; exhaustive for the same reason as in `handle_search`.
        LibraryRoute::Refused(_) => Envelope::write("hifi_play", "play").failed(
            "internal routing error: a refused zone reached library dispatch. This is a UHC bug.",
        ),
    }
}

/// Finish a successful `hifi_play`: the adapter's message verbatim as the text,
/// plus a zone read-back.
///
/// `data.message` duplicates the text on purpose, and it is the only field in
/// this envelope that does. Without it a client reading only `structuredContent`
/// would have no record of *which* item matched, because there is no addressable
/// identifier for a search result until #396 lands. It is adapter-authored prose,
/// not parsed — #396 replaces it with an opaque ref.
async fn play_success(state: &AppState, env: Envelope, message: String) -> CallToolResult {
    let zone_id = env
        .scope
        .as_ref()
        .and_then(|s| s.zone_id.clone())
        .unwrap_or_default();
    let observed = Observed::from_aggregator(state, &zone_id).await;
    env.data(&McpPlayResult {
        message: message.clone(),
    })
    .observed(observed)
    .text_result(message)
}

/// The resolved LMS play action's name, for `operation` and `params.action`.
fn lms_action_name(action: crate::adapters::lms::LmsPlayAction) -> &'static str {
    use crate::adapters::lms::LmsPlayAction;
    match action {
        LmsPlayAction::Play => "play",
        LmsPlayAction::Queue => "queue",
        LmsPlayAction::Insert => "insert",
    }
}

/// The resolved Roon play action's name.
fn roon_action_name(action: crate::adapters::roon::PlayAction) -> &'static str {
    use crate::adapters::roon::PlayAction;
    match action {
        PlayAction::Play => "play",
        PlayAction::Queue => "queue",
        PlayAction::Radio => "radio",
    }
}
