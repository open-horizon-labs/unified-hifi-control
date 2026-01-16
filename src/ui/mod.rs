//! Web UI handlers - daily-use interface for zone and HQPlayer control
//!
//! Multiple clients exist for unified-hifi-control:
//! - Web UI (this) - full control interface, better than HQPlayer Embedded UI
//! - S3 Knob (hardware surface via /now_playing, /control APIs)
//! - Apple Watch / iOS apps (via REST API + SSE)
//! - Home Assistant (via MQTT)
//!
//! Using Pico CSS (classless CSS framework) for clean, accessible,
//! mobile-friendly design without custom CSS maintenance burden.
//!
//! Migration to Dioxus:
//! - components/ - Shared Dioxus components (nav, theme, layout)
//! - pages/ - Page components (settings, dashboard, zones, etc.)
//! - Legacy pages remain in this file until migrated

pub mod components;
pub mod pages;

use axum::{
    extract::State,
    response::{Html, IntoResponse},
};
use dioxus::prelude::*;
use serde::Deserialize;

use crate::api::AppState;
use pages::{DashboardPage, HqplayerPage, KnobsPage, LmsPage, SettingsPage, ZonePage, ZonesPage};

/// Query params for zones page (to detect knob requests)
#[derive(Deserialize)]
pub struct ZonesQuery {
    pub knob_id: Option<String>,
}

/// HTML document wrapper with Pico CSS
fn html_doc(title: &str, nav_active: &str, content: &str) -> String {
    let nav = nav_html(nav_active);
    let version = env!("CARGO_PKG_VERSION");
    format!(
        r#"<!DOCTYPE html>
<html lang="en" data-theme="dark">
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <title>{title} - Unified Hi-Fi Control</title>
    <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/@picocss/pico@2/css/pico.min.css">
    <style>
        :root {{ --pico-font-size: 15px; }}
        .status-ok {{ color: var(--pico-ins-color); }}
        .status-err {{ color: var(--pico-del-color); }}
        .zone-grid {{ display: grid; grid-template-columns: repeat(auto-fill, minmax(320px, 1fr)); gap: 1rem; }}
        .controls {{ display: flex; gap: 0.5rem; margin-top: 0.5rem; }}
        .controls button {{ margin: 0; padding: 0.5rem 1rem; }}
        small {{ color: var(--pico-muted-color); }}
        /* Black theme (OLED) - extends dark theme */
        [data-theme="dark"][data-variant="black"] {{
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
        }}
        /* Theme switcher */
        .theme-switcher {{ display: flex; gap: 0.25rem; }}
        .theme-switcher button {{ padding: 0.25rem 0.5rem; font-size: 0.8rem; margin: 0; }}
        .theme-switcher button.active {{ background: var(--pico-primary-background); color: var(--pico-primary-inverse); }}
    </style>
    <script>
        (function(){{
            const t = localStorage.getItem('hifi-theme') || 'dark';
            // Pico CSS only recognizes 'light' and 'dark'; black is dark + variant
            document.documentElement.setAttribute('data-theme', t === 'black' ? 'dark' : t);
            if (t === 'black') document.documentElement.setAttribute('data-variant', 'black');
        }})();
    </script>
</head>
<body>
    <header class="container">
        {nav}
    </header>
    <main class="container">
        {content}
    </main>
    <footer class="container" style="display:flex;justify-content:space-between;align-items:center;">
        <small>Unified Hi-Fi Control v{version}</small>
        <div class="theme-switcher">
            <button onclick="setTheme('light')" id="theme-light">Light</button>
            <button onclick="setTheme('dark')" id="theme-dark">Dark</button>
            <button onclick="setTheme('black')" id="theme-black">Black</button>
        </div>
    </footer>
    <script>
        function setTheme(t) {{
            // Pico CSS only recognizes 'light' and 'dark'; black is dark + variant
            document.documentElement.setAttribute('data-theme', t === 'black' ? 'dark' : t);
            if (t === 'black') {{
                document.documentElement.setAttribute('data-variant', 'black');
            }} else {{
                document.documentElement.removeAttribute('data-variant');
            }}
            localStorage.setItem('hifi-theme', t);
            updateThemeButtons();
        }}
        function updateThemeButtons() {{
            const variant = document.documentElement.getAttribute('data-variant');
            const theme = variant === 'black' ? 'black' : (document.documentElement.getAttribute('data-theme') || 'dark');
            ['light','dark','black'].forEach(x => {{
                const btn = document.getElementById('theme-' + x);
                if (btn) btn.classList.toggle('active', x === theme);
            }});
        }}
        function applyNavVisibility() {{
            const s = JSON.parse(localStorage.getItem('hifi-ui-settings') || '{{}}');
            const hide = (id, show) => {{
                const el = document.querySelector(`nav a[href*="${{id}}"]`);
                if (el) el.style.display = show !== false ? '' : 'none';
            }};
            hide('/hqplayer', s.showHqplayer);
            hide('/lms', s.showLms);
            hide('/knobs', s.showKnobs);
        }}
        // Auto-hide LMS if not configured (only if user hasn't explicitly enabled it)
        fetch('/lms/status').then(r => r.json()).then(st => {{
            const s = JSON.parse(localStorage.getItem('hifi-ui-settings') || '{{}}');
            if (!st.host && s.showLms !== true) {{
                const el = document.querySelector('nav a[href*="/lms"]');
                if (el) el.style.display = 'none';
            }}
        }}).catch(() => {{}});
        updateThemeButtons();
        applyNavVisibility();
    </script>
</body>
</html>"#,
        version = version
    )
}

