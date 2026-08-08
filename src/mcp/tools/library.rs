//! Library search and playback.
//!
//! The tools route on `roon:`, `lms:`, `spotify:`, paired `applemusic:`, and
//! `musicassistant:` zones.
//! Everything else is refused since #398,
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
//! # Refs (#396): search -> hold ref -> play by ref
//!
//! `hifi_search` now mints an opaque `ref` alongside each result that has a
//! durable-enough handle (a Roon `item_key`, or an LMS `Library`/`Url`
//! target), and `HifiPlayRefTool` (below) is the one tool that consumes it —
//! not an optional parameter on `hifi_play`, so a model can never send both a
//! `query` and a `ref` and force the server to silently pick one. `hifi_play`
//! itself is unchanged: its `query` path still re-searches and takes the
//! first match, exactly as before this issue; that is #392's original
//! keystone finding, still true for that path and recorded as such in
//! `FIELD_ROLES` in `tests/mcp_contract.rs`. Refs are the way *around* it, not
//! a change to it.
//!
//! A result can legitimately have no `ref`: LMS's `GlobalSearchItem` handle is
//! a positional breadcrumb, not an identifier (see
//! `crate::adapters::lms::LmsPlayTarget`), and minting a ref through it would
//! trade an honest gap for a token that can silently play the wrong thing. So
//! "no ref" is expected for some results, not a bug to chase.
//!
//! Refs live in [`crate::mcp::refs::RefTable`], are short-lived, and expire
//! silently — resolving one that is gone reads as `unknown_ref` in
//! `hifi_play_ref`'s refusal, which tells the client to call `hifi_search`
//! again rather than failing generically. This module is also the target for
//! #399 (hierarchical browse), which will mint refs the same way.

