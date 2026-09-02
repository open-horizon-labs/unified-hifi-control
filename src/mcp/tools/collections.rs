//! Provider-neutral read-only library collections.
//!
//! The wire contract deliberately speaks only of collections, locations,
//! pages, and opaque refs. Provider adapters translate those concepts to
//! their native library APIs; no provider URI or identifier is returned to a
//! client. `location` is durable canonical identity; legacy `path` refs remain
//! short-lived MCP continuations.
//!
//! # #531: per-provider slices, not one blanket gate
//!
//! #492 wired only Music Assistant, behind a blanket "not implemented for
//! this provider yet" refusal for every other route. That refusal did not
//! distinguish "this provider cannot" from "UHC has not wired it" and, worse,
//! disagreed with [`crate::mcp::capabilities`] for Spotify: `hifi_spotify`
//! already exposes playlists/favorites/browse there, so the capability table
//! reports `browse`/`saved_playlists`/`favorites` as `supported` for Spotify
//! zones independent of this tool. This module now implements LMS and Roon
//! (#531's biggest UX win -- local-library and Roon-library browsing had no
//! neutral surface at all) and refuses Spotify and Apple Music by name,
//! honestly: reachable through their own provider tools today, not yet wired
//! to this one. See #531's PR body for what remains.

use crate::adapters::roon::RoonBrowseError;
use crate::api::AppState;
use crate::mcp::capabilities::{support, Capability, Support};
use crate::mcp::collection_locations::{
    CollectionBreadcrumb, CollectionLocation, CollectionStep, RoonLocationOrigin,
};
use crate::mcp::envelope::{Envelope, Refusal, Scope};
use crate::mcp::refs::{RefTarget, RoonRefTarget};
use crate::mcp::routing::{LibraryRoute, ZoneTarget};
use rust_mcp_sdk::{
    macros::{mcp_tool, JsonSchema},
    schema::{schema_utils::CallToolError, CallToolResult},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const ACTIONS: &[&str] = &["browse", "playlists", "favorites"];

/// Whether `hifi_collections` (and its HTTP mirror, `/api/collections`)
/// implements this zone's provider at all.
///
/// **Narrower than "is any collections capability `supported`"**: Spotify's
/// `browse`/`saved_playlists`/`favorites` cells read `supported` in
/// [`crate::mcp::capabilities`] because `hifi_spotify` already implements
/// them (see that module's `routed()`), not because this tool does. The web
/// UI's `CollectionsBrowser` panel (`src/app/pages/zones.rs`) calls
/// `/api/collections`, which is *this* tool -- so it must gate on this
/// narrower question, not on the general capability table, or it would light
/// up a panel that immediately refuses every call for a Spotify zone. See
/// #531's PR body for Spotify and Apple Music's plan to close this gap.
pub fn zone_supports_hifi_collections(zone_id: &str) -> bool {
    matches!(
        ZoneTarget::classify(zone_id).for_library(),
        LibraryRoute::MusicAssistant | LibraryRoute::Lms | LibraryRoute::Roon
    )
}

/// Which Library-page tabs this zone's provider can actually serve, in the
/// page's own tab vocabulary (`browse`, `playlists`, `favorites`, `radio`).
///
/// #573 defect 6: the web Library page rendered a static four-tab strip for
/// every provider, so Roon zones showed Favorites and Radio tabs whose every
/// call was refused. This derives the visible set from the same capability
/// facts the matrix reports (`crate::mcp::capabilities::support`), through
/// the same [`zone_supports_hifi_collections`] gate this tool routes on --
/// the tabs and the refusals can never disagree. `favorites` and `radio` are
/// one capability cell: both map to the `favorites` action (`media_type` is
/// what tells them apart).
pub fn collections_tabs_for_zone(zone_id: &str) -> Vec<&'static str> {
    if !zone_supports_hifi_collections(zone_id) {
        return Vec::new();
    }
    let target = ZoneTarget::classify(zone_id);
    let mut tabs = Vec::new();
    if matches!(support(target, Capability::Browse), Support::Supported) {
        tabs.push("browse");
    }
    if matches!(
        support(target, Capability::SavedPlaylists),
        Support::Supported
    ) {
        tabs.push("playlists");
    }
    if matches!(support(target, Capability::Favorites), Support::Supported) {
        tabs.push("favorites");
        tabs.push("radio");
    }
    tabs
}

