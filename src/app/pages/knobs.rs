//! Knobs page component.
//!
//! Knob device management:
//! - List registered knob devices
//! - Configure individual knobs (name, display rotation, power timers)
//! - Firmware management (version check, fetch from GitHub)
//! - Flash new knobs

use dioxus::prelude::*;

use crate::app::components::Layout;

/// Client-side JavaScript for the Knobs page.
const KNOBS_SCRIPT: &str = r#"

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

        container.innerHTML = '<form id="knob-config-form">' +
            '<label>Name<input type="text" name="name" value="' + escAttr(c.name || '') + '" placeholder="Living Room Knob"></label>' +
            '<fieldset><legend>Display Rotation</legend>' +
            '<div class="grid"><label>Charging: ' + rotSel('rotation_charging', c.rotation_charging ?? 180) + '</label>' +
            '<label>Battery: ' + rotSel('rotation_not_charging', c.rotation_not_charging ?? 0) + '</label></div></fieldset>' +
            '<footer><button type="button" class="secondary" onclick="closeModal()">Cancel</button><button type="submit">Save</button></footer></form>';

        document.getElementById('knob-config-form').addEventListener('submit', saveConfig);
    } catch (e) {
        container.innerHTML = '<p class="status-err">Error: ' + esc(e.message) + '</p>';
    }
}

async function saveConfig(e) {
    e.preventDefault();
    const f = e.target;
    const cfg = {
        name: f.querySelector('[name="name"]')?.value || null,
        rotation_charging: parseInt(f.querySelector('[name="rotation_charging"]')?.value) || 0,
        rotation_not_charging: parseInt(f.querySelector('[name="rotation_not_charging"]')?.value) || 0,
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

document.getElementById('config-modal').addEventListener('click', e => {
    if (e.target.id === 'config-modal') closeModal();
});

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

const es = new EventSource('/events');
es.onmessage = (e) => {
    try {
        const event = JSON.parse(e.data);
        if (['ZoneUpdated', 'ZoneRemoved', 'RoonConnected', 'RoonDisconnected', 'LmsConnected', 'LmsDisconnected'].includes(event.type)) {
            loadKnobs();
        }
    } catch (err) {}
};
es.onerror = () => { es.close(); setInterval(loadKnobs, 10000); };
"#;

/// Knobs page component.
#[component]
pub fn Knobs() -> Element {
    rsx! {
        Layout {
            title: "Knobs".to_string(),
            nav_active: "knobs".to_string(),
            scripts: Some(KNOBS_SCRIPT.to_string()),

            h1 { "Knob Devices" }

            p {
                a { href: "https://community.roonlabs.com/t/50-esp32-s3-knob-roon-controller/311363", target: "_blank", rel: "noopener", "Knob Community Thread" }
                " - build info, firmware updates, discussion"
            }

            section { id: "knobs-section",
                article { aria_busy: "true", "Loading knobs..." }
            }

            section { id: "firmware-section",
                hgroup { h2 { "Firmware" } p { "Manage knob firmware updates" } }
                article {
                    p { "Current: ", strong { id: "fw-version", "checking..." } }
                    button { id: "fetch-btn", "Fetch Latest from GitHub" }
                    a { href: "/knobs/flash", style: "margin-left:1rem;", "Flash a new knob" }
                    span { id: "fw-msg" }
                }
            }

            div {
                dangerous_inner_html: r#"
                    <dialog id="config-modal">
                        <article>
                            <header>
                                <button aria-label="Close" rel="prev" onclick="closeModal()"></button>
                                <h2>Knob Configuration</h2>
                            </header>
                            <div id="config-form-container">Loading...</div>
                        </article>
                    </dialog>
                "#
            }
        }
    }
}
