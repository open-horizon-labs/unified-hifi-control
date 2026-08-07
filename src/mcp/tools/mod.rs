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

pub mod apple_music;
pub mod capabilities;
pub mod hqplayer;
pub mod library;
pub mod queue;
pub mod spotify;
pub mod status;
pub mod transport;
pub mod zones;

use rust_mcp_sdk::schema::Tool;
use rust_mcp_sdk::tool_box;

pub use apple_music::HifiAppleMusicTool;
pub use capabilities::HifiCapabilitiesTool;
pub use hqplayer::{
    HifiHqplayerLoadProfileTool, HifiHqplayerProfilesTool, HifiHqplayerSetPipelineTool,
    HifiHqplayerStatusTool,
};
pub use library::{HifiPlayRefTool, HifiPlayTool, HifiSearchTool};
pub use queue::HifiQueueTool;
pub use spotify::HifiSpotifyTool;
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
        HifiHqplayerSetPipelineTool,
        // Appended by #398. Appending is the whole rule: the ten above keep
        // their positions, so `tools/list` reads as an additive diff.
        HifiCapabilitiesTool,
        // Appended by #396 (opaque content refs): the one consumer of
        // hifi_search's new `ref` field.
        HifiPlayRefTool,
        HifiQueueTool,
        HifiSpotifyTool,
        HifiAppleMusicTool
    ]
);

/// Every tool name, as a `'static` string.
///
/// `HifiTools::tools()` yields owned `String`s, but the envelope's `tool` field is
/// `&'static str` — so a call that fails *before* dispatch (see
/// [`crate::mcp::handler`]) needs this to name the tool it was aimed at.
///
/// A hand-written match rather than a derived list because there is nothing to
/// derive it from at compile time; [`tests::static_names_match_the_advertised_tools`]
/// checks it against `list_tools(true)`, so it cannot drift.
pub fn static_name(name: &str) -> Option<&'static str> {
    Some(match name {
        "hifi_zones" => "hifi_zones",
        "hifi_now_playing" => "hifi_now_playing",
        "hifi_control" => "hifi_control",
        "hifi_search" => "hifi_search",
        "hifi_play" => "hifi_play",
        "hifi_status" => "hifi_status",
        "hifi_hqplayer_status" => "hifi_hqplayer_status",
        "hifi_hqplayer_profiles" => "hifi_hqplayer_profiles",
        "hifi_hqplayer_load_profile" => "hifi_hqplayer_load_profile",
        "hifi_hqplayer_set_pipeline" => "hifi_hqplayer_set_pipeline",
        "hifi_capabilities" => "hifi_capabilities",
        "hifi_play_ref" => "hifi_play_ref",
        "hifi_queue" => "hifi_queue",
        "hifi_spotify" => "hifi_spotify",
        "hifi_apple_music" => "hifi_apple_music",
        _ => return None,
    })
}

/// The input parameters each tool declares, as `'static` strings.
///
/// Needed for the same reason as [`static_name`]: an envelope refusal names the
/// offending parameter as `&'static str`, and a pre-dispatch failure has no typed
/// arguments to read one from.
///
/// [`tests::declared_params_match_the_advertised_input_schemas`] checks this
/// against the `inputSchema` properties in `tools/list`, so it cannot drift from
/// what is advertised.
pub fn declared_params(tool: &str) -> &'static [&'static str] {
    match tool {
        "hifi_zones" | "hifi_status" | "hifi_hqplayer_status" | "hifi_hqplayer_profiles" => &[],
        "hifi_now_playing" => &["zone_id"],
        "hifi_capabilities" => &["zone_id"],
        "hifi_control" => &["zone_id", "action", "value"],
        "hifi_search" => &["query", "zone_id", "source"],
        "hifi_play" => &["query", "zone_id", "source", "action"],
        "hifi_play_ref" => &["ref", "zone_id", "action"],
        "hifi_queue" => &["zone_id"],
        "hifi_spotify" => &[
            "action",
            "playlist_id",
            "category_id",
            "name",
            "description",
            "uri",
            "track_id",
            "public",
            "limit",
        ],
        "hifi_apple_music" => &[
            "action",
            "id",
            "query",
            "uri",
            "zone_id",
            "name",
            "description",
            "confirm",
            "limit",
            "items",
        ],
        "hifi_hqplayer_load_profile" => &["profile"],
        "hifi_hqplayer_set_pipeline" => &["setting", "value"],
        _ => &[],
    }
}