#[mcp_tool(
    name = "hifi_collections",
    description = "Alpha capability: expect refinement across releases. Browse a provider library or list saved playlists and favorites. Results are paged with limit/offset. Navigable entries include a durable opaque location for canonical navigation and a short-lived path for MCP continuation; playable entries include a short-lived opaque ref for hifi_play_ref. Send location or path, never both. Implemented for Roon, LMS, and Music Assistant zones. Spotify and Apple Music are reachable via hifi_spotify and the Apple Music tools today, not yet through this one."
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiCollectionsTool {
    /// Zone ID whose provider library is queried.
    pub zone_id: String,
    /// browse, playlists, or favorites.
    pub action: String,
    /// Short-lived collection continuation returned by a preceding browse call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Durable provider-neutral collection location returned by a preceding browse call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// Media type when listing favorites (tracks by default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// Maximum entries to return (1-50; default 20).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Zero-based page offset (default 0).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
}

#[derive(Debug, Serialize)]
struct CollectionItem {
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    subtitle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    location: Option<String>,
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    r#ref: Option<String>,
    /// Artwork URL (#549), present only when the provider has art for this
    /// row. Always a same-origin `/api/collections/image?ref=...` path over
    /// an opaque token minted by `state.image_refs` -- never a provider
    /// image key or a raw remote URL, following the same opaque-ref
    /// discipline as `path`/`ref` above. Absent (not `null`-valued) when the
    /// provider has no art for this row, so a client can tell "no artwork"
    /// from "artwork omitted" the same way `subtitle` already does.
    #[serde(skip_serializing_if = "Option::is_none")]
    image: Option<String>,
}

/// Mint an image ref for a browse row and return the path a client resolves
/// it through, or `None` if this row's adapter response carried no image
/// key at all. Shared by all three provider handlers below so the URL shape
/// (`/api/collections/image?ref=...`) is defined in exactly one place.
/// Also used by `hifi_search`'s Roon mapping (#573 defect 10), so search
/// results and browse rows resolve artwork through one identical URL shape.
pub(crate) async fn mint_image(
    state: &AppState,
    zone_id: &str,
    image_key: Option<&str>,
) -> Option<String> {
    let image_key = image_key?;
    let token = state
        .image_refs
        .mint(crate::mcp::refs::ImageRef {
            zone_id: zone_id.to_string(),
            image_key: image_key.to_string(),
        })
        .await;
    Some(format!("/api/collections/image?ref={token}"))
}

#[derive(Debug, Serialize)]
struct CollectionPage {
    items: Vec<CollectionItem>,
    breadcrumbs: Vec<CollectionBreadcrumb>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_offset: Option<u32>,
}

