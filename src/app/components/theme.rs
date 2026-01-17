//! Theme switcher component for light/dark/black modes.

use dioxus::prelude::*;

/// Theme switcher with light, dark, and black (OLED) options.
/// Uses localStorage for persistence and Pico CSS data-theme attribute.
#[component]
pub fn ThemeSwitcher() -> Element {
    rsx! {
        div {
            class: "theme-switcher",
            dangerous_inner_html: r#"
                <button id="theme-light" onclick="setTheme('light')">Light</button>
                <button id="theme-dark" onclick="setTheme('dark')">Dark</button>
                <button id="theme-black" onclick="setTheme('black')">Black</button>
            "#
        }
    }
}

/// Client-side JavaScript for theme management (included in head).
pub const THEME_SCRIPT: &str = r#"
(function(){
    const t = localStorage.getItem('hifi-theme') || 'dark';
    document.documentElement.setAttribute('data-theme', t === 'black' ? 'dark' : t);
    if (t === 'black') document.documentElement.setAttribute('data-variant', 'black');
})();
"#;

/// Client-side JavaScript for theme switching (included in body).
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
updateThemeButtons();
"#;
