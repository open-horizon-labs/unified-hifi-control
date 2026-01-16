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

use axum::{
    extract::State,
    response::{Html, IntoResponse},
};
use serde::Deserialize;

use crate::api::AppState;

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
pub async fn dashboard_page(State(_state): State<AppState>) -> impl IntoResponse {
    let content = r#"
<h1>Dashboard</h1>

<section id="status">
    <hgroup>
        <h2>Service Status</h2>
        <p>Connection status for all adapters</p>
    </hgroup>
    <article aria-busy="true">Loading status...</article>
</section>

<script>
function esc(s) { return String(s).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'})[c]); }

async function loadStatus() {
    const section = document.querySelector('#status article');
    try {
        const [status, roon, hqp, lms] = await Promise.all([
            fetch('/status').then(r => r.json()),
            fetch('/roon/status').then(r => r.json()).catch(() => ({ connected: false })),
            fetch('/hqplayer/status').then(r => r.json()).catch(() => ({ connected: false })),
            fetch('/lms/status').then(r => r.json()).catch(() => ({ connected: false }))
        ]);

        section.removeAttribute('aria-busy');
        section.innerHTML = `
            <p><strong>Version:</strong> ${esc(status.version)}</p>
            <p><strong>Uptime:</strong> ${status.uptime_secs}s</p>
            <p><strong>Event Bus Subscribers:</strong> ${status.bus_subscribers}</p>
            <hr>
            <table>
                <thead><tr><th>Adapter</th><th>Status</th><th>Details</th></tr></thead>
                <tbody>
                    <tr>
                        <td>Roon</td>
                        <td class="${roon.connected ? 'status-ok' : 'status-err'}">${roon.connected ? '✓ Connected' : '✗ Disconnected'}</td>
                        <td><small>${esc(roon.core_name || '')} ${roon.core_version ? 'v' + esc(roon.core_version) : ''}</small></td>
                    </tr>
                    <tr>
                        <td>HQPlayer</td>
                        <td class="${hqp.connected ? 'status-ok' : 'status-err'}">${hqp.connected ? '✓ Connected' : '✗ Disconnected'}</td>
                        <td><small>${esc(hqp.host || '')}</small></td>
                    </tr>
                    <tr>
                        <td>LMS</td>
                        <td class="${lms.connected ? 'status-ok' : 'status-err'}">${lms.connected ? '✓ Connected' : '✗ Disconnected'}</td>
                        <td><small>${lms.host ? esc(lms.host) + ':' + lms.port : ''}</small></td>
                    </tr>
                    <tr>
                        <td>MQTT</td>
                        <td class="${status.mqtt_connected ? 'status-ok' : 'status-err'}">${status.mqtt_connected ? '✓ Connected' : '✗ Disconnected'}</td>
                        <td></td>
                    </tr>
                </tbody>
            </table>
        `;
    } catch (e) {
        section.removeAttribute('aria-busy');
        section.innerHTML = `<p class="status-err">Error loading status: ${esc(e.message)}</p>`;
    }
}
loadStatus();
setInterval(loadStatus, 10000);
</script>
"#;

    Html(html_doc("Dashboard", "dashboard", content))
}

/// GET /ui/zones - Zones listing and control (HTML page)
pub async fn zones_page(State(_state): State<AppState>) -> impl IntoResponse {
    let content = r#"
<h1>Zones</h1>

<section id="zones">
    <article aria-busy="true">Loading zones...</article>
</section>

<section id="hqp-dsp" style="display:none;">
    <hgroup>
        <h2>HQPlayer DSP</h2>
        <p>Matrix profiles for linked zones</p>
    </hgroup>
    <article id="hqp-dsp-controls">Loading...</article>
</section>

<script>
function esc(s) { return String(s).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'})[c]); }

let hqpZoneLinks = {};
let matrixProfiles = [];

async function loadZones() {
    const section = document.querySelector('#zones');
    try {
        const [zonesRes, linksRes] = await Promise.all([
            fetch('/zones').then(r => r.json()),
            fetch('/hqp/zones/links').then(r => r.json()).catch(() => ({ links: [] }))
        ]);
        // /zones returns {zones: [...]} with zone_id and zone_name
        const zones = zonesRes.zones || zonesRes || [];

        // Build HQP link lookup (API returns {links: [...]})
        const links = linksRes.links || linksRes || [];
        hqpZoneLinks = {};
        links.forEach(l => { hqpZoneLinks[l.zone_id] = l.instance; });

        if (!zones.length) {
            section.innerHTML = '<article>No zones available. Check that adapters are connected.</article>';
            return;
        }

        section.innerHTML = '<div class="zone-grid">' + zones.map(zone => {
            const playIcon = zone.state === 'playing' ? '⏸︎' : '▶';
            const hqpLink = hqpZoneLinks[zone.zone_id];
            const hqpBadge = hqpLink ? `<mark style="font-size:0.7em;padding:0.1em 0.3em;margin-left:0.5em;">HQP</mark>` : '';
            const sourceBadge = zone.source ? `<mark style="font-size:0.7em;padding:0.1em 0.3em;margin-left:0.5em;background:var(--pico-muted-background);">${esc(zone.source)}</mark>` : '';

            return `
                <article>
                    <header>
                        <strong>${esc(zone.zone_name)}</strong>${hqpBadge}${sourceBadge}
                        <small> (${esc(zone.state)})</small>
                    </header>
                    <div id="zone-info-${esc(zone.zone_id)}" style="min-height:40px;overflow:hidden;"><small>Loading...</small></div>
                    <footer>
                        <div class="controls" data-zone-id="${esc(zone.zone_id)}">
                            <button data-action="previous">◀◀</button>
                            <button data-action="play_pause">${playIcon}</button>
                            <button data-action="next">▶▶</button>
                        </div>
                    </footer>
                </article>
            `;
        }).join('') + '</div>';

        // Fetch now playing info for each zone
        zones.forEach(async zone => {
            const infoEl = document.getElementById('zone-info-' + zone.zone_id);
            if (!infoEl) return;
            try {
                const np = await fetch('/now_playing?zone_id=' + encodeURIComponent(zone.zone_id)).then(r => r.json());
                if (np && np.line1 && np.line1 !== 'Idle') {
                    infoEl.innerHTML = '<strong style="font-size:0.9em;">' + esc(np.line1) + '</strong><br><small>' + esc(np.line2 || '') + '</small>';
                } else {
                    infoEl.innerHTML = '<small>Nothing playing</small>';
                }
            } catch (e) {
                infoEl.innerHTML = '<small>—</small>';
            }
        });

        // Show HQP DSP section if any zone is linked
        const hasHqpLinks = Object.keys(hqpZoneLinks).length > 0;
        document.getElementById('hqp-dsp').style.display = hasHqpLinks ? 'block' : 'none';
        if (hasHqpLinks) loadHqpDsp();
    } catch (e) {
        section.innerHTML = `<article class="status-err">Error: ${esc(e.message)}</article>`;
    }
}

async function loadHqpDsp() {
    const section = document.getElementById('hqp-dsp-controls');
    try {
        const [profiles, pipeline] = await Promise.all([
            fetch('/hqplayer/matrix/profiles').then(r => r.json()).catch(() => []),
            fetch('/hqplayer/pipeline').then(r => r.json()).catch(() => null)
        ]);
        matrixProfiles = profiles || [];

        const st = pipeline?.status || {};
        const currentProfile = st.active_convolution || st.convolution || 'None';

        if (!matrixProfiles.length) {
            section.innerHTML = '<p>No matrix profiles available. Configure HQPlayer first.</p>';
            return;
        }

        section.innerHTML = `
            <div style="display:flex;gap:1rem;align-items:center;flex-wrap:wrap;">
                <label style="margin:0;">Matrix Profile:
                    <select id="matrix-select" style="width:auto;margin-left:0.5rem;">
                        <option value="">-- Select --</option>
                        ${matrixProfiles.map(p => {
                            const name = p.name || p;
                            const selected = name === currentProfile ? ' selected' : '';
                            return `<option value="${esc(name)}"${selected}>${esc(name)}</option>`;
                        }).join('')}
                    </select>
                </label>
                <span id="matrix-status"></span>
            </div>
            <p style="margin-top:0.5rem;"><small>Current: <strong>${esc(st.active_filter || 'N/A')}</strong> / <strong>${esc(st.active_shaper || 'N/A')}</strong></small></p>
        `;

        document.getElementById('matrix-select').addEventListener('change', async (e) => {
            const profile = e.target.value;
            if (!profile) return;
            const statusEl = document.getElementById('matrix-status');
            statusEl.textContent = 'Loading...';
            try {
                await fetch('/hqplayer/matrix/profile', {
                    method: 'POST',
                    headers: { 'Content-Type': 'application/json' },
                    body: JSON.stringify({ profile })
                });
                statusEl.innerHTML = '<span class="status-ok">✓</span>';
                setTimeout(loadHqpDsp, 500);
            } catch (err) {
                statusEl.innerHTML = '<span class="status-err">Failed</span>';
            }
        });
    } catch (e) {
        section.innerHTML = `<p class="status-err">Error: ${esc(e.message)}</p>`;
    }
}

async function control(zoneId, action) {
    try {
        await fetch('/control', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ zone_id: zoneId, action })
        });
        setTimeout(loadZones, 300);
    } catch (e) {
        console.error('Control error:', e);
    }
}