pub async fn handle_collections(
    state: &AppState,
    args: HifiCollectionsTool,
) -> Result<CallToolResult, CallToolError> {
    if !ACTIONS.contains(&args.action.as_str()) {
        return Envelope::read("hifi_collections", "invalid_action").refused(
            "Unknown collection action.",
            Refusal::invalid_parameter("action", ACTIONS, "Choose a documented collection action."),
        );
    }
    let target = ZoneTarget::classify(&args.zone_id);
    let capability = match args.action.as_str() {
        "browse" => Capability::Browse,
        "playlists" => Capability::SavedPlaylists,
        _ => Capability::Favorites,
    };
    let env = Envelope::read("hifi_collections", &args.action)
        .param("zone_id", &*args.zone_id)
        .param("action", &*args.action)
        .param_opt("path", args.path.as_deref())
        .param_opt("location", args.location.as_deref())
        .param_opt("media_type", args.media_type.as_deref())
        .param_opt("limit", args.limit)
        .param_opt("offset", args.offset)
        .scope(Scope::for_zone(state, &args.zone_id, target.provider()).await);
    let route = target.for_library();
    if let LibraryRoute::Refused(_) = route {
        return env.failed("This zone has no library path");
    }

    if args.path.is_some() && args.location.is_some() {
        return env.refused(
            "path and location cannot be used together.",
            Refusal::invalid_parameter(
                "location",
                &["a durable location without path"],
                "Use location for canonical Library navigation or path for a short-lived MCP continuation, not both.",
            ),
        );
    }

    // A `path`'s provider-boundary check runs before anything else,
    // regardless of whether this zone's provider is wired to
    // `hifi_collections` at all: a ref minted for one provider must never be
    // treated as valid input for a different one, even a zone whose provider
    // this tool cannot yet browse. Each provider handler below re-resolves
    // the token itself to extract its specific shape; this pass only rules
    // out "wrong provider" and "unknown token" up front.
    if let Some(token) = args.path.as_deref() {
        match state.mcp_refs.resolve(token).await {
            Some(resolved) if resolved.provider() != target.provider() => {
                return refuse_foreign_path(env);
            }
            None => return refuse_unknown_path(env),
            Some(_) => {}
        }
    }
    if let Some(token) = args.location.as_deref() {
        let Some(location) = state.collection_locations.resolve(token) else {
            return refuse_unknown_location(env);
        };
        if !matches!(
            (location.provider(), target),
            ("roon", ZoneTarget::Roon)
                | ("lms", ZoneTarget::Lms)
                | ("musicassistant", ZoneTarget::MusicAssistant)
        ) {
            return refuse_foreign_location(env);
        }
    }

    // Spotify and Apple Music already expose (some of) this surface through
    // their own provider-specific tools (`hifi_spotify`, the Apple Music
    // tools), which is why `hifi_capabilities` can report `supported` for
    // them independent of this tool. Refusing here names that honestly
    // instead of either lying "not implemented" for a capability the
    // provider actually has, or silently trying an adapter operation this
    // tool never taught it (`collections_browse` and friends) and surfacing
    // whatever generic error falls out.
    if matches!(route, LibraryRoute::Spotify | LibraryRoute::AppleMusic) {
        let alternative = match route {
            LibraryRoute::Spotify => "hifi_spotify",
            _ => "the Apple Music companion tools",
        };
        return env.refused(
            format!(
                "hifi_collections does not reach {} zones yet; use {alternative} for this \
                 provider's collections today.",
                target.label()
            ),
            Refusal::NotImplemented {
                operation: capability.name().to_string(),
                tracked_by: "#531",
                alternatives: vec![alternative.to_string()],
                detail: format!(
                    "{alternative} already exposes this provider's content operations; #531 \
                     tracks adapting them behind the neutral hifi_collections contract."
                ),
            },
        );
    }

    // #573 defect 6 (sub-issue): a capability gap is not a backend failure.
    // This used to be `env.failed(...)`, which serializes as
    // `reason: backend_error` -- telling a client "retrying may work" about
    // Roon's unwired favorites. Report the capability table's own state
    // (`not_implemented`/`provider_limitation`) so the refusal and
    // `hifi_capabilities` cannot disagree.
    match support(target, capability) {
        Support::Supported => {}
        Support::NotImplemented {
            tracked_by,
            evidence,
        } => {
            return env.refused(
                format!(
                    "{} is not available for {} zones yet.",
                    capability.name(),
                    target.label()
                ),
                Refusal::NotImplemented {
                    operation: capability.name().to_string(),
                    tracked_by,
                    alternatives: Vec::new(),
                    detail: evidence.to_string(),
                },
            );
        }
        Support::Unsupported { evidence } => {
            return env.refused(
                format!(
                    "{} is not available for {} zones.",
                    capability.name(),
                    target.label()
                ),
                Refusal::ProviderLimitation {
                    operation: capability.name().to_string(),
                    alternatives: Vec::new(),
                    detail: evidence.to_string(),
                },
            );
        }
    }

    let limit = args.limit.unwrap_or(20).clamp(1, 50);
    let offset = args.offset.unwrap_or(0);

    match route {
        LibraryRoute::MusicAssistant => {
            handle_music_assistant(state, env, &args, limit, offset).await
        }
        LibraryRoute::Lms => handle_lms(state, env, &args, limit, offset).await,
        LibraryRoute::Roon => handle_roon(state, env, &args, limit, offset).await,
        // Handled above; exhaustive so a new route fails to compile here
        // rather than silently falling through.
        LibraryRoute::Spotify | LibraryRoute::AppleMusic => env.failed(
            "internal routing error: Spotify/Apple Music reached collections dispatch after \
             being refused above. This is a UHC bug.",
        ),
        LibraryRoute::Refused(_) => env.failed(
            "internal routing error: a refused zone reached collections dispatch. This is a \
             UHC bug.",
        ),
    }
}

// =============================================================================
// Music Assistant (#492): unchanged from before #531, minus the blanket gate.
// =============================================================================