/// Navigation HTML
fn nav_html(active: &str) -> String {
    let links = [
        ("dashboard", "Dashboard", "/"),
        ("zones", "Zones", "/ui/zones"),
        ("zone", "Zone", "/zone"),
        ("hqplayer", "HQPlayer", "/hqplayer"),
        ("lms", "LMS", "/lms"),
        ("knobs", "Knobs", "/knobs"),
        ("settings", "Settings", "/settings"),
    ];

    let items: String = links
        .iter()
        .map(|(id, label, href)| {
            if *id == active {
                format!(
                    r#"<li><a href="{href}" aria-current="page"><strong>{label}</strong></a></li>"#
                )
            } else {
                format!(r#"<li><a href="{href}">{label}</a></li>"#)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"<nav>
        <ul><li><strong>Hi-Fi Control</strong></li></ul>
        <ul>{items}</ul>
    </nav>"#
    )
}

/// GET / - Dashboard with status overview
/// Migrated to Dioxus SSR.
pub async fn dashboard_page(State(_state): State<AppState>) -> impl IntoResponse {
    let html = dioxus::ssr::render_element(rsx! { DashboardPage {} });
    Html(format!(
        "<!DOCTYPE html>\n<html lang=\"en\" data-theme=\"dark\">\n{}</html>",
        html
    ))
}

/// GET /ui/zones - Zones listing and control (HTML page)
/// Migrated to Dioxus SSR.
pub async fn zones_page(State(_state): State<AppState>) -> impl IntoResponse {
    let html = dioxus::ssr::render_element(rsx! { ZonesPage {} });
    Html(format!(
        "<!DOCTYPE html>\n<html lang=\"en\" data-theme=\"dark\">\n{}</html>",
        html
    ))
}

/// GET /hqplayer - HQPlayer status and DSP controls
/// Migrated to Dioxus SSR.
pub async fn hqplayer_page(State(_state): State<AppState>) -> impl IntoResponse {
    let html = dioxus::ssr::render_element(rsx! { HqplayerPage {} });
    Html(format!(
        "<!DOCTYPE html>\n<html lang=\"en\" data-theme=\"dark\">\n{}</html>",
        html
    ))
}

/// GET /lms - LMS status and players
/// Migrated to Dioxus SSR.
pub async fn lms_page(State(_state): State<AppState>) -> impl IntoResponse {
    let html = dioxus::ssr::render_element(rsx! { LmsPage {} });
    Html(format!(
        "<!DOCTYPE html>\n<html lang=\"en\" data-theme=\"dark\">\n{}</html>",
        html
    ))
}

/// GET /zone - Single zone control view
/// Migrated to Dioxus SSR.
pub async fn zone_page(State(_state): State<AppState>) -> impl IntoResponse {
    let html = dioxus::ssr::render_element(rsx! { ZonePage {} });
    Html(format!(
        "<!DOCTYPE html>\n<html lang=\"en\" data-theme=\"dark\">\n{}</html>",
        html
    ))
}

/// GET /knobs - Knob device management
/// Migrated to Dioxus SSR.
pub async fn knobs_page(State(_state): State<AppState>) -> impl IntoResponse {
    let html = dioxus::ssr::render_element(rsx! { KnobsPage {} });
    Html(format!(
        "<!DOCTYPE html>\n<html lang=\"en\" data-theme=\"dark\">\n{}</html>",
        html
    ))
}

/// GET /settings - Settings page (adapter configuration)
/// Migrated to Dioxus SSR.
pub async fn settings_page(State(_state): State<AppState>) -> impl IntoResponse {
    // Render the Dioxus component to HTML string
    let html = dioxus::ssr::render_element(rsx! { SettingsPage {} });
    Html(format!("<!DOCTYPE html>\n<html lang=\"en\" data-theme=\"dark\">\n{}</html>", html))
}

/// GET /knobs/flash - Web flasher redirect page
pub async fn flash_page() -> impl IntoResponse {
    let content = r#"
<h1>Flash Knob Firmware</h1>

<article>
    <p><strong>HTTPS Required</strong></p>
    <p>Browser-based flashing requires HTTPS. Use the official web flasher hosted on GitHub Pages:</p>
    <p>
        <a href="https://roon-knob.muness.com/" target="_blank" rel="noopener" role="button">
            Open Web Flasher →
        </a>
    </p>
    <footer>
        <small>The web flasher uses <a href="https://esphome.github.io/esp-web-tools/" target="_blank" rel="noopener">ESP Web Tools</a> to flash firmware directly from Chrome or Edge. No software installation required.</small>
    </footer>
</article>
"#;
    Html(html_doc("Flash Knob", "knobs", content))
}

/// Legacy redirects
pub async fn control_redirect() -> impl IntoResponse {
    axum::response::Redirect::to("/ui/zones")
}

pub async fn settings_redirect() -> impl IntoResponse {
    axum::response::Redirect::to("/settings")
}