use crate::api::AppState;
use crate::mcp::capabilities::{support, Capability};
use crate::mcp::envelope::{Envelope, Observed, Provider, Refusal, Scope};
use crate::mcp::refs::{RefTarget, RoonRefTarget};
use crate::mcp::routing::{
    unplaceable_zone_refusal, unplaceable_zone_text, LibraryRoute, ZoneTarget,
};
use crate::mcp::types::{McpPlayResult, McpSearchResult};
use rust_mcp_sdk::{
    macros::{mcp_tool, JsonSchema},
    schema::{schema_utils::CallToolError, CallToolResult},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// How many results to request from LMS.
///
/// Kept separate from [`ROON_SEARCH_LIMIT`] even though both are 10 today. They
/// were two independent literals before #394, and a refactor mandated to change
/// no behavior should not quietly couple them: the two backends page and rank
/// differently, so a future tuning of one is not a decision about the other.
const LMS_SEARCH_LIMIT: usize = 10;

/// How many results to request from Roon.
const ROON_SEARCH_LIMIT: usize = 10;
const SPOTIFY_SEARCH_LIMIT: usize = 10;

/// Search for music
#[mcp_tool(
    name = "hifi_search",
    description = "Search for tracks, albums, or artists. Roon: searches Library, TIDAL, or Qobuz (use source param). LMS: searches all installed providers including streaming plugins. Spotify: searches the Spotify catalog for a Spotify Connect zone. Apple Music: searches through a paired iPhone companion and returns owner-scoped opaque refs; physical companion validation remains tracked in #465. Music Assistant searches its configured catalog for a Music Assistant zone. Each result may carry a short-lived `ref` token; hold it and pass it to hifi_play_ref to play or queue that exact result.",
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
    description = "Search and play music. Searches and plays or queues the first matching result. Use action='queue' to add to queue. action='radio' and source param are Roon-only; Spotify supports play and queue for Spotify Connect zones. Apple Music play/queue uses a paired iPhone companion and owner-scoped opaque refs; physical validation remains tracked in #465. Music Assistant supports play and queue against its active player queue. To act on a specific hifi_search result rather than the first match for a title, use hifi_play_ref with that result's `ref` instead."
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

/// Play, queue, or start radio from a specific `hifi_search` result (#396)
#[mcp_tool(
    name = "hifi_play_ref",
    description = "Play or queue a specific hifi_search result using its `ref` — the opaque token returned alongside a result, not a title. Refs are short-lived; call hifi_search again if one expires. zone_id must belong to the same provider (Roon, LMS, Spotify, Music Assistant, or a paired Apple Music companion) the ref was minted for. Music Assistant refs support play and queue against its active player queue. Apple Music refs and play/queue use the paired iPhone companion; physical validation remains tracked in #465.",
    read_only_hint = false
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiPlayRefTool {
    /// The opaque `ref` from a hifi_search result. Do not construct, guess, or edit one.
    #[serde(rename = "ref")]
    pub r#ref: String,
    /// Zone ID to play on (get from hifi_zones). Must be the same provider the ref was minted for.
    pub zone_id: String,
    /// What to do: "play" (default), "queue", "radio" (Roon only), or "next" (LMS play-next only).
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
        LibraryRoute::Lms
        | LibraryRoute::Spotify
        | LibraryRoute::AppleMusic
        | LibraryRoute::MusicAssistant
        | LibraryRoute::Refused(_) => env,
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
                    let mut mcp_results = Vec::with_capacity(results.len());
                    for item in results {
                        // #396: mint a ref only for a durable-enough handle
                        // (Library or Url). A GlobalSearchItem-only result
                        // gets no ref at all, honestly, rather than one that
                        // could silently mis-resolve — see
                        // `LmsPlayTarget::mintable`'s own docs.
                        let ref_token = match crate::adapters::lms::LmsPlayTarget::mintable(&item) {
                            Some(target) => Some(
                                state
                                    .mcp_refs
                                    .mint(RefTarget::Lms {
                                        target,
                                        title: item.title.clone(),
                                    })
                                    .await,
                            ),
                            None => None,
                        };
                        mcp_results.push(McpSearchResult {
                            subtitle: lms_subtitle(&item),
                            title: item.title,
                            r#ref: ref_token,
                        });
                    }
                    Ok(env.json_result(&mcp_results))
                }
                Err(e) => env.failed(format!("Search error: {}", e)),
            }
        }
        LibraryRoute::Spotify => {
            match state
                .adapter_registry
                .search_library("spotify", &args.query, SPOTIFY_SEARCH_LIMIT)
                .await
            {
                Ok(results) => {
                    let mut mcp_results = Vec::with_capacity(results.len());
                    for item in results {
                        let ref_token = state
                            .mcp_refs
                            .mint(RefTarget::Spotify {
                                uri: item.uri,
                                title: item.title.clone(),
                            })
                            .await;
                        mcp_results.push(McpSearchResult {
                            title: item.title,
                            subtitle: item.subtitle,
                            r#ref: Some(ref_token),
                        });
                    }
                    Ok(env.json_result(&mcp_results))
                }
                Err(e) => env.failed(format!("Search error: {}", e)),
            }
        }
        LibraryRoute::MusicAssistant => {
            match state
                .adapter_registry
                .search_library("musicassistant", &args.query, SPOTIFY_SEARCH_LIMIT)
                .await
            {
                Ok(results) => {
                    let mut mcp_results = Vec::with_capacity(results.len());
                    for item in results {
                        let ref_token = state
                            .mcp_refs
                            .mint(RefTarget::MusicAssistant {
                                uri: item.uri,
                                title: item.title.clone(),
                            })
                            .await;
                        mcp_results.push(McpSearchResult {
                            title: item.title,
                            subtitle: item.subtitle,
                            r#ref: Some(ref_token),
                        });
                    }
                    Ok(env.json_result(&mcp_results))
                }
                Err(e) => env.failed(format!("Search error: {}", e)),
            }
        }
        LibraryRoute::AppleMusic => {
            match state
                .adapter_registry
                .search_library_for_zone(
                    "applemusic",
                    args.zone_id.as_deref().unwrap_or_default(),
                    &args.query,
                    SPOTIFY_SEARCH_LIMIT,
                )
                .await
            {
                Ok(results) => {
                    let mut mcp_results = Vec::with_capacity(results.len());
                    for item in results {
                        let ref_token = state
                            .mcp_refs
                            .mint(RefTarget::AppleMusic {
                                companion_id: args
                                    .zone_id
                                    .as_deref()
                                    .and_then(|zone| zone.strip_prefix("applemusic:"))
                                    .unwrap_or_default()
                                    .to_string(),
                                handle: item.uri,
                                title: item.title.clone(),
                            })
                            .await;
                        mcp_results.push(McpSearchResult {
                            title: item.title,
                            subtitle: item.subtitle,
                            r#ref: Some(ref_token),
                        });
                    }
                    Ok(env.json_result(&mcp_results))
                }
                Err(e) => env.failed(format!("Search error: {}", e)),
            }
        }
        LibraryRoute::Roon => {
            match state
                .roon
                .search_with_session(
                    &args.query,
                    args.zone_id.as_deref(),
                    Some(ROON_SEARCH_LIMIT),
                    roon_search_source(args.source.as_deref()),
                )
                .await
            {
                Ok((session_key, results)) => {
                    let mut mcp_results = Vec::with_capacity(results.len());
                    for item in results {
                        // #396 (found live, ship-gate re-review): a real
                        // Core's search results mix the actual hit with
                        // grouping rows ("Albums", "Tracks", "Artists", ...)
                        // that carry the same `hint: "list"` and a real
                        // `item_key` -- `FakeRoonCore`'s search model never
                        // included these, so nothing caught this minting a
                        // ref for them indistinguishably from real content.
                        // `crate::adapters::roon::is_ungrounded_grouping`
                        // combines the adapter's own pre-existing title-list
                        // check with a second, source-independent signal
                        // (subtitle shaped like "<N> Results") verified live
                        // for Library results but not for TIDAL/Qobuz -- see
                        // that function's own doc comment for why the second
                        // check is safe to include even where unverified.
                        // Resolving a grouping row's ref would land in
                        // `resolve_item_key`'s "guess the first item"
                        // fallback and could silently play something the
                        // client never asked for. Only a real, navigable
                        // item_key on a non-grouping row gets a ref; a
                        // grouping row (or one with no item_key at all — a
                        // header, or non-navigable) mints nothing.
                        let ref_token = match &item.item_key {
                            Some(item_key)
                                if !crate::adapters::roon::is_ungrounded_grouping(&item) =>
                            {
                                Some(
                                    state
                                        .mcp_refs
                                        .mint(RefTarget::Roon {
                                            target: RoonRefTarget {
                                                item_key: item_key.clone(),
                                                multi_session_key: session_key.clone(),
                                            },
                                            title: item.title.clone(),
                                        })
                                        .await,
                                )
                            }
                            _ => None,
                        };
                        mcp_results.push(McpSearchResult {
                            title: item.title,
                            subtitle: item.subtitle,
                            r#ref: ref_token,
                        });
                    }
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
        LibraryRoute::Spotify => {
            let action = args.action.as_deref().unwrap_or("play");
            if !matches!(action, "play" | "queue") {
                let env = Envelope::write("hifi_play", action)
                    .param("query", &*args.query)
                    .param("zone_id", &*args.zone_id)
                    .param("action", action)
                    .scope(Scope::for_zone(state, &args.zone_id, target.provider()).await);
                return env.refused(
                    "Spotify query playback supports action='play' or action='queue'; radio is not part of the Spotify controller surface.",
                    Refusal::InvalidParameter {
                        parameter: "action",
                        accepted: vec!["play".to_string(), "queue".to_string()],
                        detail: "Spotify Connect zones can play a catalog result or add it to Spotify's queue. Radio is not exposed by this controller surface.".to_string(),
                    },
                );
            }
            let env = Envelope::write("hifi_play", action)
                .param("query", &*args.query)
                .param("zone_id", &*args.zone_id)
                .param("action", action)
                .scope(Scope::for_zone(state, &args.zone_id, target.provider()).await);
            match state
                .adapter_registry
                .search_library("spotify", &args.query, SPOTIFY_SEARCH_LIMIT)
                .await
            {
                Ok(mut results) => match results.drain(..).next() {
                    Some(item) => {
                        let result = if action == "queue" {
                            state
                                .adapter_registry
                                .queue_library_uri("spotify", &args.zone_id, &item.uri)
                                .await
                                .map(|_| format!("Queued {} on Spotify", item.title))
                        } else {
                            state
                                .adapter_registry
                                .play_library_uri("spotify", &args.zone_id, &item.uri)
                                .await
                        };
                        match result {
                            Ok(message) => Ok(play_success(state, env, message).await),
                            Err(e) => env.failed(format!("Play error: {}", e)),
                        }
                    }
                    None => {
                        env.failed("Play error: Spotify search returned no results".to_string())
                    }
                },
                Err(e) => env.failed(format!("Search error: {}", e)),
            }
        }
        LibraryRoute::MusicAssistant => {
            let action = args.action.as_deref().unwrap_or("play");
            if !matches!(action, "play" | "queue") {
                let env = Envelope::write("hifi_play", action)
                    .param("query", &*args.query)
                    .param("zone_id", &*args.zone_id)
                    .param("action", action)
                    .scope(Scope::for_zone(state, &args.zone_id, target.provider()).await);
                return env.refused(
                    "Music Assistant query playback supports action='play' or action='queue'.",
                    Refusal::InvalidParameter {
                        parameter: "action",
                        accepted: vec!["play".to_string(), "queue".to_string()],
                        detail: "Music Assistant can start a catalog result or append it to its active queue.".to_string(),
                    },
                );
            }
            let env = Envelope::write("hifi_play", action)
                .param("query", &*args.query)
                .param("zone_id", &*args.zone_id)
                .param("action", action)
                .scope(Scope::for_zone(state, &args.zone_id, target.provider()).await);
            match state
                .adapter_registry
                .search_library("musicassistant", &args.query, SPOTIFY_SEARCH_LIMIT)
                .await
            {
                Ok(mut results) => match results.drain(..).next() {
                    Some(item) => {
                        let result = if action == "queue" {
                            state
                                .adapter_registry
                                .queue_library_uri("musicassistant", &args.zone_id, &item.uri)
                                .await
                                .map(|_| format!("Queued {} on Music Assistant", item.title))
                        } else {
                            state
                                .adapter_registry
                                .play_library_uri("musicassistant", &args.zone_id, &item.uri)
                                .await
                        };
                        match result {
                            Ok(message) => Ok(play_success(state, env, message).await),
                            Err(e) => env.failed(format!("Play error: {}", e)),
                        }
                    }
                    None => env.failed("Play error: Music Assistant search returned no results"),
                },
                Err(e) => env.failed(format!("Search error: {}", e)),
            }
        }
        LibraryRoute::AppleMusic => {
            let action = args.action.as_deref().unwrap_or("play");
            if !matches!(action, "play" | "queue") {
                let env = Envelope::write("hifi_play", action)
                    .param("query", &*args.query)
                    .param("zone_id", &*args.zone_id)
                    .param("action", action)
                    .scope(Scope::for_zone(state, &args.zone_id, target.provider()).await);
                return env.refused(
                    "Apple Music query playback supports action='play' or action='queue'.",
                    Refusal::InvalidParameter {
                        parameter: "action",
                        accepted: vec!["play".to_string(), "queue".to_string()],
                        detail: "Apple Music companion playback does not expose radio through this MCP surface.".to_string(),
                    },
                );
            }
            let env = Envelope::write("hifi_play", action)
                .param("query", &*args.query)
                .param("zone_id", &*args.zone_id)
                .param("action", action)
                .scope(Scope::for_zone(state, &args.zone_id, target.provider()).await);
            match state
                .adapter_registry
                .search_library_for_zone(
                    "applemusic",
                    &args.zone_id,
                    &args.query,
                    SPOTIFY_SEARCH_LIMIT,
                )
                .await
            {
                Ok(mut results) => match results.drain(..).next() {
                    Some(item) => {
                        let result = if action == "queue" {
                            let companion_id = args
                                .zone_id
                                .strip_prefix("applemusic:")
                                .unwrap_or_default()
                                .to_string();
                            let plan_ref = state
                                .mcp_refs
                                .mint(RefTarget::AppleMusic {
                                    companion_id,
                                    handle: item.uri.clone(),
                                    title: item.title.clone(),
                                })
                                .await;
                            if let Err(error) = state
                                .listening_plans
                                .append(
                                    &args.zone_id,
                                    vec![crate::mcp::listening_plan::ListeningPlanItem {
                                        reference: plan_ref,
                                        title: item.title.clone(),
                                    }],
                                )
                                .await
                            {
                                return env.failed(format!(
                                    "Apple Music listening plan could not be persisted: {error}"
                                ));
                            }
                            state
                                .adapter_registry
                                .queue_library_uri("applemusic", &args.zone_id, &item.uri)
                                .await
                                .map(|_| format!("Queued {} on Apple Music", item.title))
                        } else {
                            state
                                .adapter_registry
                                .play_library_uri("applemusic", &args.zone_id, &item.uri)
                                .await
                        };
                        match result {
                            Ok(message) => Ok(play_success(state, env, message).await),
                            Err(e) => env.failed(format!("Play error: {}", e)),
                        }
                    }
                    None => {
                        env.failed("Play error: Apple Music search returned no results".to_string())
                    }
                },
                Err(e) => env.failed(format!("Search error: {}", e)),
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

/// Finish a successful `hifi_play` or `hifi_play_ref`: the adapter's message
/// verbatim as the text, plus a zone read-back. Shared by both tools so their
/// success shape (`McpPlayResult` plus `observed`) is identical whichever path
/// a client took.
///
/// `data.message` duplicates the text on purpose, and it is the only field in
/// this envelope that does. For `hifi_play`'s query path it remains the only
/// record of *which* item matched — that path has no addressable identifier
/// by design (`hifi_play_ref` is the way around it, not a change to it; see
/// this module's docs). For `hifi_play_ref` the message still comes from the
/// adapter/ref title, kept for the same reason: a client reading only
/// `structuredContent` should not have to guess what played.
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

// =============================================================================
// hifi_play_ref (#396): the one consumer of a hifi_search `ref`
// =============================================================================

/// Every action `hifi_play_ref` accepts for a Roon-minted ref, in the order
/// the tool description lists them. A closed set: an action outside it is
/// refused by name rather than silently defaulted, which is what
/// `PlayAction::parse` would otherwise do (see `handle_search`'s `roon_source_name`
/// for the same "report what was actually resolved" principle applied to a
/// different parameter).
const ROON_REF_ACTIONS: &[&str] = &["play", "queue", "radio"];

/// Every action `hifi_play_ref` accepts for an LMS-minted ref.
const LMS_REF_ACTIONS: &[&str] = &["play", "queue", "next"];
const SPOTIFY_REF_ACTIONS: &[&str] = &["play", "queue"];
const MUSIC_ASSISTANT_REF_ACTIONS: &[&str] = &["play", "queue"];

/// Validate `action` against a provider's closed action set, defaulting to
/// the first (always `"play"`) when absent. Never falls through to a default
/// for a *present but unrecognised* value -- that silent-fallback is the
/// defect this whole issue exists to end.
fn validate_ref_action(
    action: Option<&str>,
    accepted: &'static [&'static str],
) -> Result<&'static str, Refusal> {
    match action {
        None => Ok(accepted[0]),
        Some(requested) => {
            let lower = requested.to_lowercase();
            accepted
                .iter()
                .find(|candidate| **candidate == lower)
                .copied()
                .ok_or_else(|| {
                    Refusal::invalid_parameter(
                        "action",
                        accepted,
                        format!(
                            "'{requested}' is not an accepted hifi_play_ref action for this \
                             ref's provider. Accepted: {}.",
                            accepted.join(", ")
                        ),
                    )
                })
        }
    }
}

/// The lowercase label for a [`Provider`], for refusal prose. Every variant is
/// listed explicitly so a new provider fails to compile here rather than
/// silently falling through to a wrong label.
fn provider_label(provider: Provider) -> &'static str {
    match provider {
        Provider::Roon => "roon",
        Provider::Lms => "lms",
        Provider::OpenHome => "openhome",
        Provider::Upnp => "upnp",
        Provider::HqPlayer => "hqplayer",
        Provider::AppleMusic => "applemusic",
        Provider::Spotify => "spotify",
        Provider::MusicAssistant => "musicassistant",
        Provider::Unknown => "unknown",
    }
}

pub async fn handle_play_ref(
    state: &AppState,
    args: HifiPlayRefTool,
) -> Result<CallToolResult, CallToolError> {
    let target = ZoneTarget::classify(&args.zone_id);
    let route = target.for_library();

    // `operation` starts as the constant `"play_ref"` -- matching
    // `hifi_play`'s own zone-refused path (`Envelope::write("hifi_play",
    // "play")`, unconditionally, never `args.action`) -- because at this
    // point `action` has not been validated against either provider's
    // accepted set, so echoing it verbatim would let unvalidated client
    // input (garbage, or a value that's valid for the *other* provider) flow
    // straight into a field clients are told to branch on. `play_ref_roon`/
    // `play_ref_lms` overwrite it once an action is actually known: the
    // validated name on success (matching `hifi_play`'s
    // `roon_action_name`/`lms_action_name` convention), or the normalized
    // sentinel `"invalid_action"` on refusal (matching `hifi_control`'s
    // `unknown_action` convention, `src/mcp/tools/transport.rs`).
    let mut env = Envelope::write("hifi_play_ref", "play_ref")
        .param("ref", &*args.r#ref)
        .param("zone_id", &*args.zone_id)
        .scope(Scope::for_zone(state, &args.zone_id, target.provider()).await);
    if let Some(action) = args.action.as_deref() {
        env = env.param("action", action);
    }

    // A zone with no library path refuses the same way `hifi_play` does --
    // the routing question ("can this zone accept a library-sourced play at
    // all") is identical; only the resolution mechanism (ref vs. query)
    // differs downstream of it.
    if let LibraryRoute::Refused(refused) = route {
        return refuse_library_zone(env, &args.zone_id, refused, Capability::PlayByQuery);
    }

    let Some(ref_target) = state.mcp_refs.resolve(&args.r#ref).await else {
        return env.refused(
            "ref is unknown or expired. Refs are short-lived -- call hifi_search again for a \
             fresh one; never guess or reuse an old ref.",
            Refusal::UnknownTarget {
                parameter: "ref",
                discover_with: "hifi_search",
                detail: "This ref is not in UHC's ref table. It may have expired, been evicted \
                         under load, or never existed -- all three look identical from here by \
                         design, so there is nothing more specific to report. hifi_search mints \
                         a fresh ref on every call; use the new one rather than an old one."
                    .to_string(),
            },
        );
    };

    // Capability-honest cross-provider refusal: a ref minted for one provider
    // must never be resolved against a zone of another, even though the zone
    // itself has a perfectly good library path of its own.
    let zone_provider = target.provider();
    if ref_target.provider() != zone_provider {
        let ref_provider_label = provider_label(ref_target.provider());
        let zone_provider_label = provider_label(zone_provider);
        return env.refused(
            format!(
                "this ref was minted for {ref_provider_label}, but zone_id '{}' is {zone_provider_label}.",
                args.zone_id
            ),
            Refusal::InvalidParameter {
                parameter: "zone_id",
                accepted: vec![format!(
                    "a {ref_provider_label} zone_id (this ref was minted for {ref_provider_label})"
                )],
                detail: format!(
                    "hifi_search minted this ref against {ref_provider_label}; hifi_play_ref \
                     cannot resolve it against a {zone_provider_label} zone. Call hifi_search \
                     again with a {ref_provider_label} zone_id, or use hifi_zones to find one."
                ),
            },
        );
    }

    if let RefTarget::AppleMusic { companion_id, .. } = &ref_target {
        let zone_companion = args.zone_id.strip_prefix("applemusic:").unwrap_or_default();
        if companion_id != zone_companion {
            return env.refused(
                "this Apple Music ref belongs to a different companion execution owner.",
                Refusal::InvalidParameter {
                    parameter: "zone_id",
                    accepted: vec!["the applemusic:<companion> zone used by hifi_search".to_string()],
                    detail: "Apple Music catalog and library identifiers are scoped to the companion that minted the ref; search again for the selected companion.".to_string(),
                },
            );
        }
    }

    match (route, ref_target) {
        (LibraryRoute::Roon, RefTarget::Roon { target, title }) => {
            play_ref_roon(state, env, &args, target, title).await
        }
        (LibraryRoute::Lms, RefTarget::Lms { target, title }) => {
            play_ref_lms(state, env, &args, target, title).await
        }
        (LibraryRoute::Spotify, RefTarget::Spotify { uri, title }) => {
            play_ref_spotify(state, env, &args, uri, title).await
        }
        (LibraryRoute::MusicAssistant, RefTarget::MusicAssistant { uri, title }) => {
            play_ref_music_assistant(state, env, &args, uri, title).await
        }
        (LibraryRoute::AppleMusic, RefTarget::AppleMusic { handle, title, .. }) => {
            play_ref_apple_music(state, env, &args, handle, title).await
        }
        // Unreachable: the provider check above already refused any mismatch
        // between the zone's provider and the ref's. Kept exhaustive so a
        // third provider gaining a ref target fails to compile here rather
        // than silently mis-dispatching.
        (LibraryRoute::Roon, RefTarget::Lms { .. })
        | (LibraryRoute::Roon, RefTarget::Spotify { .. })
        | (LibraryRoute::Lms, RefTarget::Roon { .. })
        | (LibraryRoute::Lms, RefTarget::Spotify { .. })
        | (LibraryRoute::Spotify, RefTarget::Roon { .. })
        | (LibraryRoute::Spotify, RefTarget::Lms { .. })
        | (LibraryRoute::AppleMusic, RefTarget::Roon { .. })
        | (LibraryRoute::AppleMusic, RefTarget::Lms { .. })
        | (LibraryRoute::AppleMusic, RefTarget::Spotify { .. })
        | (LibraryRoute::AppleMusic, RefTarget::MusicAssistant { .. })
        | (LibraryRoute::Roon, RefTarget::AppleMusic { .. })
        | (LibraryRoute::Lms, RefTarget::AppleMusic { .. })
        | (LibraryRoute::Spotify, RefTarget::AppleMusic { .. })
        | (LibraryRoute::Roon, RefTarget::MusicAssistant { .. })
        | (LibraryRoute::Lms, RefTarget::MusicAssistant { .. })
        | (LibraryRoute::Spotify, RefTarget::MusicAssistant { .. })
        | (LibraryRoute::MusicAssistant, RefTarget::Roon { .. })
        | (LibraryRoute::MusicAssistant, RefTarget::Lms { .. })
        | (LibraryRoute::MusicAssistant, RefTarget::Spotify { .. })
        | (LibraryRoute::MusicAssistant, RefTarget::AppleMusic { .. }) => env.failed(
            "internal routing error: ref/zone provider mismatch reached dispatch after the \
                 capability check. This is a UHC bug.",
        ),
        (LibraryRoute::Refused(_), _) => env.failed(
            "internal routing error: a refused zone reached ref dispatch. This is a UHC bug.",
        ),
    }
}

async fn play_ref_roon(
    state: &AppState,
    mut env: Envelope,
    args: &HifiPlayRefTool,
    target: RoonRefTarget,
    _title: String,
) -> Result<CallToolResult, CallToolError> {
    use crate::adapters::roon::PlayAction;

    let action_name = match validate_ref_action(args.action.as_deref(), ROON_REF_ACTIONS) {
        Ok(name) => name,
        Err(refusal) => {
            let detail = match &refusal {
                Refusal::InvalidParameter { detail, .. } => detail.clone(),
                _ => String::new(),
            };
            // A normalized sentinel, not an echo of the raw (unrecognised)
            // request -- matching `hifi_control`'s own `unknown_action`
            // convention for the same class of refusal
            // (`src/mcp/tools/transport.rs`). A client branching on
            // `operation` must never see arbitrary client input reflected
            // back as if it were a real value.
            env.operation = "invalid_action".to_string();
            return env.refused(detail, refusal);
        }
    };
    // `operation` follows `hifi_play`'s own convention (`roon_action_name`):
    // the resolved action, not a constant tool-name-shaped string, so a
    // client reading only `operation` can tell play_ref=queue from
    // play_ref=play without also reading `params.action`.
    let mut env = env.param("action", action_name);
    env.operation = action_name.to_string();
    let action = PlayAction::parse(action_name);

    match state
        .roon
        .play_ref(
            &target.item_key,
            &target.multi_session_key,
            &args.zone_id,
            action,
        )
        .await
    {
        Ok(message) => Ok(play_success(state, env, message).await),
        Err(e) => env.failed(format!("Play error: {}", e)),
    }
}

async fn play_ref_lms(
    state: &AppState,
    mut env: Envelope,
    args: &HifiPlayRefTool,
    target: crate::adapters::lms::LmsPlayTarget,
    title: String,
) -> Result<CallToolResult, CallToolError> {
    use crate::adapters::lms::LmsPlayAction;

    let action_name = match validate_ref_action(args.action.as_deref(), LMS_REF_ACTIONS) {
        Ok(name) => name,
        Err(refusal) => {
            let detail = match &refusal {
                Refusal::InvalidParameter { detail, .. } => detail.clone(),
                _ => String::new(),
            };
            // See play_ref_roon's identical comment: a normalized sentinel,
            // matching hifi_control's `unknown_action` convention.
            env.operation = "invalid_action".to_string();
            return env.refused(detail, refusal);
        }
    };
    let mut env = env.param("action", action_name);
    env.operation = action_name.to_string();
    let action = LmsPlayAction::parse(Some(action_name));

    // `Some(&title)`: this target was resolved from a ref minted earlier in
    // a possibly-different conversation turn, so it is validated against the
    // live library before the mutating command runs (`LmsAdapter::play_target`'s
    // own docs explain why, and why it is keyed off `title` rather than a
    // dedicated existence query).
    match state
        .lms
        .play_target(&target, &args.zone_id, action, Some(&title))
        .await
    {
        Ok(()) => {
            let action_verb = match action {
                LmsPlayAction::Play => "Playing",
                LmsPlayAction::Queue => "Queued",
                LmsPlayAction::Insert => "Playing next",
            };
            let message = format!("{action_verb} {title}");
            Ok(play_success(state, env, message).await)
        }
        Err(e) => env.failed(format!("Play error: {}", e)),
    }
}

async fn play_ref_spotify(
    state: &AppState,
    mut env: Envelope,
    args: &HifiPlayRefTool,
    uri: String,
    title: String,
) -> Result<CallToolResult, CallToolError> {
    let action_name = match validate_ref_action(args.action.as_deref(), SPOTIFY_REF_ACTIONS) {
        Ok(name) => name,
        Err(refusal) => {
            env.operation = "invalid_action".to_string();
            let detail = match &refusal {
                Refusal::InvalidParameter { detail, .. } => detail.clone(),
                _ => String::new(),
            };
            return env.refused(detail, refusal);
        }
    };
    let mut env = env.param("action", action_name);
    env.operation = action_name.to_string();
    let result = if action_name == "queue" {
        state
            .adapter_registry
            .queue_library_uri("spotify", &args.zone_id, &uri)
            .await
            .map(|_| format!("Queued {title} on Spotify"))
    } else {
        state
            .adapter_registry
            .play_library_uri("spotify", &args.zone_id, &uri)
            .await
    };
    match result {
        Ok(message) => Ok(play_success(state, env, message).await),
        Err(error) => env.failed(format!("Play error for {title}: {}", error)),
    }
}

async fn play_ref_music_assistant(
    state: &AppState,
    mut env: Envelope,
    args: &HifiPlayRefTool,
    uri: String,
    title: String,
) -> Result<CallToolResult, CallToolError> {
    let action_name = match validate_ref_action(args.action.as_deref(), MUSIC_ASSISTANT_REF_ACTIONS)
    {
        Ok(name) => name,
        Err(refusal) => {
            env.operation = "invalid_action".to_string();
            let detail = match &refusal {
                Refusal::InvalidParameter { detail, .. } => detail.clone(),
                _ => String::new(),
            };
            return env.refused(detail, refusal);
        }
    };
    let mut env = env.param("action", action_name);
    env.operation = action_name.to_string();
    let result = if action_name == "queue" {
        state
            .adapter_registry
            .queue_library_uri("musicassistant", &args.zone_id, &uri)
            .await
            .map(|_| format!("Queued {title} on Music Assistant"))
    } else {
        state
            .adapter_registry
            .play_library_uri("musicassistant", &args.zone_id, &uri)
            .await
    };
    match result {
        Ok(message) => Ok(play_success(state, env, message).await),
        Err(error) => env.failed(format!("Play error for {title}: {}", error)),
    }
}

async fn play_ref_apple_music(
    state: &AppState,
    mut env: Envelope,
    args: &HifiPlayRefTool,
    uri: String,
    title: String,
) -> Result<CallToolResult, CallToolError> {
    const ACTIONS: &[&str] = &["play", "queue"];
    let action_name = match validate_ref_action(args.action.as_deref(), ACTIONS) {
        Ok(name) => name,
        Err(refusal) => {
            env.operation = "invalid_action".to_string();
            let detail = match &refusal {
                Refusal::InvalidParameter { detail, .. } => detail.clone(),
                _ => String::new(),
            };
            return env.refused(detail, refusal);
        }
    };
    let mut env = env.param("action", action_name);
    env.operation = action_name.to_string();
    let plan = if action_name == "queue" {
        match state
            .listening_plans
            .append(
                &args.zone_id,
                vec![crate::mcp::listening_plan::ListeningPlanItem {
                    reference: args.r#ref.clone(),
                    title: title.clone(),
                }],
            )
            .await
        {
            Ok(plan) => Some(plan),
            Err(error) => {
                return env.failed(format!(
                    "Apple Music listening plan could not be persisted: {error}"
                ))
            }
        }
    } else {
        None
    };
    let result = if action_name == "queue" {
        state
            .adapter_registry
            .queue_library_uri("applemusic", &args.zone_id, &uri)
            .await
            .map(|_| format!("Queued {title} on Apple Music"))
    } else {
        state
            .adapter_registry
            .play_library_uri("applemusic", &args.zone_id, &uri)
            .await
    };
    match result {
        Ok(message) => Ok(play_success(state, env, message).await),
        Err(error) if plan.is_some() => Ok(env.json_result(&json!({
            "plan": plan,
            "provider": {"outcome": "refused", "detail": error.to_string()}
        }))),
        Err(error) => env.failed(format!("Play error for {title}: {}", error)),
    }
}