async fn handle_music_assistant(
    state: &AppState,
    env: Envelope,
    args: &HifiCollectionsTool,
    limit: u32,
    offset: u32,
) -> Result<CallToolResult, CallToolError> {
    // Browse paths are server-side refs, just like playable media refs. MA's
    // native `library://…` paths contain provider implementation details and
    // must not become MCP client input.
    let parent_location = match (args.location.as_deref(), args.path.as_deref()) {
        (Some(token), None) => match state.collection_locations.resolve(token) {
            Some(location @ CollectionLocation::MusicAssistant { .. }) => location,
            Some(_) => return refuse_foreign_location(env),
            None => return refuse_unknown_location(env),
        },
        (None, Some(token)) => match state.mcp_refs.resolve(token).await {
            Some(RefTarget::MusicAssistantBrowse { location, .. }) => location,
            Some(_) => return refuse_foreign_path(env),
            None => return refuse_unknown_path(env),
        },
        (None, None) => CollectionLocation::MusicAssistant { steps: Vec::new() },
        (Some(_), Some(_)) => unreachable!("path/location exclusivity checked by caller"),
    };
    let provider_path = parent_location.last_provider_path();
    let operation = format!("collections_{}", args.action);
    let response = match state
        .adapter_registry
        .library_content(
            "musicassistant",
            &operation,
            &json!({
                "zone_id": args.zone_id,
                "path": provider_path,
                "media_type": args.media_type,
                "limit": limit,
                "offset": offset,
            }),
        )
        .await
    {
        Ok(value) => value,
        Err(error) => return env.failed(format!("Collection error: {error}")),
    };
    let next_offset = response
        .get("next_offset")
        .and_then(Value::as_u64)
        .map(|v| v as u32);
    let mut items = Vec::new();
    for item in response
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Untitled")
            .to_string();
        let subtitle = item
            .get("subtitle")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let (path, location) = match item.get("path").and_then(Value::as_str) {
            Some(provider_path) => {
                let target =
                    parent_location.appended(CollectionStep::new(title.clone(), provider_path));
                let location = match state.collection_locations.mint(target.clone()) {
                    Ok(token) => token,
                    Err(error) => {
                        return env.failed(format!(
                            "Collection error: could not preserve location: {error:#}"
                        ))
                    }
                };
                let path = state
                    .mcp_refs
                    .mint(RefTarget::MusicAssistantBrowse {
                        path: provider_path.to_string(),
                        title: title.clone(),
                        location: target,
                    })
                    .await;
                (Some(path), Some(location))
            }
            None => (None, None),
        };
        let r#ref = match item.get("uri").and_then(Value::as_str) {
            Some(uri) => Some(
                state
                    .mcp_refs
                    .mint(RefTarget::MusicAssistant {
                        uri: uri.to_string(),
                        title: title.clone(),
                    })
                    .await,
            ),
            None => None,
        };
        let image = mint_image(
            state,
            &args.zone_id,
            item.get("image_key").and_then(Value::as_str),
        )
        .await;
        items.push(CollectionItem {
            title,
            subtitle,
            path,
            location,
            r#ref,
            image,
        });
    }
    let breadcrumbs = match state.collection_locations.breadcrumbs(&parent_location) {
        Ok(breadcrumbs) => breadcrumbs,
        Err(error) => {
            return env.failed(format!(
                "Collection error: could not preserve breadcrumbs: {error:#}"
            ))
        }
    };
    Ok(env.json_result(&CollectionPage {
        items,
        breadcrumbs,
        next_offset,
    }))
}

// =============================================================================
// LMS (#531): albums/artists/playlists/favorites over the CLI/jsonrpc the
// adapter already speaks for search and playback.
// =============================================================================

