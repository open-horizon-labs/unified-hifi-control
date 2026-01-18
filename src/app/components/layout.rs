//! Layout component wrapping all pages with Pico CSS and common elements.

use dioxus::prelude::*;

use super::nav::Nav;
use super::theme::{ThemeSwitcher, THEME_SCRIPT};

/// CSS styles for the application (extends Pico CSS).
const CUSTOM_STYLES: &str = r#"
:root { --pico-font-size: 15px; }
.status-ok { color: var(--pico-ins-color); }
.status-err { color: var(--pico-del-color); }
.status-disabled { color: var(--pico-muted-color); }
.zone-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(320px, 1fr)); gap: 1rem; }
.controls { display: flex; gap: 0.5rem; margin-top: 0.5rem; }
.controls button { margin: 0; padding: 0.5rem 1rem; }
small { color: var(--pico-muted-color); }
/* Black theme (OLED) - extends dark theme */
[data-theme="dark"][data-variant="black"] {
    --pico-background-color: #000;
    --pico-card-background-color: #0a0a0a;
    --pico-card-sectioning-background-color: #0a0a0a;
    --pico-modal-overlay-background-color: rgba(0,0,0,.9);
    --pico-primary-background: #1a1a1a;
    --pico-secondary-background: #111;
    --pico-contrast-background: #0a0a0a;
    --pico-muted-border-color: #1a1a1a;
    --pico-form-element-background-color: #0a0a0a;
    --pico-table-border-color: #1a1a1a;
}
/* Theme switcher */
.theme-switcher { display: flex; gap: 0.25rem; }
.theme-switcher button { padding: 0.25rem 0.5rem; font-size: 0.8rem; margin: 0; }
.theme-switcher button.active { background: var(--pico-primary-background); color: var(--pico-primary-inverse); }
/* Responsive navigation */
nav.responsive-nav { display: flex; flex-wrap: wrap; justify-content: space-between; align-items: center; }
nav.responsive-nav ul.nav-brand { display: flex !important; justify-content: space-between; align-items: center; width: 100%; margin-bottom: 0; }
nav.responsive-nav ul.nav-brand li.nav-toggle { display: none !important; }
nav.responsive-nav li.nav-toggle button { background: transparent; border: none; font-size: 1.5rem; padding: 0.25rem 0.5rem; cursor: pointer; color: var(--pico-color); }
nav.responsive-nav ul.nav-links { display: flex !important; flex-wrap: wrap; gap: 0; margin-bottom: 0; }
@media (max-width: 768px) {
    nav.responsive-nav { flex-direction: column; align-items: stretch; }
    nav.responsive-nav ul.nav-brand { width: 100%; }
    nav.responsive-nav ul.nav-brand li.nav-toggle { display: block !important; }
    nav.responsive-nav ul.nav-links { display: none !important; flex-direction: column; width: 100%; padding: 0; }
    nav.responsive-nav ul.nav-links.show { display: flex !important; }
    nav.responsive-nav ul.nav-links li { width: 100%; text-align: center; padding: 0.5rem 0; }
    nav.responsive-nav ul.nav-links li a { display: block; padding: 0.5rem; }
}
@media (min-width: 769px) {
    nav.responsive-nav ul.nav-brand { width: auto; }
    nav.responsive-nav ul.nav-links { width: auto; }
}
"#;

#[derive(Props, Clone, PartialEq)]
pub struct LayoutProps {
    /// Page title (shown in browser tab)
    pub title: String,
    /// Active navigation item ID
    pub nav_active: String,
    /// Page content
    pub children: Element,
    /// Hide HQPlayer tab in nav
    #[props(default = false)]
    pub hide_hqp: bool,
    /// Hide LMS tab in nav
    #[props(default = false)]
    pub hide_lms: bool,
    /// Hide Knobs tab in nav
    #[props(default = false)]
    pub hide_knobs: bool,
}

/// Main layout component wrapping all pages.
#[component]
pub fn Layout(props: LayoutProps) -> Element {
    let version = env!("CARGO_PKG_VERSION");
    let full_title = format!("{} - Unified Hi-Fi Control", props.title);

    rsx! {
        // Head elements - Dioxus hoists these to the real <head>
        document::Title { "{full_title}" }
        document::Link { rel: "stylesheet", href: "https://cdn.jsdelivr.net/npm/@picocss/pico@2/css/pico.min.css" }
        document::Style { {CUSTOM_STYLES} }
        // Theme init runs immediately (no DOM needed) to prevent flash
        document::Script { {THEME_SCRIPT} }

        // Body content
        header { class: "container",
            Nav {
                active: props.nav_active.clone(),
                hide_hqp: props.hide_hqp,
                hide_lms: props.hide_lms,
                hide_knobs: props.hide_knobs,
            }
        }
        main { class: "container",
            {props.children}
        }
        footer {
            class: "container",
            style: "display:flex;justify-content:space-between;align-items:center;",
            small { "Unified Hi-Fi Control v{version}" }
            ThemeSwitcher {}
        }
    }
}
