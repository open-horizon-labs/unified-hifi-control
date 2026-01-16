//! Settings page component.
//!
//! Allows users to:
//! - Enable/disable adapter sources (Roon, LMS, OpenHome, UPnP)
//! - Show/hide navigation tabs (HQPlayer, LMS, Knobs)
//! - View auto-discovery status

use dioxus::prelude::*;

use crate::ui::components::Layout;

/// Client-side JavaScript for the Settings page.
const SETTINGS_SCRIPT: &str = r#"

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

// SSE for real-time updates (no polling jitter)
const es = new EventSource('/events');
es.onmessage = (e) => {
    try {
        const event = JSON.parse(e.data);
        // Reload discovery status on connection events
        if (['RoonConnected', 'RoonDisconnected', 'HqpConnected', 'HqpDisconnected',
             'LmsConnected', 'LmsDisconnected'].includes(event.type)) {
            loadDiscoveryStatus();
        }
    } catch (err) { console.error('SSE parse error:', err); }
};
es.onerror = () => {
    console.warn('SSE disconnected, falling back to polling');
    es.close();
    setInterval(loadDiscoveryStatus, 10000);
};
"#;

/// Settings page component.
#[component]
pub fn SettingsPage() -> Element {
    rsx! {
        Layout {
            title: "Settings".to_string(),
            nav_active: "settings".to_string(),
            scripts: Some(SETTINGS_SCRIPT.to_string()),

            h1 { "Settings" }

            // Adapter Settings section
            section { id: "adapter-settings",
                hgroup {
                    h2 { "Adapter Settings" }
                    p { "Enable or disable zone sources" }
                }
                article { id: "adapter-toggles",
                    div {
                        style: "display:flex;flex-wrap:wrap;gap:1.5rem;",
                        label {
                            input {
                                r#type: "checkbox",
                                id: "adapter-roon"
                            }
                            " Roon"
                        }
                        label {
                            input {
                                r#type: "checkbox",
                                id: "adapter-lms"
                            }
                            " LMS"
                        }
                        label {
                            input {
                                r#type: "checkbox",
                                id: "adapter-openhome"
                            }
                            " OpenHome"
                        }
                        label {
                            input {
                                r#type: "checkbox",
                                id: "adapter-upnp"
                            }
                            " UPnP/DLNA"
                        }
                    }
                    p {
                        style: "margin-top:0.5rem;",
                        small { "Changes take effect immediately. Disabled adapters won't contribute zones." }
                    }
                }
            }

            // UI Settings section
            section { id: "ui-settings",
                hgroup {
                    h2 { "UI Settings" }
                    p { "Customize navigation tabs" }
                }
                article {
                    // Using dangerous_inner_html for checkboxes with onchange handlers
                    // since Dioxus SSR doesn't support string event handlers directly
                    div {
                        style: "display:flex;flex-wrap:wrap;gap:1.5rem;",
                        dangerous_inner_html: r#"
                            <label><input type="checkbox" id="show-hqplayer" checked onchange="saveUiSettings()"> HQPlayer tab</label>
                            <label><input type="checkbox" id="show-lms" checked onchange="saveUiSettings()"> LMS tab</label>
                            <label><input type="checkbox" id="show-knobs" checked onchange="saveUiSettings()"> Knobs tab</label>
                        "#
                    }
                    p {
                        style: "margin-top:0.5rem;",
                        small { "Uncheck to hide tabs you don't use. Refresh page to apply." }
                    }
                }
            }

            // Discovery Status section
            section { id: "discovery-status",
                hgroup {
                    h2 { "Auto-Discovery" }
                    p { "Devices found via SSDP (no configuration needed)" }
                }
                article {
                    table {
                        thead {
                            tr {
                                th { "Protocol" }
                                th { "Status" }
                                th { "Devices" }
                            }
                        }
                        tbody { id: "discovery-table",
                            tr {
                                td { colspan: "3", "Loading..." }
                            }
                        }
                    }
                }
            }
        }
    }
}