async fn handle_lms(
    state: &AppState,
    env: Envelope,
    args: &HifiCollectionsTool,
    limit: u32,
    offset: u32,
) -> Result<CallToolResult, CallToolError> {
    // Same opaque-path split as Music Assistant: the plain collection path
    // this adapter invents ("albums", "album:<id>", ...) never becomes
    // client-visible; only the ref token is.
    let parent_location = match (args.location.as_deref(), args.path.as_deref()) {
        (Some(token), None) => match state.collection_locations.resolve(token) {
            Some(location @ CollectionLocation::Lms { .. }) => location,
            Some(_) => return refuse_foreign_location(env),
            None => return refuse_unknown_location(env),
        },
        (None, Some(token)) => match state.mcp_refs.resolve(token).await {
            Some(RefTarget::LmsBrowse { location, .. }) => location,
            Some(_) => return refuse_foreign_path(env),
            None => return refuse_unknown_path(env),
        },
        (None, None) => CollectionLocation::Lms { steps: Vec::new() },
        (Some(_), Some(_)) => unreachable!("path/location exclusivity checked by caller"),
    };
    let provider_path = parent_location.last_provider_path();
    let operation = format!("collections_{}", args.action);
    let response = match state
        .adapter_registry
        .library_content(
            "lms",
            &operation,
            &json!({
                "path": provider_path,
                "media_type": args.media_type,
                "limit": limit,
                "offset": offset,
            }),
        )
        .await
    {
        Ok(value) => value,
        Err(error) => return env.failed(format!("Collection error: {error}")),
    };
    let next_offset = response
        .get("next_offset")
        .and_then(Value::as_u64)
        .map(|v| v as u32);
    let mut items = Vec::new();
    for item in response
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Untitled")
            .to_string();
        let subtitle = item
            .get("subtitle")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        // A navigable row carries `path` (another collection to browse); a
        // playable leaf carries either `kind`+`id` (a durable LMS entity --
        // `LmsPlayTarget::Library`) or `url` (a favorite, which LMS gives no
        // durable id for -- `LmsPlayTarget::Url`). At most one of the three
        // is ever present on one row.
        let (path, location) = match item.get("path").and_then(Value::as_str) {
            Some(provider_path) => {
                let target =
                    parent_location.appended(CollectionStep::new(title.clone(), provider_path));
                let location = match state.collection_locations.mint(target.clone()) {
                    Ok(token) => token,
                    Err(error) => {
                        return env.failed(format!(
                            "Collection error: could not preserve location: {error:#}"
                        ))
                    }
                };
                let path = state
                    .mcp_refs
                    .mint(RefTarget::LmsBrowse {
                        path: provider_path.to_string(),
                        title: title.clone(),
                        location: target,
                    })
                    .await;
                (Some(path), Some(location))
            }
            None => (None, None),
        };
        let r#ref = match (
            item.get("kind").and_then(Value::as_str),
            item.get("id").and_then(Value::as_i64),
            item.get("url").and_then(Value::as_str),
        ) {
            (Some("track"), Some(id), _) => Some(
                state
                    .mcp_refs
                    .mint(RefTarget::Lms {
                        target: crate::adapters::lms::LmsPlayTarget::Library {
                            kind: crate::adapters::lms::LmsSearchResultType::Track,
                            id,
                        },
                        title: title.clone(),
                    })
                    .await,
            ),
            (_, _, Some(url)) => Some(
                state
                    .mcp_refs
                    .mint(RefTarget::Lms {
                        target: crate::adapters::lms::LmsPlayTarget::Url {
                            url: url.to_string(),
                        },
                        title: title.clone(),
                    })
                    .await,
            ),
            _ => None,
        };
        let image = mint_image(
            state,
            &args.zone_id,
            item.get("image_key").and_then(Value::as_str),
        )
        .await;
        items.push(CollectionItem {
            title,
            subtitle,
            path,
            location,
            r#ref,
            image,
        });
    }
    let breadcrumbs = match state.collection_locations.breadcrumbs(&parent_location) {
        Ok(breadcrumbs) => breadcrumbs,
        Err(error) => {
            return env.failed(format!(
                "Collection error: could not preserve breadcrumbs: {error:#}"
            ))
        }
    };
    Ok(env.json_result(&CollectionPage {
        items,
        breadcrumbs,
        next_offset,
    }))
}

// =============================================================================
// Roon (#531): the same browse/load session machinery `hifi_search` already
// drives (src/adapters/roon.rs), exposed as hierarchy walking.
// =============================================================================

