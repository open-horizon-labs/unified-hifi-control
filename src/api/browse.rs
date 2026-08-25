//! HTTP surface for library browse, queue and play-ref, for the web UI (#507).
//!
//! # One contract, two consumers
//!
//! `hifi_collections`, `hifi_queue` and `hifi_play_ref` are MCP tools with a
//! fully worked-out verb vocabulary: capability checks, opaque browse paths
//! and playable refs (so no provider URI reaches a client), and a structured
//! envelope describing what happened. The web UI needs exactly the same
//! verbs -- browse a collection, transfer a queue, play or queue a browsed
//! item -- so these handlers call the MCP tool handlers directly rather than
//! re-implementing any of that against `AdapterRegistry` a second time.
//!
//! `crate::mcp::refs::RefTable` (`AppState::mcp_refs`) is a plain server-side
//! table keyed by opaque token, not an MCP-transport concept, so a ref minted
//! for a browse result served over this HTTP surface resolves the same way a
//! ref minted for an MCP client would. The two consumers share one path
//! server-side; only the transport (HTTP JSON here, JSON-RPC there) differs.
//!
//! Every response body is the tool's own envelope
//! (`structuredContent`/`outcome`/`data`/`refusal`, see
//! [`crate::mcp::envelope`]), verbatim. The HTTP status is derived from
//! `outcome` so a browser client can branch on the status without parsing the
//! body, while the body still carries the same detail an MCP client sees.

use crate::api::AppState;
use crate::mcp::tools::collections::{handle_collections, HifiCollectionsTool};
use crate::mcp::tools::library::{handle_play_ref, handle_search, HifiPlayRefTool, HifiSearchTool};
use crate::mcp::tools::queue::{handle_queue, HifiQueueTool};
use axum::{extract::State, http::StatusCode, Json};
use rust_mcp_sdk::schema::{schema_utils::CallToolError, CallToolResult};
use serde_json::Value;

/// Project a tool handler's result onto an HTTP response.
///
/// The body is the envelope `structuredContent` carries (or `null` if a tool
/// somehow attached none -- unreachable today, see
/// [`crate::mcp::envelope::Envelope::structured`]), verbatim. The HTTP status
/// stays `200` for every envelope outcome, refusals included: `outcome` is
/// already the machine-readable signal (`ok`/`accepted`/`unsupported`/
/// `invalid`/`error`, see [`crate::mcp::envelope::Outcome`]) and `refusal`
/// carries the detail, so folding that into HTTP status codes as well would
/// just be two places for a client to check the same fact. `500` is reserved
/// for the one case the envelope cannot describe: the MCP transport layer
/// itself erroring before a tool ran.
fn envelope_response(result: Result<CallToolResult, CallToolError>) -> (StatusCode, Json<Value>) {
    match result {
        Ok(call_result) => {
            let body = call_result
                .structured_content
                .map(Value::Object)
                .unwrap_or(Value::Null);
            (StatusCode::OK, Json(body))
        }
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": error.to_string()})),
        ),
    }
}

/// `POST /api/collections` -- browse, playlists or favorites for one zone.
/// Same verb and paging semantics as the `hifi_collections` MCP tool.
pub async fn collections_handler(
    State(state): State<AppState>,
    Json(args): Json<HifiCollectionsTool>,
) -> (StatusCode, Json<Value>) {
    envelope_response(handle_collections(&state, args).await)
}

/// `POST /api/queue` -- read, jump, reorder, remove, clear, or transfer a
/// zone's queue. Same verb vocabulary as the `hifi_queue` MCP tool.
pub async fn queue_handler(
    State(state): State<AppState>,
    Json(args): Json<HifiQueueTool>,
) -> (StatusCode, Json<Value>) {
    envelope_response(handle_queue(&state, args).await)
}

/// `POST /api/play_ref` -- play or queue a ref minted by `/api/collections`.
/// Same verb vocabulary as the `hifi_play_ref` MCP tool.
pub async fn play_ref_handler(
    State(state): State<AppState>,
    Json(args): Json<HifiPlayRefTool>,
) -> (StatusCode, Json<Value>) {
    envelope_response(handle_play_ref(&state, args).await)
}

/// `POST /api/search` -- catalog search across the zone's provider, minting
/// the same short-lived refs `/api/collections` does. Added for #550's
/// unified Library search field: the field filters the current browse level
/// locally *and* fires this endpoint (debounced) for "Everywhere" results.
/// Same verb vocabulary as the `hifi_search` MCP tool -- this is a thin HTTP
/// mirror, not a new capability (see this module's doc comment).
pub async fn search_handler(
    State(state): State<AppState>,
    Json(args): Json<HifiSearchTool>,
) -> (StatusCode, Json<Value>) {
    envelope_response(handle_search(&state, args).await)
}