// Event delegation for zone controls (prevents XSS)
document.querySelector('#zones').addEventListener('click', e => {
    const btn = e.target.closest('button[data-action]');
    if (!btn) return;
    const container = btn.closest('[data-zone-id]');
    if (!container) return;
    control(container.dataset.zoneId, btn.dataset.action);
});

loadZones();
setInterval(loadZones, 4000);
</script>
"#;

    Html(html_doc("Zones", "zones", content))
}

/// GET /hqplayer - HQPlayer status and DSP controls
pub async fn hqplayer_page(State(_state): State<AppState>) -> impl IntoResponse {
    let content = r#"
<h1>HQPlayer</h1>

<section id="hqp-config">
    <hgroup>
        <h2>Configuration</h2>
        <p>HQPlayer connection settings</p>
    </hgroup>
    <article aria-busy="true">Loading...</article>
</section>

<section id="hqp-status">
    <hgroup>
        <h2>Connection Status</h2>
        <p>HQPlayer DSP engine connection</p>
    </hgroup>
    <article aria-busy="true">Loading...</article>
</section>

<section id="hqp-pipeline">
    <hgroup>
        <h2>Pipeline Settings</h2>
        <p>Current DSP configuration</p>
    </hgroup>
    <article aria-busy="true">Loading...</article>
</section>

<section id="hqp-profiles">
    <hgroup>
        <h2>Profiles</h2>
        <p>Saved configurations (requires web credentials)</p>
    </hgroup>
    <article aria-busy="true">Loading...</article>
</section>

<section id="hqp-zone-links">
    <hgroup>
        <h2>Zone Linking</h2>
        <p>Link audio zones to HQPlayer for DSP processing</p>
    </hgroup>
    <article aria-busy="true">Loading...</article>
</section>

<script>
function esc(s) { return String(s).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'})[c]); }

async function loadHqpConfig() {
    const section = document.querySelector('#hqp-config article');
    try {
        const config = await fetch('/hqplayer/config').then(r => r.json());
        section.removeAttribute('aria-busy');

        section.innerHTML = `
            <form id="hqp-config-form">
                <label>Host (IP or hostname)
                    <input type="text" name="host" value="${esc(config.host || '')}" placeholder="192.168.1.100" required>
                </label>
                <div class="grid">
                    <label>Native Port (TCP)
                        <input type="number" name="port" value="${config.port || 4321}" min="1" max="65535">
                    </label>
                    <label>Web Port (HTTP)
                        <input type="number" name="web_port" value="${config.web_port || 8088}" min="1" max="65535">
                        <small>For profile loading (HQPlayer Embedded)</small>
                    </label>
                </div>
                <div class="grid">
                    <label>Web Username
                        <input type="text" name="username" placeholder="admin">
                    </label>
                    <label>Web Password
                        <input type="password" name="password" placeholder="password">
                    </label>
                </div>
                <small>Web credentials enable profile switching via HQPlayer's web UI</small>
                <button type="submit">Save Configuration</button>
                <span id="config-status"></span>
            </form>
        `;
        document.getElementById('hqp-config-form').addEventListener('submit', saveHqpConfig);
    } catch (e) {
        section.removeAttribute('aria-busy');
        section.innerHTML = `<p class="status-err">Error: ${esc(e.message)}</p>`;
    }
}

async function saveHqpConfig(e) {
    e.preventDefault();
    const form = e.target;
    const statusEl = document.getElementById('config-status');
    statusEl.textContent = 'Saving...';

    const data = {
        host: form.host.value,
        port: parseInt(form.port.value) || 4321,
        web_port: parseInt(form.web_port.value) || 8088,
        username: form.username.value || null,
        password: form.password.value || null
    };

    try {
        const res = await fetch('/hqplayer/configure', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(data)
        });
        const result = await res.json();
        if (result.connected) {
            statusEl.innerHTML = '<span class="status-ok">✓ Connected!</span>';
        } else {
            statusEl.innerHTML = '<span class="status-err">Saved but not connected</span>';
        }
        loadHqpStatus();
        loadHqpPipeline();
        loadHqpProfiles();
    } catch (err) {
        statusEl.innerHTML = '<span class="status-err">Error: ' + esc(err.message) + '</span>';
    }
}

async function loadHqpStatus() {
    const section = document.querySelector('#hqp-status article');
    try {
        const [status, pipeline] = await Promise.all([
            fetch('/hqplayer/status').then(r => r.json()),
            fetch('/hqplayer/pipeline').then(r => r.json()).catch(() => null)
        ]);
        section.removeAttribute('aria-busy');

        if (!status.connected) {
            section.innerHTML = '<p class="status-err">Not connected to HQPlayer</p>';
            return;
        }

        const state = pipeline?.status?.state || 'Unknown';
        const info = status.info ? ` (${esc(status.info.name || status.info.product)})` : '';

        section.innerHTML = `
            <p class="status-ok">✓ Connected to ${esc(status.host || 'HQPlayer')}${info}</p>
            <p>State: <strong>${esc(state)}</strong></p>
        `;
    } catch (e) {
        section.removeAttribute('aria-busy');
        section.innerHTML = `<p class="status-err">Error: ${esc(e.message)}</p>`;
    }
}

async function loadHqpPipeline() {
    const section = document.querySelector('#hqp-pipeline article');
    try {
        const data = await fetch('/hqplayer/pipeline').then(r => r.json());
        section.removeAttribute('aria-busy');

        const st = data.status || {};
        const vol = data.volume || {};
        const formatRate = (r) => r >= 1000000 ? (r/1000000).toFixed(1) + ' MHz' : (r/1000).toFixed(1) + ' kHz';

        section.innerHTML = `
            <table>
                <tr><td>Mode</td><td>${esc(st.active_mode || st.mode || 'N/A')}</td></tr>
                <tr><td>Filter</td><td>${esc(st.active_filter || 'N/A')}</td></tr>
                <tr><td>Shaper</td><td>${esc(st.active_shaper || 'N/A')}</td></tr>
                <tr><td>Sample Rate</td><td>${st.active_rate ? formatRate(st.active_rate) : 'N/A'}</td></tr>
                <tr><td>Volume</td><td>${vol.value != null ? vol.value + ' dB' : 'N/A'}${vol.is_fixed ? ' (fixed)' : ''}</td></tr>
            </table>
            ${!vol.is_fixed ? `
            <hr>
            <label>Volume Control
                <input type="range" min="${vol.min || -60}" max="${vol.max || 0}" value="${vol.value || -20}"
                    oninput="this.nextElementSibling.textContent = this.value + ' dB'"
                    onchange="setVolume(this.value)">
                <output>${vol.value || -20} dB</output>
            </label>
            ` : ''}
        `;
    } catch (e) {
        section.removeAttribute('aria-busy');
        section.innerHTML = `<p class="status-err">Error: ${esc(e.message)}</p>`;
    }
}

async function loadHqpProfiles() {
    const section = document.querySelector('#hqp-profiles article');
    try {
        const profiles = await fetch('/hqplayer/profiles').then(r => r.json());
        section.removeAttribute('aria-busy');

        if (!profiles || !profiles.length) {
            section.innerHTML = '<p>No profiles available</p>';
            return;
        }

        section.innerHTML = `
            <table id="profiles-table">
                <thead><tr><th>Profile</th><th>Status</th><th>Action</th></tr></thead>
                <tbody>
                    ${profiles.map(p => `
                        <tr>
                            <td>${esc(p.title || p.name || p.value)}</td>
                            <td></td>
                            <td><button data-profile="${esc(p.value || p.title || p.name)}">Load</button></td>
                        </tr>
                    `).join('')}
                </tbody>
            </table>
        `;
        // Attach click handlers safely
        section.querySelectorAll('button[data-profile]').forEach(btn => {
            btn.addEventListener('click', () => loadProfile(btn.dataset.profile));
        });
    } catch (e) {
        section.removeAttribute('aria-busy');
        section.innerHTML = `<p class="status-err">Error: ${esc(e.message)}</p>`;
    }
}

async function setVolume(value) {
    try {
        await fetch('/hqplayer/volume', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ value: parseInt(value) })
        });
    } catch (e) { console.error(e); }
}

