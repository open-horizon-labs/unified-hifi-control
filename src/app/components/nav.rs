//! Navigation component for the web UI.

use dioxus::prelude::*;

#[derive(Props, Clone, PartialEq)]
pub struct NavProps {
    /// The currently active page ID (e.g., "dashboard", "zones")
    pub active: String,
    /// Hide HQPlayer tab
    #[props(default = false)]
    pub hide_hqp: bool,
    /// Hide LMS tab
    #[props(default = false)]
    pub hide_lms: bool,
    /// Hide Knobs tab
    #[props(default = false)]
    pub hide_knobs: bool,
}

/// Navigation bar component using Pico CSS nav pattern with mobile responsiveness.
#[component]
pub fn Nav(props: NavProps) -> Element {
    let mut menu_open = use_signal(|| false);

    rsx! {
        nav { class: "responsive-nav",
            // Brand + hamburger row
            ul { class: "nav-brand",
                li {
                    strong { "Hi-Fi Control" }
                }
                li { class: "nav-toggle",
                    button {
                        "aria-label": "Toggle navigation",
                        "aria-expanded": "{menu_open}",
                        onclick: move |_| menu_open.toggle(),
                        "☰"
                    }
                }
            }
            // Navigation links
            ul {
                class: if menu_open() { "nav-links show" } else { "nav-links" },
                li {
                    if props.active == "dashboard" {
                        a { href: "/", "aria-current": "page", onclick: move |_| menu_open.set(false), strong { "Dashboard" } }
                    } else {
                        a { href: "/", onclick: move |_| menu_open.set(false), "Dashboard" }
                    }
                }
                li {
                    if props.active == "zones" {
                        a { href: "/ui/zones", "aria-current": "page", onclick: move |_| menu_open.set(false), strong { "Zones" } }
                    } else {
                        a { href: "/ui/zones", onclick: move |_| menu_open.set(false), "Zones" }
                    }
                }
                li {
                    if props.active == "zone" {
                        a { href: "/zone", "aria-current": "page", onclick: move |_| menu_open.set(false), strong { "Zone" } }
                    } else {
                        a { href: "/zone", onclick: move |_| menu_open.set(false), "Zone" }
                    }
                }
                if !props.hide_hqp {
                    li {
                        if props.active == "hqplayer" {
                            a { href: "/hqplayer", "aria-current": "page", onclick: move |_| menu_open.set(false), strong { "HQPlayer" } }
                        } else {
                            a { href: "/hqplayer", onclick: move |_| menu_open.set(false), "HQPlayer" }
                        }
                    }
                }
                if !props.hide_lms {
                    li {
                        if props.active == "lms" {
                            a { href: "/lms", "aria-current": "page", onclick: move |_| menu_open.set(false), strong { "LMS" } }
                        } else {
                            a { href: "/lms", onclick: move |_| menu_open.set(false), "LMS" }
                        }
                    }
                }
                if !props.hide_knobs {
                    li {
                        if props.active == "knobs" {
                            a { href: "/knobs", "aria-current": "page", onclick: move |_| menu_open.set(false), strong { "Knobs" } }
                        } else {
                            a { href: "/knobs", onclick: move |_| menu_open.set(false), "Knobs" }
                        }
                    }
                }
                li {
                    if props.active == "settings" {
                        a { href: "/settings", "aria-current": "page", onclick: move |_| menu_open.set(false), strong { "Settings" } }
                    } else {
                        a { href: "/settings", onclick: move |_| menu_open.set(false), "Settings" }
                    }
                }
            }
        }
    }
}
