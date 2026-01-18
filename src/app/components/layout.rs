//! Layout component wrapping all pages with Tailwind CSS and DioxusLabs components.

use dioxus::prelude::*;

use super::nav::Nav;

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
    let version = env!("CARGO_PKG_VERSION");
    let full_title = format!("{} - Unified Hi-Fi Control", props.title);

    rsx! {
        // Head elements - Dioxus hoists these to the real <head>
        document::Title { "{full_title}" }
        // DioxusLabs components theme (CSS variables for dark/light mode)
        document::Link {
            rel: "stylesheet",
            href: asset!("/public/dx-components-theme.css")
        }
        // Tailwind CSS utilities
        document::Link {
            rel: "stylesheet",
            href: asset!("/public/tailwind.css")
        }
        // Favicons
        document::Link {
            rel: "icon",
            r#type: "image/png",
            sizes: "32x32",
            href: asset!("/public/favicon-32x32.png")
        }
        document::Link {
            rel: "icon",
            r#type: "image/png",
            sizes: "16x16",
            href: asset!("/public/favicon-16x16.png")
        }
        document::Link {
            rel: "apple-touch-icon",
            sizes: "180x180",
            href: asset!("/public/apple-touch-icon.png")
        }

        // Body content
        Nav {
            active: props.nav_active.clone(),
            hide_hqp: props.hide_hqp,
            hide_lms: props.hide_lms,
            hide_knobs: props.hide_knobs,
        }
        main { class: "max-w-7xl mx-auto px-4 sm:px-6 lg:px-8 mt-4",
            {props.children}
        }
        footer { class: "max-w-7xl mx-auto px-4 sm:px-6 lg:px-8 text-center py-3",
            small { class: "text-muted", "Unified Hi-Fi Control v{version}" }
        }
    }
}
