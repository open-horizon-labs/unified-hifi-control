//! Theme switcher component for light/dark/black modes.

use dioxus::prelude::*;

/// Theme switcher with light, dark, and black (OLED) options.
/// Uses localStorage for persistence and Pico CSS data-theme attribute.
/// Note: We use raw HTML onclick attributes for SSR since the JavaScript
/// is already included in the page and we don't need Dioxus event handling.
#[component]
pub fn ThemeSwitcher() -> Element {
    rsx! {
        div {
            class: "theme-switcher",
            // Using dangerous_inner_html to render buttons with onclick handlers
            // since Dioxus SSR doesn't support string event handlers directly
            dangerous_inner_html: r#"
                <button id="theme-light" onclick="setTheme('light')">Light</button>
                <button id="theme-dark" onclick="setTheme('dark')">Dark</button>
                <button id="theme-black" onclick="setTheme('black')">Black</button>
            "#
        }
    }
}

/// Client-side JavaScript for theme management.
/// This is included in the Layout component's head.
pub const THEME_SCRIPT: &str = r#"
(function(){
    const t = localStorage.getItem('hifi-theme') || 'dark';
    document.documentElement.setAttribute('data-theme', t === 'black' ? 'dark' : t);
    if (t === 'black') document.documentElement.setAttribute('data-variant', 'black');
})();
"#;

/// Client-side JavaScript for theme switching.
/// This is included in the Layout component's body.
pub const THEME_FUNCTIONS: &str = r#"
function setTheme(t) {
    document.documentElement.setAttribute('data-theme', t === 'black' ? 'dark' : t);
    if (t === 'black') {
        document.documentElement.setAttribute('data-variant', 'black');
    } else {
        document.documentElement.removeAttribute('data-variant');
    }
    localStorage.setItem('hifi-theme', t);
    updateThemeButtons();
}
function updateThemeButtons() {
    const variant = document.documentElement.getAttribute('data-variant');
    const theme = variant === 'black' ? 'black' : (document.documentElement.getAttribute('data-theme') || 'dark');
    ['light','dark','black'].forEach(x => {
        const btn = document.getElementById('theme-' + x);
        if (btn) btn.classList.toggle('active', x === theme);
    });
}
function applyNavVisibility() {
    const s = JSON.parse(localStorage.getItem('hifi-ui-settings') || '{}');
    const hide = (id, show) => {
        const el = document.querySelector(`nav a[href*="${id}"]`);
        if (el) el.style.display = show !== false ? '' : 'none';
    };
    hide('/hqplayer', s.showHqplayer);
    hide('/lms', s.showLms);
    hide('/knobs', s.showKnobs);
}
updateThemeButtons();
applyNavVisibility();
"#;
