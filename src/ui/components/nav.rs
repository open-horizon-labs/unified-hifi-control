//! Navigation component for the web UI.

use dioxus::prelude::*;

/// Navigation links for the main menu.
const NAV_LINKS: &[(&str, &str, &str)] = &[
    ("dashboard", "Dashboard", "/"),
    ("zones", "Zones", "/ui/zones"),
    ("zone", "Zone", "/zone"),
    ("hqplayer", "HQPlayer", "/hqplayer"),
    ("lms", "LMS", "/lms"),
    ("knobs", "Knobs", "/knobs"),
    ("settings", "Settings", "/settings"),
];

#[derive(Props, Clone, PartialEq)]
pub struct NavProps {
    /// The currently active page ID (e.g., "dashboard", "zones")
    pub active: String,
}

/// Navigation bar component.
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
                for (id, label, href) in NAV_LINKS.iter() {
                    li {
                        if *id == props.active.as_str() {
                            a {
                                href: *href,
                                "aria-current": "page",
                                strong { "{label}" }
                            }
                        } else {
                            a {
                                href: *href,
                                "{label}"
                            }
                        }
                    }
                }
            }
        }
    }
}