/// Browse one Roon collection level.
///
/// # Cheap in the steady state, correct after a session loss
///
/// Roon's continuation is a `(item_key, multi_session_key)` pair, not a flat
/// string, and both halves only mean anything inside the Core-side browse
/// session that minted them (same reasoning as a playable Roon ref; see
/// [`RoonRefTarget`]'s docs). A durable `location` therefore cannot carry that
/// pair -- it names the walk, and the pair has to be reconstructed.
///
/// Reconstructing it means `collections_resolve_location`: a root browse plus
/// a paged scan per trail step. That is far too expensive to pay on every
/// request, and the Library page sends `location` for every level *and* every
/// "Load more" page. So the pair is treated as a cache, not as something to
/// recompute:
///
/// 1. Every navigable child minted below gets its `(session_key, item_key)`
///    written into [`AppState::collection_location_keys`] alongside its
///    location token -- free, since this response already holds both.
/// 2. A `location` request looks that pair up and browses with it directly.
///    A `path` request takes the same pair from its `RoonRefTarget`. From here
///    the two branches are one code path.
/// 3. If the Core rejects the pair *recoverably* (the session is gone), the
///    stale entry is evicted, the saved trail is re-walked once, the fresh
///    pair is cached, and the same node is retried -- rather than throwing the
///    user back to the root. A cache miss (first request for a location after
///    a restart, or an entry aged out) simply re-walks up front.
///
/// The cache is deliberately not durable: it holds live-session material, so
/// the first request per location after a restart re-walks once and every
/// later one is cheap again.
async fn handle_roon(
    state: &AppState,
    env: Envelope,
    args: &HifiCollectionsTool,
    limit: u32,
    offset: u32,
) -> Result<CallToolResult, CallToolError> {
    // Both entry shapes reduce to the same thing: the parent trail this page
    // hangs off, plus (maybe) a session-scoped key pair to resume it with.
    // `can_rewalk` is whether that trail has anything to re-walk -- a root
    // browse has no saved node to recover.
    let (mut resume_pair, parent_location, can_rewalk) =
        match (args.location.as_deref(), args.path.as_deref()) {
            (Some(token), None) => match state.collection_locations.resolve(token) {
                Some(location @ CollectionLocation::Roon { .. }) => {
                    let cached = state.collection_location_keys.get(token);
                    let can_rewalk = !location.steps().is_empty();
                    (cached, location, can_rewalk)
                }
                Some(_) => return refuse_foreign_location(env),
                None => return refuse_unknown_location(env),
            },
            (None, Some(token)) => match state.mcp_refs.resolve(token).await {
                Some(RefTarget::RoonBrowse {
                    target, location, ..
                }) => {
                    let can_rewalk = !target.trail.is_empty();
                    let RoonRefTarget {
                        item_key,
                        multi_session_key,
                        ..
                    } = target;
                    (Some((multi_session_key, item_key)), location, can_rewalk)
                }
                Some(_) => return refuse_foreign_path(env),
                None => return refuse_unknown_path(env),
            },
            (None, None) => {
                let steps = if args.action == "playlists" {
                    vec![CollectionStep::hidden_roon("Playlists")]
                } else {
                    Vec::new()
                };
                (
                    None,
                    CollectionLocation::Roon {
                        origin: RoonLocationOrigin::BrowseRoot,
                        steps,
                    },
                    false,
                )
            }
            (Some(_), Some(_)) => unreachable!("path/location exclusivity checked by caller"),
        };

    let mut params = json!({
        "zone_id": args.zone_id,
        "limit": limit,
        "offset": offset,
    });
    // Cache miss on a saved location: nothing to try, so re-walk up front.
    if resume_pair.is_none() && args.location.is_some() && can_rewalk {
        match resolve_and_cache_roon_location(state, &args.zone_id, &parent_location, args.location.as_deref())
            .await
        {
            Ok(pair) => resume_pair = Some(pair),
            Err(error) => {
                return env.failed(format!(
                    "Collection error: this saved Roon location can no longer be resolved safely: {error:#}"
                ))
            }
        }
    }
    if let Some((session_key, item_key)) = &resume_pair {
        params["item_key"] = json!(item_key);
        params["session_key"] = json!(session_key);
    }
    // #616: the trail this page hangs off. Children extend it by their own
    // title, so every ref minted below carries the full root-to-item walk
    // and can be re-resolved after its session is gone.
    let parent_trail: Vec<String> = parent_location
        .steps()
        .iter()
        .map(|step| step.title.clone())
        .collect();
    // Roon has no separate playlists/favorites protocol feature: both arrive
    // as named nodes in the same browse hierarchy (see the capability
    // table's note on this). `favorites` never reaches here -- `support()`
    // reports it `not_implemented` for Roon and the shared check above
    // already refused it.
    let operation = if !parent_location.steps().is_empty() && args.location.is_some() {
        "collections_browse"
    } else {
        match args.action.as_str() {
            "browse" => "collections_browse",
            "playlists" => "collections_playlists",
            other => {
                return env.failed(format!(
                    "internal routing error: unexpected hifi_collections action {other:?} reached \
                     Roon dispatch after the capability check. This is a UHC bug."
                ))
            }
        }
    };
    let response = match state
        .adapter_registry
        .library_content("roon", operation, &params)
        .await
    {
        Ok(value) => value,
        Err(error)
            if can_rewalk
                && resume_pair.is_some()
                && RoonBrowseError::from_error(&error)
                    .is_some_and(|rejection| rejection.kind.is_recoverable()) =>
        {
            // Roon item keys are scoped to a Core-side browse session. If the
            // Core reconnects, the cached pair is dead: drop it, re-walk the
            // saved breadcrumb trail, and retry the same node rather than
            // making the user start at the root. This arm is shared by
            // `location` and `path` requests -- both arrive here holding a
            // pair whose session may have expired.
            if let Some(token) = args.location.as_deref() {
                state.collection_location_keys.evict(token);
            }
            let (session_key, item_key) = match resolve_and_cache_roon_location(
                state,
                &args.zone_id,
                &parent_location,
                args.location.as_deref(),
            )
            .await
            {
                Ok(pair) => pair,
                Err(error) => {
                    return env.failed(format!(
                        "Collection error: this saved Roon location can no longer be resolved safely: {error:#}"
                    ))
                }
            };
            params["item_key"] = json!(item_key);
            params["session_key"] = json!(session_key);
            match state
                .adapter_registry
                .library_content("roon", operation, &params)
                .await
            {
                Ok(value) => value,
                Err(error) => return env.failed(format!("Collection error: {error}")),
            }
        }
        Err(error) => return env.failed(format!("Collection error: {error}")),
    };

    let Some(session_key) = response.get("session_key").and_then(Value::as_str) else {
        return env.failed("Collection error: Roon adapter returned no session_key");
    };
    // `RoonAdapter::content` already dropped grouping rows (headers,
    // result-count rows, and -- #545 -- the Action/ActionList rows Roon's
    // play-resolution walk uses internally) -- see that method's docs.
    // `navigable` and `playable` are independent booleans, not a single
    // either/or choice: #545 found albums and playlists that are both (you
    // can browse in to see tracks *and* play the whole thing directly) as
    // well as leaf tracks that are playable only (a browse-in would land on
    // nothing but the now-filtered action row). This loop mints whichever
    // ref(s) apply, possibly both, for the same `item_key`.
    let mut items = Vec::new();
    for item in response
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Untitled")
            .to_string();
        let subtitle = item
            .get("subtitle")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let item_key = item.get("item_key").and_then(Value::as_str);
        let navigable = item
            .get("navigable")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let playable = item
            .get("playable")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let position = item
            .get("position")
            .and_then(Value::as_u64)
            .map(|value| value as u32);
        let child_location = parent_location.appended(CollectionStep::roon(
            title.clone(),
            subtitle.clone(),
            position,
        ));
        let (path, location, r#ref) = match item_key {
            None => (None, None, None),
            Some(item_key) => {
                let mut trail = parent_trail.clone();
                trail.push(title.clone());
                let target = RoonRefTarget {
                    item_key: item_key.to_string(),
                    multi_session_key: session_key.to_string(),
                    trail,
                };
                let path = if navigable {
                    Some(
                        state
                            .mcp_refs
                            .mint(RefTarget::RoonBrowse {
                                target: target.clone(),
                                title: title.clone(),
                                location: child_location.clone(),
                            })
                            .await,
                    )
                } else {
                    None
                };
                let location = if navigable {
                    match state.collection_locations.mint(child_location) {
                        Ok(token) => {
                            // Free cache fill: this response already holds the
                            // session-scoped pair behind the token we just
                            // minted, so opening this row costs one browse
                            // instead of a re-walk of its trail.
                            state
                                .collection_location_keys
                                .insert(&token, session_key, item_key);
                            Some(token)
                        }
                        Err(error) => {
                            return env.failed(format!(
                                "Collection error: could not preserve location: {error:#}"
                            ))
                        }
                    }
                } else {
                    None
                };
                let r#ref = if playable {
                    Some(
                        state
                            .mcp_refs
                            .mint(RefTarget::Roon {
                                target,
                                title: title.clone(),
                            })
                            .await,
                    )
                } else {
                    None
                };
                (path, location, r#ref)
            }
        };
        let image = mint_image(
            state,
            &args.zone_id,
            item.get("image_key").and_then(Value::as_str),
        )
        .await;
        items.push(CollectionItem {
            title,
            subtitle,
            path,
            location,
            r#ref,
            image,
        });
    }
    let next_offset = response
        .get("next_offset")
        .and_then(Value::as_u64)
        .map(|v| v as u32);
    let breadcrumbs = match state.collection_locations.breadcrumbs(&parent_location) {
        Ok(breadcrumbs) => breadcrumbs,
        Err(error) => {
            return env.failed(format!(
                "Collection error: could not preserve breadcrumbs: {error:#}"
            ))
        }
    };
    Ok(env.json_result(&CollectionPage {
        items,
        breadcrumbs,
        next_offset,
    }))
}