async function loadProfile(name) {
    try {
        await fetch('/hqplayer/profile', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ profile: name })
        });
        setTimeout(loadHqpProfiles, 500);
        setTimeout(loadHqpPipeline, 500);
    } catch (e) { console.error(e); }
}

// Zone link storage for badge display
let zoneLinkMap = {};

async function loadZoneLinks() {
    const section = document.querySelector('#hqp-zone-links article');
    try {
        const [linksRes, instancesRes, zonesRes] = await Promise.all([
            fetch('/hqp/zones/links').then(r => r.json()).catch(() => []),
            fetch('/hqp/instances').then(r => r.json()).catch(() => ({ instances: [] })),
            fetch('/knob/zones').then(r => r.json()).catch(() => ({ zones: [] }))
        ]);

        section.removeAttribute('aria-busy');
        const hqpInstances = instancesRes.instances || instancesRes || [];
        const zones = zonesRes.zones || zonesRes || [];
        const links = linksRes.links || linksRes || [];

        // Build link lookup: zone_id -> instance_name (for badge display)
        zoneLinkMap = {};
        links.forEach(l => { zoneLinkMap[l.zone_id] = l.instance; });

        if (!zones.length) {
            section.innerHTML = '<p>No audio zones available. Check that adapters are connected.</p>';
            return;
        }

        const instanceOptions = hqpInstances.length > 0
            ? hqpInstances.map(i => `<option value="${esc(i.name)}">${esc(i.name)} (${esc(i.host || 'unconfigured')})</option>`).join('')
            : '<option value="default">default</option>';

        // Get backend type from zone_id prefix
        const getBackend = (zid) => {
            if (zid.startsWith('lms:')) return 'LMS';
            if (zid.startsWith('openhome:')) return 'OpenHome';
            if (zid.startsWith('upnp:')) return 'UPnP';
            return 'Roon';
        };

        // Group zones by backend
        const backends = ['Roon', 'LMS', 'OpenHome', 'UPnP'];
        const zonesByBackend = {};
        backends.forEach(b => { zonesByBackend[b] = []; });
        zones.forEach(z => {
            const backend = getBackend(z.zone_id);
            if (zonesByBackend[backend]) zonesByBackend[backend].push(z);
        });

        section.innerHTML = `
            <details open>
                <summary>Filter by source</summary>
                <div style="display:flex;gap:1rem;margin:0.5rem 0;">
                    ${backends.map(b => `<label><input type="checkbox" class="backend-filter" data-backend="${b}" checked> ${b} (${zonesByBackend[b].length})</label>`).join('')}
                </div>
            </details>
            <table id="zone-links-table">
                <thead><tr><th>Zone</th><th>Source</th><th>HQPlayer Instance</th><th>Action</th></tr></thead>
                <tbody>
                    ${zones.map(z => {
                        const linked = zoneLinkMap[z.zone_id];
                        const backend = getBackend(z.zone_id);
                        return `
                        <tr data-zone-id="${esc(z.zone_id)}" data-backend="${backend}">
                            <td>${esc(z.zone_name)}</td>
                            <td><small>${backend}</small></td>
                            <td>
                                ${linked
                                    ? `<strong>${esc(linked)}</strong>`
                                    : `<select class="hqp-instance-select">${instanceOptions}</select>`
                                }
                            </td>
                            <td>
                                ${linked
                                    ? `<button class="unlink-btn outline secondary">Unlink</button>`
                                    : `<button class="link-btn">Link</button>`
                                }
                            </td>
                        </tr>`;
                    }).join('')}
                </tbody>
            </table>
        `;

        // Backend filter handlers
        section.querySelectorAll('.backend-filter').forEach(cb => {
            cb.addEventListener('change', () => {
                const backend = cb.dataset.backend;
                const show = cb.checked;
                section.querySelectorAll(`tr[data-backend="${backend}"]`).forEach(row => {
                    row.style.display = show ? '' : 'none';
                });
            });
        });

        // Attach link/unlink handlers
        section.querySelectorAll('.link-btn').forEach(btn => {
            btn.addEventListener('click', async () => {
                const row = btn.closest('tr');
                const zoneId = row.dataset.zoneId;
                const select = row.querySelector('.hqp-instance-select');
                const instanceName = select ? select.value : 'default';
                btn.disabled = true;
                btn.setAttribute('aria-busy', 'true');
                try {
                    await fetch('/hqp/zones/link', {
                        method: 'POST',
                        headers: { 'Content-Type': 'application/json' },
                        body: JSON.stringify({ zone_id: zoneId, instance: instanceName })
                    });
                    loadZoneLinks();
                } catch (e) {
                    console.error('Link failed:', e);
                    btn.disabled = false;
                    btn.removeAttribute('aria-busy');
                }
            });
        });

        section.querySelectorAll('.unlink-btn').forEach(btn => {
            btn.addEventListener('click', async () => {
                const row = btn.closest('tr');
                const zoneId = row.dataset.zoneId;
                btn.disabled = true;
                btn.setAttribute('aria-busy', 'true');
                try {
                    await fetch('/hqp/zones/unlink', {
                        method: 'POST',
                        headers: { 'Content-Type': 'application/json' },
                        body: JSON.stringify({ zone_id: zoneId })
                    });
                    loadZoneLinks();
                } catch (e) {
                    console.error('Unlink failed:', e);
                    btn.disabled = false;
                    btn.removeAttribute('aria-busy');
                }
            });
        });
    } catch (e) {
        section.removeAttribute('aria-busy');
        section.innerHTML = `<p class="status-err">Error: ${esc(e.message)}</p>`;
    }
}

loadHqpConfig();
loadHqpStatus();
loadHqpPipeline();
loadHqpProfiles();
loadZoneLinks();
setInterval(loadHqpStatus, 5000);
setInterval(loadHqpPipeline, 5000);
</script>
"#;

    Html(html_doc("HQPlayer", "hqplayer", content))
}

