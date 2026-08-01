//! Overall bridge status.

use crate::api::AppState;
use crate::mcp::envelope::Envelope;
use rust_mcp_sdk::{
    macros::{mcp_tool, JsonSchema},
    schema::{schema_utils::CallToolError, CallToolResult},
};
use serde::{Deserialize, Serialize};

/// Get overall bridge status
#[mcp_tool(
    name = "hifi_status",
    description = "Get overall bridge status (Roon connection, HQPlayer config)",
    read_only_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiStatusTool {}

/// # Do not convert this to a typed struct
///
/// `serde_json` without the `preserve_order` feature serializes maps as
/// `BTreeMap`, i.e. alphabetically, so this payload emits `hqplayer` before
/// `roon`. A struct would serialize in declaration order instead and silently
/// reorder the keys in the text clients receive.
///
/// `tests/mcp_contract.rs::hifi_status_shape_is_pinned` asserts the ordering, so
/// the mistake is caught — but it is cheaper to not make it.
/// Shared by [`handle_status`] and the `hifi://status` resource
/// ([`crate::mcp::resources`]), so the tool and the resource read exactly the
/// same adapter accessors and cannot disagree.
pub async fn status_payload(state: &AppState) -> serde_json::Value {
    let roon_status = state.roon.get_status().await;
    let hqp_status = state.hqplayer.get_status().await;

    serde_json::json!({
        "roon": {
            "connected": roon_status.connected,
            "core_name": roon_status.core_name,
        },
        "hqplayer": {
            "connected": hqp_status.connected,
            "host": hqp_status.host,
        }
    })
}

pub async fn handle_status(state: &AppState) -> Result<CallToolResult, CallToolError> {
    // No scope: this tool reports on the bridge, spanning every provider. The
    // `json!` value is already a map, so `data` and the text agree exactly here —
    // the declaration-order/BTreeMap divergence only applies to struct payloads.
    Ok(Envelope::read("hifi_status", "get_status").json_result(&status_payload(state).await))
}
