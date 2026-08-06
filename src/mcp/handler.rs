//! The MCP `ServerHandler`: advertise tools, dispatch calls to them, and (#397)
//! advertise and serve resources.
//!
//! Dispatch only. Every arm delegates to a handler in [`crate::mcp::tools`] or
//! [`crate::mcp::resources`]; no tool or resource behavior lives here.

use crate::api::{load_app_settings, AppState};
use crate::mcp::envelope::{Envelope, Refusal};
use crate::mcp::tools::{self, HifiTools};
use async_trait::async_trait;
use rust_mcp_sdk::{
    mcp_server::ServerHandler,
    schema::{
        schema_utils::CallToolError, CallToolRequestParams, CallToolResult,
        ListResourceTemplatesResult, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
        ReadResourceRequestParams, ReadResourceResult, RpcError,
    },
    McpServer,
};
use std::sync::Arc;

/// MCP server handler with access to app state.
pub struct HifiMcpHandler {
    state: AppState,
}

impl HifiMcpHandler {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

#[async_trait]
impl ServerHandler for HifiMcpHandler {
    /// Fires once per session, when the client sends `notifications/initialized`
    /// — the SDK hands us that session's own `runtime` here, which is the only
    /// place a per-session background notifier can be started. See
    /// [`crate::mcp::resources::spawn_list_changed_notifier`] for what this
    /// notifier does and does not promise.
    async fn on_initialized(&self, runtime: Arc<dyn McpServer>) {
        crate::mcp::resources::spawn_list_changed_notifier(
            self.state.bus.clone(),
            runtime,
            self.state.shutdown.clone(),
        );
    }

    async fn handle_list_tools_request(
        &self,
        _params: Option<PaginatedRequestParams>,
        _runtime: Arc<dyn McpServer>,
    ) -> Result<ListToolsResult, RpcError> {
        // Read settings per request: the operator can toggle the HQPlayer
        // adapter while the server is running, and the advertised tool list has
        // to follow.
        let settings = load_app_settings();

        Ok(ListToolsResult {
            meta: None,
            next_cursor: None,
            tools: tools::list_tools(settings.adapters.hqplayer),
        })
    }

    async fn handle_call_tool_request(
        &self,
        params: CallToolRequestParams,
        _runtime: Arc<dyn McpServer>,
    ) -> Result<CallToolResult, CallToolError> {
        // Argument parsing happens before any tool runs, and its failures are the
        // commonest `invalid` a real client produces — a required parameter left
        // out. The SDK turns them into `content: [text], isError: true`, which a
        // client cannot tell apart from a tool's own result, so #395's envelope
        // has to reach them too. `requested_tool_name` is captured first because
        // `try_from` consumes `params`.
        let requested = params.name.clone();
        let tool: HifiTools = match HifiTools::try_from(params) {
            Ok(tool) => tool,
            Err(e) => {
                let error = CallToolError::new(e);
                return Ok(match tools::static_name(&requested) {
                    // A known tool with bad arguments: the client can fix this,
                    // and the envelope says which parameter and how.
                    Some(name) => invalid_arguments_envelope(name, &error).errored_result(error),
                    // An unknown tool name has no tool to scope an envelope to,
                    // and inventing one would claim a surface that does not exist.
                    // Deliberately left bare; asserted in the contract tests so
                    // the line is a decision rather than an oversight.
                    None => CallToolResult::with_error(error),
                });
            }
        };
        let state = &self.state;

        match tool {
            HifiTools::HifiZonesTool(_) => tools::zones::handle_zones(state).await,
            HifiTools::HifiNowPlayingTool(args) => {
                tools::zones::handle_now_playing(state, args).await
            }
            HifiTools::HifiControlTool(args) => tools::transport::handle_control(state, args).await,
            HifiTools::HifiSearchTool(args) => tools::library::handle_search(state, args).await,
            HifiTools::HifiPlayTool(args) => tools::library::handle_play(state, args).await,
            HifiTools::HifiStatusTool(_) => tools::status::handle_status(state).await,
            HifiTools::HifiHqplayerStatusTool(_) => tools::hqplayer::handle_status(state).await,
            HifiTools::HifiHqplayerProfilesTool(_) => tools::hqplayer::handle_profiles(state).await,
            HifiTools::HifiHqplayerLoadProfileTool(args) => {
                tools::hqplayer::handle_load_profile(state, args).await
            }
            HifiTools::HifiHqplayerSetPipelineTool(args) => {
                tools::hqplayer::handle_set_pipeline(state, args).await
            }
            HifiTools::HifiCapabilitiesTool(args) => {
                tools::capabilities::handle_capabilities(state, args).await
            }
            HifiTools::HifiPlayRefTool(args) => tools::library::handle_play_ref(state, args).await,
            HifiTools::HifiQueueTool(args) => tools::queue::handle_queue(state, args).await,
            HifiTools::HifiSpotifyTool(args) => tools::spotify::handle_spotify(state, args).await,
        }
    }