/// [`resolve_roon_location`] plus the cache write every caller wants: a fresh
/// pair is only useful to the *next* request if it is remembered.
///
/// `token` is the location token this walk was performed for, when the request
/// carried one. A `path`-driven re-walk has no token in hand, so it derives the
/// canonical one for the same trail -- minting is deterministic and idempotent,
/// and the breadcrumb pass below mints this very location anyway.
async fn resolve_and_cache_roon_location(
    state: &AppState,
    zone_id: &str,
    location: &CollectionLocation,
    token: Option<&str>,
) -> anyhow::Result<(String, String)> {
    let (session_key, item_key) = resolve_roon_location(state, zone_id, location).await?;
    let token = token
        .map(ToOwned::to_owned)
        .or_else(|| state.collection_locations.mint(location.clone()).ok());
    if let Some(token) = token {
        state
            .collection_location_keys
            .insert(token, &session_key, &item_key);
    }
    Ok((session_key, item_key))
}

async fn resolve_roon_location(
    state: &AppState,
    zone_id: &str,
    location: &CollectionLocation,
) -> anyhow::Result<(String, String)> {
    let resolved = state
        .adapter_registry
        .library_content(
            "roon",
            "collections_resolve_location",
            &json!({
                "zone_id": zone_id,
                "location": location,
            }),
        )
        .await?;
    let session_key = resolved
        .get("session_key")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Roon adapter returned no session_key"))?;
    let item_key = resolved
        .get("item_key")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Roon adapter returned no item_key"))?;
    Ok((session_key.to_string(), item_key.to_string()))
}

