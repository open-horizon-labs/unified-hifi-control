//! The `initialize` response: server identity, declared capabilities,
//! instructions, and protocol version.
//!
//! Extracted from `create_mcp_extension` so it can be inspected without
//! building an `AppState` or a transport.
//!
//! # Declared capabilities gate request dispatch
//!
//! rust-mcp-sdk checks the declared capability set *before* any handler runs. So
//! declaring only `tools` is not merely "resources are unimplemented" — every
//! resource method returns JSON-RPC `-32603 Server does not support resources`.
//! That refusal is pinned by
//! `tests/mcp_contract.rs::resource_methods_are_refused_while_only_tools_is_declared`,
//! so #397 shows up as a visible refusal-to-success diff rather than as a silent
//! change. The SDK gate keys only on the *presence* of `resources`; it never
//! inspects `subscribe` or `listChanged`.
//!
//! This function is #397's primary target.

use rust_mcp_sdk::schema::{
    Implementation, InitializeResult, ProtocolVersion, ServerCapabilities, ServerCapabilitiesTools,
};

/// The `initialize` result this server advertises.
///
/// Every field is snapshotted in `tests/fixtures/mcp_initialize.json`. Changing
/// any of them — instructions included, since a model reads them — fails
/// `initialize_result_matches_fixture`, which is the intended behavior: the
/// fixture is regenerated deliberately, never incidentally.
///
/// `server_info.version` comes from `CARGO_PKG_VERSION`, which release builds
/// inject from the git tag, so the fixture redacts it and asserts it separately.
pub fn server_details() -> InitializeResult {
    InitializeResult {
        server_info: Implementation {
            name: "unified-hifi-control".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            title: Some("Unified Hi-Fi Control".into()),
            description: Some("Control your music system via MCP".into()),
            icons: vec![],
            website_url: Some("https://github.com/open-horizon-labs/unified-hifi-control".into()),
        },
        capabilities: ServerCapabilities {
            tools: Some(ServerCapabilitiesTools { list_changed: None }),
            ..Default::default()
        },
        meta: None,
        instructions: Some(
            "Unified Hi-Fi Control MCP Server - Control Your Music System\n\n\
            Use hifi_zones to list available zones, hifi_now_playing to see what's playing, \
            hifi_control for playback control, hifi_search to find music, and hifi_play to play it.\n\n\
            Note: hifi_search and hifi_play currently work with Roon and LMS zones only. \
            Transport controls (play/pause/next/volume) work with all zones (Roon, LMS, OpenHome, UPnP).\n\n\
            To build a playlist: call hifi_play multiple times with action='queue'. The first track \
            can use action='play' to start playback, then subsequent tracks use action='queue' to add to the queue."
                .into(),
        ),
        protocol_version: ProtocolVersion::V2025_11_25.into(),
    }
}
