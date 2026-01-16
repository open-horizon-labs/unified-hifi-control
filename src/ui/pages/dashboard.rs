//! Dashboard page component.
//!
//! Shows service status overview:
//! - Version and uptime
//! - Adapter connection statuses (Roon, HQPlayer, LMS, MQTT)

use dioxus::prelude::*;

use crate::ui::components::Layout;

/// Client-side JavaScript for the Dashboard page.
const DASHBOARD_SCRIPT: &str = r#"

async function loadStatus() {
    const section = document.querySelector('#status article');
    try {
        const [status, roon, hqp, lms] = await Promise.all([
            fetch('/status').then(r => r.json()),
            fetch('/roon/status').then(r => r.json()).catch(() => ({ connected: false })),
            fetch('/hqp/status').then(r => r.json()).catch(() => ({ connected: false })),
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

// SSE for real-time updates (no polling jitter)
const es = new EventSource('/events');
es.onmessage = (e) => {
    try {
        const event = JSON.parse(e.data);
        // Reload status on any connection event
        if (['RoonConnected', 'RoonDisconnected', 'HqpConnected', 'HqpDisconnected',
             'LmsConnected', 'LmsDisconnected'].includes(event.type)) {
            loadStatus();
        }
    } catch (err) { console.error('SSE parse error:', err); }
};
es.onerror = () => {
    console.warn('SSE disconnected, falling back to polling');
    es.close();
    setInterval(loadStatus, 10000);
};
"#;

/// Dashboard page component.
#[component]
pub fn DashboardPage() -> Element {
    rsx! {
        Layout {
            title: "Dashboard".to_string(),
            nav_active: "dashboard".to_string(),
            scripts: Some(DASHBOARD_SCRIPT.to_string()),

            h1 { "Dashboard" }

            section { id: "status",
                hgroup {
                    h2 { "Service Status" }
                    p { "Connection status for all adapters" }
                }
                article {
                    aria_busy: "true",
                    "Loading status..."
                }
            }
        }
    }
}
