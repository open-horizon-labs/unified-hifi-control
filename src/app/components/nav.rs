//! Navigation component using Tailwind CSS.

use crate::app::embedded_assets::LOGO_DATA_URL;
use crate::app::settings_context::use_settings;
use crate::app::Route;
use dioxus::prelude::*;

#[derive(Props, Clone, PartialEq)]
pub struct NavProps {
    /// The currently active page ID (e.g., "zones", "hqplayer", "settings")
    pub active: String,
    /// Hide HQPlayer tab (fallback if settings not loaded)
    #[props(default = false)]
    pub hide_hqp: bool,
    /// Hide LMS tab (fallback if settings not loaded)
    #[props(default = false)]
    pub hide_lms: bool,
    /// Hide Spotify tab (fallback if settings not loaded)
    #[props(default = false)]
    pub hide_spotify: bool,
    /// Hide Knobs tab (fallback if settings not loaded)
    #[props(default = false)]
    pub hide_knobs: bool,
}

/// Navigation bar using Tailwind CSS with mobile toggle.
#[component]
pub fn Nav(props: NavProps) -> Element {
    let mut menu_open = use_signal(|| false);

    // Use shared settings context for reactive updates
    let settings_ctx = use_settings();

    // Use context values if loaded, otherwise fall back to props
    let (hide_hqp, hide_lms, hide_spotify, hide_knobs) = if settings_ctx.is_loaded() {
        (
            settings_ctx.hide_hqp(),
            settings_ctx.hide_lms(),
            settings_ctx.hide_spotify(),
            settings_ctx.hide_knobs(),
        )
    } else {
        (
            props.hide_hqp,
            props.hide_lms,
            props.hide_spotify,
            props.hide_knobs,
        )
    };

    let nav_link_class = |page: &str| {
        if props.active == page {
            "nav-link-active"
        } else {
            "nav-link"
        }
    };

    let mobile_menu_class = if menu_open() {
        "block lg:hidden"
    } else {
        "hidden lg:hidden"
    };

    rsx! {
        nav { class: "nav-container",
            div { class: "nav-inner",
                // Logo / Brand
                div { class: "flex items-center",
                    Link { class: "nav-brand flex items-center", to: Route::Library { source: None, tab: None, path: None, zone: None },
                        img {
                            // base-path-ok: an inlined data: URL carries no path to map.
                            src: "{*LOGO_DATA_URL}",
                            alt: "Hi-Fi Control",
                            class: "h-6 w-6 rounded"
                        }
                    }
                }

                // Desktop navigation - use Link for client-side routing (no page reload)
                div { class: "hidden lg:flex items-center space-x-4",
                    Link {
                        class: nav_link_class("library"),
                        to: Route::Library { source: None, tab: None, path: None, zone: None },
                        "Library"
                    }
                    Link { class: nav_link_class("zones"), to: Route::Zones {}, "Zones" }
                    // Keep the link topology stable while settings load. Omitting links
                    // after hydration shifts the event indices of every later control.
                    Link {
                        class: nav_link_class("hqplayer"),
                        to: Route::HqPlayer {},
                        hidden: hide_hqp,
                        aria_hidden: hide_hqp,
                        tabindex: if hide_hqp { "-1" } else { "0" },
                        "HQPlayer"
                    }
                    Link {
                        class: nav_link_class("lms"),
                        to: Route::Lms {},
                        hidden: hide_lms,
                        aria_hidden: hide_lms,
                        tabindex: if hide_lms { "-1" } else { "0" },
                        "LMS"
                    }
                    Link {
                        class: nav_link_class("spotify"),
                        to: Route::Spotify {},
                        hidden: hide_spotify,
                        aria_hidden: hide_spotify,
                        tabindex: if hide_spotify { "-1" } else { "0" },
                        "Spotify"
                    }
                    Link {
                        class: nav_link_class("knobs"),
                        to: Route::Knobs {},
                        hidden: hide_knobs,
                        aria_hidden: hide_knobs,
                        tabindex: if hide_knobs { "-1" } else { "0" },
                        "Controllers"
                    }
                    Link { class: nav_link_class("settings"), to: Route::Settings {}, "Settings" }
                }

                // Mobile menu button
                div { class: "lg:hidden",
                    button {
                        class: "nav-mobile-toggle",
                        r#type: "button",
                        onclick: move |_| menu_open.toggle(),
                        span { class: "sr-only", "Toggle menu" }
                        if menu_open() {
                            // X icon
                            svg { class: "h-6 w-6", fill: "none", view_box: "0 0 24 24", stroke: "currentColor", "stroke-width": "2",
                                path { "stroke-linecap": "round", "stroke-linejoin": "round", d: "M6 18L18 6M6 6l12 12" }
                            }
                        } else {
                            // Hamburger icon
                            svg { class: "h-6 w-6", fill: "none", view_box: "0 0 24 24", stroke: "currentColor", "stroke-width": "2",
                                path { "stroke-linecap": "round", "stroke-linejoin": "round", d: "M4 6h16M4 12h16M4 18h16" }
                            }
                        }
                    }
                }
            }

            // Mobile menu - use Link for client-side routing
            div { class: "{mobile_menu_class}", id: "mobile-menu",
                div { class: "px-2 pt-2 pb-3 space-y-1",
                    Link {
                        class: nav_link_class("library"),
                        to: Route::Library { source: None, tab: None, path: None, zone: None },
                        onclick: move |_| menu_open.set(false),
                        "Library"
                    }
                    Link { class: nav_link_class("zones"), to: Route::Zones {}, onclick: move |_| menu_open.set(false), "Zones" }
                    Link {
                        class: nav_link_class("hqplayer"),
                        to: Route::HqPlayer {},
                        hidden: hide_hqp,
                        aria_hidden: hide_hqp,
                        tabindex: if hide_hqp { "-1" } else { "0" },
                        onclick: move |_| menu_open.set(false),
                        "HQPlayer"
                    }
                    Link {
                        class: nav_link_class("lms"),
                        to: Route::Lms {},
                        hidden: hide_lms,
                        aria_hidden: hide_lms,
                        tabindex: if hide_lms { "-1" } else { "0" },
                        onclick: move |_| menu_open.set(false),
                        "LMS"
                    }
                    Link {
                        class: nav_link_class("spotify"),
                        to: Route::Spotify {},
                        hidden: hide_spotify,
                        aria_hidden: hide_spotify,
                        tabindex: if hide_spotify { "-1" } else { "0" },
                        onclick: move |_| menu_open.set(false),
                        "Spotify"
                    }
                    Link {
                        class: nav_link_class("knobs"),
                        to: Route::Knobs {},
                        hidden: hide_knobs,
                        aria_hidden: hide_knobs,
                        tabindex: if hide_knobs { "-1" } else { "0" },
                        onclick: move |_| menu_open.set(false),
                        "Controllers"
                    }
                    Link { class: nav_link_class("settings"), to: Route::Settings {}, onclick: move |_| menu_open.set(false), "Settings" }
                }
            }
        }
    }
}