/// GET /lms - LMS status and players
pub async fn lms_page(State(_state): State<AppState>) -> impl IntoResponse {
    let content = r#"
<h1>Logitech Media Server</h1>

<section id="lms-config">
    <hgroup>
        <h2>Server Configuration</h2>
        <p>Configure connection to your Squeezebox server</p>
    </hgroup>
    <article id="lms-config-card">
        <div id="lms-status-line">Checking...</div>
        <div id="lms-config-form" style="display:none;">
            <div class="grid">
                <label>Host
                    <input type="text" id="lms-host" placeholder="192.168.1.x or hostname">
                </label>
                <label>Port
                    <input type="number" id="lms-port" value="9000" min="1" max="65535">
                </label>
            </div>
            <div class="grid">
                <label>Username (optional)
                    <input type="text" id="lms-username" placeholder="Leave blank if not required">
                </label>
                <label>Password (optional)
                    <input type="password" id="lms-password" placeholder="Leave blank if not required">
                </label>
            </div>
            <button onclick="saveLmsConfig()">Save & Connect</button>
            <span id="lms-save-msg"></span>
        </div>
        <button id="lms-reconfig-btn" style="display:none;" onclick="showLmsForm()">Reconfigure</button>
    </article>
</section>

<section id="lms-players">
    <hgroup>
        <h2>Players</h2>
        <p>Connected Squeezebox players</p>
    </hgroup>
    <article aria-busy="true">Loading...</article>
</section>

<script>
function esc(s) { return String(s).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'})[c]); }

async function loadLmsStatus() {
    const section = document.querySelector('#lms-status article');
    try {
        const status = await fetch('/lms/status').then(r => r.json());
        section.removeAttribute('aria-busy');

        if (!status.connected) {
            section.innerHTML = '<p class="status-err">Not connected to LMS</p>';
            return;
        }

        section.innerHTML = `
            <p class="status-ok">✓ Connected to ${esc(status.host)}:${status.port}</p>
            <p>Players: <strong>${status.player_count}</strong></p>
        `;
    } catch (e) {
        section.removeAttribute('aria-busy');
        section.innerHTML = `<p class="status-err">Error: ${esc(e.message)}</p>`;
    }
}

async function loadLmsPlayers() {
    const section = document.querySelector('#lms-players article');
    try {
        const players = await fetch('/lms/players').then(r => r.json());
        section.removeAttribute('aria-busy');

        if (!players || !players.length) {
            section.innerHTML = '<p>No players found</p>';
            return;
        }

        section.innerHTML = '<div class="zone-grid" id="lms-grid">' + players.map(player => {
            const playIcon = player.mode === 'play' ? '⏸' : '▶';
            return `
            <article>
                <header>
                    <strong>${esc(player.name)}</strong>
                    <small> (${esc(player.mode)})</small>
                </header>
                <p>
                    ${player.current_title ? esc(player.current_title) : '<small>Nothing playing</small>'}
                    ${player.artist ? `<br><small>${esc(player.artist)}</small>` : ''}
                </p>
                <footer>
                    <div class="controls" data-player-id="${esc(player.player_id)}">
                        <button data-action="previous">◀◀</button>
                        <button data-action="play_pause">${playIcon}</button>
                        <button data-action="next">▶▶</button>
                    </div>
                    <p>Volume: ${player.volume}%</p>
                </footer>
            </article>
        `}).join('') + '</div>';
    } catch (e) {
        section.removeAttribute('aria-busy');
        section.innerHTML = `<p class="status-err">Error: ${esc(e.message)}</p>`;
    }
}

async function lmsControl(playerId, action) {
    try {
        await fetch('/lms/control', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ player_id: playerId, action })
        });
        setTimeout(loadLmsPlayers, 300);
    } catch (e) { console.error(e); }
}

// Event delegation for LMS player controls (runs once, not per-refresh)
document.querySelector('#lms-players').addEventListener('click', e => {
    const btn = e.target.closest('button[data-action]');
    if (!btn) return;
    const container = btn.closest('[data-player-id]');
    if (!container) return;
    lmsControl(container.dataset.playerId, btn.dataset.action);
});

// LMS Config
async function loadLmsConfig() {
    const statusLine = document.getElementById('lms-status-line');
    const form = document.getElementById('lms-config-form');
    const reconfigBtn = document.getElementById('lms-reconfig-btn');

    try {
        const res = await fetch('/lms/config');
        const data = await res.json();

        if (data.configured && data.connected) {
            statusLine.innerHTML = `<span class="status-ok">✓ Connected to ${esc(data.host)}:${data.port}</span>`;
            form.style.display = 'none';
            reconfigBtn.style.display = 'inline-block';
            document.getElementById('lms-host').value = data.host || '';
            document.getElementById('lms-port').value = data.port || 9000;
        } else if (data.configured) {
            statusLine.innerHTML = `<span class="status-err">✗ Configured but not connected (${esc(data.host)}:${data.port})</span>`;
            form.style.display = 'none';
            reconfigBtn.style.display = 'inline-block';
        } else {
            statusLine.textContent = 'Not configured';
            form.style.display = 'block';
            reconfigBtn.style.display = 'none';
        }
    } catch (e) {
        statusLine.innerHTML = `<span class="status-err">Error: ${esc(e.message)}</span>`;
        form.style.display = 'block';
    }
}

function showLmsForm() {
    document.getElementById('lms-config-form').style.display = 'block';
    document.getElementById('lms-reconfig-btn').style.display = 'none';
}

async function saveLmsConfig() {
    const msg = document.getElementById('lms-save-msg');
    const host = document.getElementById('lms-host').value.trim();
    const port = parseInt(document.getElementById('lms-port').value) || 9000;
    const username = document.getElementById('lms-username').value.trim() || null;
    const password = document.getElementById('lms-password').value || null;

    if (!host) {
        msg.innerHTML = '<span class="status-err">Host is required</span>';
        return;
    }

    msg.textContent = 'Connecting...';
    try {
        const res = await fetch('/lms/configure', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ host, port, username, password })
        });
        const data = await res.json();
        if (res.ok) {
            msg.innerHTML = '<span class="status-ok">✓ Connected</span>';
            setTimeout(loadLmsConfig, 500);
            setTimeout(loadLmsPlayers, 500);
        } else {
            msg.innerHTML = `<span class="status-err">${esc(data.error || 'Connection failed')}</span>`;
        }
    } catch (e) {
        msg.innerHTML = `<span class="status-err">Error: ${esc(e.message)}</span>`;
    }
}

loadLmsConfig();
loadLmsPlayers();
setInterval(loadLmsConfig, 10000);
setInterval(loadLmsPlayers, 4000);
</script>
"#;

    Html(html_doc("LMS", "lms", content))
}

