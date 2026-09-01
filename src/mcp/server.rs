//! The `initialize` response: server identity, declared capabilities,
//! instructions, and protocol version.
//!
//! Extracted from `create_mcp_extension` so it can be inspected without
//! building an `AppState` or a transport.
//!
//! # Declared capabilities gate request dispatch
//!
//! rust-mcp-sdk checks the declared capability set *before* any handler runs. So
//! before #397, declaring only `tools` was not merely "resources are
//! unimplemented" — every resource method returned JSON-RPC
//! `-32603 Server does not support resources`. That refusal was pinned by
//! `tests/mcp_contract.rs` (previously
//! `resource_methods_are_refused_while_only_tools_is_declared`, now
//! `resource_methods_dispatch_now_that_resources_is_declared`), so #397 shows up
//! as a visible refusal-to-success diff rather than as a silent change. The SDK
//! gate keys only on the *presence* of `resources`; it never inspects
//! `subscribe` or `listChanged`.
//!
//! # What #397 declares, and why not more
//!
//! `resources.list_changed: Some(true)` — implemented, see
//! [`crate::mcp::resources::spawn_list_changed_notifier`]. `resources.subscribe`
//! is left `None`: the pinned SDK has no subscription registry and no event
//! replay (`event_store: None` in [`crate::mcp::create_mcp_extension`]), so
//! advertising `subscribe` would claim something this server cannot honor —
//! exactly the capability-honesty problem this epic exists to fix everywhere
//! else. A client that calls `resources/subscribe` anyway reaches the SDK's own
//! default handler, a plain `method_not_found`, which is a truthful answer.

use rust_mcp_sdk::schema::{
    Implementation, InitializeResult, ProtocolVersion, ServerCapabilities,
    ServerCapabilitiesResources, ServerCapabilitiesTools,
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
            // #397: only the sub-features actually implemented. See the module
            // docs for why `subscribe` stays unset.
            resources: Some(ServerCapabilitiesResources {
                list_changed: Some(true),
                subscribe: None,
            }),
            ..Default::default()
        },
        meta: None,
        instructions: Some(
            "Unified Hi-Fi Control MCP Server - Control Your Music System\n\n\
            Use hifi_zones to list available zones, hifi_now_playing to see what's playing, \
            hifi_control for playback control, hifi_search to find music, and hifi_play to play it.\n\n\
            Note: hifi_search and hifi_play currently work with Roon and LMS zones only. \
            Transport controls (play/pause/next/volume) work with all zones (Roon, LMS, OpenHome, UPnP, HQPlayer).\n\n\
            To build a playlist: call hifi_play multiple times with action='queue'. The first track \
            can use action='play' to start playback, then subsequent tracks use action='queue' to add to the queue."
                .into(),
        ),
        protocol_version: ProtocolVersion::V2025_11_25.into(),
    }
}
