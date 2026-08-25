//! Dioxus fullstack application entry point.
//!
//! This module provides the main App component that serves as the root
//! of the Dioxus application with client-side hydration.

use dioxus::prelude::*;

pub mod api;
pub mod components;
pub mod embedded_assets;
pub mod pages;
pub mod settings_context;
pub mod sse;
pub mod theme;

use pages::{HqPlayer, Knobs, Library, Lms, Settings, Spotify, Zones};
use settings_context::use_settings_provider;
use sse::use_sse_provider;
use theme::use_theme_provider;

/// Secure origin required by the browser's Web Serial firmware flasher.
pub const KNOB_FLASHER_URL: &str = "https://roon-knob.muness.com/";

/// MCP connection details injected by the server during SSR.
#[derive(Clone, Debug, PartialEq)]
pub struct McpEndpoint {
    pub url: String,
}

impl McpEndpoint {
    pub fn new(host: &str, port: u16) -> Self {
        let host = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.to_string()
        };

        Self {
            url: format!("http://{host}:{port}/mcp"),
        }
    }

    pub fn agent_config(&self) -> String {
        serde_json::to_string_pretty(&serde_json::json!({
            "mcpServers": {
                "unified-hifi-control": {
                    "type": "http",
                    "url": self.url,
                }
            }
        }))
        .unwrap_or_default()
    }

    #[cfg(target_arch = "wasm32")]
    fn from_document() -> Option<Self> {
        const META_SELECTOR: &str = "meta[name=\"uhc-mcp-endpoint\"]";

        let document = web_sys::window()?.document()?;
        let element = document.query_selector(META_SELECTOR).ok()??;
        let url = element.get_attribute("content")?;
        Some(Self { url })
    }
}

impl Default for McpEndpoint {
    fn default() -> Self {
        Self::new("your-bridge-host", 8088)
    }
}

/// Root app component with routing
#[component]
pub fn App() -> Element {
    // SSR injects a reachable LAN endpoint. During hydration, read the same
    // value from the rendered document so client and server markup agree.
    #[cfg(target_arch = "wasm32")]
    let mcp_endpoint = try_consume_context::<McpEndpoint>()
        .or_else(McpEndpoint::from_document)
        .unwrap_or_default();
    #[cfg(not(target_arch = "wasm32"))]
    let mcp_endpoint = try_consume_context::<McpEndpoint>().unwrap_or_default();
    use_context_provider(|| mcp_endpoint.clone());

    // Initialize SSE context at app root (single EventSource for all pages)
    use_sse_provider();

    // Initialize theme context at app root (handles localStorage + DOM class)
    use_theme_provider();

    // Initialize navigation visibility from the persisted server snapshot. The
    // same snapshot is emitted below so WASM hydrates the exact tab set that
    // SSR rendered, before its asynchronous settings refresh begins.
    let navigation_visibility = use_settings_provider();
    let navigation_visibility_json =
        serde_json::to_string(&navigation_visibility).unwrap_or_else(|_| "{}".to_string());
    let settings_bootstrap_json = settings_context::settings_bootstrap_json();

    rsx! {
        document::Meta {
            name: "uhc-mcp-endpoint",
            content: "{mcp_endpoint.url}"
        }
        document::Meta {
            name: "uhc-navigation-visibility",
            content: "{navigation_visibility_json}"
        }
        document::Meta {
            name: "uhc-settings-bootstrap",
            content: "{settings_bootstrap_json}"
        }
        Router::<Route> {}
    }
}

/// Application routes.
///
/// Library is the home page (#550): a full-page browse/search surface that
/// replaced the old per-zone-card browse panel. Its state -- which
/// provider/zone is being browsed, which tab, and the breadcrumb path -- is
/// carried entirely in the query string so a level is refresh/back/share
/// safe, per the issue's URL-addressability requirement. `path` is a
/// base64url-encoded JSON breadcrumb stack (`Vec<(String, Option<String>)>`,
/// see `pages::library::BreadcrumbEntry`) rather than raw path segments:
/// provider browse paths are opaque tokens that may contain characters a URL
/// path segment can't carry safely, and a flat list of segments would lose
/// the breadcrumb titles on a fresh (deep-linked) load.
#[derive(Clone, Routable, Debug, PartialEq)]
pub enum Route {
    #[route("/?:source&:tab&:path&:zone")]
    Library {
        source: Option<String>,
        tab: Option<String>,
        path: Option<String>,
        zone: Option<String>,
    },
    #[route("/zones")]
    Zones {},
    #[route("/hqplayer")]
    HqPlayer {},
    #[route("/lms")]
    Lms {},
    #[route("/spotify")]
    Spotify {},
    #[route("/knobs")]
    Knobs {},
    #[route("/settings")]
    Settings {},
}

#[cfg(test)]
mod tests {
    use super::McpEndpoint;

    #[test]
    fn mcp_endpoint_uses_reachable_ipv4_address_in_agent_config() {
        let endpoint = McpEndpoint::new("192.168.1.24", 8088);

        assert_eq!(endpoint.url, "http://192.168.1.24:8088/mcp");
        assert_eq!(
            endpoint.agent_config(),
            r#"{
  "mcpServers": {
    "unified-hifi-control": {
      "type": "http",
      "url": "http://192.168.1.24:8088/mcp"
    }
  }
}"#
        );
    }

    #[test]
    fn mcp_endpoint_brackets_ipv6_addresses() {
        let endpoint = McpEndpoint::new("fd00::24", 9000);

        assert_eq!(endpoint.url, "http://[fd00::24]:9000/mcp");
    }
}