// =============================================================================
// Shared refusals
// =============================================================================

fn refuse_foreign_path(env: Envelope) -> Result<CallToolResult, CallToolError> {
    env.refused(
        "path does not name a collection for this zone.",
        Refusal::invalid_parameter(
            "path",
            &["a path returned by hifi_collections for this zone"],
            "Browse again from this zone and use that result's path.",
        ),
    )
}

fn refuse_foreign_location(env: Envelope) -> Result<CallToolResult, CallToolError> {
    env.refused(
        "location does not name a collection for this zone.",
        Refusal::invalid_parameter(
            "location",
            &["a location returned by hifi_collections for this provider"],
            "Open the collection from the matching provider.",
        ),
    )
}

fn refuse_unknown_location(env: Envelope) -> Result<CallToolResult, CallToolError> {
    env.refused(
        "location is unknown.",
        Refusal::UnknownTarget {
            parameter: "location",
            discover_with: "hifi_collections",
            detail: "Open the collection from its provider root to create a durable location."
                .to_string(),
        },
    )
}

#[cfg(test)]
mod tab_gating_tests {
    use super::*;

    /// #573 defect 6: Roon serves Browse and Playlists only -- Favorites
    /// and Radio (both the `favorites` capability) are not wired, so the
    /// Library page must not render those tabs for Roon zones.
    #[test]
    fn roon_zones_serve_browse_and_playlists_only() {
        assert_eq!(
            collections_tabs_for_zone("roon:zone_1"),
            vec!["browse", "playlists"]
        );
    }

    /// The tab list is exactly the capability table's view: a provider this
    /// tool does not reach at all serves no tabs.
    #[test]
    fn unreached_providers_serve_no_tabs() {
        assert!(collections_tabs_for_zone("spotify:acct").is_empty());
        assert!(collections_tabs_for_zone("hqplayer:main").is_empty());
    }

    /// Music Assistant keeps all four tabs (favorites supported implies the
    /// Radio tab, which is the same capability under `media_type: radio`).
    #[test]
    fn music_assistant_serves_all_tabs() {
        assert_eq!(
            collections_tabs_for_zone("musicassistant:player_1"),
            vec!["browse", "playlists", "favorites", "radio"]
        );
    }
}

fn refuse_unknown_path(env: Envelope) -> Result<CallToolResult, CallToolError> {
    env.refused(
        "path is unknown or expired. Browse again from the collection root.",
        Refusal::UnknownTarget {
            parameter: "path",
            discover_with: "hifi_collections",
            detail: "Collection paths are short-lived opaque references. Call hifi_collections \
                      without path to start again."
                .to_string(),
        },
    )
}