/// Which declared parameter a deserializer message is about, if it can be told.
///
/// `serde`'s messages are prose (`"missing field \`zone_id\`"`), so this looks for
/// a declared parameter name in the text rather than parsing a field path the SDK
/// does not provide. `None` when nothing matches — reported as such rather than
/// guessed, because naming the wrong parameter sends a client to fix the wrong
/// thing.
pub fn static_param(tool: &str, message: &str) -> Option<&'static str> {
    declared_params(tool)
        .iter()
        .copied()
        .find(|param| message.contains(param))
}

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
    fn advertises_fifteen_tools_when_hqplayer_is_enabled() {
        assert_eq!(list_tools(true).len(), 15);
    }

    /// The filter must remove exactly the four HQPlayer tools and nothing else.
    #[test]
    fn hides_only_the_hqplayer_tools_when_disabled() {
        let enabled: Vec<String> = list_tools(true).into_iter().map(|t| t.name).collect();
        let disabled: Vec<String> = list_tools(false).into_iter().map(|t| t.name).collect();

        assert_eq!(disabled.len(), 11);
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

    /// [`static_name`] must recognise exactly the advertised tools — no more, no
    /// fewer. A tool added to `tool_box!` and forgotten here would silently lose
    /// its envelope on argument-parse failures.
    #[test]
    fn static_names_match_the_advertised_tools() {
        for tool in list_tools(true) {
            assert_eq!(
                static_name(&tool.name),
                Some(tool.name.as_str()),
                "static_name does not recognise the advertised tool {:?}; add it, or a \
                 malformed call to it loses its envelope",
                tool.name
            );
        }
        assert_eq!(static_name("hifi_frobnicate"), None);
        assert_eq!(static_name(""), None);
    }

    /// [`declared_params`] must match the `inputSchema` properties every tool
    /// advertises. Derived from `list_tools`, not restated, so a renamed parameter
    /// fails here instead of producing a refusal that blames a field the client
    /// cannot set.
    #[test]
    fn declared_params_match_the_advertised_input_schemas() {
        for tool in list_tools(true) {
            let advertised: std::collections::BTreeSet<String> = tool
                .input_schema
                .properties
                .as_ref()
                .map(|props| props.keys().cloned().collect())
                .unwrap_or_default();
            let declared: std::collections::BTreeSet<String> = declared_params(&tool.name)
                .iter()
                .map(|p| (*p).to_string())
                .collect();
            assert_eq!(
                declared, advertised,
                "declared_params({:?}) disagrees with the advertised inputSchema",
                tool.name
            );
        }
    }

    /// The parameter is identified when the message names it, and reported as
    /// unidentified rather than guessed when it does not.
    #[test]
    fn static_param_identifies_only_what_the_message_names() {
        assert_eq!(
            static_param("hifi_control", "missing field `zone_id`"),
            Some("zone_id")
        );
        assert_eq!(
            static_param("hifi_hqplayer_set_pipeline", "missing field `setting`"),
            Some("setting")
        );
        // Nothing declared by this tool appears in the message.
        assert_eq!(
            static_param("hifi_control", "invalid type: expected a map"),
            None
        );
        // A parameter another tool declares must not be borrowed.
        assert_eq!(
            static_param("hifi_now_playing", "missing field `query`"),
            None
        );
    }
}
