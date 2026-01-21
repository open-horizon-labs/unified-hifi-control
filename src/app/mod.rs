//! Dioxus fullstack application entry point.
//!
//! This module provides the main App component that serves as the root
//! of the Dioxus application with client-side hydration.

use dioxus::prelude::*;

pub mod api;
pub mod components;
pub mod pages;
pub mod settings_context;
pub mod sse;
pub mod theme;

use pages::{Dashboard, HqPlayer, Knobs, Lms, Settings, Zone, Zones};
use settings_context::use_settings_provider;
use sse::use_sse_provider;
use theme::use_theme_provider;

/// Root app component with routing
#[component]
pub fn App() -> Element {
    // Initialize SSE context at app root (single EventSource for all pages)
    use_sse_provider();

    // Initialize theme context at app root (handles localStorage + DOM class)
    use_theme_provider();

    // Initialize settings context at app root (shared nav visibility state)
    use_settings_provider();

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
