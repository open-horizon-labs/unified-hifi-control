//! LMS (Logitech Media Server) page component.
//!
//! LMS status and player management:
//! - Server configuration (host/port/credentials)
//! - Connection status
//! - Connected players with transport controls

use dioxus::prelude::*;

use crate::app::components::Layout;

/// Client-side JavaScript for the LMS page.
const LMS_SCRIPT: &str = r#"

async function loadLmsPlayers() {
    const section = document.querySelector('#lms-players article');
    try {
        const settings = await fetch('/api/settings').then(r => r.json()).catch(() => ({}));
        const lmsEnabled = settings?.adapters?.lms === true;

        if (!lmsEnabled) {
            section.removeAttribute('aria-busy');
            section.innerHTML = '<p>LMS adapter is disabled. <a href="/settings">Enable it in Settings</a> to discover players.</p>';
            return;
        }

        const players = await fetch('/lms/players').then(r => r.json());
        section.removeAttribute('aria-busy');

        if (!players || !players.length) {
            section.innerHTML = '<p>No players found. Make sure your Squeezebox server is configured and reachable.</p>';
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

document.querySelector('#lms-players').addEventListener('click', e => {
    const btn = e.target.closest('button[data-action]');
    if (!btn) return;
    const container = btn.closest('[data-player-id]');
    if (!container) return;
    lmsControl(container.dataset.playerId, btn.dataset.action);
});

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

const es = new EventSource('/events');
es.onmessage = (e) => {
    try {
        const event = JSON.parse(e.data);
        if (['LmsConnected', 'LmsDisconnected'].includes(event.type)) {
            loadLmsConfig();
            loadLmsPlayers();
        }
        if (event.type === 'LmsPlayerStateChanged') loadLmsPlayers();
    } catch (err) {}
};
es.onerror = () => { es.close(); setInterval(loadLmsPlayers, 4000); };
"#;

/// LMS page component.
#[component]
pub fn Lms() -> Element {
    rsx! {
        Layout {
            title: "LMS".to_string(),
            nav_active: "lms".to_string(),
            scripts: Some(LMS_SCRIPT.to_string()),

            h1 { "Logitech Media Server" }

            section { id: "lms-config",
                hgroup { h2 { "Server Configuration" } p { "Configure connection to your Squeezebox server" } }
                article { id: "lms-config-card",
                    dangerous_inner_html: r#"
                        <div id="lms-status-line">Checking...</div>
                        <div id="lms-config-form" style="display:none;">
                            <div class="grid">
                                <label>Host<input type="text" id="lms-host" placeholder="192.168.1.x or hostname"></label>
                                <label>Port<input type="number" id="lms-port" value="9000" min="1" max="65535"></label>
                            </div>
                            <div class="grid">
                                <label>Username (optional)<input type="text" id="lms-username" placeholder="Leave blank if not required"></label>
                                <label>Password (optional)<input type="password" id="lms-password" placeholder="Leave blank if not required"></label>
                            </div>
                            <button onclick="saveLmsConfig()">Save & Connect</button>
                            <span id="lms-save-msg"></span>
                        </div>
                        <button id="lms-reconfig-btn" style="display:none;" onclick="showLmsForm()">Reconfigure</button>
                    "#
                }
            }

            section { id: "lms-players",
                hgroup { h2 { "Players" } p { "Connected Squeezebox players" } }
                article { aria_busy: "true", "Loading..." }
            }
        }
    }
}
