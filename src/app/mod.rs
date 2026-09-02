//! Dioxus fullstack application entry point.
//!
//! This module provides the main App component that serves as the root
//! of the Dioxus application with client-side hydration.

use dioxus::prelude::*;

pub mod api;
pub mod base_path;
pub mod components;
pub mod controller_auth;
pub mod embedded_assets;
pub mod pages;
pub mod settings_context;
pub mod sse;
pub mod theme;

use components::BootstrapPrompt;
use pages::{
    HqPlayer, Knobs, LibraryHome, LibraryLocation, LibrarySource, LibraryView, Lms, Settings,
    Spotify, Zones,
};
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

    // HA Ingress (#581): when the document was served behind a subpath
    // proxy (the server injected a uhc-base-path meta tag), the router must
    // treat that prefix as its base path -- Link hrefs, pushes, and route
    // parsing all include it. Provided before Router mounts so it shadows
    // the default (prefixless) WebHistory the web launch installs at the
    // root. Direct mode has no meta tag and keeps the default history.
    #[cfg(all(target_arch = "wasm32", feature = "web"))]
    use_hook(|| {
        let base = base_path::base_path();
        if !base.is_empty() {
            provide_context::<std::rc::Rc<dyn dioxus::history::History>>(std::rc::Rc::new(
                dioxus::web::WebHistory::new(Some(base), true),
            ));
        }
    });

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
        // Mounted once at the app root so any page's owner-gated fetch can
        // open it via `crate::app::controller_auth::open_bootstrap_prompt`
        // without threading state through every route (#570).
        BootstrapPrompt {}
    }
}

/// Application routes.
///
/// Library is the home page (#550). Source, view, and durable collection
/// location use one readable path grammar for every provider. The armed
/// playback zone is client preference, not content identity, so it never
/// appears in these URLs.
#[derive(Clone, Routable, Debug, PartialEq)]
pub enum Route {
    #[route("/")]
    LibraryHome {},
    #[route("/library/:source")]
    LibrarySource { source: String },
    #[route("/library/:source/:view")]
    LibraryView { source: String, view: String },
    #[route("/library/:source/:view/:location")]
    LibraryLocation {
        source: String,
        view: String,
        location: String,
    },
    // #560: was `/zones`, which collides with the JSON protocol route
    // `GET /zones` (knobs::knob_zones_handler, registered in main.rs) --
    // that API route wins the axum route table, so the SPA page was
    // unreachable (the URL just served raw JSON). Moved to a path the API
    // doesn't own; the API route itself is untouched.
    #[route("/ui/zones")]
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
    use super::{McpEndpoint, Route};

    #[test]
    fn library_routes_are_canonical_and_provider_neutral() {
        assert_eq!(Route::LibraryHome {}.to_string(), "/");
        assert_eq!(
            Route::LibrarySource {
                source: "roon".to_string(),
            }
            .to_string(),
            "/library/roon"
        );
        assert_eq!(
            Route::LibraryView {
                source: "lms".to_string(),
                view: "playlists".to_string(),
            }
            .to_string(),
            "/library/lms/playlists"
        );
        assert_eq!(
            Route::LibraryLocation {
                source: "musicassistant".to_string(),
                view: "browse".to_string(),
                location: "loc_4fNE3TqO1n0Yzg".to_string(),
            }
            .to_string(),
            "/library/musicassistant/browse/loc_4fNE3TqO1n0Yzg"
        );
    }

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
