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
  button { padding: 0.5em 1em; cursor: pointer; }
  .section { margin: 1.5em 0; padding: 1em; border: 1px solid #ddd; border-radius: 4px; }
  #firmware-status { margin-top: 0.5em; }
  .success { color: green; }
  .error { color: red; }
</style>
</head>
<body>
<h1>Unified Hi-Fi Control</h1>

<div class="section">
  <h2>Firmware</h2>
  <p>Current: <span id="fw-version">checking...</span></p>
  <button id="fetch-btn" onclick="fetchFirmware()">Fetch Latest from GitHub</button>
  <div id="firmware-status"></div>
</div>

<div class="section">
  <h2>Status</h2>
  <pre id="status"></pre>
</div>

<script>
// Load current firmware version
fetch('/firmware/version')
  .then(r => r.ok ? r.json() : Promise.reject('No firmware'))
  .then(d => document.getElementById('fw-version').textContent = 'v' + d.version)
  .catch(() => document.getElementById('fw-version').textContent = 'Not installed');

// Load bridge status
fetch('/admin/status.json').then(r => r.json()).then(d => {
  document.getElementById('status').textContent = JSON.stringify(d, null, 2);
});

// Fetch latest firmware
function fetchFirmware() {
  const btn = document.getElementById('fetch-btn');
  const status = document.getElementById('firmware-status');
  btn.disabled = true;
  status.textContent = 'Fetching...';
  status.className = '';

  fetch('/admin/fetch-firmware', { method: 'POST' })
    .then(r => r.json().then(d => ({ ok: r.ok, data: d })))
    .then(({ ok, data }) => {
      if (ok) {
        status.textContent = 'Downloaded v' + data.version;
        status.className = 'success';
        document.getElementById('fw-version').textContent = 'v' + data.version;
      } else {
        status.textContent = 'Error: ' + data.error;
        status.className = 'error';
      }
    })
    .catch(e => {
      status.textContent = 'Error: ' + e.message;
      status.className = 'error';
    })
    .finally(() => btn.disabled = false);
}
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
