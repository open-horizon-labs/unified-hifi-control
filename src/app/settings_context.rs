//! Settings context for shared app settings state.
//!
//! Provides reactive signals for navigation visibility settings that are
//! shared between the Settings page and Nav component.

use dioxus::prelude::*;

#[cfg(target_arch = "wasm32")]
use crate::app::api::AppSettings;

/// Global settings state shared via context
#[derive(Clone, Copy)]
pub struct SettingsContext {
    hide_knobs: Signal<bool>,
    /// HQPlayer adapter enabled (page visible when true)
    hqp_enabled: Signal<bool>,
    /// LMS adapter enabled (page visible when true)
    lms_enabled: Signal<bool>,
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

    /// Update settings - now takes adapter enabled states
    pub fn update(&self, hide_knobs: bool, hqp_enabled: bool, lms_enabled: bool) {
        let mut hk = self.hide_knobs;
        let mut he = self.hqp_enabled;
        let mut le = self.lms_enabled;
        hk.set(hide_knobs);
        he.set(hqp_enabled);
        le.set(lms_enabled);
    }

    /// Mark settings as loaded
    pub fn mark_loaded(&self) {
        let mut loaded = self.loaded;
        loaded.set(true);
    }
}

/// Initialize settings context provider - call once at app root
pub fn use_settings_provider() {
    let hide_knobs = use_signal(|| false);
    let hqp_enabled = use_signal(|| false);
    let lms_enabled = use_signal(|| false);
    let loaded = use_signal(|| false);

    let ctx = SettingsContext {
        hide_knobs,
        hqp_enabled,
        lms_enabled,
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
                    );
                    ctx.mark_loaded();
                }
            });
        });
    }
}

/// Get settings context - use in any component
pub fn use_settings() -> SettingsContext {
    use_context::<SettingsContext>()
}
