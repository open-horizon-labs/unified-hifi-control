//! Theme management with localStorage persistence.
//!
//! Provides a theme context for managing light/dark/OLED theme preferences.

use dioxus::prelude::*;

/// Theme options.
///
/// HiPhi Dark (navy surfaces, cyan accent, sampled from hiphi.audio) is the
/// app default — this is a LAN appliance whose UI should read as one product
/// with the brand site. Light remains available and is fully AA-contrast
/// checked; System and Oled are additional options layered on the same
/// token set.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum Theme {
    System,
    Light,
    #[default]
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
            Theme::Dark => "HiPhi Dark",
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
    let mut current = use_signal(Theme::default);

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
    Theme::default()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hiphi_dark_is_the_default_theme() {
        // #555: the app default is the HiPhi brand dark theme, not System.
        assert_eq!(Theme::default(), Theme::Dark);
        assert_eq!(Theme::default().as_str(), "dark");
        assert_eq!(Theme::default().label(), "HiPhi Dark");
    }

    #[test]
    fn light_remains_a_selectable_option() {
        assert_eq!(Theme::parse("light"), Theme::Light);
        assert_eq!(Theme::Light.label(), "Light");
        assert_eq!(Theme::Light.css_class(), "theme-light");
    }

    #[test]
    fn parse_round_trips_every_variant() {
        for theme in [Theme::System, Theme::Light, Theme::Dark, Theme::Oled] {
            assert_eq!(Theme::parse(theme.as_str()), theme);
        }
    }
}
