//! Zone page component.
//!
//! Single zone control view with:
//! - Zone selection dropdown
//! - Album art and now playing info
//! - Transport controls (prev/play/next)
//! - Volume controls
//! - HQPlayer DSP settings (when zone is linked)

use dioxus::prelude::*;

use crate::app::components::Layout;

/// Client-side JavaScript for the Zone page.
const ZONE_SCRIPT: &str = r#"

let selectedZone = localStorage.getItem('hifi-zone') || null;
let zonesData = [];
let lastHqpZone = null;
let hqpPipelineLoaded = false;
let nowPlayingData = null;

async function loadZones() {
    try {
        const zonesRes = await fetch('/zones').then(r => r.json());
        zonesData = zonesRes.zones || zonesRes || [];

        const sel = document.getElementById('zone-select');
        sel.innerHTML = '<option value="">-- Select Zone --</option>' +
            zonesData.map(z => {
                const hqpBadge = z.dsp?.type === 'hqplayer' ? ' [HQP]' : '';
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

    if (np && np.volume != null) {
        const suffix = np.volume_type === 'db' ? ' dB' : '';
        document.getElementById('zone-volume').textContent = Math.round(np.volume) + suffix;
    } else {
        document.getElementById('zone-volume').textContent = '—';
    }

    const isPlaying = np?.is_playing || false;
    document.getElementById('btn-prev').disabled = !np?.is_previous_allowed;
    document.getElementById('btn-next').disabled = !np?.is_next_allowed;
    document.getElementById('btn-play').textContent = isPlaying ? '⏸︎' : '▶';

    const hasHqp = zone.dsp?.type === 'hqplayer';
    const hqpSection = document.getElementById('hqp-section');
    if (hasHqp) {
        hqpSection.style.display = 'block';
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

async function loadHqpPipeline() {
    const loading = document.getElementById('hqp-loading');
    const settings = document.getElementById('hqp-settings');
    loading.style.display = 'block';
    settings.style.display = 'none';

    try {
        const [pipeline, matrixRes] = await Promise.all([
            fetch('/hqp/pipeline').then(r => r.json()),
            fetch('/hqplayer/matrix/profiles').then(r => r.json()).catch(() => ({ profiles: [] }))
        ]);

        const s = pipeline.settings || {};
        populateSelect('hqp-mode', s.mode?.options, s.mode?.selected?.value);
        populateSelect('hqp-samplerate', s.samplerate?.options, s.samplerate?.selected?.value);
        populateSelect('hqp-filter1x', s.filter1x?.options, s.filter1x?.selected?.value);
        populateSelect('hqp-filterNx', s.filterNx?.options, s.filterNx?.selected?.value);
        populateSelect('hqp-shaper', s.shaper?.options, s.shaper?.selected?.value);

        const modeLabel = (s.mode?.selected?.label || '').toLowerCase();
        document.getElementById('hqp-shaper-label').textContent =
            (modeLabel.includes('sdm') || modeLabel.includes('dsd')) ? 'Modulator' : 'Dither';

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

    const selects = ['hqp-mode', 'hqp-samplerate', 'hqp-filter1x', 'hqp-filterNx', 'hqp-shaper', 'hqp-matrix'];
    selects.forEach(id => { const el = document.getElementById(id); if (el) el.disabled = true; });

    try {
        const res = await fetch('/hqp/pipeline', {
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

document.getElementById('btn-prev').addEventListener('click', () => control('previous'));
document.getElementById('btn-play').addEventListener('click', () => control('play_pause'));
document.getElementById('btn-next').addEventListener('click', () => control('next'));
document.getElementById('btn-vol-down').addEventListener('click', () => volume(-2));
document.getElementById('btn-vol-up').addEventListener('click', () => volume(2));

loadZones();

// SSE for real-time updates
const es = new EventSource('/events');
es.onmessage = (e) => {
    try {
        const event = JSON.parse(e.data);
        if (['ZoneUpdated', 'ZoneRemoved', 'NowPlayingChanged', 'VolumeChanged',
             'RoonConnected', 'RoonDisconnected'].includes(event.type)) {
            loadZones();
        }
        if (['HqpConnected', 'HqpDisconnected', 'HqpStateChanged', 'HqpPipelineChanged'].includes(event.type)) {
            if (hqpPipelineLoaded) loadHqpPipeline();
        }
    } catch (err) {}
};
es.onerror = () => {
    es.close();
    setInterval(loadZones, 3000);
};
"#;

/// Zone page component.
#[component]
pub fn Zone() -> Element {
    rsx! {
        Layout {
            title: "Zone".to_string(),
            nav_active: "zone".to_string(),
            scripts: Some(ZONE_SCRIPT.to_string()),

            h1 { "Zone Control" }
            p { small { "Select a zone for focused listening and DSP control." } }

            label { r#for: "zone-select",
                "Zone"
                select { id: "zone-select",
                    option { value: "", "Loading zones..." }
                }
            }

            article { id: "zone-display", style: "display:none;",
                div { style: "display:flex;gap:1.5rem;align-items:flex-start;flex-wrap:wrap;",
                    img { id: "zone-art", src: "", alt: "Album art",
                        style: "width:200px;height:200px;object-fit:cover;border-radius:8px;background:#222;" }
                    div { style: "flex:1;min-width:200px;",
                        h2 { id: "zone-name", style: "margin-bottom:0.25rem;" }
                        p { id: "zone-state", style: "margin:0;", small { "—" } }
                        hr {}
                        p { id: "zone-track", style: "margin:0.5rem 0;", strong { "—" } }
                        p { id: "zone-artist", style: "margin:0;", small { "—" } }
                        p { id: "zone-album", style: "margin:0;color:var(--pico-muted-color);", small { "—" } }
                        hr {}
                        div { style: "display:flex;gap:0.5rem;align-items:center;margin:1rem 0;",
                            button { id: "btn-prev", "◀◀" }
                            button { id: "btn-play", "▶" }
                            button { id: "btn-next", "▶▶" }
                            span { style: "margin-left:1rem;", "Volume: ", strong { id: "zone-volume", "—" } }
                            button { id: "btn-vol-down", style: "width:2.5rem;", "−" }
                            button { id: "btn-vol-up", style: "width:2.5rem;", "+" }
                        }
                    }
                }
            }

            section { id: "hqp-section", style: "display:none;",
                hgroup { h2 { "HQPlayer DSP" } p { "Pipeline controls for zone-linked HQPlayer" } }
                article { id: "hqp-controls",
                    div { id: "hqp-loading", aria_busy: "true", "Loading DSP settings..." }
                    div { id: "hqp-settings", style: "display:none;",
                        dangerous_inner_html: r#"
                            <div class="grid" style="margin-bottom:0.5rem;">
                                <label>Matrix Profile<select id="hqp-matrix" onchange="setMatrixProfile(this.value)"></select></label>
                            </div>
                            <div class="grid">
                                <label>Mode<select id="hqp-mode" onchange="setPipeline('mode', this.value)"></select></label>
                                <label>Sample Rate<select id="hqp-samplerate" onchange="setPipeline('samplerate', this.value)"></select></label>
                            </div>
                            <div class="grid">
                                <label>Filter (1x)<select id="hqp-filter1x" onchange="setPipeline('filter1x', this.value)"></select></label>
                                <label>Filter (Nx)<select id="hqp-filterNx" onchange="setPipeline('filterNx', this.value)"></select></label>
                            </div>
                            <div class="grid">
                                <label><span id="hqp-shaper-label">Shaper</span><select id="hqp-shaper" onchange="setPipeline('shaper', this.value)"></select></label>
                            </div>
                            <p id="hqp-msg" style="margin-top:0.5rem;"></p>
                        "#
                    }
                }
            }
        }
    }
}
