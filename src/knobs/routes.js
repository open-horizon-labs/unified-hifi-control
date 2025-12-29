const express = require('express');
const sharp = require('sharp');

function extractKnob(req) {
  const headerId = req.get('x-knob-id') || req.get('x-device-id');
  const queryId = req.query?.knob_id;
  const bodyId = req.body?.knob_id;
  const id = headerId || queryId || bodyId;
  const version = req.get('x-knob-version') || req.get('x-device-version');
  if (!id && !version) return null;
  return { id, version };
}

function createKnobRoutes({ roon, knobs, logger }) {
  const router = express.Router();
  const log = logger || console;

  // GET /zones - List all Roon zones
  router.get('/zones', (req, res) => {
    const knob = extractKnob(req);
    log.debug('Zones requested', { ip: req.ip, knob_id: knob?.id });

    const zones = roon.getZones();
    res.json({ zones });
  });

  // GET /now_playing - Get current playback state for a zone
  router.get('/now_playing', (req, res) => {
    const zoneId = req.query.zone_id;
    const knob = extractKnob(req);

    if (!zoneId) {
      const zones = roon.getZones();
      return res.status(400).json({ error: 'zone_id required', zones });
    }

    // Update knob status from query params
    if (knob?.id) {
      const statusUpdates = { zone_id: zoneId };

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

    const data = roon.getNowPlaying(zoneId);
    if (!data) {
      const zones = roon.getZones();
      log.warn('now_playing miss', { zoneId, ip: req.ip });
      return res.status(404).json({ error: 'zone not found', zones });
    }

    log.debug('now_playing served', { zoneId, ip: req.ip });

    const image_url = `/now_playing/image?zone_id=${encodeURIComponent(zoneId)}`;
    const config_sha = knob?.id ? knobs.getConfigSha(knob.id) : null;
    const zones = roon.getZones();

    res.json({ ...data, image_url, zones, config_sha });
  });

  // GET /now_playing/image - Get album artwork
  router.get('/now_playing/image', async (req, res) => {
    const zoneId = req.query.zone_id;
    if (!zoneId) {
      return res.status(400).json({ error: 'zone_id required' });
    }

    const data = roon.getNowPlaying(zoneId);
    if (!data) {
      return res.status(404).json({ error: 'zone not found' });
    }

    log.debug('now_playing image requested', { zoneId, ip: req.ip });
    const { width, height, format } = req.query || {};

    try {
      if (data.image_key && roon.getImage) {
        // RGB565 format for ESP32 display
        if (format === 'rgb565') {
          const { body } = await roon.getImage(data.image_key, {
            width: width || 360,
            height: height || 360,
            format: 'image/jpeg',
          });

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
          const { contentType, body } = await roon.getImage(data.image_key, {
            width: width || 360,
            height: height || 360,
            format: 'image/jpeg',
          });

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
      await roon.control(zone_id, action, value);
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

  // GET /knobs - List all known knobs
  router.get('/knobs', (req, res) => {
    log.debug('Knobs list requested', { ip: req.ip });
    res.json({ knobs: knobs.listKnobs() });
  });

  // GET /admin/status.json - Admin diagnostics
  router.get('/admin/status.json', (req, res) => {
    res.json({
      bridge: roon.getStatus(),
      knobs: knobs.listKnobs(),
    });
  });

  // GET /admin or /dashboard - Admin HTML placeholder
  router.get(['/admin', '/dashboard'], (req, res) => {
    res.send(`<!DOCTYPE html>
<html>
<head><title>Unified Hi-Fi Control</title>
<style>
  body { font-family: system-ui, sans-serif; max-width: 800px; margin: 2em auto; padding: 0 1em; }
  button { padding: 0.5em 1em; cursor: pointer; margin-right: 0.5em; }
  .section { margin: 1.5em 0; padding: 1em; border: 1px solid #ddd; border-radius: 4px; }
  .status-msg { margin-top: 0.5em; }
  .success { color: green; }
  .error { color: red; }
  .muted { color: #666; }
  select { padding: 0.4em; min-width: 200px; }
  label { display: inline-block; min-width: 100px; margin-right: 0.5em; }
  .form-row { margin: 0.8em 0; }
  input[type="text"] { padding: 0.4em; width: 200px; }
  .hidden { display: none; }
  .config-form { margin-top: 1em; padding-top: 1em; border-top: 1px solid #eee; }
</style>
</head>
<body>
<h1>Unified Hi-Fi Control</h1>

<div class="section">
  <h2>HQPlayer</h2>
  <div id="hqp-not-configured">
    <p class="muted">Not configured</p>
    <button onclick="toggleHqpConfig()">Configure HQPlayer</button>
  </div>
  <div id="hqp-configured" class="hidden">
    <p>Status: <span id="hqp-status">checking...</span></p>

    <div class="form-row">
      <label>Profile:</label>
      <select id="hqp-profile" onchange="loadProfile(this.value)">
        <option value="">Loading...</option>
      </select>
      <div class="muted" style="font-size: 0.85em; margin-top: 0.3em;">Current profile detected by matching config title to profile name</div>
    </div>

    <div class="form-row">
      <label>Filter (1x):</label>
      <select id="hqp-filter1x" onchange="setPipeline('filter1x', this.value)"></select>
    </div>

    <div class="form-row">
      <label>Filter (Nx):</label>
      <select id="hqp-filterNx" onchange="setPipeline('filterNx', this.value)"></select>
    </div>

    <div class="form-row">
      <label>Shaper:</label>
      <select id="hqp-shaper" onchange="setPipeline('shaper', this.value)"></select>
    </div>

    <div id="hqp-status-msg" class="status-msg"></div>
    <button onclick="toggleHqpConfig()">Reconfigure</button>
  </div>

  <div id="hqp-config-form" class="config-form hidden">
    <h3>HQPlayer Configuration</h3>
    <div class="form-row">
      <label></label>
      <button onclick="discoverHqp()">Discover on Network</button>
      <span id="hqp-discover-status" class="muted"></span>
    </div>
    <div id="hqp-discovered" class="form-row hidden">
      <label>Found:</label>
      <select id="hqp-discovered-select" onchange="selectDiscovered(this.value)">
        <option value="">-- Select --</option>
      </select>
    </div>
    <div class="form-row">
      <label>Host:</label>
      <input type="text" id="hqp-host" placeholder="">
    </div>
    <div class="form-row">
      <label>Port (Web UI):</label>
      <input type="text" id="hqp-port" value="8088" placeholder="8088">
    </div>
    <div class="form-row">
      <label>Username:</label>
      <input type="text" id="hqp-username" placeholder="(required for profiles)">
    </div>
    <div class="form-row">
      <label>Password:</label>
      <input type="password" id="hqp-password" placeholder="(required for profiles)">
    </div>
    <button onclick="saveHqpConfig()">Save</button>
    <button onclick="toggleHqpConfig()">Cancel</button>
    <div id="hqp-config-status" class="status-msg"></div>
  </div>
</div>

<div class="section">
  <h2>Firmware</h2>
  <p>Current: <span id="fw-version">checking...</span></p>
  <button id="fetch-btn" onclick="fetchFirmware()">Fetch Latest from GitHub</button>
  <div id="firmware-status" class="status-msg"></div>
</div>

<div class="section">
  <h2>Status</h2>
  <pre id="status"></pre>
</div>

<script>
// ===== HQPlayer Functions =====
let hqpConfigured = false;

async function loadHqpStatus() {
  try {
    const res = await fetch('/hqp/status');
    const data = await res.json();

    if (data.enabled) {
      hqpConfigured = true;
      document.getElementById('hqp-not-configured').classList.add('hidden');
      document.getElementById('hqp-configured').classList.remove('hidden');
      document.getElementById('hqp-status').textContent = data.connected ? 'Connected' : 'Disconnected';
      document.getElementById('hqp-status').className = data.connected ? 'success' : 'error';

      // Load profiles and pipeline
      loadHqpProfiles();
      loadHqpPipeline();
    } else {
      hqpConfigured = false;
      document.getElementById('hqp-not-configured').classList.remove('hidden');
      document.getElementById('hqp-configured').classList.add('hidden');
    }
  } catch (e) {
    document.getElementById('hqp-status').textContent = 'Error: ' + e.message;
  }
}

async function loadHqpProfiles() {
  try {
    // Get current config name from status
    const statusRes = await fetch('/hqp/status');
    const statusData = await statusRes.json();
    const currentConfigName = statusData.configName || '';

    const res = await fetch('/hqp/profiles');
    const data = await res.json();
    const select = document.getElementById('hqp-profile');
    select.innerHTML = '<option value="">-- Select Profile --</option>';
    if (data.profiles) {
      data.profiles.forEach(p => {
        const opt = document.createElement('option');
        opt.value = p.value || p;
        const title = p.title || p.value || p;
        opt.textContent = title;
        // Auto-select if title matches current config name
        // Note: This only works if your HQPlayer config title matches the profile name exactly
        if (currentConfigName && title.toLowerCase() === currentConfigName.toLowerCase()) {
          opt.selected = true;
        }
        select.appendChild(opt);
      });
    }
  } catch (e) {
    console.error('Failed to load profiles', e);
  }
}

async function loadHqpPipeline() {
  try {
    const res = await fetch('/hqp/pipeline');
    const data = await res.json();
    if (!data.enabled || !data.settings) return;

    // Populate filter/shaper dropdowns from settings object
    const s = data.settings;
    populateSelect('hqp-filter1x', s.filter1x?.options || [], s.filter1x?.selected?.value);
    populateSelect('hqp-filterNx', s.filterNx?.options || [], s.filterNx?.selected?.value);
    populateSelect('hqp-shaper', s.shaper?.options || [], s.shaper?.selected?.value);
  } catch (e) {
    console.error('Failed to load pipeline', e);
  }
}

function populateSelect(id, options, currentValue) {
  const select = document.getElementById(id);
  select.innerHTML = '';
  options.forEach(opt => {
    const o = document.createElement('option');
    o.value = opt.value || opt;
    o.textContent = opt.label || opt.value || opt;
    if ((opt.value || opt) === currentValue) o.selected = true;
    select.appendChild(o);
  });
}

async function loadProfile(profile) {
  if (!profile) return;
  const status = document.getElementById('hqp-status-msg');
  status.textContent = 'Loading profile...';
  status.className = 'status-msg';

  try {
    const res = await fetch('/hqp/profiles/load', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ profile })
    });
    const data = await res.json();
    if (res.ok) {
      status.textContent = 'Profile loaded (HQPlayer restarting)';
      status.className = 'status-msg success';
      setTimeout(loadHqpPipeline, 3000);
    } else {
      status.textContent = 'Error: ' + data.error;
      status.className = 'status-msg error';
    }
  } catch (e) {
    status.textContent = 'Error: ' + e.message;
    status.className = 'status-msg error';
  }
}

async function setPipeline(setting, value) {
  const status = document.getElementById('hqp-status-msg');
  try {
    const res = await fetch('/hqp/pipeline', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ setting, value })
    });
    if (res.ok) {
      status.textContent = setting + ' updated';
      status.className = 'status-msg success';
    } else {
      const data = await res.json();
      status.textContent = 'Error: ' + data.error;
      status.className = 'status-msg error';
    }
  } catch (e) {
    status.textContent = 'Error: ' + e.message;
    status.className = 'status-msg error';
  }
}

function toggleHqpConfig() {
  const form = document.getElementById('hqp-config-form');
  form.classList.toggle('hidden');
}

async function saveHqpConfig() {
  const host = document.getElementById('hqp-host').value;
  const port = document.getElementById('hqp-port').value || '8088';
  const username = document.getElementById('hqp-username').value;
  const password = document.getElementById('hqp-password').value;
  const status = document.getElementById('hqp-config-status');

  if (!host) {
    status.textContent = 'Host is required';
    status.className = 'status-msg error';
    return;
  }

  const config = { host, port: parseInt(port) };
  if (username) config.username = username;
  if (password) config.password = password;

  try {
    const res = await fetch('/hqp/configure', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(config)
    });
    if (res.ok) {
      status.textContent = 'Configured!';
      status.className = 'status-msg success';
      document.getElementById('hqp-config-form').classList.add('hidden');
      loadHqpStatus();
    } else {
      const data = await res.json();
      status.textContent = 'Error: ' + data.error;
      status.className = 'status-msg error';
    }
  } catch (e) {
    status.textContent = 'Error: ' + e.message;
    status.className = 'status-msg error';
  }
}

let discoveredHqp = [];

async function discoverHqp() {
  const status = document.getElementById('hqp-discover-status');
  const select = document.getElementById('hqp-discovered-select');
  const container = document.getElementById('hqp-discovered');

  status.textContent = 'Scanning...';
  container.classList.add('hidden');

  try {
    const res = await fetch('/hqp/discover?timeout=5000');
    const data = await res.json();

    if (data.services && data.services.length > 0) {
      discoveredHqp = data.services;
      select.innerHTML = '<option value="">-- Select --</option>';
      data.services.forEach((s, i) => {
        const opt = document.createElement('option');
        opt.value = i;
        const addr = s.addresses?.[0] || s.host;
        opt.textContent = s.name + ' (' + addr + ':' + s.port + ')';
        select.appendChild(opt);
      });
      container.classList.remove('hidden');
      status.textContent = 'Found ' + data.services.length;
      status.className = 'muted success';
    } else {
      status.textContent = 'No HQPlayer found';
      status.className = 'muted';
      container.classList.add('hidden');
    }
  } catch (e) {
    status.textContent = 'Error: ' + e.message;
    status.className = 'muted error';
  }
}

function selectDiscovered(index) {
  if (index === '' || !discoveredHqp[index]) return;
  const s = discoveredHqp[index];
  const addr = s.addresses?.[0] || s.host;
  document.getElementById('hqp-host').value = addr;
  document.getElementById('hqp-port').value = s.port || 8088;
}

// ===== Firmware Functions =====
function fetchFirmware() {
  const btn = document.getElementById('fetch-btn');
  const status = document.getElementById('firmware-status');
  btn.disabled = true;
  status.textContent = 'Fetching...';
  status.className = 'status-msg';

  fetch('/admin/fetch-firmware', { method: 'POST' })
    .then(r => r.json().then(d => ({ ok: r.ok, data: d })))
    .then(({ ok, data }) => {
      if (ok) {
        status.textContent = 'Downloaded v' + data.version;
        status.className = 'status-msg success';
        document.getElementById('fw-version').textContent = 'v' + data.version;
      } else {
        status.textContent = 'Error: ' + data.error;
        status.className = 'status-msg error';
      }
    })
    .catch(e => {
      status.textContent = 'Error: ' + e.message;
      status.className = 'status-msg error';
    })
    .finally(() => btn.disabled = false);
}

// ===== Init =====
// Set subnet placeholder for HQPlayer host
(function setSubnetPlaceholder() {
  const host = window.location.hostname;
  const match = host.match(/^(\\d+\\.\\d+\\.\\d+)\\./);
  if (match) {
    document.getElementById('hqp-host').placeholder = match[1] + '.x';
  } else {
    document.getElementById('hqp-host').placeholder = '192.168.1.x';
  }
})();

// Load HQPlayer status
loadHqpStatus();

// Load current firmware version
fetch('/firmware/version')
  .then(r => r.ok ? r.json() : Promise.reject('No firmware'))
  .then(d => document.getElementById('fw-version').textContent = 'v' + d.version)
  .catch(() => document.getElementById('fw-version').textContent = 'Not installed');

// Load bridge status
fetch('/admin/status.json').then(r => r.json()).then(d => {
  document.getElementById('status').textContent = JSON.stringify(d, null, 2);
});
</script>
</body>
</html>`);
  });

  // Shared requires for firmware handling
  const fs = require('fs');
  const path = require('path');
  const https = require('https');
  const FIRMWARE_DIR = process.env.FIRMWARE_DIR || path.join(__dirname, '..', '..', 'firmware');
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

  return router;
}

module.exports = { createKnobRoutes };
