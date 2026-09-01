//! Theme management with localStorage persistence.
//!
//! Provides a theme context for managing light/dark/OLED theme preferences.

use dioxus::prelude::*;

/// Theme options
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
    Oled,
}

impl Theme {
    pub fn as_str(&self) -> &'static str {
        match self {
            Theme::System => "system",
            Theme::Light => "light",
            Theme::Dark => "dark",
            Theme::Oled => "oled",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "light" => Theme::Light,
            "dark" => Theme::Dark,
            "oled" => Theme::Oled,
            _ => Theme::System,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Theme::System => "System",
            Theme::Light => "Light",
            Theme::Dark => "Dark",
            Theme::Oled => "OLED Black",
        }
    }

    /// CSS class to apply to :root (empty for system)
    pub fn css_class(&self) -> &'static str {
        match self {
            Theme::System => "",
            Theme::Light => "theme-light",
            Theme::Dark => "theme-dark",
            Theme::Oled => "theme-oled",
        }
    }
}

/// Global theme state shared via context
#[derive(Clone, Copy)]
pub struct ThemeContext {
    pub current: Signal<Theme>,
}

impl ThemeContext {
    /// Get current theme
    pub fn get(&self) -> Theme {
        (self.current)()
    }

    /// Set and persist theme
    pub fn set(&self, theme: Theme) {
        let mut current = self.current;
        current.set(theme);

        // Apply to DOM and save to localStorage
        #[cfg(target_arch = "wasm32")]
        {
            apply_theme_to_dom(theme);
            save_theme_to_storage(theme);
        }
    }
}

/// Initialize theme context provider - call once at app root
pub fn use_theme_provider() {
    #[allow(unused_mut)] // mut needed for WASM target
    let mut current = use_signal(|| Theme::System);

    let ctx = ThemeContext { current };
    use_context_provider(|| ctx);

    // Client-side only: load from localStorage and apply
    #[cfg(target_arch = "wasm32")]
    {
        use_effect(move || {
            let saved = load_theme_from_storage();
            current.set(saved);
            apply_theme_to_dom(saved);
        });
    }
}

/// Get theme context - use in any component
pub fn use_theme() -> ThemeContext {
    use_context::<ThemeContext>()
}

// ============ WASM-only helpers ============

#[cfg(target_arch = "wasm32")]
fn load_theme_from_storage() -> Theme {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            if let Ok(Some(value)) = storage.get_item("hifi-theme") {
                return Theme::parse(&value);
            }
        }
    }
    Theme::System
}

#[cfg(target_arch = "wasm32")]
fn save_theme_to_storage(theme: Theme) {
    if let Some(window) = web_sys::window() {
        if let Ok(Some(storage)) = window.local_storage() {
            let _ = storage.set_item("hifi-theme", theme.as_str());
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn apply_theme_to_dom(theme: Theme) {
    if let Some(window) = web_sys::window() {
        if let Some(document) = window.document() {
            if let Some(root) = document.document_element() {
                // Remove all theme classes
                let _ = root
                    .class_list()
                    .remove_3("theme-light", "theme-dark", "theme-oled");

                // Add the selected theme class (if not system)
                let class = theme.css_class();
                if !class.is_empty() {
                    let _ = root.class_list().add_1(class);
                }
            }
        }
    }
}
