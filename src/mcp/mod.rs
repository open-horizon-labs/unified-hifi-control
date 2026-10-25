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
//! | `mod.rs`     | Axum route handlers, stateless HTTP, SDK wiring         |
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
pub mod collection_locations;
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
    mcp_http::{McpAppState, McpHttpHandler},
    mcp_server::ToMcpServerHandler,
    TransportOptions,
};
use std::{sync::Arc, time::Duration};

pub use handler::HifiMcpHandler;
pub use server::server_details;
pub use tools::HifiTools;

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
        id_generator: Arc::new(UuidGenerator {}),
        stream_id_gen: Arc::new(FastIdGenerator::new(Some("s_"))),
        server_details: Arc::new(server::server_details()),
        handler: handler.to_mcp_server_handler(),
        ping_interval: Duration::from_secs(12),
        transport_options: Arc::new(TransportOptions::default()),
        enable_json_response: false,
        message_observer: None,
        extensions: Arc::new(tokio::sync::RwLock::new(None)),
        active_listen_streams: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        max_listen_streams: 256,
    });

    // Create HTTP handler (no auth, no middleware)
    let http_handler = Arc::new(McpHttpHandler::new(vec![], None));

    // Bundle into extension state
    let ext_state = McpExtState {
        mcp_state,
        http_handler,
    };

    tracing::info!("MCP endpoint available at /mcp (Streamable HTTP)");

    Extension(ext_state)
}
