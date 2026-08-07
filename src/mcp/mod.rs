//! MCP (Model Context Protocol) server for AI assistant integration.
//!
//! Provides HTTP endpoints for MCP clients over the Streamable HTTP transport.
//! Routes are integrated into the main Axum app on port 8088 at `/mcp`.
//!
//! # Layout
//!
//! This module is the transport and wiring layer only. Everything else lives in
//! a submodule, so concurrent work on the MCP surface (epic #392) edits
//! different files:
//!
//! | Module      | Responsibility                                           |
//! |-------------|----------------------------------------------------------|
//! | `mod.rs`     | Axum route handlers, session recovery, SDK wiring         |
//! | [`server`]   | the `initialize` result: identity, capabilities, protocol |
//! | [`handler`]  | the `ServerHandler` impl: advertise tools, dispatch calls |
//! | [`tools`]    | tool definitions + handlers, by family                    |
//! | [`types`]    | response shapes and result constructors                   |
//! | [`envelope`] | the structured result envelope every tool fills           |
//! | [`routing`]  | zone-prefix -> adapter, in exactly one place              |
//! | [`capabilities`] | per-provider capability truth, in three states        |
//! | [`refs`]     | the opaque ref table `hifi_search` mints into and `hifi_play_ref` resolves from (#396) |
//! | [`resources`] | readable bridge state addressable by URI (#397)          |
//!
//! # The MCP surface is pinned by tests
//!
//! `tests/mcp_contract.rs` snapshots `tools/list` and `initialize` against
//! committed fixtures and pins every tool's runtime behavior, including the
//! error strings a model reads. Epic #392 is additive-only, so a fixture diff on
//! a PR in that epic is a bug report, not a chore. Regenerate deliberately:
//!
//! ```sh
//! UPDATE_MCP_FIXTURES=1 cargo test --test mcp_contract
//! ```

pub mod capabilities;
pub mod envelope;
pub mod feedback;
pub mod handler;
pub mod listening_plan;
pub mod observation_history;
pub mod refs;
pub mod resources;
pub mod routing;
pub mod server;
pub mod tools;
pub mod types;

use crate::api::AppState;
use axum::http::{HeaderMap, Method, Uri};
use axum::{body::Body, extract::Extension, response::IntoResponse};
use rust_mcp_sdk::{
    id_generator::{FastIdGenerator, UuidGenerator},
    mcp_server::{McpAppState, McpHttpHandler, ToMcpServerHandler},
    session_store::InMemorySessionStore,
    TransportOptions,
};
use std::{sync::Arc, time::Duration};

pub use handler::HifiMcpHandler;
pub use server::server_details;
pub use tools::HifiTools;

/// MCP session header name
const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";

// ============================================================================
// MCP State Container (for Extension layer)
// ============================================================================

/// Container for MCP-specific state, passed via Extension
#[derive(Clone)]
pub struct McpExtState {
    pub mcp_state: Arc<McpAppState>,
    pub http_handler: Arc<McpHttpHandler>,
}

// ============================================================================
// Axum Route Handlers (mirrors rust-mcp-sdk's internal handlers)
// ============================================================================

pub async fn handle_mcp_get(
    headers: HeaderMap,
    uri: Uri,
    Extension(ext): Extension<McpExtState>,
) -> impl IntoResponse {
    let request = McpHttpHandler::create_request(Method::GET, uri, headers, None);
    match ext
        .http_handler
        .handle_streamable_http(request, ext.mcp_state)
        .await
    {
        Ok(res) => {
            let (parts, body) = res.into_parts();
            axum::response::Response::from_parts(parts, Body::new(body))
        }
        // Response builder with valid status/body cannot fail
        #[allow(clippy::unwrap_used)]
        Err(e) => axum::response::Response::builder()
            .status(500)
            .body(Body::from(format!("MCP error: {}", e)))
            .unwrap(),
    }
}

pub async fn handle_mcp_post(
    headers: HeaderMap,
    uri: Uri,
    Extension(ext): Extension<McpExtState>,
    payload: String,
) -> impl IntoResponse {
    // Check for stale session and auto-recover
    let headers = match auto_recover_session(&headers, &uri, &ext, &payload).await {
        Some(new_headers) => new_headers,
        None => headers,
    };

    let request = McpHttpHandler::create_request(Method::POST, uri, headers, Some(&payload));
    match ext
        .http_handler
        .handle_streamable_http(request, ext.mcp_state)
        .await
    {
        Ok(res) => {
            let (parts, body) = res.into_parts();
            axum::response::Response::from_parts(parts, Body::new(body))
        }
        // Response builder with valid status/body cannot fail
        #[allow(clippy::unwrap_used)]
        Err(e) => axum::response::Response::builder()
            .status(500)
            .body(Body::from(format!("MCP error: {}", e)))
            .unwrap(),
    }
}

