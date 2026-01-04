const express = require('express');
const sharp = require('sharp');
const fs = require('fs');
const path = require('path');
const busDebug = require('../bus/debug');

function extractKnob(req) {
  const headerId = req.get('x-knob-id') || req.get('x-device-id');
  const queryId = req.query?.knob_id;
  const bodyId = req.body?.knob_id;
  const id = headerId || queryId || bodyId;
  const version = req.get('x-knob-version') || req.get('x-device-version');
  if (!id && !version) return null;
  return { id, version };
}

function createKnobRoutes({ bus, roon, knobs, adapterFactory, logger }) {
  const router = express.Router();
  const log = logger || console;

  // GET /zones - List all zones from bus (multi-backend)
  router.get('/zones', (req, res) => {
    const knob = extractKnob(req);
    log.debug('Zones requested', { ip: req.ip, knob_id: knob?.id });

    // TODO(Phase 3): Remove roon fallback after HQP migration to bus
    const zones = bus.getZones();
    res.json({ zones });
  });

  // GET /now_playing - Get current playback state for a zone
  router.get('/now_playing', (req, res) => {
    const zoneId = req.query.zone_id;
    const knob = extractKnob(req);

    if (!zoneId) {
      const zones = bus ? bus.getZones() : roon.getZones();
      return res.status(400).json({ error: 'zone_id required', zones });
    }

    // Update knob status from query params
    if (knob?.id) {
      // Update version if provided (ensures version updates after OTA)
      if (knob.version) {
        knobs.getOrCreateKnob(knob.id, knob.version);
      }

      const statusUpdates = { zone_id: zoneId, ip: req.ip };

      if (req.query.battery_level !== undefined) {
        const level = parseInt(req.query.battery_level, 10);
        if (!isNaN(level) && level >= 0 && level <= 100) {
          statusUpdates.battery_level = level;
        }
      }
      if (req.query.battery_charging !== undefined) {
        statusUpdates.battery_charging = req.query.battery_charging === '1' || req.query.battery_charging === 'true';
      }

      knobs.updateKnobStatus(knob.id, statusUpdates);
    }

    const sender = { ip: req.ip, knob_id: knob?.id, user_agent: req.get('user-agent') };
    const data = bus ? bus.getNowPlaying(zoneId, { sender }) : roon.getNowPlaying(zoneId);
    if (!data) {
      const zones = bus ? bus.getZones() : roon.getZones();
      log.warn('now_playing miss', { zoneId, ip: req.ip });
      return res.status(404).json({ error: 'zone not found', zones });
    }

    log.debug('now_playing served', { zoneId, ip: req.ip });

    const image_url = `/now_playing/image?zone_id=${encodeURIComponent(zoneId)}`;
    const config_sha = knob?.id ? knobs.getConfigSha(knob.id) : null;
    const zones = bus ? bus.getZones() : roon.getZones();

    res.json({ ...data, image_url, zones, config_sha });
  });

  // GET /now_playing/image - Get album artwork
  router.get('/now_playing/image', async (req, res) => {
    const zoneId = req.query.zone_id;

    if (!zoneId) {
      return res.status(400).json({ error: 'zone_id required' });
    }

    const sender = { ip: req.ip, user_agent: req.get('user-agent') };
    const data = bus ? bus.getNowPlaying(zoneId, { sender }) : roon.getNowPlaying(zoneId);
    if (!data) {
      return res.status(404).json({ error: 'zone not found' });
    }

    log.debug('now_playing image requested', { zoneId, ip: req.ip });
    const { width, height, format } = req.query || {};

    try {
      if (data.image_key) {
        // RGB565 format for ESP32 display
        if (format === 'rgb565') {
          const imageOpts = {
            width: width || 360,
            height: height || 360,
            format: 'image/jpeg',
            zone_id: zoneId,  // Add for bus routing
          };
          const { body } = bus ? await bus.getImage(data.image_key, imageOpts) : await roon.getImage(data.image_key, imageOpts);

          const targetWidth = parseInt(width) || 360;
          const targetHeight = parseInt(height) || 360;

          const rgb565Buffer = await sharp(body)
            .resize(targetWidth, targetHeight, { fit: 'cover' })
            .raw()
            .toBuffer({ resolveWithObject: true });

          const rgb888 = rgb565Buffer.data;
          const rgb565 = Buffer.alloc(targetWidth * targetHeight * 2);

          for (let i = 0; i < rgb888.length; i += 3) {
            const r = rgb888[i] >> 3;
            const g = rgb888[i + 1] >> 2;
            const b = rgb888[i + 2] >> 3;

            const rgb565Pixel = (r << 11) | (g << 5) | b;
            const pixelIndex = (i / 3) * 2;

            rgb565[pixelIndex] = rgb565Pixel & 0xFF;
            rgb565[pixelIndex + 1] = (rgb565Pixel >> 8) & 0xFF;
          }

          log.info('Converted image to RGB565', {
            width: targetWidth,
            height: targetHeight,
            size: rgb565.length,
          });

          res.set('Content-Type', 'application/octet-stream');
          res.set('X-Image-Width', targetWidth.toString());
          res.set('X-Image-Height', targetHeight.toString());
          res.set('X-Image-Format', 'rgb565');
          return res.send(rgb565);
        } else {
          // Return JPEG (optionally resized)
          const imageOpts = {
            width: width || 360,
            height: height || 360,
            format: 'image/jpeg',
            zone_id: zoneId,  // Add for bus routing
          };
          const { contentType, body } = bus ? await bus.getImage(data.image_key, imageOpts) : await roon.getImage(data.image_key, imageOpts);

          if ((width || height) && contentType && contentType.startsWith('image/')) {
            const targetWidth = parseInt(width) || parseInt(height) || 360;
            const targetHeight = parseInt(height) || parseInt(width) || 360;

            const resizedBody = await sharp(body)
              .resize(targetWidth, targetHeight, { fit: 'cover' })
              .jpeg({ quality: 80, progressive: false, mozjpeg: false })
              .toBuffer();

            log.info('Resized JPEG image', {
              originalSize: body.length,
              resizedSize: resizedBody.length,
              width: targetWidth,
              height: targetHeight,
            });

            res.set('Content-Type', 'image/jpeg');
            return res.send(resizedBody);
          }

          res.set('Content-Type', contentType || 'application/octet-stream');
          return res.send(body);
        }
      }
    } catch (error) {
      log.warn('now_playing image failed; returning placeholder', { zoneId, error: error.message });
    }

    // Placeholder SVG
    const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="360" height="360">
      <rect width="100%" height="100%" fill="#333"/>
      <text x="50%" y="50%" fill="#888" text-anchor="middle" dy=".3em" font-family="sans-serif" font-size="24">No Image</text>
    </svg>`;
    res.set('Content-Type', 'image/svg+xml');
    res.send(svg);
  });

  // POST /control - Send control commands
  router.post('/control', async (req, res) => {
    const { zone_id, action, value } = req.body || {};

    if (!zone_id || !action) {
      log.warn('control missing params', { zone_id, action, ip: req.ip });
      return res.status(400).json({ error: 'zone_id and action required' });
    }

    try {
      log.info('control', { zone_id, action, value, ip: req.ip });
      const sender = { ip: req.ip, user_agent: req.get('user-agent') };
      await (bus ? bus.control(zone_id, action, value, { sender }) : roon.control(zone_id, action, value));
      res.json({ status: 'ok' });
    } catch (error) {
      log.error('control failed', { zone_id, action, value, ip: req.ip, error: error.message });
      res.status(500).json({ error: error.message || 'control failed' });
    }
  });

  // GET /config/:knob_id - Get knob configuration
  router.get('/config/:knob_id', (req, res) => {
    const knobId = req.params.knob_id;
    const version = req.get('x-knob-version');
    log.debug('Config requested', { knobId, version, ip: req.ip });

    const knob = knobs.getOrCreateKnob(knobId, version);
    if (!knob) {
      return res.status(400).json({ error: 'knob_id required' });
    }

    res.json({
      config: {
        knob_id: knobId,
        ...knob.config,
        name: knob.name,
      },
      config_sha: knob.config_sha,
    });
  });

  // PUT /config/:knob_id - Update knob configuration
  router.put('/config/:knob_id', (req, res) => {
    const knobId = req.params.knob_id;
    const updates = req.body || {};
    log.info('Config update', { knobId, updates, ip: req.ip });

    const knob = knobs.updateKnobConfig(knobId, updates);
    if (!knob) {
      return res.status(400).json({ error: 'knob_id required' });
    }

    res.json({
      config: {
        knob_id: knobId,
        ...knob.config,
        name: knob.name,
      },
      config_sha: knob.config_sha,
    });
  });

  // GET /api/knobs - List all known knobs (JSON API)
  router.get('/api/knobs', (req, res) => {
    log.debug('Knobs list requested', { ip: req.ip });
    res.json({ knobs: knobs.listKnobs() });
  });

  // GET /admin/status.json - Admin diagnostics
  router.get('/admin/status.json', (req, res) => {
    const busStatus = bus.getStatus();
    const zones = bus.getZones();
    const nowPlaying = zones.map(z => bus.getNowPlaying(z.zone_id)).filter(np => np);

    res.json({
      zones,
      now_playing: nowPlaying,
      backends: busStatus,
      bus: {
        backends: Object.keys(busStatus),
        zone_count: zones.length,
      },
      knobs: knobs.listKnobs(),
      debug: busDebug.getDebugInfo(),
    });
  });

  // GET /admin/bus - Bus debug panel
  router.get('/admin/bus', (req, res) => {
    if (!bus) return res.status(404).send('Bus not available');
    const debug = busDebug.getDebugInfo();
    res.send(`<!DOCTYPE html><html><head><title>Bus Debug</title><meta http-equiv="refresh" content="5"><style>body{font-family:monospace;margin:20px}table{border-collapse:collapse;width:100%}th,td{border:1px solid #ddd;padding:8px;text-align:left}.error{color:red}.sender{color:#666;font-size:10px}</style></head><body><h1>Bus (${debug.message_count} msgs, 5m)</h1><table><tr><th>Time</th><th>Type</th><th>Zone</th><th>Details</th><th>Sender</th></tr>${debug.messages.slice(-50).reverse().map(m=>{const t=new Date(m.timestamp).toLocaleTimeString();const c=m.error?'class="error"':'';const d=m.action?m.action+(m.value!==undefined?' ('+m.value+')':''):m.has_data!==undefined?'data:'+m.has_data:m.error||'';const s=m.sender?(m.sender.knob_id?'knob:'+m.sender.knob_id:m.sender.ip||''):'';return`<tr ${c}><td>${t}</td><td>${m.type}</td><td>${m.zone_id||m.backend||'-'}</td><td>${d}</td><td class="sender">${s}</td></tr>`;}).join('')}</table></body></html>`);
  });

  // App settings (UI preferences) - use shared module
  const { loadAppSettings, saveAppSettings } = require('../lib/settings');

  router.get('/api/settings', (req, res) => {
    res.json(loadAppSettings());
  });

  router.post('/api/settings', express.json(), async (req, res) => {
    const current = loadAppSettings();
    const updated = { ...current, ...req.body };

    // Handle dynamic adapter enable/disable
    if (req.body.adapters && adapterFactory) {
      const currentAdapters = current.adapters || {};
      const newAdapters = req.body.adapters;

      // Check each adapter for changes
      const adapterMap = {
        roon: adapterFactory.createRoon,
        upnp: adapterFactory.createUPnP,
        openhome: adapterFactory.createOpenHome,
        lms: adapterFactory.createLMS,
      };

      for (const [name, createFn] of Object.entries(adapterMap)) {
        const wasEnabled = name === 'roon' ? currentAdapters[name] !== false : !!currentAdapters[name];
        const nowEnabled = name === 'roon' ? newAdapters[name] !== false : !!newAdapters[name];

        if (wasEnabled && !nowEnabled) {
          // Disable: unregister the backend
          log.info(`Disabling ${name} adapter`);
          await bus.unregisterBackend(name);
        } else if (!wasEnabled && nowEnabled) {
          // Enable: create and register the backend
          log.info(`Enabling ${name} adapter`);
          await bus.enableBackend(name, createFn());
        }
      }
    }

    saveAppSettings(updated);
    res.json(updated);
  });

  // Root redirect to Control page (normal listening)
  router.get('/', (req, res) => res.redirect('/control'));

  // ========== JTBD-Organized Admin Pages ==========
  // Jobs: Control (normal listening), Zone (single zone + DSP), Knobs (setup), Settings (admin)

  const baseStyles = `
    /* Theme variables */
    :root, [data-theme="light"] {
      --bg: #ffffff;
      --bg-surface: #f5f5f5;
      --bg-elevated: #ffffff;
      --text: #333333;
      --text-muted: #666666;
      --border: #dddddd;
      --border-light: #eeeeee;
      --accent: #4CAF50;
      --accent-hover: #45a049;
      --success: #28a745;
      --error: #dc3545;
      --ctrl-bg: #f5f5f5;
      --ctrl-hover: #e5e5e5;
      --code-bg: #f5f5f5;
      --card-selected: #f8fff8;
      --art-bg: #f0f0f0;
    }
    [data-theme="dark"] {
      --bg: #1a1a1a;
      --bg-surface: #2d2d2d;
      --bg-elevated: #3d3d3d;
      --text: #e0e0e0;
      --text-muted: #999999;
      --border: #444444;
      --border-light: #3d3d3d;
      --accent: #4CAF50;
      --accent-hover: #5cbf60;
      --success: #5cb85c;
      --error: #d9534f;
      --ctrl-bg: #3d3d3d;
      --ctrl-hover: #4d4d4d;
      --code-bg: #2d2d2d;
      --card-selected: #1a2f1a;
      --art-bg: #2d2d2d;
    }
    [data-theme="black"] {
      --bg: #000000;
      --bg-surface: #111111;
      --bg-elevated: #1a1a1a;
      --text: #e0e0e0;
      --text-muted: #888888;
      --border: #333333;
      --border-light: #222222;
      --accent: #4CAF50;
      --accent-hover: #5cbf60;
      --success: #5cb85c;
      --error: #d9534f;
      --ctrl-bg: #1a1a1a;
      --ctrl-hover: #2a2a2a;
      --code-bg: #111111;
      --card-selected: #0a1f0a;
      --art-bg: #111111;
    }

    body { font-family: system-ui, sans-serif; max-width: 900px; margin: 0 auto; padding: 0 1em 2em; background: var(--bg); color: var(--text); }
    nav { background: var(--bg-surface); margin: 0 -1em; padding: 0.8em 1em; border-bottom: 1px solid var(--border); display: flex; align-items: center; gap: 1em; flex-wrap: wrap; }
    nav h1 { margin: 0; font-size: 1.1em; }
    nav a { text-decoration: none; color: var(--text-muted); padding: 0.4em 0.8em; border-radius: 4px; }
    nav a:hover { background: var(--ctrl-hover); }
    nav a.active { background: var(--accent); color: white; }
    .nav-right { margin-left: auto; display: flex; align-items: center; gap: 1em; }
    .version { color: var(--text-muted); font-size: 0.85em; }
    .theme-toggle { background: var(--ctrl-bg); border: 1px solid var(--border); border-radius: 4px; padding: 0.3em 0.6em; cursor: pointer; font-size: 0.9em; color: var(--text); }
    .theme-toggle:hover { background: var(--ctrl-hover); }
    h2 { margin-top: 1.5em; }
    button { padding: 0.5em 1em; cursor: pointer; margin-right: 0.5em; background: var(--ctrl-bg); border: 1px solid var(--border); color: var(--text); border-radius: 4px; }
    button:hover { background: var(--ctrl-hover); }
    .section { margin: 1.5em 0; padding: 1em; border: 1px solid var(--border); border-radius: 4px; background: var(--bg-surface); }
    .status-msg { margin-top: 0.5em; }
    .success { color: var(--success); }
    .error { color: var(--error); }
    .muted { color: var(--text-muted); }
    select { padding: 0.4em; min-width: 150px; background: var(--bg-elevated); border: 1px solid var(--border); color: var(--text); border-radius: 4px; }
    label { display: inline-block; min-width: 100px; margin-right: 0.5em; }
    .form-row { margin: 0.8em 0; }
    input[type="text"], input[type="number"], input[type="password"] { padding: 0.4em; background: var(--bg-elevated); border: 1px solid var(--border); color: var(--text); border-radius: 4px; }
    .hidden { display: none; }
    table { width: 100%; border-collapse: collapse; }
    th, td { text-align: left; padding: 0.5em; border-bottom: 1px solid var(--border-light); }
    img.art { width: 80px; height: 80px; border-radius: 4px; object-fit: cover; background: var(--art-bg); }
    .ctrl { padding: 0.4em 0.7em; margin: 0 0.15em; background: var(--ctrl-bg); border: 1px solid var(--border); cursor: pointer; border-radius: 4px; font-size: 1em; color: var(--text); }
    .ctrl:hover { background: var(--ctrl-hover); }
    .config-btn { padding: 0.3em 0.8em; background: var(--accent); border: none; color: #fff; border-radius: 4px; cursor: pointer; }
    .config-btn:hover { background: var(--accent-hover); }
    code { background: var(--code-bg); padding: 0.1em 0.3em; border-radius: 3px; font-size: 0.85em; color: var(--text); }
    .modal-overlay { display: none; position: fixed; top: 0; left: 0; width: 100%; height: 100%; background: rgba(0,0,0,0.5); z-index: 1000; justify-content: center; align-items: center; }
    .modal-overlay.open { display: flex; }
    .modal { background: var(--bg-elevated); border-radius: 8px; padding: 1.5em; max-width: 550px; width: 90%; max-height: 85vh; overflow-y: auto; color: var(--text); }
    .modal h2 { margin-top: 0; }
    .modal-close { float: right; background: none; border: none; font-size: 1.5em; cursor: pointer; color: var(--text-muted); }
    .form-section { border-top: 1px solid var(--border-light); padding-top: 1em; margin-top: 1em; }
    .form-section h3 { margin: 0 0 0.8em 0; font-size: 1em; }
    .form-actions { display: flex; gap: 0.5em; justify-content: flex-end; margin-top: 1.5em; }
    .btn-primary { background: var(--accent); border: none; color: #fff; padding: 0.5em 1em; border-radius: 4px; cursor: pointer; }
    .btn-primary:hover { background: var(--accent-hover); }
    .btn-secondary { background: var(--ctrl-bg); border: 1px solid var(--border); padding: 0.5em 1em; border-radius: 4px; cursor: pointer; color: var(--text); }
    .btn-secondary:hover { background: var(--ctrl-hover); }
    .config-form { margin-top: 1em; padding-top: 1em; border-top: 1px solid var(--border-light); }
    .zone-card { border: 1px solid var(--border); border-radius: 8px; padding: 1em; margin-bottom: 1em; display: flex; gap: 1em; align-items: center; background: var(--bg-surface); }
    .zone-card.selected { border-color: var(--accent); background: var(--card-selected); }
    img.art-lg { width: 120px; height: 120px; border-radius: 6px; object-fit: cover; background: var(--art-bg); }
    .zone-info { flex: 1; min-width: 0; }
    .zone-info h3 { margin: 0 0 0.3em 0; }
    .zone-controls { display: flex; gap: 0.3em; margin-top: 0.8em; }
    /* Mobile responsive */
    @media (max-width: 600px) {
      nav { gap: 0.5em; padding: 0.6em 0.8em; }
      nav h1 { font-size: 1em; width: 100%; }
      nav a { padding: 0.3em 0.6em; font-size: 0.9em; }
      .nav-right { width: 100%; justify-content: space-between; margin-left: 0; }
      .zone-card { flex-direction: column; align-items: stretch; text-align: center; }
      img.art-lg { width: 100%; max-width: 200px; height: auto; aspect-ratio: 1; margin: 0 auto; }
      .zone-controls { justify-content: center; }
      .modal { padding: 1em; max-height: 90vh; }
      table { font-size: 0.85em; }
      th, td { padding: 0.3em; }
    }
  `;

  const navHtml = (active) => `
    <nav>
      <h1>Hi-Fi Control</h1>
      <a href="/control" class="${active === 'control' ? 'active' : ''}">Control</a>
      <a href="/zone" class="${active === 'zone' ? 'active' : ''}">Zone</a>
      <a href="/knobs" class="${active === 'knobs' ? 'active' : ''}">Knobs</a>
      <a href="/settings" class="${active === 'settings' ? 'active' : ''}">Settings</a>
      <div class="nav-right">
        <button class="theme-toggle" onclick="cycleTheme()" title="Toggle theme" aria-label="Toggle theme">
          <span id="theme-icon">☀</span>
        </button>
        <span class="version" id="app-version"></span>
      </div>
    </nav>
  `;

  const versionScript = `
    fetch('/status').then(r=>r.json()).then(d=>{document.getElementById('app-version').textContent='v'+d.version;});
    fetch('/api/settings').then(r=>r.json()).then(s=>{if(s.hideKnobsPage){const k=document.querySelector('nav a[href="/knobs"]');if(k)k.style.display='none';}}).catch(()=>{});
    // Theme handling
    const themes = ['light', 'dark', 'black'];
    const themeIcons = { light: '☀', dark: '🌙', black: '●' };
    function getTheme() { return localStorage.getItem('hifi-theme') || 'light'; }
    function setTheme(t) {
      document.documentElement.setAttribute('data-theme', t);
      localStorage.setItem('hifi-theme', t);
      const icon = document.getElementById('theme-icon');
      if (icon) icon.textContent = themeIcons[t] || '☀';
    }
    function cycleTheme() {
      const current = getTheme();
      const next = themes[(themes.indexOf(current) + 1) % themes.length];
      setTheme(next);
    }
    setTheme(getTheme());
  `;

  // HTML escape helper to prevent XSS
  const escapeScript = `
function esc(s) {
  if (s == null) return '';
  return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;').replace(/'/g,'&#39;');
}
function escAttr(s) { return esc(s); }
`;

  // GET /control - Normal listening: all zones, basic controls
  router.get(['/control', '/admin/control'], (req, res) => {
    res.send(`<!DOCTYPE html><html><head><title>Control - Hi-Fi</title><style>${baseStyles}</style></head><body>
${navHtml('control')}
<h2>All Zones</h2>
<div id="zones">Loading...</div>
<script>
${versionScript}
${escapeScript}

let hqpProfiles = [];
let hqpCurrentProfile = null;

async function loadHqpProfiles() {
  try {
    const [statusRes, profilesRes] = await Promise.all([
      fetch('/hqp/status'),
      fetch('/hqp/profiles')
    ]);
    const status = await statusRes.json();
    const profiles = await profilesRes.json();
    // Only update if we got valid data (preserve cache on HQPlayer restart)
    if (profiles.profiles && profiles.profiles.length > 0) {
      hqpProfiles = profiles.profiles;
    }
    if (status.configName) {
      hqpCurrentProfile = status.configName;
    }
  } catch (e) { /* HQPlayer not configured or restarting - keep cached profiles */ }
}

async function loadZones() {
  try {
    const res = await fetch('/admin/status.json');
    const data = await res.json();
    const zones = data.zones || [];
    const nowPlaying = {};
    (data.now_playing || []).forEach(np => nowPlaying[np.zone_id] = np);

    if (zones.length === 0) {
      document.getElementById('zones').innerHTML = '<p class="muted">No zones found. Check that your audio sources are connected.</p>';
      return;
    }

    // Group zones by prefix (roon:, upnp:, openhome:)
    const groupedZones = {};
    zones.forEach(zone => {
      const prefix = zone.zone_id.split(':')[0] || 'unknown';
      if (!groupedZones[prefix]) groupedZones[prefix] = [];
      groupedZones[prefix].push(zone);
    });

    const protocolLabels = { openhome: 'OpenHome', upnp: 'UPnP/DLNA', roon: 'Roon', lms: 'Lyrion' };
    let html = '';
    
    Object.keys(groupedZones).sort().forEach(protocol => {
      const protocolZones = groupedZones[protocol];
      html += '<h2 style="margin-top:1.5em;margin-bottom:0.5em;color:#666;font-size:1.1em;">' + (protocolLabels[protocol] || protocol) + '</h2>';
      
      html += protocolZones.map(zone => {
        const unsupported = zone.unsupported || [];
        const supportsNextPrev = !unsupported.includes('next');
        const supportsAlbumArt = !unsupported.includes('album_art');
        const supportsTrackInfo = !unsupported.includes('track_metadata');
      const np = nowPlaying[zone.zone_id] || {};
      const track = esc(np.line1 || 'Stopped');
      const artist = esc(np.line2 || '');
      const album = esc(np.line3 || '');
      const volUnit = np.volume_type === 'db' ? ' dB' : '';
      const vol = typeof np.volume === 'number' ? np.volume + volUnit : '—';
      const step = np.volume_step || 2;
      const playIcon = np.is_playing ? '⏸' : '▶';
      const deviceInfo = zone.device_name ? ' <span class="muted">(' + esc(zone.device_name) + ')</span>' : '';
      const isHqp = (zone.output_name || '').toLowerCase().includes('hqplayer');
      const profileSelect = isHqp && hqpProfiles.length > 0 ?
        '<p class="muted" style="margin-top:0.5em;">Configuration: <select class="hqp-profile-select" style="padding:0.2em;">' +
        hqpProfiles.map(p => '<option value="' + escAttr(p.value) + '"' +
          ((hqpCurrentProfile && (p.title.toLowerCase() === hqpCurrentProfile.toLowerCase() || p.value === hqpCurrentProfile)) ? ' selected' : '') + '>' +
          esc(p.title) + '</option>').join('') +
        '</select></p>' : '';
      return '<div class="zone-card" data-zone-id="' + escAttr(zone.zone_id) + '" data-step="' + step + '">' +
        (supportsAlbumArt 
          ? '<img class="art-lg" src="/now_playing/image?zone_id=' + encodeURIComponent(zone.zone_id) + '&width=120&height=120" alt="">'
          : '<div class="art-lg" style="background:#f5f5f5;display:flex;align-items:center;justify-content:center;color:#999;border:1px solid #ddd;border-radius:6px;">No Art</div>') +
        '<div class="zone-info">' +
          '<h3>' + esc(zone.zone_name) + deviceInfo + '</h3>' +
          (supportsTrackInfo 
            ? '<p><strong>' + track + '</strong></p><p>' + artist + (album ? ' • ' + album : '') + '</p>'
            : '<p class="muted">Basic UPnP device - transport controls only</p>') +
          '<p class="muted">Volume: ' + vol + '</p>' +
          profileSelect +
          '<div style="display:flex;gap:1em;align-items:center;">' +
            '<div class="zone-controls">' +
              (supportsNextPrev ? '<button class="ctrl" data-action="previous">⏮</button>' : '') +
              '<button class="ctrl" data-action="play_pause">' + playIcon + '</button>' +
              (supportsNextPrev ? '<button class="ctrl" data-action="next">⏭</button>' : '') +
            '</div>' +
            '<div style="display:flex;flex-direction:column;gap:0.2em;">' +
              '<button class="ctrl" data-action="vol_rel" data-value="1">+</button>' +
              '<button class="ctrl" data-action="vol_rel" data-value="-1">−</button>' +
            '</div>' +
          '</div>' +
        '</div>' +
      '</div>';
    }).join('');
    });

    document.getElementById('zones').innerHTML = html;
  } catch (e) {
    document.getElementById('zones').innerHTML = '<p class="error">Error: ' + esc(e.message) + '</p>';
  }
}

async function ctrl(zoneId, action, value) {
  await fetch('/control', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ zone_id: zoneId, action, value })
  });
  setTimeout(loadZones, 300);
}

async function loadProfile(profile) {
  // Update UI immediately to show selection
  hqpCurrentProfile = profile;
  loadZones();

  await fetch('/hqp/profiles/load', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ profile })
  });

  // HQPlayer restarts when loading a profile - wait before re-fetching
  setTimeout(async () => {
    await loadHqpProfiles();
    loadZones();
  }, 5000);
}

// Event delegation for zone control buttons
document.getElementById('zones').addEventListener('click', function(e) {
  const btn = e.target.closest('.ctrl');
  if (!btn) return;
  const card = btn.closest('.zone-card');
  const zoneId = card.dataset.zoneId;
  const action = btn.dataset.action;
  const step = parseInt(card.dataset.step) || 2;
  let value = btn.dataset.value ? parseInt(btn.dataset.value) * step : undefined;
  ctrl(zoneId, action, value);
});

// Event delegation for HQPlayer profile selection
document.getElementById('zones').addEventListener('change', function(e) {
  const sel = e.target.closest('.hqp-profile-select');
  if (!sel || !sel.value) return;
  loadProfile(sel.value);
});

// Initialize
loadHqpProfiles().then(loadZones);
setInterval(loadZones, 4000);
</script></body></html>`);
  });

  // GET /zone - Single zone focus + HQPlayer DSP controls
  router.get(['/zone', '/critical', '/admin/critical'], (req, res) => {
    res.send(`<!DOCTYPE html><html><head><title>Zone - Hi-Fi</title><style>${baseStyles}</style></head><body>
${navHtml('zone')}
<h2>Zone</h2>
<p class="muted">Select a zone and tweak DSP settings for focused listening.</p>

<div class="form-row">
  <label>Zone:</label>
  <select id="zone-select" onchange="selectZone(this.value)">
    <option value="">Loading zones...</option>
  </select>
</div>

<div id="zone-display" class="section hidden">
  <div style="display:flex;gap:1em;align-items:center;">
    <img id="zone-art" class="art-lg" src="" alt="">
    <div style="flex:1;">
      <h3 id="zone-name"></h3>
      <p id="zone-status"></p>
      <p class="muted">Volume: <span id="zone-vol">—</span></p>
      <div style="display:flex;gap:1em;align-items:center;">
        <div class="zone-controls">
          <button class="ctrl" onclick="ctrl('previous')">⏮</button>
          <button class="ctrl" id="play-btn" onclick="ctrl('play_pause')">▶</button>
          <button class="ctrl" onclick="ctrl('next')">⏭</button>
        </div>
        <div style="display:flex;flex-direction:column;gap:0.2em;">
          <button class="ctrl" onclick="ctrl('vol_rel',2)">+</button>
          <button class="ctrl" onclick="ctrl('vol_rel',-2)">−</button>
        </div>
      </div>
    </div>
  </div>
</div>

<div id="hqp-section" class="section hidden">
  <h3>HQPlayer DSP</h3>
  <div id="hqp-not-configured">
    <p class="muted">HQPlayer not configured. <a href="/admin/settings">Configure in Settings</a></p>
  </div>
  <div id="hqp-configured" class="hidden">
    <p>Status: <span id="hqp-status">checking...</span></p>
    <div class="form-row"><label>Configuration:</label><select id="hqp-profile" onchange="loadProfile(this.value)"></select></div>
    <div class="form-row"><label>Mode:</label><select id="hqp-mode" onchange="setPipeline('mode',this.value)"></select></div>
    <div class="form-row"><label>Sample Rate:</label><select id="hqp-samplerate" onchange="setPipeline('samplerate',this.value)"></select></div>
    <div class="form-row"><label>Filter (1x):</label><select id="hqp-filter1x" onchange="setPipeline('filter1x',this.value)"></select></div>
    <div class="form-row"><label>Filter (Nx):</label><select id="hqp-filterNx" onchange="setPipeline('filterNx',this.value)"></select></div>
    <div class="form-row"><label id="hqp-shaper-label">Shaper:</label><select id="hqp-shaper" onchange="setPipeline('shaper',this.value)"></select></div>
    <div id="hqp-msg" class="status-msg"></div>
  </div>
</div>

<script>
${versionScript}
${escapeScript}
let selectedZone = localStorage.getItem('hifi-zone') || null;
let zonesData = [];
let initialLoad = true;

async function loadZones() {
  const res = await fetch('/admin/status.json');
  const data = await res.json();
  zonesData = data.zones || [];
  const nowPlaying = {};
  (data.now_playing || []).forEach(np => nowPlaying[np.zone_id] = np);

  const sel = document.getElementById('zone-select');
  sel.innerHTML = '<option value="">-- Select Zone --</option>' + zonesData.map(z =>
    '<option value="' + escAttr(z.zone_id) + '"' + (z.zone_id === selectedZone ? ' selected' : '') + '>' + esc(z.zone_name) + '</option>'
  ).join('');

  // Auto-restore saved zone on first load
  if (initialLoad && selectedZone) {
    initialLoad = false;
    const exists = zonesData.some(z => z.zone_id === selectedZone);
    if (exists) {
      document.getElementById('zone-display').classList.remove('hidden');
    } else {
      selectedZone = null;
      localStorage.removeItem('hifi-zone');
    }
  }
  initialLoad = false;

  if (selectedZone) updateZoneDisplay(nowPlaying[selectedZone]);
}

function selectZone(zoneId) {
  selectedZone = zoneId;
  if (zoneId) {
    localStorage.setItem('hifi-zone', zoneId);
  } else {
    localStorage.removeItem('hifi-zone');
  }
  if (!zoneId) {
    document.getElementById('zone-display').classList.add('hidden');
    return;
  }
  document.getElementById('zone-display').classList.remove('hidden');
  loadZones();
}

function updateZoneDisplay(np) {
  if (!np) np = {};
  const zone = zonesData.find(z => z.zone_id === selectedZone);
  const deviceInfo = zone?.device_name ? ' (' + zone.device_name + ')' : '';
  document.getElementById('zone-name').textContent = (zone?.zone_name || '') + deviceInfo;
  const track = esc(np.line1 || 'Stopped');
  const artist = esc(np.line2 || '');
  const album = esc(np.line3 || '');
  document.getElementById('zone-status').innerHTML = '<strong>' + track + '</strong>' + (artist ? '<br>' + artist + (album ? ' • ' + album : '') : '');
  const volUnit = np.volume_type === 'db' ? ' dB' : '';
  document.getElementById('zone-vol').textContent = typeof np.volume === 'number' ? np.volume + volUnit : '—';
  document.getElementById('zone-art').src = '/now_playing/image?zone_id=' + encodeURIComponent(selectedZone) + '&width=120&height=120&t=' + Date.now();
  document.getElementById('play-btn').textContent = np.is_playing ? '⏸' : '▶';

  // Show/hide HQPlayer section based on zone (check if output contains HQPlayer)
  const isHqpZone = zone && (zone.output_name || '').toLowerCase().includes('hqplayer');
  document.getElementById('hqp-section').classList.toggle('hidden', !isHqpZone);
}

async function ctrl(action, value) {
  if (!selectedZone) return;
  await fetch('/control', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ zone_id: selectedZone, action, value })
  });
  setTimeout(loadZones, 300);
}

// HQPlayer
async function loadHqpStatus() {
  try {
    const res = await fetch('/hqp/status');
    const data = await res.json();
    if (data.enabled) {
      document.getElementById('hqp-not-configured').classList.add('hidden');
      document.getElementById('hqp-configured').classList.remove('hidden');
      document.getElementById('hqp-status').textContent = data.connected ? 'Connected' : 'Disconnected';
      document.getElementById('hqp-status').className = data.connected ? 'success' : 'error';
      loadHqpProfiles(data.configName);
      loadHqpPipeline();
    }
  } catch (e) { console.error('HQPlayer status error:', e); }
}

async function loadHqpProfiles(configName) {
  const res = await fetch('/hqp/profiles');
  const data = await res.json();
  const sel = document.getElementById('hqp-profile');
  sel.innerHTML = '';
  (data.profiles || []).forEach(p => {
    const opt = document.createElement('option');
    opt.value = p.value || p;
    opt.textContent = p.title || p.value || p;
    if (configName && (p.title || '').toLowerCase() === configName.toLowerCase()) opt.selected = true;
    sel.appendChild(opt);
  });
}

async function loadHqpPipeline() {
  const res = await fetch('/hqp/pipeline');
  const data = await res.json();
  if (!data.settings) return;
  const s = data.settings;
  popSel('hqp-mode', s.mode?.options, s.mode?.selected?.value);
  popSel('hqp-samplerate', s.samplerate?.options, s.samplerate?.selected?.value);
  popSel('hqp-filter1x', s.filter1x?.options, s.filter1x?.selected?.value);
  popSel('hqp-filterNx', s.filterNx?.options, s.filterNx?.selected?.value);
  popSel('hqp-shaper', s.shaper?.options, s.shaper?.selected?.value);
  // Update shaper label based on mode: PCM uses Dither, SDM uses Modulator
  const shaperLabel = document.getElementById('hqp-shaper-label');
  if (shaperLabel) {
    const modeLabel = s.mode?.selected?.label?.toLowerCase() || '';
    shaperLabel.textContent = modeLabel.includes('sdm') || modeLabel.includes('dsd') ? 'Modulator:' : 'Dither:';
  }
}

function popSel(id, opts, cur) {
  const sel = document.getElementById(id);
  sel.innerHTML = '';
  (opts || []).forEach(o => {
    const opt = document.createElement('option');
    opt.value = o.value || o;
    opt.textContent = o.label || o.value || o;
    if ((o.value || o) === cur) opt.selected = true;
    sel.appendChild(opt);
  });
}

async function loadProfile(profile) {
  if (!profile) return;
  document.getElementById('hqp-msg').textContent = 'Loading...';
  const res = await fetch('/hqp/profiles/load', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ profile }) });
  document.getElementById('hqp-msg').textContent = res.ok ? 'Configuration loading...' : 'Error';
  document.getElementById('hqp-msg').className = 'status-msg ' + (res.ok ? 'success' : 'error');
  setTimeout(loadHqpPipeline, 2000);
}

async function setPipeline(setting, value) {
  // Disable all HQPlayer controls and show loading
  const selects = ['hqp-mode', 'hqp-samplerate', 'hqp-filter1x', 'hqp-filterNx', 'hqp-shaper', 'hqp-profile'];
  selects.forEach(id => {
    const el = document.getElementById(id);
    if (el) el.disabled = true;
  });
  const msg = document.getElementById('hqp-msg');
  msg.textContent = 'Updating...';
  msg.className = 'status-msg';
  document.body.style.cursor = 'wait';

  const res = await fetch('/hqp/pipeline', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ setting, value }) });

  if (res.ok) {
    // Refresh pipeline to show updated values
    await loadHqpPipeline();
    msg.textContent = setting + ' updated';
    msg.className = 'status-msg success';
  } else {
    msg.textContent = 'Error';
    msg.className = 'status-msg error';
  }

  // Re-enable controls
  selects.forEach(id => {
    const el = document.getElementById(id);
    if (el) el.disabled = false;
  });
  document.body.style.cursor = 'default';
}

loadZones();
loadHqpStatus();
setInterval(loadZones, 4000);
</script></body></html>`);
  });

  // GET /knobs - Knob device management
  router.get(['/knobs', '/admin/knobs'], (req, res) => {
    res.send(`<!DOCTYPE html><html><head><title>Knobs - Hi-Fi</title><style>${baseStyles}</style></head><body>
${navHtml('knobs')}
<h2>Knob Devices</h2>
<p id="community-link" style="margin-bottom:1em;"><a href="https://community.roonlabs.com/t/50-esp32-s3-knob-roon-controller/311363" target="_blank" rel="noopener">Knob Community Thread</a> - build info, firmware updates, discussion</p>
<table>
  <thead><tr><th>ID</th><th>Name</th><th>Version</th><th>IP</th><th>Zone</th><th>Battery</th><th>Last Seen</th><th></th></tr></thead>
  <tbody id="knobs-body"><tr><td colspan="8" class="muted">Loading...</td></tr></tbody>
</table>

<div class="section" style="margin-top:2em;">
  <h3>Firmware</h3>
  <p>Current: <span id="fw-version">checking...</span></p>
  <button id="fetch-btn" onclick="fetchFirmware()">Fetch Latest from GitHub</button>
  <a href="/knobs/flash" style="margin-left:1em;">Flash a new knob</a>
  <span id="fw-msg" class="status-msg"></span>
</div>

<div id="configModal" class="modal-overlay">
  <div class="modal">
    <button class="modal-close" onclick="closeModal()">&times;</button>
    <h2>Knob Configuration</h2>
    <div id="configForm">Loading...</div>
  </div>
</div>

<script>
${versionScript}
${escapeScript}
let zonesData = [];


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

async function loadKnobs() {
  const res = await fetch('/admin/status.json');
  const data = await res.json();
  const knobs = data.knobs || [];
  zonesData = data.zones || [];

  const tbody = document.getElementById('knobs-body');
  if (knobs.length === 0) {
    tbody.innerHTML = '<tr><td colspan="8" class="muted">No knobs registered. Connect a knob to see it here.</td></tr>';
    return;
  }

  tbody.innerHTML = knobs.map(k => {
    const st = k.status || {};
    const bat = st.battery_level != null ? st.battery_level + '%' + (st.battery_charging ? ' ⚡' : '') : '—';
    const zone = st.zone_id ? esc(zonesData.find(z => z.zone_id === st.zone_id)?.zone_name || st.zone_id) : '—';
    const ip = st.ip || '—';
    return '<tr><td><code>' + esc(k.knob_id || '') + '</code></td><td>' + (k.name ? esc(k.name) : '<span class="muted">unnamed</span>') + '</td><td>' + esc(k.version || '—') + '</td><td>' + esc(ip) + '</td><td>' + zone + '</td><td>' + bat + '</td><td>' + ago(k.last_seen) + '</td><td><button class="config-btn" data-knob-id="' + escAttr(k.knob_id) + '">Config</button></td></tr>';
  }).join('');
}

// Event delegation for config buttons
document.getElementById('knobs-body').addEventListener('click', function(e) {
  const btn = e.target.closest('.config-btn');
  if (btn) openConfig(btn.dataset.knobId);
});

function openModal() { document.getElementById('configModal').classList.add('open'); }
function closeModal() { document.getElementById('configModal').classList.remove('open'); }

let currentKnobId = null;

async function openConfig(knobId) {
  currentKnobId = knobId;
  openModal();
  document.getElementById('configForm').innerHTML = 'Loading...';

  const res = await fetch('/config/' + encodeURIComponent(knobId));
  const data = await res.json();
  const c = data.config || {};

  const rotSel = (n, v) => '<select name="' + n + '"><option value="0"' + (v === 0 ? ' selected' : '') + '>0°</option><option value="180"' + (v === 180 ? ' selected' : '') + '>180°</option></select>';
  const artChg = c.art_mode_charging || { enabled: true, timeout_sec: 60 };
  const artBat = c.art_mode_battery || { enabled: true, timeout_sec: 30 };
  const dimChg = c.dim_charging || { enabled: true, timeout_sec: 120 };
  const dimBat = c.dim_battery || { enabled: true, timeout_sec: 30 };
  const slpChg = c.sleep_charging || { enabled: false, timeout_sec: 0 };
  const slpBat = c.sleep_battery || { enabled: true, timeout_sec: 60 };

  document.getElementById('configForm').innerHTML = '<form id="knobConfigForm">' +
    '<div class="form-row"><label>Name:</label><input type="text" name="name" value="' + escAttr(c.name || '') + '" placeholder="Living Room Knob"></div>' +
    '<div class="form-section"><h3>Display Rotation</h3>' +
    '<p class="muted" style="margin:0 0 0.8em;font-size:0.85em;">Flip the display based on how the knob is oriented. Useful if you mount it differently when docked vs handheld.</p>' +
    '<div class="form-row"><label>Charging:</label>' + rotSel('rotation_charging', c.rotation_charging ?? 180) + ' <label style="margin-left:1em;">Battery:</label>' + rotSel('rotation_not_charging', c.rotation_not_charging ?? 0) + '</div></div>' +
    '<div class="form-section"><h3>Power Timers</h3>' +
    '<p class="muted" style="margin:0 0 0.8em;font-size:0.85em;">After inactivity, the display transitions: <strong>Art Mode</strong> (album art) → <strong>Dim</strong> (reduced brightness) → <strong>Sleep</strong> (display off). Timeout in seconds, 0 to skip.</p>' +
    '<table style="font-size:0.9em;"><tr><th></th><th>Charging</th><th>Battery</th></tr>' +
    '<tr><td>Art Mode</td><td><input type="number" name="art_chg_sec" value="' + artChg.timeout_sec + '" style="width:50px;"> <label><input type="checkbox" name="art_chg_on"' + (artChg.enabled ? ' checked' : '') + '> On</label></td><td><input type="number" name="art_bat_sec" value="' + artBat.timeout_sec + '" style="width:50px;"> <label><input type="checkbox" name="art_bat_on"' + (artBat.enabled ? ' checked' : '') + '> On</label></td></tr>' +
    '<tr><td>Dim</td><td><input type="number" name="dim_chg_sec" value="' + dimChg.timeout_sec + '" style="width:50px;"> <label><input type="checkbox" name="dim_chg_on"' + (dimChg.enabled ? ' checked' : '') + '> On</label></td><td><input type="number" name="dim_bat_sec" value="' + dimBat.timeout_sec + '" style="width:50px;"> <label><input type="checkbox" name="dim_bat_on"' + (dimBat.enabled ? ' checked' : '') + '> On</label></td></tr>' +
    '<tr><td>Sleep</td><td><input type="number" name="slp_chg_sec" value="' + slpChg.timeout_sec + '" style="width:50px;"> <label><input type="checkbox" name="slp_chg_on"' + (slpChg.enabled ? ' checked' : '') + '> On</label></td><td><input type="number" name="slp_bat_sec" value="' + slpBat.timeout_sec + '" style="width:50px;"> <label><input type="checkbox" name="slp_bat_on"' + (slpBat.enabled ? ' checked' : '') + '> On</label></td></tr></table></div>' +
    '<div class="form-section"><h3>Power Management</h3>' +
    '<p class="muted" style="margin:0 0 0.8em;font-size:0.85em;">Battery optimization settings. May slightly increase response latency.</p>' +
    '<div class="form-row"><label><input type="checkbox" name="wifi_ps"' + (c.wifi_power_save_enabled ? ' checked' : '') + '> WiFi Sleep</label> <span class="muted" style="font-size:0.8em;">— reduces WiFi power when idle</span></div>' +
    '<div class="form-row"><label><input type="checkbox" name="cpu_scale"' + (c.cpu_freq_scaling_enabled ? ' checked' : '') + '> CPU Scaling</label> <span class="muted" style="font-size:0.8em;">— lowers CPU speed when idle</span></div>' +
    '<div class="form-row" style="margin-top:0.8em;"><label>Poll interval when stopped:</label><input type="number" name="sleep_poll_stopped" value="' + (c.sleep_poll_stopped_sec ?? 60) + '" style="width:50px;"> sec <span class="muted" style="font-size:0.8em;">— how often to check for updates when nothing is playing</span></div></div>' +
    '<div class="form-actions"><button type="button" class="btn-secondary" onclick="closeModal()">Cancel</button><button type="submit" class="btn-primary">Save</button></div></form>';

  document.getElementById('knobConfigForm').addEventListener('submit', saveConfig);
}

async function saveConfig(e) {
  e.preventDefault();
  const f = e.target;
  const knobId = currentKnobId;
  const v = n => f.querySelector('[name="' + n + '"]')?.value || '';
  const num = n => parseInt(v(n)) || 0;
  const chk = n => f.querySelector('[name="' + n + '"]')?.checked || false;

  const cfg = {
    name: v('name'),
    rotation_charging: num('rotation_charging'),
    rotation_not_charging: num('rotation_not_charging'),
    art_mode_charging: { enabled: chk('art_chg_on'), timeout_sec: num('art_chg_sec') },
    art_mode_battery: { enabled: chk('art_bat_on'), timeout_sec: num('art_bat_sec') },
    dim_charging: { enabled: chk('dim_chg_on'), timeout_sec: num('dim_chg_sec') },
    dim_battery: { enabled: chk('dim_bat_on'), timeout_sec: num('dim_bat_sec') },
    sleep_charging: { enabled: chk('slp_chg_on'), timeout_sec: num('slp_chg_sec') },
    sleep_battery: { enabled: chk('slp_bat_on'), timeout_sec: num('slp_bat_sec') },
    wifi_power_save_enabled: chk('wifi_ps'),
    cpu_freq_scaling_enabled: chk('cpu_scale'),
    sleep_poll_stopped_sec: num('sleep_poll_stopped'),
  };

  const res = await fetch('/config/' + encodeURIComponent(knobId), { method: 'PUT', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(cfg) });
  if (res.ok) { closeModal(); loadKnobs(); } else { alert('Save failed'); }
}

document.getElementById('configModal').addEventListener('click', e => { if (e.target.id === 'configModal') closeModal(); });
document.addEventListener('keydown', e => { if (e.key === 'Escape') closeModal(); });

// Firmware
async function loadFirmwareVersion() {
  try {
    const res = await fetch('/firmware/version');
    if (res.ok) {
      const data = await res.json();
      document.getElementById('fw-version').textContent = 'v' + data.version;
    } else {
      document.getElementById('fw-version').textContent = 'Not installed';
    }
  } catch (e) {
    document.getElementById('fw-version').textContent = 'Not installed';
  }
}

async function fetchFirmware() {
  const btn = document.getElementById('fetch-btn');
  const msg = document.getElementById('fw-msg');
  btn.disabled = true;
  msg.textContent = 'Fetching...';
  msg.className = 'status-msg';

  try {
    const res = await fetch('/admin/fetch-firmware', { method: 'POST' });
    const data = await res.json();
    if (res.ok) {
      msg.textContent = 'Downloaded v' + data.version;
      msg.className = 'status-msg success';
      document.getElementById('fw-version').textContent = 'v' + data.version;
    } else {
      msg.textContent = 'Error: ' + data.error;
      msg.className = 'status-msg error';
    }
  } catch (e) {
    msg.textContent = 'Error: ' + e.message;
    msg.className = 'status-msg error';
  }
  btn.disabled = false;
}

loadKnobs();
loadFirmwareVersion();
setInterval(loadKnobs, 5000);
</script></body></html>`);
  });

  // GET /knobs/flash - Web flasher for ESP32-S3
  router.get('/knobs/flash', (req, res) => {
    res.send(`<!DOCTYPE html><html><head>
<title>Flash Knob - Hi-Fi Control</title>
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<script type="module" src="https://unpkg.com/esp-web-tools@10/dist/web/install-button.js?module"></script>
<style>${baseStyles}
  .flash-card { background: var(--bg-surface); border-radius: 12px; padding: 24px; margin: 20px 0; }
  .flash-card h3 { margin-top: 0; }
  .flash-section { display: flex; align-items: center; gap: 20px; flex-wrap: wrap; margin: 1em 0; }
  esp-web-install-button button { background: var(--accent); color: white; border: none; padding: 12px 24px; border-radius: 8px; font-size: 16px; cursor: pointer; font-weight: 600; }
  esp-web-install-button button:hover { background: var(--accent-hover); }
  .version-info { color: var(--text-muted); }
  .info-box { background: var(--bg-surface); border-left: 4px solid #2196f3; padding: 12px 16px; border-radius: 4px; margin: 1em 0; }
  .warning-box { background: var(--bg-surface); border-left: 4px solid #ff9800; padding: 12px 16px; border-radius: 4px; margin: 1em 0; }
  .unsupported { display: none; background: var(--bg-surface); border-left: 4px solid var(--error); padding: 12px 16px; border-radius: 4px; margin: 1em 0; }
  .steps ol { padding-left: 20px; }
  .steps li { margin-bottom: 8px; color: var(--text-muted); }
  .steps code { background: var(--code-bg); padding: 2px 6px; border-radius: 4px; font-size: 0.9em; }
</style>
</head><body>
${navHtml('knobs')}
<h2>Flash Knob Firmware</h2>
<p class="muted">Flash firmware directly from your browser - no tools to install</p>

<div class="unsupported" id="unsupported">
  <strong>Browser Not Supported</strong><br>
  Web Serial requires Chrome or Edge (version 89+). Safari and Firefox are not supported.
</div>

<div class="info-box">
  <strong>Chip Auto-Detection</strong><br>
  Not sure which chip? Just try - ESP Web Tools detects the chip and warns if it doesn't match.
</div>

<div class="flash-card">
  <h3>ESP32-S3 Controller</h3>
  <p class="muted">The main Roon Knob controller - handles display, rotary encoder, touch, WiFi, and music source communication.</p>
  <div class="flash-section">
    <esp-web-install-button id="flash-btn" manifest="/manifest-s3.json">
      <button slot="activate">Flash ESP32-S3</button>
      <span slot="unsupported">Not supported</span>
      <span slot="not-allowed">HTTPS required</span>
    </esp-web-install-button>
    <span class="version-info" id="fw-version">Loading...</span>
  </div>
  <div id="no-firmware-msg" class="warning-box" style="display:none;">
    <strong>No firmware available</strong><br>
    Go to <a href="/knobs">Knobs page</a> and click "Fetch Latest from GitHub" first.
  </div>
  <div class="steps">
    <strong>Steps:</strong>
    <ol>
      <li>Turn on the device (power slider towards USB-C port)</li>
      <li>Connect via USB-C</li>
      <li>Click "Flash ESP32-S3" and select the serial port</li>
      <li>Wait ~30 seconds for flashing to complete</li>
    </ol>
    <p class="muted"><strong>Port name:</strong> macOS: <code>cu.usbmodem*</code> · Linux: <code>ttyACM0</code> · Windows: <code>COM*</code></p>
  </div>
</div>

<div class="warning-box">
  <strong>First time?</strong><br>
  After flashing, the device creates a WiFi access point called <code>roon-knob-setup</code>. Connect to it to configure your WiFi credentials.
</div>

<div class="section">
  <h3>Requirements</h3>
  <ul>
    <li><strong>Browser:</strong> Chrome or Edge 89+</li>
    <li><strong>USB cable</strong> connected to your computer</li>
    <li><strong>Driver:</strong> <a href="https://www.silabs.com/developers/usb-to-uart-bridge-vcp-drivers" target="_blank">CP210x</a> or <a href="https://www.wch-ic.com/downloads/CH341SER_ZIP.html" target="_blank">CH340</a> may be needed</li>
  </ul>
</div>

<script>
${versionScript}
if (!('serial' in navigator)) {
  document.getElementById('unsupported').style.display = 'block';
}
fetch('/firmware/version')
  .then(r => {
    if (!r.ok) throw new Error('No firmware');
    return r.json();
  })
  .then(data => {
    document.getElementById('fw-version').textContent = 'v' + data.version;
  })
  .catch(() => {
    document.getElementById('fw-version').textContent = '';
    document.getElementById('flash-btn').style.display = 'none';
    document.getElementById('no-firmware-msg').style.display = 'block';
  });
</script>
</body></html>`);
  });

  // GET /settings - Settings page (HQPlayer config, firmware, status)
  router.get(['/settings', '/admin', '/admin/settings', '/dashboard'], (req, res) => {
    res.send(`<!DOCTYPE html><html><head><title>Settings - Hi-Fi</title><style>${baseStyles}</style></head><body>
${navHtml('settings')}
<h2>Settings</h2>

<div class="section">
  <h3>HQPlayer Configuration</h3>
  <div id="hqp-status-line" class="muted">Checking...</div>
  <button id="hqp-reconfig-btn" onclick="showHqpConfig()" style="display:none;margin-top:0.5em;">Reconfigure</button>
  <div id="hqp-config-form" style="display:none;">
    <p class="muted" style="margin:0.5em 0;">Filter/shaper/rate control uses native protocol (port 4321).</p>
    <div class="form-row"><label>Host:</label><input type="text" id="hqp-host" placeholder="192.168.1.x"></div>
    <div id="hqp-embedded-fields" style="display:none;">
      <p class="muted" style="margin:0.5em 0;">Configuration switching requires web UI credentials (Embedded only):</p>
      <div class="form-row"><label>Port (Web UI):</label><input type="text" id="hqp-port" value="8088"></div>
      <div class="form-row"><label>Username:</label><input type="text" id="hqp-username" placeholder="(optional)"></div>
      <div class="form-row"><label>Password:</label><input type="password" id="hqp-password"></div>
    </div>
    <button onclick="saveHqpConfig()">Save</button>
    <span id="hqp-save-msg" class="status-msg"></span>
  </div>
</div>

<div class="section">
  <h3>Lyrion Music Server Configuration</h3>
  <div id="lms-status-line" class="muted">Checking...</div>
  <button id="lms-reconfig-btn" onclick="showLmsConfig()" style="display:none;margin-top:0.5em;">Reconfigure</button>
  <div id="lms-config-form" style="display:none;">
    <p class="muted" style="margin:0.5em 0;">Connect to Lyrion Music Server (formerly LMS/Squeezebox) for playback control.</p>
    <div class="form-row"><label>Host:</label><input type="text" id="lms-host" placeholder="192.168.1.x"></div>
    <div class="form-row"><label>Port:</label><input type="number" id="lms-port" value="9000" placeholder="9000"></div>
    <p class="muted" style="margin:0.5em 0;">Authentication (optional, if Lyrion has password protection enabled):</p>
    <div class="form-row"><label>Username:</label><input type="text" id="lms-username" placeholder="(optional)"></div>
    <div class="form-row"><label>Password:</label><input type="password" id="lms-password"></div>
    <button onclick="saveLmsConfig()">Save</button>
    <span id="lms-save-msg" class="status-msg"></span>
  </div>
</div>

<div class="section">
  <h3>UI Settings</h3>
  <div class="form-row">
    <label><input type="checkbox" id="hide-knobs-page"> Hide Knobs page (if you don't have a knob)</label>
  </div>
  <button onclick="saveUiSettings()">Save</button>
  <span id="ui-save-msg" class="status-msg"></span>
</div>

<div class="section">
  <h3>Audio Backends</h3>
  <p class="muted" style="margin:0 0 1em;">Select which audio backends to enable. Changes apply immediately but may take a minute to discover devices. Restart knobs to pick up new zones.</p>
  <div class="form-row">
    <label><input type="checkbox" id="adapter-roon"> Roon</label>
  </div>
  <div class="form-row">
    <label><input type="checkbox" id="adapter-upnp"> UPnP/DLNA (basic renderers)</label>
  </div>
  <div class="form-row">
    <label><input type="checkbox" id="adapter-openhome"> OpenHome (BubbleUPnP, Linn, etc.)</label>
  </div>
  <div class="form-row">
    <label><input type="checkbox" id="adapter-lms"> Lyrion (formerly LMS/Squeezebox)</label>
  </div>
  <button onclick="saveAdapterSettings()">Save</button>
  <span id="adapter-save-msg" class="status-msg"></span>
</div>

<div class="section">
  <h3>Status</h3>
  <pre id="status" style="font-size:0.85em;overflow-x:auto;"></pre>
  <details style="margin-top:1em;">
    <summary style="cursor:pointer;color:#888;">Debug Info (Bus Activity)</summary>
    <pre id="debug-status" style="font-size:0.75em;overflow-x:auto;margin-top:0.5em;"></pre>
  </details>
</div>

<script>
${versionScript}

// HQPlayer config
function showHqpConfig() {
  document.getElementById('hqp-config-form').style.display = 'block';
  document.getElementById('hqp-reconfig-btn').style.display = 'none';
}

async function loadHqpConfig() {
  try {
    const res = await fetch('/hqp/status');
    const data = await res.json();
    const statusLine = document.getElementById('hqp-status-line');
    const embeddedFields = document.getElementById('hqp-embedded-fields');
    const configForm = document.getElementById('hqp-config-form');
    const reconfigBtn = document.getElementById('hqp-reconfig-btn');

    if (data.enabled && data.connected) {
      const product = data.product || 'HQPlayer';
      const version = data.version ? ' v' + data.version : '';
      statusLine.textContent = product + version + ' at ' + (data.host || 'unknown') + ' ✓';
      statusLine.className = 'success';

      // Hide form, show reconfigure button
      configForm.style.display = 'none';
      reconfigBtn.style.display = 'inline-block';

      // Show embedded fields only for HQPlayer Embedded
      if (data.isEmbedded) {
        embeddedFields.style.display = 'block';
      }
    } else {
      statusLine.textContent = data.enabled ? 'Configured but disconnected' : 'Not configured';
      statusLine.className = 'muted';
      // Show form, hide reconfigure button
      configForm.style.display = 'block';
      reconfigBtn.style.display = 'none';
    }
  } catch (e) {
    // Show form on error
    document.getElementById('hqp-config-form').style.display = 'block';
    document.getElementById('hqp-reconfig-btn').style.display = 'none';
  }
}

async function saveHqpConfig() {
  const host = document.getElementById('hqp-host').value;
  const port = document.getElementById('hqp-port').value || '8088';
  const username = document.getElementById('hqp-username').value;
  const password = document.getElementById('hqp-password').value;
  const msg = document.getElementById('hqp-save-msg');

  if (!host) { msg.textContent = 'Host required'; msg.className = 'status-msg error'; return; }

  const cfg = { host, port: parseInt(port) };
  if (username) cfg.username = username;
  if (password) cfg.password = password;

  const res = await fetch('/hqp/configure', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(cfg) });
  msg.textContent = res.ok ? 'Saved!' : 'Error';
  msg.className = 'status-msg ' + (res.ok ? 'success' : 'error');
  loadHqpConfig();
}

// LMS config
function showLmsConfig() {
  document.getElementById('lms-config-form').style.display = 'block';
  document.getElementById('lms-reconfig-btn').style.display = 'none';
}

async function loadLmsConfig() {
  try {
    const res = await fetch('/lms/status');
    const data = await res.json();
    const statusLine = document.getElementById('lms-status-line');
    const configForm = document.getElementById('lms-config-form');
    const reconfigBtn = document.getElementById('lms-reconfig-btn');

    if (data.enabled && data.connected) {
      statusLine.textContent = 'Lyrion at ' + (data.host || 'unknown') + ':' + (data.port || 9000) + ' ✓ (' + data.player_count + ' players)';
      statusLine.className = 'success';
      configForm.style.display = 'none';
      reconfigBtn.style.display = 'inline-block';
    } else if (data.enabled && data.host) {
      statusLine.textContent = 'Configured (' + data.host + ') but disconnected';
      statusLine.className = 'muted';
      configForm.style.display = 'block';
      reconfigBtn.style.display = 'none';
      document.getElementById('lms-host').value = data.host || '';
      document.getElementById('lms-port').value = data.port || 9000;
    } else {
      statusLine.textContent = 'Not configured';
      statusLine.className = 'muted';
      configForm.style.display = 'block';
      reconfigBtn.style.display = 'none';
    }
  } catch (e) {
    document.getElementById('lms-config-form').style.display = 'block';
    document.getElementById('lms-reconfig-btn').style.display = 'none';
  }
}

async function saveLmsConfig() {
  const host = document.getElementById('lms-host').value;
  const port = document.getElementById('lms-port').value || '9000';
  const username = document.getElementById('lms-username').value;
  const password = document.getElementById('lms-password').value;
  const msg = document.getElementById('lms-save-msg');

  if (!host) { msg.textContent = 'Host required'; msg.className = 'status-msg error'; return; }

  const cfg = { host, port: parseInt(port) };
  if (username) cfg.username = username;
  if (password) cfg.password = password;

  const res = await fetch('/lms/configure', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(cfg) });
  msg.textContent = res.ok ? 'Saved!' : 'Error';
  msg.className = 'status-msg ' + (res.ok ? 'success' : 'error');
  if (res.ok) {
    setTimeout(loadLmsConfig, 1000);
  }
}

// Status
async function loadStatus() {
  const res = await fetch('/admin/status.json');
  const data = await res.json();

  // Separate debug from main status
  const debug = data.debug;
  delete data.debug;

  document.getElementById('status').textContent = JSON.stringify(data, null, 2);
  if (debug) {
    document.getElementById('debug-status').textContent = JSON.stringify(debug, null, 2);
  }
}

// UI Settings
async function loadUiSettings() {
  try {
    const res = await fetch('/api/settings');
    const data = await res.json();
    document.getElementById('hide-knobs-page').checked = data.hideKnobsPage || false;
  } catch (e) {}
}

async function saveUiSettings() {
  const msg = document.getElementById('ui-save-msg');
  const hideKnobsPage = document.getElementById('hide-knobs-page').checked;
  try {
    await fetch('/api/settings', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ hideKnobsPage })
    });
    msg.textContent = 'Saved! Refresh to see nav changes.';
    msg.className = 'status-msg success';
  } catch (e) {
    msg.textContent = 'Error saving';
    msg.className = 'status-msg error';
  }
}

// Adapter Settings
async function loadAdapterSettings() {
  try {
    const res = await fetch('/api/settings');
    const data = await res.json();
    const adapters = data.adapters || { roon: true, upnp: false, openhome: false };
    document.getElementById('adapter-roon').checked = adapters.roon !== false;
    document.getElementById('adapter-upnp').checked = adapters.upnp || false;
    document.getElementById('adapter-openhome').checked = adapters.openhome || false;
    document.getElementById('adapter-lms').checked = adapters.lms || false;
  } catch (e) {}
}

async function saveAdapterSettings() {
  const msg = document.getElementById('adapter-save-msg');
  const adapters = {
    roon: document.getElementById('adapter-roon').checked,
    upnp: document.getElementById('adapter-upnp').checked,
    openhome: document.getElementById('adapter-openhome').checked,
    lms: document.getElementById('adapter-lms').checked
  };
  try {
    await fetch('/api/settings', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ adapters })
    });
    msg.textContent = 'Saved!';
    msg.className = 'status-msg success';
  } catch (e) {
    msg.textContent = 'Error saving';
    msg.className = 'status-msg error';
  }
}

loadHqpConfig();
loadLmsConfig();
loadStatus();
loadUiSettings();
loadAdapterSettings();
</script></body></html>`);
  });

  // Firmware download config
  const https = require('https');
  const CONFIG_DIR = process.env.CONFIG_DIR || path.join(__dirname, '..', '..', 'data');
  const FIRMWARE_DIR = process.env.FIRMWARE_DIR || path.join(CONFIG_DIR, 'firmware');
  const GITHUB_REPO = process.env.FIRMWARE_REPO || 'muness/roon-knob';

  // POST /admin/fetch-firmware - Download latest firmware from GitHub releases
  router.post('/admin/fetch-firmware', async (req, res) => {

    log.info('Fetching latest firmware from GitHub', { repo: GITHUB_REPO });

    try {
      // Get latest release info from GitHub API
      const releaseData = await new Promise((resolve, reject) => {
        const options = {
          hostname: 'api.github.com',
          path: `/repos/${GITHUB_REPO}/releases/latest`,
          headers: { 'User-Agent': 'unified-hifi-control' }
        };
        https.get(options, (response) => {
          let data = '';
          response.on('data', chunk => data += chunk);
          response.on('end', () => {
            if (response.statusCode === 200) {
              resolve(JSON.parse(data));
            } else {
              reject(new Error(`GitHub API error: ${response.statusCode}`));
            }
          });
        }).on('error', reject);
      });

      const version = releaseData.tag_name.replace(/^v/, '');
      const asset = releaseData.assets.find(a => a.name === 'roon_knob.bin');

      if (!asset) {
        return res.status(404).json({ error: 'No roon_knob.bin in release' });
      }

      // Download the firmware binary
      const downloadUrl = asset.browser_download_url;
      log.info('Downloading firmware', { version, url: downloadUrl });

      // Ensure firmware directory exists
      if (!fs.existsSync(FIRMWARE_DIR)) {
        fs.mkdirSync(FIRMWARE_DIR, { recursive: true });
      }

      const firmwarePath = path.join(FIRMWARE_DIR, 'roon_knob.bin');
      const file = fs.createWriteStream(firmwarePath);

      await new Promise((resolve, reject) => {
        const download = (url) => {
          https.get(url, (response) => {
            if (response.statusCode === 302 || response.statusCode === 301) {
              download(response.headers.location);
              return;
            }
            if (response.statusCode !== 200) {
              reject(new Error(`Download failed: ${response.statusCode}`));
              return;
            }
            response.pipe(file);
            file.on('finish', () => {
              file.close();
              resolve();
            });
          }).on('error', reject);
        };
        download(downloadUrl);
      });

      // Write version.json
      const versionPath = path.join(FIRMWARE_DIR, 'version.json');
      fs.writeFileSync(versionPath, JSON.stringify({
        version,
        file: 'roon_knob.bin',
        fetched_at: new Date().toISOString(),
        release_url: releaseData.html_url
      }, null, 2));

      const stats = fs.statSync(firmwarePath);
      log.info('Firmware downloaded successfully', { version, size: stats.size });

      res.json({ version, size: stats.size, file: 'roon_knob.bin' });
    } catch (err) {
      log.error('Failed to fetch firmware', { error: err.message });
      res.status(500).json({ error: err.message });
    }
  });

  // OTA Firmware endpoints
  router.get('/firmware/version', (req, res) => {
    const knob = extractKnob(req);
    log.info('Firmware version check', { knob, ip: req.ip });

    if (!fs.existsSync(FIRMWARE_DIR)) {
      return res.status(404).json({ error: 'No firmware available' });
    }

    const files = fs.readdirSync(FIRMWARE_DIR).filter(f => f.endsWith('.bin'));
    if (files.length === 0) {
      return res.status(404).json({ error: 'No firmware available' });
    }

    const versionFile = path.join(FIRMWARE_DIR, 'version.json');
    let version = null;
    let firmwareFile = null;

    if (fs.existsSync(versionFile)) {
      try {
        const versionData = JSON.parse(fs.readFileSync(versionFile, 'utf8'));
        version = versionData.version;
        firmwareFile = versionData.file || 'roon_knob.bin';
      } catch (e) {
        log.warn('Failed to parse version.json', { error: e.message });
      }
    }

    if (!firmwareFile) {
      firmwareFile = files[0];
      const match = firmwareFile.match(/roon_knob[_-]?v?(\d+\.\d+\.\d+)\.bin/i);
      if (match) {
        version = match[1];
      }
    }

    if (!version) {
      return res.status(404).json({ error: 'No firmware version available' });
    }

    const firmwarePath = path.join(FIRMWARE_DIR, firmwareFile);
    if (!fs.existsSync(firmwarePath)) {
      return res.status(404).json({ error: 'Firmware file not found' });
    }

    const stats = fs.statSync(firmwarePath);
    log.info('Firmware available', { version, size: stats.size, file: firmwareFile });

    res.json({ version, size: stats.size, file: firmwareFile });
  });

  router.get('/firmware/download', (req, res) => {
    const knob = extractKnob(req);
    log.info('Firmware download requested', { knob, ip: req.ip });

    if (!fs.existsSync(FIRMWARE_DIR)) {
      return res.status(404).json({ error: 'No firmware available' });
    }

    let firmwareFile = 'roon_knob.bin';
    const versionFile = path.join(FIRMWARE_DIR, 'version.json');

    if (fs.existsSync(versionFile)) {
      try {
        const versionData = JSON.parse(fs.readFileSync(versionFile, 'utf8'));
        firmwareFile = versionData.file || firmwareFile;
      } catch (e) {
        log.warn('Failed to parse version.json', { error: e.message });
      }
    }

    let firmwarePath = path.join(FIRMWARE_DIR, firmwareFile);
    if (!fs.existsSync(firmwarePath)) {
      const files = fs.readdirSync(FIRMWARE_DIR).filter(f => f.endsWith('.bin'));
      if (files.length > 0) {
        firmwareFile = files[0];
        firmwarePath = path.join(FIRMWARE_DIR, firmwareFile);
      } else {
        return res.status(404).json({ error: 'Firmware file not found' });
      }
    }

    const stats = fs.statSync(firmwarePath);
    log.info('Serving firmware', { file: firmwareFile, size: stats.size });

    res.set('Content-Type', 'application/octet-stream');
    res.set('Content-Length', stats.size);
    res.set('Content-Disposition', `attachment; filename="${firmwareFile}"`);

    fs.createReadStream(firmwarePath).pipe(res);
  });

  // GET /manifest-s3.json - ESP Web Tools manifest for ESP32-S3 flasher
  router.get('/manifest-s3.json', (req, res) => {
    try {
      const versionFile = path.join(FIRMWARE_DIR, 'version.json');
      let version = 'latest';
      if (fs.existsSync(versionFile)) {
        const versionData = JSON.parse(fs.readFileSync(versionFile, 'utf8'));
        version = versionData.version || 'latest';
      }
      res.json({
        name: 'Hi-Fi Control Knob',
        version: version,
        new_install_prompt_erase: true,
        builds: [{
          chipFamily: 'ESP32-S3',
          parts: [{
            path: '/firmware/download',
            offset: 0
          }]
        }]
      });
    } catch (err) {
      log.warn('Manifest generation failed', { error: err.message });
      res.status(500).json({ error: 'Manifest generation failed' });
    }
  });

  return router;
}

module.exports = { createKnobRoutes };
