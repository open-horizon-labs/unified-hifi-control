//! Knobs page component.
//!
//! Knob device management:
//! - List registered knob devices
//! - Configure individual knobs (name, display rotation, power timers)
//! - Firmware management (version check, fetch from GitHub)
//! - Flash new knobs

use dioxus::prelude::*;

use crate::ui::components::Layout;

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

// SSE for real-time updates (knobs don't have specific events yet, but zones matter for zone display)
const es = new EventSource('/events');
es.onmessage = (e) => {
    try {
        const event = JSON.parse(e.data);
        // Reload knobs when zones change (affects zone names in table)
        if (['ZoneUpdated', 'ZoneRemoved', 'RoonConnected', 'RoonDisconnected',
             'LmsConnected', 'LmsDisconnected'].includes(event.type)) {
            loadKnobs();
        }
    } catch (err) { console.error('SSE parse error:', err); }
};
es.onerror = () => {
    console.warn('SSE disconnected, falling back to polling');
    es.close();
    setInterval(loadKnobs, 10000);
};
"#;

/// Knobs page component.
#[component]
pub fn KnobsPage() -> Element {
    rsx! {
        Layout {
            title: "Knobs".to_string(),
            nav_active: "knobs".to_string(),
            scripts: Some(KNOBS_SCRIPT.to_string()),

            h1 { "Knob Devices" }

            p {
                a {
                    href: "https://community.roonlabs.com/t/50-esp32-s3-knob-roon-controller/311363",
                    target: "_blank",
                    rel: "noopener",
                    "Knob Community Thread"
                }
                " - build info, firmware updates, discussion"
            }

            section { id: "knobs-section",
                article {
                    aria_busy: "true",
                    "Loading knobs..."
                }
            }

            section { id: "firmware-section",
                hgroup {
                    h2 { "Firmware" }
                    p { "Manage knob firmware updates" }
                }
                article {
                    p {
                        "Current: "
                        strong { id: "fw-version", "checking..." }
                    }
                    button { id: "fetch-btn", "Fetch Latest from GitHub" }
                    a { href: "/knobs/flash", style: "margin-left:1rem;", "Flash a new knob" }
                    span { id: "fw-msg" }
                }
            }

            // Config modal - using div with dangerous_inner_html for onclick handlers
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
