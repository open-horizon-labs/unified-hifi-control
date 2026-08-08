use crate::api::AppState;
use crate::mcp::envelope::{Envelope, Refusal};
use crate::mcp::routing::ZoneTarget;
use rust_mcp_sdk::{
    macros::{mcp_tool, JsonSchema},
    schema::{schema_utils::CallToolError, CallToolResult},
};
use serde::{Deserialize, Serialize};

#[mcp_tool(
    name = "hifi_zone_group",
    description = "Inspect or change Music Assistant zone groups. action=status is read-only. join and leave require confirm=true because grouping changes playback topology."
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiZoneGroupTool {
    /// status, join, or leave.
    pub action: String,
    /// Music Assistant leader zone for join.
    #[serde(default)]
    pub leader_zone_id: Option<String>,
    /// Music Assistant member zones to add or leave.
    #[serde(default)]
    pub member_zone_ids: Option<Vec<String>>,
    /// Required true for topology-changing join and leave actions.
    #[serde(default)]
    pub confirm: Option<bool>,
}

pub async fn handle_zone_group(
    state: &AppState,
    args: HifiZoneGroupTool,
) -> Result<CallToolResult, CallToolError> {
    let write = matches!(args.action.as_str(), "join" | "leave");
    let env = if write {
        Envelope::write("hifi_zone_group", "multiroom_sync")
    } else {
        Envelope::read("hifi_zone_group", "multiroom_sync")
    };
    let member_zone_ids = args.member_zone_ids.unwrap_or_default();
    if write && args.confirm != Some(true) {
        return env.refused(
            "Zone grouping requires explicit confirm=true.",
            Refusal::invalid_parameter(
                "confirm",
                &["true"],
                "Joining or leaving zones can change their shared playback topology.",
            ),
        );
    }
    if write && member_zone_ids.is_empty() {
        return env.refused(
            "join and leave require at least one member_zone_id.",
            Refusal::invalid_parameter(
                "member_zone_ids",
                &["musicassistant:<player>"],
                "Specify one or more Music Assistant zones to change.",
            ),
        );
    }
    let operation = match args.action.as_str() {
        "status" => "multiroom_status",
        "join" => "multiroom_set_members",
        "leave" => "multiroom_ungroup",
        _ => {
            return env.refused(
                "Unknown zone group action.",
                Refusal::invalid_parameter(
                    "action",
                    &["status", "join", "leave"],
                    "Choose a documented zone group action.",
                ),
            )
        }
    };
    if args.action == "join"
        && args.leader_zone_id.as_deref().map(ZoneTarget::classify)
            != Some(ZoneTarget::MusicAssistant)
    {
        return env.refused(
            "join requires a musicassistant: leader_zone_id.",
            Refusal::invalid_parameter(
                "leader_zone_id",
                &["musicassistant:<player>"],
                "Groups are owned by Music Assistant.",
            ),
        );
    }
    if member_zone_ids
        .iter()
        .any(|id| ZoneTarget::classify(id) != ZoneTarget::MusicAssistant)
    {
        return env.refused(
            "Every member_zone_id must be a Music Assistant zone.",
            Refusal::invalid_parameter(
                "member_zone_ids",
                &["musicassistant:<player>"],
                "Cross-provider zone groups are not supported.",
            ),
        );
    }
    let params = serde_json::json!({"leader_zone_id": args.leader_zone_id, "member_zone_ids": member_zone_ids});
    match state
        .adapter_registry
        .library_content("musicassistant", operation, &params)
        .await
    {
        Ok(value) => Ok(env.json_result(&value)),
        Err(error) => env.failed(format!("Zone group error: {error}")),
    }
}