/// Check if client has a stale session and auto-initialize a new one.
/// Returns new headers with fresh session ID, or None if no recovery needed.
async fn auto_recover_session(
    headers: &HeaderMap,
    uri: &Uri,
    ext: &McpExtState,
    _payload: &str,
) -> Option<HeaderMap> {
    // Get session ID from header
    let session_id = headers
        .get(MCP_SESSION_ID_HEADER)
        .and_then(|v| v.to_str().ok())?;

    // Check if session exists
    if ext
        .mcp_state
        .session_store
        .has(&session_id.to_string())
        .await
    {
        return None; // Session is valid, no recovery needed
    }

    tracing::info!(
        "MCP session '{}' not found, auto-initializing new session",
        session_id
    );

    // Create initialize request to get a new session
    let init_payload = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"auto-recovery","version":"1.0"}}}"#;

    // Create headers without the stale session ID (so SDK creates new session)
    let mut init_headers = headers.clone();
    init_headers.remove(MCP_SESSION_ID_HEADER);

    let init_request =
        McpHttpHandler::create_request(Method::POST, uri.clone(), init_headers, Some(init_payload));

    // Process initialize request
    let init_response = ext
        .http_handler
        .handle_streamable_http(init_request, ext.mcp_state.clone())
        .await
        .ok()?;

    // Extract new session ID from response headers
    let new_session_id = init_response
        .headers()
        .get(MCP_SESSION_ID_HEADER)
        .and_then(|v| v.to_str().ok())?;

    tracing::info!("Auto-initialized new MCP session: {}", new_session_id);

    // Create new headers with the fresh session ID
    let mut new_headers = headers.clone();
    new_headers.remove(MCP_SESSION_ID_HEADER);
    new_headers.insert(MCP_SESSION_ID_HEADER, new_session_id.parse().ok()?);

    Some(new_headers)
}

pub async fn handle_mcp_delete(
    headers: HeaderMap,
    uri: Uri,
    Extension(ext): Extension<McpExtState>,
) -> impl IntoResponse {
    let request = McpHttpHandler::create_request(Method::DELETE, uri, headers, None);
    match ext
        .http_handler
        .handle_streamable_http(request, ext.mcp_state)
        .await
    {
        Ok(res) => {
            let (parts, body) = res.into_parts();
            axum::response::Response::from_parts(parts, Body::new(body))
        }
        // Response builder with valid status/body cannot fail
        #[allow(clippy::unwrap_used)]
        Err(e) => axum::response::Response::builder()
            .status(500)
            .body(Body::from(format!("MCP error: {}", e)))
            .unwrap(),
    }
}

// ============================================================================
// Router Creation
// ============================================================================

/// Create MCP extension layer for the main Axum app
///
/// Call this to get the extension layer, then add MCP routes and the layer to your router.
pub fn create_mcp_extension(state: AppState) -> axum::Extension<McpExtState> {
    let handler = HifiMcpHandler::new(state);

    // Create MCP app state (mirrors what HyperServer does internally)
    let mcp_state: Arc<McpAppState> = Arc::new(McpAppState {
        session_store: Arc::new(InMemorySessionStore::new()),
        id_generator: Arc::new(UuidGenerator {}),
        stream_id_gen: Arc::new(FastIdGenerator::new(Some("s_"))),
        server_details: Arc::new(server::server_details()),
        handler: handler.to_mcp_server_handler(),
        ping_interval: Duration::from_secs(12),
        transport_options: Arc::new(TransportOptions::default()),
        enable_json_response: false,
        event_store: None,
        task_store: None,
        client_task_store: None,
    });

    // Create HTTP handler (no auth, no middleware)
    let http_handler = Arc::new(McpHttpHandler::new(vec![]));

    // Bundle into extension state
    let ext_state = McpExtState {
        mcp_state,
        http_handler,
    };

    tracing::info!("MCP endpoint available at /mcp (Streamable HTTP)");

    Extension(ext_state)
}
