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

/// Navigation bar component using Pico CSS nav pattern.
#[component]
pub fn Nav(props: NavProps) -> Element {
    rsx! {
        nav {
            ul {
                li {
                    strong { "Hi-Fi Control" }
                }
            }
            ul {
                li {
                    if props.active == "dashboard" {
                        a { href: "/", "aria-current": "page", strong { "Dashboard" } }
                    } else {
                        a { href: "/", "Dashboard" }
                    }
                }
                li {
                    if props.active == "zones" {
                        a { href: "/ui/zones", "aria-current": "page", strong { "Zones" } }
                    } else {
                        a { href: "/ui/zones", "Zones" }
                    }
                }
                li {
                    if props.active == "zone" {
                        a { href: "/zone", "aria-current": "page", strong { "Zone" } }
                    } else {
                        a { href: "/zone", "Zone" }
                    }
                }
                if !props.hide_hqp {
                    li {
                        if props.active == "hqplayer" {
                            a { href: "/hqplayer", "aria-current": "page", strong { "HQPlayer" } }
                        } else {
                            a { href: "/hqplayer", "HQPlayer" }
                        }
                    }
                }
                if !props.hide_lms {
                    li {
                        if props.active == "lms" {
                            a { href: "/lms", "aria-current": "page", strong { "LMS" } }
                        } else {
                            a { href: "/lms", "LMS" }
                        }
                    }
                }
                if !props.hide_knobs {
                    li {
                        if props.active == "knobs" {
                            a { href: "/knobs", "aria-current": "page", strong { "Knobs" } }
                        } else {
                            a { href: "/knobs", "Knobs" }
                        }
                    }
                }
                li {
                    if props.active == "settings" {
                        a { href: "/settings", "aria-current": "page", strong { "Settings" } }
                    } else {
                        a { href: "/settings", "Settings" }
                    }
                }
            }
        }
    }
}
