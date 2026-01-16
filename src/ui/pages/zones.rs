//! Zones listing page component.
//!
//! Shows all available zones with:
//! - Zone cards in a grid layout
//! - Now playing info for each zone
//! - Transport and volume controls per zone

use dioxus::prelude::*;

use crate::ui::components::Layout;

/// Client-side JavaScript for the Zones page.
const ZONES_SCRIPT: &str = r#"

async function loadZones() {
    const section = document.querySelector('#zones');
    try {
        const zonesRes = await fetch('/zones').then(r => r.json());
        // /zones returns {zones: [...]} with zone_id and zone_name
        const zones = zonesRes.zones || zonesRes || [];

        if (!zones.length) {
            section.innerHTML = '<article>No zones available. Check that adapters are connected.</article>';
            return;
        }

        section.innerHTML = '<div class="zone-grid">' + zones.map(zone => {
            const playIcon = zone.state === 'playing' ? '⏸︎' : '▶';
            const hqpBadge = zone.dsp?.type === 'hqplayer' ? `<mark style="font-size:0.7em;padding:0.1em 0.3em;margin-left:0.5em;">HQP</mark>` : '';
            const sourceBadge = zone.source ? `<mark style="font-size:0.7em;padding:0.1em 0.3em;margin-left:0.5em;background:var(--pico-muted-background);">${esc(zone.source)}</mark>` : '';

            return `
                <article>
                    <header>
                        <strong>${esc(zone.zone_name)}</strong>${hqpBadge}${sourceBadge}
                        <small> (${esc(zone.state)})</small>
                    </header>
                    <div id="zone-info-${esc(zone.zone_id)}" style="min-height:40px;overflow:hidden;"><small>Loading...</small></div>
                    <footer>
                        <div class="controls" data-zone-id="${esc(zone.zone_id)}" style="align-items:center;">
                            <button data-action="previous">◀◀</button>
                            <button data-action="play_pause">${playIcon}</button>
                            <button data-action="next">▶▶</button>
                            <span style="margin-left:auto;display:flex;align-items:center;gap:0.25rem;">
                                <button data-action="vol_down" style="padding:0.3rem 0.6rem;">−</button>
                                <span id="zone-vol-${esc(zone.zone_id)}" style="min-width:3.5rem;text-align:center;font-size:0.9em;">—</span>
                                <button data-action="vol_up" style="padding:0.3rem 0.6rem;">+</button>
                            </span>
                        </div>
                    </footer>
                </article>
            `;
        }).join('') + '</div>';

        // Fetch now playing info for each zone (includes volume)
        zones.forEach(async zone => {
            const infoEl = document.getElementById('zone-info-' + zone.zone_id);
            const volEl = document.getElementById('zone-vol-' + zone.zone_id);
            if (!infoEl) return;
            try {
                const np = await fetch('/now_playing?zone_id=' + encodeURIComponent(zone.zone_id)).then(r => r.json());
                if (np && np.line1 && np.line1 !== 'Idle') {
                    infoEl.innerHTML = '<strong style="font-size:0.9em;">' + esc(np.line1) + '</strong><br><small>' + esc(np.line2 || '') + '</small>';
                } else {
                    infoEl.innerHTML = '<small>Nothing playing</small>';
                }
                // Volume display
                if (volEl && np.volume != null) {
                    const suffix = np.volume_type === 'db' ? ' dB' : '';
                    volEl.textContent = Math.round(np.volume) + suffix;
                }
            } catch (e) {
                infoEl.innerHTML = '<small>—</small>';
            }
        });
    } catch (e) {
        section.innerHTML = `<article class="status-err">Error: ${esc(e.message)}</article>`;
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

// SSE for real-time updates (no polling jitter)
const es = new EventSource('/events');
es.onmessage = (e) => {
    try {
        const event = JSON.parse(e.data);
        // Reload zones on any zone-related event
        if (['ZoneUpdated', 'ZoneRemoved', 'NowPlayingChanged', 'VolumeChanged',
             'RoonConnected', 'RoonDisconnected', 'LmsConnected', 'LmsDisconnected'].includes(event.type)) {
            loadZones();
        }
    } catch (err) { console.error('SSE parse error:', err); }
};
es.onerror = () => {
    // Fallback to polling if SSE fails
    console.warn('SSE disconnected, falling back to polling');
    es.close();
    setInterval(loadZones, 4000);
};
"#;

/// Zones listing page component.
#[component]
pub fn ZonesPage() -> Element {
    rsx! {
        Layout {
            title: "Zones".to_string(),
            nav_active: "zones".to_string(),
            scripts: Some(ZONES_SCRIPT.to_string()),

            h1 { "Zones" }

            section { id: "zones",
                article {
                    aria_busy: "true",
                    "Loading zones..."
                }
            }
        }
    }
}
