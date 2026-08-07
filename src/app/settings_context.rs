//! Settings context for shared app settings state.
//!
//! Provides reactive signals for navigation visibility settings that are
//! shared between the Settings page and Nav component.

use dioxus::prelude::*;

/// The optional-page visibility snapshot embedded in the server-rendered
/// document.  It is deliberately separate from the reactive context: this is
/// the value both SSR and WASM use *before* the browser can fetch settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct NavigationVisibility {
    pub hide_hqp: bool,
    pub hide_lms: bool,
    pub hide_spotify: bool,
    pub hide_knobs: bool,
}

#[cfg(not(target_arch = "wasm32"))]
fn initial_navigation_visibility() -> Option<NavigationVisibility> {
    let settings = crate::api::load_app_settings();
    Some(NavigationVisibility {
        hide_hqp: !settings.adapters.hqplayer,
        hide_lms: !settings.adapters.lms,
        hide_spotify: !settings.adapters.spotify,
        hide_knobs: settings.hide_knobs_page,
    })
}

#[cfg(target_arch = "wasm32")]
fn initial_navigation_visibility() -> Option<NavigationVisibility> {
    const META_SELECTOR: &str = "meta[name=\"uhc-navigation-visibility\"]";

    let document = web_sys::window()?.document()?;
    let content = document
        .query_selector(META_SELECTOR)
        .ok()??
        .get_attribute("content")?;
    serde_json::from_str(&content).ok()
}

/// Global settings state shared via context
#[derive(Clone, Copy)]
pub struct SettingsContext {
    hide_knobs: Signal<bool>,
    /// HQPlayer adapter enabled (page visible when true)
    hqp_enabled: Signal<bool>,
    /// LMS adapter enabled (page visible when true)
    lms_enabled: Signal<bool>,
    /// Spotify adapter enabled (page visible when true)
    spotify_enabled: Signal<bool>,
    /// Whether settings have been loaded from server
    loaded: Signal<bool>,
}

impl SettingsContext {
    /// Check if settings have been loaded
    pub fn is_loaded(&self) -> bool {
        (self.loaded)()
    }

    /// Get hide_knobs value
    pub fn hide_knobs(&self) -> bool {
        (self.hide_knobs)()
    }

    /// Get hide_hqp value (true if adapter disabled)
    pub fn hide_hqp(&self) -> bool {
        !(self.hqp_enabled)()
    }

    /// Get hide_lms value (true if adapter disabled)
    pub fn hide_lms(&self) -> bool {
        !(self.lms_enabled)()
    }

    /// Whether the Spotify page should be visible.
    pub fn hide_spotify(&self) -> bool {
        !(self.spotify_enabled)()
    }

    /// Update settings - now takes adapter enabled states
    pub fn update(
        &self,
        hide_knobs: bool,
        hqp_enabled: bool,
        lms_enabled: bool,
        spotify_enabled: bool,
    ) {
        let mut hk = self.hide_knobs;
        let mut he = self.hqp_enabled;
        let mut le = self.lms_enabled;
        hk.set(hide_knobs);
        he.set(hqp_enabled);
        le.set(lms_enabled);
        let mut se = self.spotify_enabled;
        se.set(spotify_enabled);
    }

    /// Mark settings as loaded
    pub fn mark_loaded(&self) {
        let mut loaded = self.loaded;
        loaded.set(true);
    }
}

/// Initialize settings context provider - call once at app root
pub fn use_settings_provider() -> NavigationVisibility {
    let initial = initial_navigation_visibility();
    let visibility = initial.unwrap_or_default();
    let hide_knobs = use_signal(move || visibility.hide_knobs);
    let hqp_enabled = use_signal(move || !visibility.hide_hqp);
    let lms_enabled = use_signal(move || !visibility.hide_lms);
    let spotify_enabled = use_signal(move || !visibility.hide_spotify);
    // When the SSR bootstrap marker is available, its settings are already
    // authoritative for the first Nav render.  The browser still refreshes
    // them below so another client can update visibility later.
    let loaded = use_signal(move || initial.is_some());

    let ctx = SettingsContext {
        hide_knobs,
        hqp_enabled,
        lms_enabled,
        spotify_enabled,
        loaded,
    };

    use_context_provider(|| ctx);

    // Fetch initial settings from server
    #[cfg(target_arch = "wasm32")]
    {
        use_effect(move || {
            spawn(async move {
                if let Ok(settings) =
                    crate::app::api::fetch_json::<AppSettings>("/api/settings").await
                {
                    // Page visibility now derived from adapter enabled state
                    ctx.update(
                        settings.hide_knobs_page,
                        settings.adapters.hqplayer,
                        settings.adapters.lms,
                        settings.adapters.spotify,
                    );
                    ctx.mark_loaded();
                }
            });
        });
    }

    visibility
}

/// Get settings context - use in any component
pub fn use_settings() -> SettingsContext {
    use_context::<SettingsContext>()
}
