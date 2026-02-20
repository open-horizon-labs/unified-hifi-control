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

use pages::{HqPlayer, Knobs, Lms, Onboarding, Settings, Zones};
use settings_context::use_settings_provider;
use sse::use_sse_provider;
use theme::use_theme_provider;

/// Root app component with routing
#[component]
pub fn App() -> Element {
    // Initialize SSE context at app root (single EventSource for all pages)
    use_sse_provider();
    use_theme_provider();
    use_settings_provider();
    let settings = settings_context::use_settings();

    // While settings are loading (SSR or pre-hydration), show branded splash
    if !settings.is_loaded() {
        return rsx! {
            document::Meta {
                name: "viewport",
                content: "width=device-width, initial-scale=1"
            }
            document::Style { {embedded_assets::DX_THEME_CSS} }
            document::Style { {embedded_assets::TAILWIND_CSS} }
            div { class: "min-h-screen flex items-center justify-center",
                p { class: "text-muted animate-pulse", "Loading..." }
            }
        };
    }

    // First-run: show onboarding instead of normal UI
    if !settings.onboarding_completed() {
        return rsx! { Onboarding {} };
    }

    rsx! {
        Router::<Route> {}
    }
}

/// Application routes
#[derive(Clone, Routable, Debug, PartialEq)]
pub enum Route {
    #[route("/")]
    Zones {},
    #[route("/hqplayer")]
    HqPlayer {},
    #[route("/lms")]
    Lms {},
    #[route("/knobs")]
    Knobs {},
    #[route("/settings")]
    Settings {},
}