/// GET /zone - Single zone control view
pub async fn zone_page(State(_state): State<AppState>) -> impl IntoResponse {
    let content = r#"
<h1>Zone Control</h1>
<p><small>Select a zone for focused listening and DSP control.</small></p>

<label for="zone-select">Zone
    <select id="zone-select">
        <option value="">Loading zones...</option>
    </select>
</label>

<article id="zone-display" style="display:none;">
    <div style="display:flex;gap:1.5rem;align-items:flex-start;flex-wrap:wrap;">
        <img id="zone-art" src="" alt="Album art" style="width:200px;height:200px;object-fit:cover;border-radius:8px;background:#222;">
        <div style="flex:1;min-width:200px;">
            <h2 id="zone-name" style="margin-bottom:0.25rem;"></h2>
            <p id="zone-state" style="margin:0;"><small>—</small></p>
            <hr>
            <p id="zone-track" style="margin:0.5rem 0;"><strong>—</strong></p>
            <p id="zone-artist" style="margin:0;"><small>—</small></p>
            <p id="zone-album" style="margin:0;color:var(--pico-muted-color);"><small>—</small></p>
            <hr>
            <div style="display:flex;gap:0.5rem;align-items:center;margin:1rem 0;">
                <button id="btn-prev">◀◀</button>
                <button id="btn-play">▶</button>
                <button id="btn-next">▶▶</button>
                <span style="margin-left:1rem;">Volume: <strong id="zone-volume">—</strong></span>
                <button id="btn-vol-down" style="width:2.5rem;" title="Volume Down">−</button>
                <button id="btn-vol-up" style="width:2.5rem;" title="Volume Up">+</button>
            </div>
        </div>
    </div>
</article>

<section id="hqp-section" style="display:none;">
    <hgroup>
        <h2>HQPlayer DSP</h2>
        <p>Pipeline controls for zone-linked HQPlayer</p>
    </hgroup>
    <article id="hqp-controls">
        <div id="hqp-loading" aria-busy="true">Loading DSP settings...</div>
        <div id="hqp-settings" style="display:none;">
            <div class="grid" style="margin-bottom:0.5rem;">
                <label>Matrix Profile
                    <select id="hqp-matrix" onchange="setMatrixProfile(this.value)"></select>
                </label>
            </div>
            <div class="grid">
                <label>Mode
                    <select id="hqp-mode" onchange="setPipeline('mode', this.value)"></select>
                </label>
                <label>Sample Rate
                    <select id="hqp-samplerate" onchange="setPipeline('samplerate', this.value)"></select>
                </label>
            </div>
            <div class="grid">
                <label>Filter (1x)
                    <select id="hqp-filter1x" onchange="setPipeline('filter1x', this.value)"></select>
                </label>
                <label>Filter (Nx)
                    <select id="hqp-filterNx" onchange="setPipeline('filterNx', this.value)"></select>
                </label>
            </div>
            <div class="grid">
                <label><span id="hqp-shaper-label">Shaper</span>
                    <select id="hqp-shaper" onchange="setPipeline('shaper', this.value)"></select>
                </label>
            </div>
            <p id="hqp-msg" style="margin-top:0.5rem;"></p>
        </div>
    </article>
</section>

<script>
function esc(s) { return String(s || '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'})[c]); }

let selectedZone = localStorage.getItem('hifi-zone') || null;
let zonesData = [];
let zoneLinkMap = {};
let lastHqpZone = null; // Track last zone for HQP to avoid reloading on every update
let hqpPipelineLoaded = false;
let nowPlayingData = null; // Current now_playing data for selected zone

async function loadZones() {
    try {
        const [zonesRes, linksRes] = await Promise.all([
            fetch('/zones').then(r => r.json()),
            fetch('/hqp/zones/links').then(r => r.json()).catch(() => ({ links: [] }))
        ]);
        // /zones returns {zones: [...]} with zone_id and zone_name
        zonesData = zonesRes.zones || zonesRes || [];

        // Build zone link map
        const links = linksRes.links || linksRes || [];
        zoneLinkMap = {};
        links.forEach(l => { zoneLinkMap[l.zone_id] = l.instance; });

        const sel = document.getElementById('zone-select');
        sel.innerHTML = '<option value="">-- Select Zone --</option>' +
            zonesData.map(z => {
                const hqpBadge = zoneLinkMap[z.zone_id] ? ' [HQP]' : '';
                const source = z.source ? ' (' + z.source + ')' : '';
                return '<option value="' + esc(z.zone_id) + '"' + (z.zone_id === selectedZone ? ' selected' : '') + '>' + esc(z.zone_name) + hqpBadge + source + '</option>';
            }).join('');

        if (selectedZone) {
            const zone = zonesData.find(z => z.zone_id === selectedZone);
            if (zone) {
                await loadNowPlaying(zone);
            } else {
                selectedZone = null;
                localStorage.removeItem('hifi-zone');
            }
        }
    } catch (e) {
        console.error('Error loading zones:', e);
    }
}

async function loadNowPlaying(zone) {
    try {
        const res = await fetch('/now_playing?zone_id=' + encodeURIComponent(zone.zone_id));
        nowPlayingData = await res.json();
        updateZoneDisplay(zone, nowPlayingData);
    } catch (e) {
        console.error('Error loading now playing:', e);
        updateZoneDisplay(zone, null);
    }
}

document.getElementById('zone-select').addEventListener('change', async e => {
    selectedZone = e.target.value;
    if (selectedZone) {
        localStorage.setItem('hifi-zone', selectedZone);
        const zone = zonesData.find(z => z.zone_id === selectedZone);
        if (zone) await loadNowPlaying(zone);
    } else {
        localStorage.removeItem('hifi-zone');
        document.getElementById('zone-display').style.display = 'none';
        document.getElementById('hqp-section').style.display = 'none';
    }
});

function updateZoneDisplay(zone, np) {
    document.getElementById('zone-display').style.display = 'block';
    document.getElementById('zone-name').textContent = zone.zone_name || zone.zone_id;
    const state = np?.is_playing ? 'playing' : 'stopped';
    document.getElementById('zone-state').innerHTML = '<small>' + esc(state) + '</small>';

    // Now playing from /now_playing API (uses line1/line2/line3)
    if (np && np.line1 && np.line1 !== 'Idle') {
        document.getElementById('zone-track').innerHTML = '<strong>' + esc(np.line1 || '—') + '</strong>';
        document.getElementById('zone-artist').innerHTML = '<small>' + esc(np.line2 || '') + '</small>';
        document.getElementById('zone-album').innerHTML = '<small>' + esc(np.line3 || '') + '</small>';
        if (np.image_url) {
            const url = np.image_url;
            const sep = url.includes('?') ? (url.endsWith('?') || url.endsWith('&') ? '' : '&') : '?';
            document.getElementById('zone-art').src = url + sep + 'width=200&height=200&t=' + Date.now();
        } else {
            document.getElementById('zone-art').src = '';
        }
    } else {
        document.getElementById('zone-track').innerHTML = '<strong>Nothing playing</strong>';
        document.getElementById('zone-artist').innerHTML = '';
        document.getElementById('zone-album').innerHTML = '';
        document.getElementById('zone-art').src = '';
    }

    // Volume from now_playing API
    if (np && np.volume != null) {
        const suffix = np.volume_type === 'db' ? ' dB' : '';
        document.getElementById('zone-volume').textContent = Math.round(np.volume) + suffix;
    } else {
        document.getElementById('zone-volume').textContent = '—';
    }

    // Update button states from now_playing API
    const isPlaying = np?.is_playing || false;
    document.getElementById('btn-prev').disabled = !np?.is_previous_allowed;
    document.getElementById('btn-next').disabled = !np?.is_next_allowed;
    document.getElementById('btn-play').textContent = isPlaying ? '⏸︎' : '▶';

    // Show/hide HQP section based on zone link
    const hqpInstance = zoneLinkMap[zone.zone_id];
    const hqpSection = document.getElementById('hqp-section');
    if (hqpInstance) {
        hqpSection.style.display = 'block';
        // Only reload HQP pipeline when zone changes or not loaded yet
        if (lastHqpZone !== zone.zone_id || !hqpPipelineLoaded) {
            lastHqpZone = zone.zone_id;
            loadHqpPipeline();
        }
    } else {
        hqpSection.style.display = 'none';
        lastHqpZone = null;
        hqpPipelineLoaded = false;
    }
}