    /// Same settings-based gate as `handle_list_tools_request`: the HQPlayer
    /// resources are hidden when the adapter is disabled, exactly like the
    /// HQPlayer tools.
    async fn handle_list_resources_request(
        &self,
        _params: Option<PaginatedRequestParams>,
        _runtime: Arc<dyn McpServer>,
    ) -> Result<ListResourcesResult, RpcError> {
        let settings = load_app_settings();
        Ok(ListResourcesResult {
            meta: None,
            next_cursor: None,
            resources: crate::mcp::resources::list_resources(
                &self.state,
                settings.adapters.hqplayer,
            )
            .await,
        })
    }

    /// No resource template is advertised — #397's solution-space gate chose
    /// live per-zone enumeration in `resources/list` instead (a newly
    /// discovered zone needs no restart there either). This returns a fixed
    /// empty list rather than the SDK's generic `method_not_found`, because
    /// resources genuinely exist here; there is just no template shape for any
    /// of them.
    async fn handle_list_resource_templates_request(
        &self,
        _params: Option<PaginatedRequestParams>,
        _runtime: Arc<dyn McpServer>,
    ) -> Result<ListResourceTemplatesResult, RpcError> {
        Ok(ListResourceTemplatesResult {
            meta: None,
            next_cursor: None,
            resource_templates: vec![],
        })
    }

    async fn handle_read_resource_request(
        &self,
        params: ReadResourceRequestParams,
        _runtime: Arc<dyn McpServer>,
    ) -> Result<ReadResourceResult, RpcError> {
        crate::mcp::resources::read_resource(&self.state, &params.uri).await
    }
}

/// The envelope for a call whose arguments did not deserialize.
///
/// The parameter name has to be recovered from the deserializer's message
/// (`"missing field \`zone_id\`"`, `"invalid type: ..."`) because that is all the
/// SDK surfaces — `try_from` returns a `serde` error, not a structured field path.
/// So the parameter is reported when it can be identified against the tool's
/// declared inputs and omitted when it cannot, rather than guessed at.
fn invalid_arguments_envelope(tool: &'static str, error: &CallToolError) -> Envelope {
    let message = error.to_string();
    let parameter = tools::static_param(tool, &message);

    // `write` rather than `read` even for the read-only tools. Unobservable —
    // `refuse` overwrites the outcome and no envelope field records read vs.
    // write — but stated because a future field derived from that distinction
    // would be wrong here, and because a parse failure genuinely is neither.
    let env = Envelope::write(tool, "parse_arguments");
    match parameter {
        Some(parameter) => env.refuse(Refusal::InvalidParameter {
            parameter,
            accepted: vec![format!("see the {tool} inputSchema in tools/list")],
            detail: format!(
                "The arguments for {tool} did not deserialize: {message}. Fix {parameter} \
                 and call again."
            ),
        }),
        None => env.refuse(Refusal::InvalidParameter {
            parameter: "arguments",
            accepted: vec![format!("see the {tool} inputSchema in tools/list")],
            detail: format!(
                "The arguments for {tool} did not deserialize: {message}. UHC could not \
                 identify which parameter from the deserializer's message."
            ),
        }),
    }
}
