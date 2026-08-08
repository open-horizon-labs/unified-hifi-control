//! Provider-neutral read-only library collections.
//!
//! The wire contract deliberately speaks only of collections, paths, pages and
//! opaque playable refs. Provider adapters translate those concepts to their
//! native library APIs; no provider URI or identifier is returned to a client.

use crate::api::AppState;
use crate::mcp::capabilities::{support, Capability, Support};
use crate::mcp::envelope::{Envelope, Refusal, Scope};
use crate::mcp::refs::RefTarget;
use crate::mcp::routing::{LibraryRoute, ZoneTarget};
use rust_mcp_sdk::{
    macros::{mcp_tool, JsonSchema},
    schema::{schema_utils::CallToolError, CallToolResult},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const ACTIONS: &[&str] = &["browse", "playlists", "favorites"];

#[mcp_tool(
    name = "hifi_collections",
    description = "Browse a provider library or list saved playlists and favorites. Results are paged with limit/offset and playable entries include a short-lived opaque ref for hifi_play_ref. Use path from a browse entry to continue into that collection."
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiCollectionsTool {
    /// Zone ID whose provider library is queried.
    pub zone_id: String,
    /// browse, playlists, or favorites.
    pub action: String,
    /// Opaque collection path returned by a preceding browse call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
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
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    r#ref: Option<String>,
}

#[derive(Debug, Serialize)]
struct CollectionPage {
    items: Vec<CollectionItem>,
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
        .param_opt("media_type", args.media_type.as_deref())
        .param_opt("limit", args.limit)
        .param_opt("offset", args.offset)
        .scope(Scope::for_zone(state, &args.zone_id, target.provider()).await);
    let prefix = match target.for_library() {
        LibraryRoute::Roon => "roon",
        LibraryRoute::Lms => "lms",
        LibraryRoute::Spotify => "spotify",
        LibraryRoute::AppleMusic => "applemusic",
        LibraryRoute::MusicAssistant => "musicassistant",
        LibraryRoute::Refused(_) => return env.failed("This zone has no library path"),
    };
    // Browse paths are server-side refs, just like playable media refs. MA's
    // native `library://…` paths contain provider implementation details and
    // must not become MCP client input.
    let provider_path = match args.path.as_deref() {
        None => None,
        Some(token) => match state.mcp_refs.resolve(token).await {
            Some(RefTarget::MusicAssistantBrowse { path, .. })
                if matches!(target, ZoneTarget::MusicAssistant) => Some(path),
            Some(_) => {
                return env.refused(
                    "path does not name a collection for this zone.",
                    Refusal::invalid_parameter(
                        "path",
                        &["a path returned by hifi_collections for this zone"],
                        "Browse again from this zone and use that result's path.",
                    ),
                )
            }
            None => {
                return env.refused(
                    "path is unknown or expired. Browse again from the collection root.",
                    Refusal::UnknownTarget {
                        parameter: "path",
                        discover_with: "hifi_collections",
                        detail: "Collection paths are short-lived opaque references. Call hifi_collections without path to start again.".to_string(),
                    },
                )
            }
        },
    };
    // This provider-neutral surface has a route vocabulary for every library
    // adapter, but #492 implements the first adapter slice only. Resolve an
    // opaque path first so a path minted for MA is never sent to another
    // provider, even when that provider does not yet implement collections.
    if !matches!(target, ZoneTarget::MusicAssistant) {
        return env.failed("Collections are not implemented for this provider yet.");
    }
    if !matches!(support(target, capability), Support::Supported) {
        return env.failed(format!(
            "{} is not available for {} zones",
            capability.name(),
            target.label()
        ));
    }
    let limit = args.limit.unwrap_or(20).clamp(1, 50);
    let offset = args.offset.unwrap_or(0);
    let operation = format!("collections_{}", args.action);
    let response = match state
        .adapter_registry
        .library_content(
            prefix,
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
        let path = match (target, item.get("path").and_then(Value::as_str)) {
            (ZoneTarget::MusicAssistant, Some(path)) => Some(
                state
                    .mcp_refs
                    .mint(RefTarget::MusicAssistantBrowse {
                        path: path.to_string(),
                        title: title.clone(),
                    })
                    .await,
            ),
            _ => None,
        };
        let r#ref = match (target, item.get("uri").and_then(Value::as_str)) {
            (ZoneTarget::MusicAssistant, Some(uri)) => Some(
                state
                    .mcp_refs
                    .mint(RefTarget::MusicAssistant {
                        uri: uri.to_string(),
                        title: title.clone(),
                    })
                    .await,
            ),
            _ => None,
        };
        items.push(CollectionItem {
            title,
            subtitle,
            path,
            r#ref,
        });
    }
    Ok(env.json_result(&CollectionPage { items, next_offset }))
}