// HQPlayer DSP functions
async function loadHqpPipeline() {
    const loading = document.getElementById('hqp-loading');
    const settings = document.getElementById('hqp-settings');
    loading.style.display = 'block';
    settings.style.display = 'none';

    try {
        const [pipeline, matrixRes] = await Promise.all([
            fetch('/hqplayer/pipeline').then(r => r.json()),
            fetch('/hqplayer/matrix/profiles').then(r => r.json()).catch(() => ({ profiles: [] }))
        ]);

        const s = pipeline.settings || {};

        // Populate selects
        populateSelect('hqp-mode', s.mode?.options, s.mode?.selected?.value);
        populateSelect('hqp-samplerate', s.samplerate?.options, s.samplerate?.selected?.value);
        populateSelect('hqp-filter1x', s.filter1x?.options, s.filter1x?.selected?.value);
        populateSelect('hqp-filterNx', s.filterNx?.options, s.filterNx?.selected?.value);
        populateSelect('hqp-shaper', s.shaper?.options, s.shaper?.selected?.value);

        // Update shaper label based on mode
        const modeLabel = (s.mode?.selected?.label || '').toLowerCase();
        document.getElementById('hqp-shaper-label').textContent =
            (modeLabel.includes('sdm') || modeLabel.includes('dsd')) ? 'Modulator' : 'Dither';

        // Populate matrix profiles
        const matrixProfiles = matrixRes.profiles || [];
        const matrixSelect = document.getElementById('hqp-matrix');
        if (matrixProfiles.length > 0) {
            matrixSelect.innerHTML = matrixProfiles.map(p => {
                const name = p.name || p;
                const selected = name === matrixRes.current ? ' selected' : '';
                return '<option value="' + esc(name) + '"' + selected + '>' + esc(name) + '</option>';
            }).join('');
            matrixSelect.closest('label').style.display = '';
        } else {
            matrixSelect.closest('label').style.display = 'none';
        }

        loading.style.display = 'none';
        settings.style.display = 'block';
        hqpPipelineLoaded = true;
    } catch (e) {
        loading.textContent = 'HQPlayer not connected';
        loading.removeAttribute('aria-busy');
        hqpPipelineLoaded = false;
        console.error('HQP pipeline error:', e);
    }
}

function populateSelect(id, options, selected) {
    const sel = document.getElementById(id);
    if (!options || !options.length) {
        sel.innerHTML = '<option>N/A</option>';
        sel.disabled = true;
        return;
    }
    sel.disabled = false;
    sel.innerHTML = options.map(o => {
        const val = o.value || o;
        const label = o.label || o.value || o;
        const sel = val === selected ? ' selected' : '';
        return '<option value="' + esc(val) + '"' + sel + '>' + esc(label) + '</option>';
    }).join('');
}

async function setPipeline(setting, value) {
    const msg = document.getElementById('hqp-msg');
    msg.textContent = 'Updating...';
    msg.className = '';

    // Disable controls during update
    const selects = ['hqp-mode', 'hqp-samplerate', 'hqp-filter1x', 'hqp-filterNx', 'hqp-shaper', 'hqp-matrix'];
    selects.forEach(id => { const el = document.getElementById(id); if (el) el.disabled = true; });

    try {
        const res = await fetch('/hqplayer/setting', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ setting, value })
        });
        if (res.ok) {
            msg.innerHTML = '<span class="status-ok">Updated</span>';
            setTimeout(() => { msg.textContent = ''; }, 2000);
            setTimeout(loadHqpPipeline, 500);
        } else {
            const data = await res.json();
            msg.innerHTML = '<span class="status-err">' + esc(data.error || 'Error') + '</span>';
        }
    } catch (e) {
        msg.innerHTML = '<span class="status-err">' + esc(e.message) + '</span>';
    } finally {
        selects.forEach(id => { const el = document.getElementById(id); if (el) el.disabled = false; });
    }
}

async function setMatrixProfile(profile) {
    if (!profile) return;
    const msg = document.getElementById('hqp-msg');
    msg.textContent = 'Setting matrix...';

    const selects = ['hqp-mode', 'hqp-samplerate', 'hqp-filter1x', 'hqp-filterNx', 'hqp-shaper', 'hqp-matrix'];
    selects.forEach(id => { const el = document.getElementById(id); if (el) el.disabled = true; });

    try {
        const res = await fetch('/hqplayer/matrix/profile', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ profile })
        });
        if (res.ok) {
            msg.innerHTML = '<span class="status-ok">Matrix updated</span>';
            setTimeout(() => { msg.textContent = ''; }, 2000);
        } else {
            msg.innerHTML = '<span class="status-err">Failed</span>';
        }
    } catch (e) {
        msg.innerHTML = '<span class="status-err">' + esc(e.message) + '</span>';
    } finally {
        selects.forEach(id => { const el = document.getElementById(id); if (el) el.disabled = false; });
    }
}

async function control(action) {
    if (!selectedZone) return;
    try {
        await fetch('/control', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ zone_id: selectedZone, action })
        });
        setTimeout(loadZones, 300);
    } catch (e) {
        console.error('Control error:', e);
    }
}

async function volume(delta) {
    if (!selectedZone) return;
    try {
        // Use unified control API with volume action
        const action = delta > 0 ? 'vol_up' : 'vol_down';
        await fetch('/control', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ zone_id: selectedZone, action, value: Math.abs(delta) })
        });
        setTimeout(loadZones, 200);
    } catch (e) {
        console.error('Volume error:', e);
    }
}

// Button handlers
document.getElementById('btn-prev').addEventListener('click', () => control('previous'));
document.getElementById('btn-play').addEventListener('click', () => control('play_pause'));
document.getElementById('btn-next').addEventListener('click', () => control('next'));
document.getElementById('btn-vol-down').addEventListener('click', () => volume(-2));
document.getElementById('btn-vol-up').addEventListener('click', () => volume(2));

loadZones();
setInterval(loadZones, 3000);
</script>
"#;

    Html(html_doc("Zone", "zone", content))
}

/// GET /knobs - Knob device management
pub async fn knobs_page(State(_state): State<AppState>) -> impl IntoResponse {
    let content = r#"
<h1>Knob Devices</h1>

<p><a href="https://community.roonlabs.com/t/50-esp32-s3-knob-roon-controller/311363" target="_blank" rel="noopener">Knob Community Thread</a> - build info, firmware updates, discussion</p>

<section id="knobs-section">
    <article aria-busy="true">Loading knobs...</article>
</section>

<section id="firmware-section">
    <hgroup>
        <h2>Firmware</h2>
        <p>Manage knob firmware updates</p>
    </hgroup>
    <article>
        <p>Current: <strong id="fw-version">checking...</strong></p>
        <button id="fetch-btn">Fetch Latest from GitHub</button>
        <a href="/knobs/flash" style="margin-left:1rem;">Flash a new knob</a>
        <span id="fw-msg"></span>
    </article>
</section>

<dialog id="config-modal">
    <article>
        <header>
            <button aria-label="Close" rel="prev" onclick="closeModal()"></button>
            <h2>Knob Configuration</h2>
        </header>
        <div id="config-form-container">Loading...</div>
    </article>
</dialog>

<script>
function esc(s) { return String(s || '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'})[c]); }
function escAttr(s) { return String(s || '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'})[c]); }

let zonesData = [];
let currentKnobId = null;

function ago(ts) {
    if (!ts) return 'never';
    const diff = Date.now() - new Date(ts).getTime();
    const s = Math.floor(diff / 1000);
    if (s < 60) return s + 's ago';
    const m = Math.floor(s / 60);
    if (m < 60) return m + 'm ago';
    const h = Math.floor(m / 60);
    if (h < 24) return h + 'h ago';
    return Math.floor(h / 24) + 'd ago';
}

function knobDisplayName(knob) {
    if (knob.name) return esc(knob.name);
    const id = knob.knob_id || '';
    const suffix = id.replace(/[:-]/g, '').slice(-6).toLowerCase();
    return suffix ? '<small>roon-knob-' + suffix + '</small>' : '<small>unnamed</small>';
}

