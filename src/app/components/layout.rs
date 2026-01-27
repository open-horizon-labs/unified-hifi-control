//! Layout component wrapping all pages with Tailwind CSS and DioxusLabs components.

use dioxus::prelude::*;

use super::nav::Nav;
use crate::app::api::AppStatus;
use crate::app::embedded_assets::{
    APPLE_TOUCH_ICON_DATA_URL, DX_THEME_CSS, FAVICON_DATA_URL, TAILWIND_CSS,
};

#[derive(Props, Clone, PartialEq)]
pub struct LayoutProps {
    /// Page title (shown in browser tab)
    pub title: String,
    /// Active navigation item ID
    pub nav_active: String,
    /// Page content
    pub children: Element,
    /// Hide HQPlayer tab in nav
    #[props(default = false)]
    pub hide_hqp: bool,
    /// Hide LMS tab in nav
    #[props(default = false)]
    pub hide_lms: bool,
    /// Hide Knobs tab in nav
    #[props(default = false)]
    pub hide_knobs: bool,
}

/// Main layout component wrapping all pages.
#[component]
pub fn Layout(props: LayoutProps) -> Element {
    // Fetch version from server to avoid WASM/server mismatch
    let status = use_resource(|| async {
        crate::app::api::fetch_json::<AppStatus>("/status")
            .await
            .ok()
    });
    let (version, git_sha) = match &*status.read() {
        Some(Some(s)) => (s.version.clone(), s.git_sha.clone()),
        _ => {
            // Fallback to compile-time values during loading/error
            (
                env!("UHC_VERSION").to_string(),
                env!("UHC_GIT_SHA").to_string(),
            )
        }
    };
    let full_title = format!("{} - Unified Hi-Fi Control", props.title);

    rsx! {
        // Head elements - Dioxus hoists these to the real <head>
        document::Title { "{full_title}" }
        // Viewport meta for mobile responsive design
        document::Meta {
            name: "viewport",
            content: "width=device-width, initial-scale=1"
        }
        // DioxusLabs components theme (CSS variables for dark/light mode) - embedded
        document::Style { {DX_THEME_CSS} }
        // Tailwind CSS utilities - embedded
        document::Style { {TAILWIND_CSS} }
        // Favicon - embedded as data URL
        document::Link {
            rel: "icon",
            href: "{*FAVICON_DATA_URL}"
        }
        document::Link {
            rel: "apple-touch-icon",
            sizes: "180x180",
            href: "{*APPLE_TOUCH_ICON_DATA_URL}"
        }

        // Body content
        Nav {
            active: props.nav_active.clone(),
            hide_hqp: props.hide_hqp,
            hide_lms: props.hide_lms,
            hide_knobs: props.hide_knobs,
        }
        main { class: "max-w-7xl mx-auto px-4 sm:px-6 lg:px-8 mt-4 overflow-x-hidden",
            {props.children}
        }
        footer { class: "max-w-7xl mx-auto px-4 sm:px-6 lg:px-8 text-center py-3",
            small { class: "text-muted", "Unified Hi-Fi Control v{version} ({git_sha})" }
        }
    }
}
