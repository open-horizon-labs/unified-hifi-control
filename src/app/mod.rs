//! Dioxus fullstack application entry point.
//!
//! This module provides the main App component that serves as the root
//! of the Dioxus application with client-side hydration.

use dioxus::prelude::*;

pub mod components;
pub mod pages;

use pages::{Dashboard, HqPlayer, Knobs, Lms, Settings, Zone, Zones};

/// Root app component with routing
#[component]
pub fn App() -> Element {
    rsx! {
        Router::<Route> {}
    }
}

/// Application routes
#[derive(Clone, Routable, Debug, PartialEq)]
pub enum Route {
    #[route("/")]
    Dashboard {},
    #[route("/ui/zones")]
    Zones {},
    #[route("/zone")]
    Zone {},
    #[route("/hqplayer")]
    HqPlayer {},
    #[route("/lms")]
    Lms {},
    #[route("/knobs")]
    Knobs {},
    #[route("/settings")]
    Settings {},
}