async function loadKnobs() {
    const section = document.querySelector('#knobs-section');
    try {
        const [devicesRes, zonesRes] = await Promise.all([
            fetch('/knob/devices').then(r => r.json()),
            fetch('/zones').then(r => r.json()).catch(() => ({ zones: [] }))
        ]);
        const knobs = devicesRes.knobs || [];
        // /zones returns {zones: [...]} with zone_id and zone_name
        zonesData = zonesRes.zones || zonesRes || [];

        if (knobs.length === 0) {
            section.innerHTML = '<article>No knobs registered. Connect a knob to see it here.</article>';
            return;
        }

        section.innerHTML = '<article><table><thead><tr><th>ID</th><th>Name</th><th>Version</th><th>IP</th><th>Zone</th><th>Battery</th><th>Last Seen</th><th></th></tr></thead><tbody id="knobs-body">' +
            knobs.map(k => {
                const st = k.status || {};
                const bat = st.battery_level != null ? st.battery_level + '%' + (st.battery_charging ? ' ⚡' : '') : '—';
                const zone = st.zone_id ? esc(zonesData.find(z => z.zone_id === st.zone_id)?.zone_name || st.zone_id) : '—';
                const ip = st.ip || '—';
                return '<tr><td><code>' + esc(k.knob_id) + '</code></td><td>' + knobDisplayName(k) + '</td><td>' + esc(k.version || '—') + '</td><td>' + esc(ip) + '</td><td>' + zone + '</td><td>' + bat + '</td><td>' + ago(k.last_seen) + '</td><td><button class="outline secondary" data-knob-id="' + escAttr(k.knob_id) + '">Config</button></td></tr>';
            }).join('') + '</tbody></table></article>';
    } catch (e) {
        section.innerHTML = '<article class="status-err">Error: ' + esc(e.message) + '</article>';
    }
}

// Event delegation for config buttons
document.querySelector('#knobs-section').addEventListener('click', e => {
    const btn = e.target.closest('button[data-knob-id]');
    if (btn) openConfig(btn.dataset.knobId);
});

function openModal() { document.getElementById('config-modal').showModal(); }
function closeModal() { document.getElementById('config-modal').close(); }

async function openConfig(knobId) {
    currentKnobId = knobId;
    openModal();
    const container = document.getElementById('config-form-container');
    container.innerHTML = '<p aria-busy="true">Loading configuration...</p>';

    try {
        const res = await fetch('/knob/config?knob_id=' + encodeURIComponent(knobId));
        const data = await res.json();
        const c = data.config || {};

        const rotSel = (n, v) => '<select name="' + n + '"><option value="0"' + (v === 0 ? ' selected' : '') + '>0°</option><option value="180"' + (v === 180 ? ' selected' : '') + '>180°</option></select>';
        const artChg = c.art_mode_charging || { enabled: true, timeout_sec: 60 };
        const artBat = c.art_mode_battery || { enabled: true, timeout_sec: 30 };
        const dimChg = c.dim_charging || { enabled: true, timeout_sec: 120 };
        const dimBat = c.dim_battery || { enabled: true, timeout_sec: 30 };
        const slpChg = c.sleep_charging || { enabled: false, timeout_sec: 0 };
        const slpBat = c.sleep_battery || { enabled: true, timeout_sec: 60 };
        const dslpChg = c.deep_sleep_charging || { enabled: false, timeout_sec: 0 };
        const dslpBat = c.deep_sleep_battery || { enabled: true, timeout_sec: 1200 };

        // Helper for timer cell: checkbox then seconds input, side by side
        const timerCell = (name, cfg) => '<td style="white-space:nowrap;padding:0.25rem;"><div style="display:flex;align-items:center;gap:0.25rem;"><input type="checkbox" name="' + name + '_on"' + (cfg.enabled ? ' checked' : '') + ' style="margin:0;flex-shrink:0;"><input type="number" name="' + name + '_sec" value="' + cfg.timeout_sec + '" style="width:5rem;margin:0;padding:0.25rem;text-align:right;" min="0"' + (cfg.enabled ? '' : ' disabled') + '><span style="flex-shrink:0;">s</span></div></td>';

        container.innerHTML = '<form id="knob-config-form">' +
            '<label>Name<input type="text" name="name" value="' + escAttr(c.name || '') + '" placeholder="Living Room Knob"></label>' +
            '<fieldset><legend>Display Rotation</legend>' +
            '<div class="grid"><label>Charging: ' + rotSel('rotation_charging', c.rotation_charging ?? 180) + '</label>' +
            '<label>Battery: ' + rotSel('rotation_not_charging', c.rotation_not_charging ?? 0) + '</label></div></fieldset>' +
            '<fieldset><legend>Power Timers</legend><p style="margin-bottom:0.5rem;"><small>After inactivity: Art Mode → Dim → Sleep → Deep Sleep</small></p>' +
            '<table style="font-size:0.9rem;"><thead><tr><th style="width:6rem;"></th><th>Charging</th><th>Battery</th></tr></thead><tbody>' +
            '<tr><td>Art Mode</td>' + timerCell('art_chg', artChg) + timerCell('art_bat', artBat) + '</tr>' +
            '<tr><td>Dim</td>' + timerCell('dim_chg', dimChg) + timerCell('dim_bat', dimBat) + '</tr>' +
            '<tr><td>Sleep</td>' + timerCell('slp_chg', slpChg) + timerCell('slp_bat', slpBat) + '</tr>' +
            '<tr><td>Deep Sleep</td>' + timerCell('dslp_chg', dslpChg) + timerCell('dslp_bat', dslpBat) + '</tr>' +
            '</tbody></table></fieldset>' +
            '<fieldset><legend>Sleep Mode</legend>' +
            '<div style="display:flex;gap:1rem;flex-wrap:wrap;align-items:center;">' +
            '<label style="margin:0;"><input type="checkbox" name="wifi_ps"' + (c.wifi_power_save_enabled ? ' checked' : '') + '> WiFi Power Save</label>' +
            '<label style="margin:0;"><input type="checkbox" name="cpu_scale"' + (c.cpu_freq_scaling_enabled ? ' checked' : '') + '> CPU Scaling</label>' +
            '<label style="margin:0;">Poll: <input type="number" name="sleep_poll_stopped" value="' + (c.sleep_poll_stopped_sec ?? 60) + '" style="width:4rem;margin:0 0.25rem;padding:0.25rem;" min="1">s</label>' +
            '</div></fieldset>' +
            '<footer><button type="button" class="secondary" onclick="closeModal()">Cancel</button><button type="submit">Save</button></footer></form>';

        document.getElementById('knob-config-form').addEventListener('submit', saveConfig);
        document.getElementById('knob-config-form').addEventListener('change', e => {
            if (e.target.type === 'checkbox' && e.target.closest('td')) {
                const numInput = e.target.closest('td').querySelector('input[type=number]');
                if (numInput) numInput.disabled = !e.target.checked;
            }
        });
    } catch (e) {
        container.innerHTML = '<p class="status-err">Error: ' + esc(e.message) + '</p>';
    }
}

async function saveConfig(e) {
    e.preventDefault();
    const f = e.target;
    const v = n => f.querySelector('[name="' + n + '"]')?.value || '';
    const num = n => parseInt(v(n)) || 0;
    const chk = n => f.querySelector('[name="' + n + '"]')?.checked || false;

    const cfg = {
        name: v('name') || null,
        rotation_charging: num('rotation_charging'),
        rotation_not_charging: num('rotation_not_charging'),
        art_mode_charging: { enabled: chk('art_chg_on'), timeout_sec: num('art_chg_sec') },
        art_mode_battery: { enabled: chk('art_bat_on'), timeout_sec: num('art_bat_sec') },
        dim_charging: { enabled: chk('dim_chg_on'), timeout_sec: num('dim_chg_sec') },
        dim_battery: { enabled: chk('dim_bat_on'), timeout_sec: num('dim_bat_sec') },
        sleep_charging: { enabled: chk('slp_chg_on'), timeout_sec: num('slp_chg_sec') },
        sleep_battery: { enabled: chk('slp_bat_on'), timeout_sec: num('slp_bat_sec') },
        deep_sleep_charging: { enabled: chk('dslp_chg_on'), timeout_sec: num('dslp_chg_sec') },
        deep_sleep_battery: { enabled: chk('dslp_bat_on'), timeout_sec: num('dslp_bat_sec') },
        wifi_power_save_enabled: chk('wifi_ps'),
        cpu_freq_scaling_enabled: chk('cpu_scale'),
        sleep_poll_stopped_sec: num('sleep_poll_stopped'),
    };

    try {
        const res = await fetch('/knob/config?knob_id=' + encodeURIComponent(currentKnobId), {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(cfg)
        });
        if (res.ok) {
            closeModal();
            loadKnobs();
        } else {
            alert('Save failed');
        }
    } catch (e) {
        alert('Error: ' + e.message);
    }
}

