//! The tool registry: which tools exist, and which are advertised.
//!
//! Each family module below holds its tools' `#[mcp_tool]` definitions *and*
//! their handlers, so a tool's advertised contract (name, description, input
//! schema) sits next to the behavior that has to match it.
//!
//! # Adding a tool
//!
//! 1. Define it, with its handler, in the family module it belongs to (or a new
//!    one).
//! 2. **Append** it to the [`tool_box!`] list below.
//! 3. Add a dispatch arm in [`crate::mcp::handler`].
//! 4. Regenerate `tests/fixtures/mcp_tools.json` deliberately, and add the
//!    tool's parameters to `EXPECTED_TOOL_PARAMS` and any new returned fields to
//!    `FIELD_ROLES`, both in `tests/mcp_contract.rs`.
//!
//! Append rather than insert: `tools/list` order follows this list, and the
//! fixture pins it. Appending leaves the existing entries untouched, so the diff
//! reads as additive. Inserting mid-list shifts everything below it.

pub mod hqplayer;
pub mod library;
pub mod status;
pub mod transport;
pub mod zones;

use rust_mcp_sdk::schema::Tool;
use rust_mcp_sdk::tool_box;

pub use hqplayer::{
    HifiHqplayerLoadProfileTool, HifiHqplayerProfilesTool, HifiHqplayerSetPipelineTool,
    HifiHqplayerStatusTool,
};
pub use library::{HifiPlayTool, HifiSearchTool};
pub use status::HifiStatusTool;
pub use transport::HifiControlTool;
pub use zones::{HifiNowPlayingTool, HifiZonesTool};

// Generate the toolbox enum with all tools.
//
// This also generates `HifiTools::tools()` and
// `TryFrom<CallToolRequestParams>`, which together produce the exact
// `tools/list` bytes (via the `JsonSchema` derive) and parse incoming calls.
// Do not hand-roll either: `tests/fixtures/mcp_tools.json` pins the output.
tool_box!(
    HifiTools,
    [
        HifiZonesTool,
        HifiNowPlayingTool,
        HifiControlTool,
        HifiSearchTool,
        HifiPlayTool,
        HifiStatusTool,
        HifiHqplayerStatusTool,
        HifiHqplayerProfilesTool,
        HifiHqplayerLoadProfileTool,
        HifiHqplayerSetPipelineTool
    ]
);

/// The tools to advertise in `tools/list`.
///
/// HQPlayer tools are hidden when the adapter is disabled, because AGENTS.md
/// requires that every advertised tool work as documented — advertising the
/// HQPlayer surface with no HQPlayer behind it fails that.
///
/// Takes the flag rather than reading settings itself so the filter is testable
/// without touching the filesystem; the caller
/// ([`crate::mcp::handler::HifiMcpHandler::handle_list_tools_request`]) supplies
/// it from `load_app_settings()`.
pub fn list_tools(hqplayer_enabled: bool) -> Vec<Tool> {
    let mut tools = HifiTools::tools();
    if !hqplayer_enabled {
        tools.retain(|t| !t.name.starts_with("hifi_hqplayer"));
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_ten_tools_when_hqplayer_is_enabled() {
        assert_eq!(list_tools(true).len(), 10);
    }

    /// The filter must remove exactly the four HQPlayer tools and nothing else.
    #[test]
    fn hides_only_the_hqplayer_tools_when_disabled() {
        let enabled: Vec<String> = list_tools(true).into_iter().map(|t| t.name).collect();
        let disabled: Vec<String> = list_tools(false).into_iter().map(|t| t.name).collect();

        assert_eq!(disabled.len(), 6);
        assert!(disabled.iter().all(|n| !n.starts_with("hifi_hqplayer")));

        let removed: Vec<&String> = enabled.iter().filter(|n| !disabled.contains(n)).collect();
        assert_eq!(removed.len(), 4, "only the HQPlayer tools may be filtered");
        assert!(removed.iter().all(|n| n.starts_with("hifi_hqplayer")));

        // Relative order of the surviving tools is preserved.
        let surviving: Vec<&String> = enabled
            .iter()
            .filter(|n| !n.starts_with("hifi_hqplayer"))
            .collect();
        assert_eq!(surviving, disabled.iter().collect::<Vec<_>>());
    }
}
