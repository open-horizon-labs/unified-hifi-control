//! HQPlayer page component.
//!
//! HQPlayer status and DSP controls:
//! - Configuration (host/port/credentials)
//! - Connection status
//! - Pipeline settings (mode, filter, shaper, sample rate)
//! - Profiles (saved configurations)
//! - Zone linking (connect audio zones to HQPlayer)

use dioxus::prelude::*;

use crate::app::components::Layout;

/// Client-side JavaScript for the HQPlayer page.
const HQPLAYER_SCRIPT: &str = r#"

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
            fetch('/hqp/status').then(r => r.json()),
            fetch('/hqp/pipeline').then(r => r.json()).catch(() => null)
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
        const data = await fetch('/hqp/pipeline').then(r => r.json());
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
        const profiles = await fetch('/hqp/profiles').then(r => r.json());
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
        await fetch('/hqp/profiles/load', {
            method: 'POST',
            headers: { 'Content-Type': 'application/json' },
            body: JSON.stringify({ profile: name })
        });
        setTimeout(loadHqpProfiles, 500);
        setTimeout(loadHqpPipeline, 500);
    } catch (e) { console.error(e); }
}

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

        zoneLinkMap = {};
        links.forEach(l => { zoneLinkMap[l.zone_id] = l.instance; });

        if (!zones.length) {
            section.innerHTML = '<p>No audio zones available. Check that adapters are connected.</p>';
            return;
        }

        const instanceOptions = hqpInstances.length > 0
            ? hqpInstances.map(i => `<option value="${esc(i.name)}">${esc(i.name)} (${esc(i.host || 'unconfigured')})</option>`).join('')
            : '<option value="default">default</option>';

        const getBackend = (zid) => {
            if (zid.startsWith('lms:')) return 'LMS';
            if (zid.startsWith('openhome:')) return 'OpenHome';
            if (zid.startsWith('upnp:')) return 'UPnP';
            return 'Roon';
        };

        section.innerHTML = `
            <table id="zone-links-table">
                <thead><tr><th>Zone</th><th>Source</th><th>HQPlayer Instance</th><th>Action</th></tr></thead>
                <tbody>
                    ${zones.map(z => {
                        const linked = zoneLinkMap[z.zone_id];
                        const backend = getBackend(z.zone_id);
                        return `
                        <tr data-zone-id="${esc(z.zone_id)}">
                            <td>${esc(z.zone_name)}</td>
                            <td><small>${backend}</small></td>
                            <td>${linked ? `<strong>${esc(linked)}</strong>` : `<select class="hqp-instance-select">${instanceOptions}</select>`}</td>
                            <td>${linked ? `<button class="unlink-btn outline secondary">Unlink</button>` : `<button class="link-btn">Link</button>`}</td>
                        </tr>`;
                    }).join('')}
                </tbody>
            </table>
        `;

        section.querySelectorAll('.link-btn').forEach(btn => {
            btn.addEventListener('click', async () => {
                const row = btn.closest('tr');
                const zoneId = row.dataset.zoneId;
                const select = row.querySelector('.hqp-instance-select');
                const instanceName = select ? select.value : 'default';
                btn.disabled = true;
                try {
                    await fetch('/hqp/zones/link', {
                        method: 'POST',
                        headers: { 'Content-Type': 'application/json' },
                        body: JSON.stringify({ zone_id: zoneId, instance: instanceName })
                    });
                    loadZoneLinks();
                } catch (e) { btn.disabled = false; }
            });
        });

        section.querySelectorAll('.unlink-btn').forEach(btn => {
            btn.addEventListener('click', async () => {
                const row = btn.closest('tr');
                const zoneId = row.dataset.zoneId;
                btn.disabled = true;
                try {
                    await fetch('/hqp/zones/unlink', {
                        method: 'POST',
                        headers: { 'Content-Type': 'application/json' },
                        body: JSON.stringify({ zone_id: zoneId })
                    });
                    loadZoneLinks();
                } catch (e) { btn.disabled = false; }
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

const es = new EventSource('/events');
es.onmessage = (e) => {
    try {
        const event = JSON.parse(e.data);
        if (['HqpConnected', 'HqpDisconnected', 'HqpStateChanged'].includes(event.type)) loadHqpStatus();
        if (['HqpConnected', 'HqpPipelineChanged'].includes(event.type)) loadHqpPipeline();
        if (['ZoneUpdated', 'ZoneRemoved', 'RoonConnected', 'RoonDisconnected', 'LmsConnected', 'LmsDisconnected'].includes(event.type)) loadZoneLinks();
    } catch (err) {}
};
es.onerror = () => { es.close(); setInterval(loadHqpStatus, 5000); };
"#;

/// HQPlayer page component.
#[component]
pub fn HqPlayer() -> Element {
    rsx! {
        Layout {
            title: "HQPlayer".to_string(),
            nav_active: "hqplayer".to_string(),
            scripts: Some(HQPLAYER_SCRIPT.to_string()),

            h1 { "HQPlayer" }

            section { id: "hqp-config",
                hgroup { h2 { "Configuration" } p { "HQPlayer connection settings" } }
                article { aria_busy: "true", "Loading..." }
            }

            section { id: "hqp-status",
                hgroup { h2 { "Connection Status" } p { "HQPlayer DSP engine connection" } }
                article { aria_busy: "true", "Loading..." }
            }

            section { id: "hqp-pipeline",
                hgroup { h2 { "Pipeline Settings" } p { "Current DSP configuration" } }
                article { aria_busy: "true", "Loading..." }
            }

            section { id: "hqp-profiles",
                hgroup { h2 { "Profiles" } p { "Saved configurations (requires web credentials)" } }
                article { aria_busy: "true", "Loading..." }
            }

            section { id: "hqp-zone-links",
                hgroup { h2 { "Zone Linking" } p { "Link audio zones to HQPlayer for DSP processing" } }
                article { aria_busy: "true", "Loading..." }
            }
        }
    }
}