// Close modal on Escape or click outside
document.getElementById('config-modal').addEventListener('click', e => {
    if (e.target.id === 'config-modal') closeModal();
});

// Firmware
async function loadFirmwareVersion() {
    const el = document.getElementById('fw-version');
    try {
        const res = await fetch('/firmware/version');
        if (res.ok) {
            const data = await res.json();
            el.textContent = 'v' + data.version;
        } else {
            el.textContent = 'Not installed';
        }
    } catch (e) {
        el.textContent = 'Not installed';
    }
}

document.getElementById('fetch-btn').addEventListener('click', async () => {
    const btn = document.getElementById('fetch-btn');
    const msg = document.getElementById('fw-msg');
    btn.disabled = true;
    btn.setAttribute('aria-busy', 'true');
    msg.textContent = '';

    try {
        const res = await fetch('/admin/fetch-firmware', { method: 'POST' });
        const data = await res.json();
        if (res.ok) {
            msg.innerHTML = ' <span class="status-ok">Downloaded v' + esc(data.version) + '</span>';
            document.getElementById('fw-version').textContent = 'v' + data.version;
        } else {
            msg.innerHTML = ' <span class="status-err">' + esc(data.error) + '</span>';
        }
    } catch (e) {
        msg.innerHTML = ' <span class="status-err">' + esc(e.message) + '</span>';
    } finally {
        btn.disabled = false;
        btn.removeAttribute('aria-busy');
    }
});

loadKnobs();
loadFirmwareVersion();
setInterval(loadKnobs, 10000);
</script>
"#;

    Html(html_doc("Knobs", "knobs", content))
}

/// GET /settings - Settings page (adapter configuration)
pub async fn settings_page(State(_state): State<AppState>) -> impl IntoResponse {
    let content = r#"
<h1>Settings</h1>

<section id="adapter-settings">
    <hgroup>
        <h2>Adapter Settings</h2>
        <p>Enable or disable zone sources</p>
    </hgroup>
    <article id="adapter-toggles">
        <div style="display:flex;flex-wrap:wrap;gap:1.5rem;">
            <label><input type="checkbox" id="adapter-roon"> Roon</label>
            <label><input type="checkbox" id="adapter-lms"> LMS</label>
            <label><input type="checkbox" id="adapter-openhome"> OpenHome</label>
            <label><input type="checkbox" id="adapter-upnp"> UPnP/DLNA</label>
        </div>
        <p style="margin-top:0.5rem;"><small>Changes take effect immediately. Disabled adapters won't contribute zones.</small></p>
    </article>
</section>

<section id="ui-settings">
    <hgroup>
        <h2>UI Settings</h2>
        <p>Customize navigation tabs</p>
    </hgroup>
    <article>
        <div style="display:flex;flex-wrap:wrap;gap:1.5rem;">
            <label><input type="checkbox" id="show-hqplayer" checked onchange="saveUiSettings()"> HQPlayer tab</label>
            <label><input type="checkbox" id="show-lms" checked onchange="saveUiSettings()"> LMS tab</label>
            <label><input type="checkbox" id="show-knobs" checked onchange="saveUiSettings()"> Knobs tab</label>
        </div>
        <p style="margin-top:0.5rem;"><small>Uncheck to hide tabs you don't use. Refresh page to apply.</small></p>
    </article>
</section>

<section id="discovery-status">
    <hgroup>
        <h2>Auto-Discovery</h2>
        <p>Devices found via SSDP (no configuration needed)</p>
    </hgroup>
    <article>
        <table>
            <thead><tr><th>Protocol</th><th>Status</th><th>Devices</th></tr></thead>
            <tbody id="discovery-table">
                <tr><td colspan="3">Loading...</td></tr>
            </tbody>
        </table>
    </article>
</section>

<script>
function esc(s) { return String(s).replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'})[c]); }

// Discovery status
async function loadDiscoveryStatus() {
    const tbody = document.getElementById('discovery-table');
    try {
        const [openhome, upnp, roon] = await Promise.all([
            fetch('/openhome/status').then(r => r.json()).catch(() => ({ connected: false, device_count: 0 })),
            fetch('/upnp/status').then(r => r.json()).catch(() => ({ connected: false, renderer_count: 0 })),
            fetch('/roon/status').then(r => r.json()).catch(() => ({ connected: false }))
        ]);

        tbody.innerHTML = `
            <tr>
                <td>Roon</td>
                <td class="${roon.connected ? 'status-ok' : 'status-err'}">${roon.connected ? '✓ Connected' : '✗ Not connected'}</td>
                <td>${roon.connected ? esc(roon.core_name || 'Core') : '-'}</td>
            </tr>
            <tr>
                <td>OpenHome</td>
                <td class="${openhome.device_count > 0 ? 'status-ok' : ''}">${openhome.device_count > 0 ? '✓ Active' : 'Searching...'}</td>
                <td>${openhome.device_count} device${openhome.device_count !== 1 ? 's' : ''}</td>
            </tr>
            <tr>
                <td>UPnP/DLNA</td>
                <td class="${upnp.renderer_count > 0 ? 'status-ok' : ''}">${upnp.renderer_count > 0 ? '✓ Active' : 'Searching...'}</td>
                <td>${upnp.renderer_count} renderer${upnp.renderer_count !== 1 ? 's' : ''}</td>
            </tr>
        `;
    } catch (e) {
        tbody.innerHTML = `<tr><td colspan="3" class="status-err">Error: ${esc(e.message)}</td></tr>`;
    }
}

// Adapter Settings
async function loadAdapterSettings() {
    try {
        const res = await fetch('/api/settings');
        const settings = await res.json();
        const adapters = settings.adapters || {};
        document.getElementById('adapter-roon').checked = adapters.roon !== false;
        document.getElementById('adapter-lms').checked = adapters.lms === true;
        document.getElementById('adapter-openhome').checked = adapters.openhome === true;
        document.getElementById('adapter-upnp').checked = adapters.upnp === true;
    } catch (e) {
        console.error('Failed to load adapter settings:', e);
    }
}

async function saveAdapterSettings() {
    try {
        const res = await fetch('/api/settings');
        const settings = await res.json();
        settings.adapters = {
            roon: document.getElementById('adapter-roon').checked,
            lms: document.getElementById('adapter-lms').checked,
            openhome: document.getElementById('adapter-openhome').checked,
            upnp: document.getElementById('adapter-upnp').checked
        };
        await fetch('/api/settings', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify(settings)
        });
    } catch (e) {
        console.error('Failed to save adapter settings:', e);
    }
}

// Wire up adapter toggle handlers
['roon', 'lms', 'openhome', 'upnp'].forEach(id => {
    document.getElementById('adapter-' + id).addEventListener('change', saveAdapterSettings);
});

// UI Settings (tab visibility)
function loadUiSettings() {
    const settings = JSON.parse(localStorage.getItem('hifi-ui-settings') || '{}');
    document.getElementById('show-hqplayer').checked = settings.showHqplayer !== false;
    document.getElementById('show-lms').checked = settings.showLms !== false;
    document.getElementById('show-knobs').checked = settings.showKnobs !== false;
}

function saveUiSettings() {
    const settings = {
        showHqplayer: document.getElementById('show-hqplayer').checked,
        showLms: document.getElementById('show-lms').checked,
        showKnobs: document.getElementById('show-knobs').checked,
    };
    localStorage.setItem('hifi-ui-settings', JSON.stringify(settings));
}

// Load all on page load
loadDiscoveryStatus();
loadAdapterSettings();
loadUiSettings();
setInterval(loadDiscoveryStatus, 10000);
</script>
"#;

    Html(html_doc("Settings", "settings", content))
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
