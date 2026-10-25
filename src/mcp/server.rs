//! Identity and capabilities advertised by MCP `server/discover`.
//! Each request declares its protocol version and client capabilities.

use rust_mcp_sdk::schema::{
    Implementation, ServerCapabilities, ServerCapabilitiesResources, ServerCapabilitiesTools,
};
use rust_mcp_sdk::ServerDetails;

/// The `initialize` result this server advertises.
///
/// Every field is snapshotted in `tests/fixtures/mcp_initialize.json`. Changing
/// any of them — instructions included, since a model reads them — fails
/// `initialize_result_matches_fixture`, which is the intended behavior: the
/// fixture is regenerated deliberately, never incidentally.
///
/// `server_info.version` comes from `CARGO_PKG_VERSION`, which release builds
/// inject from the git tag, so the fixture redacts it and asserts it separately.
pub fn server_details() -> ServerDetails {
    ServerDetails {
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
            "Use hifi_zones for zone IDs and hifi_capabilities for provider/device support. \
            Search or browse collections, then pass the returned ref to hifi_play_ref for exact playback. \
            hifi_play searches and plays the first match; action=queue appends to the playback queue, not a saved playlist. \
            For live stations, use hifi_collections action=favorites with media_type=radio; hifi_play action=radio starts Roon Radio. \
            Spotify controls existing Connect devices. Apple Music requires a paired companion."
                .into(),
        ),
    }
}
