#!/usr/bin/env node
/**
 * Minimal entry point for LMS plugin
 *
 * Stripped version that only includes:
 * - LMS client (for player status, artwork, control)
 * - Image processing (sharp for resizing)
 * - mDNS advertising (for client discovery)
 * - Single knob config (from LMS plugin settings)
 * - Firmware management
 * - Unified API endpoints (/zones, /now_playing, /control, etc.)
 *
 * No web UI, no Roon/UPnP/OpenHome/HQPlayer.
 */

const os = require('os');
const fs = require('fs');
const path = require('path');
const crypto = require('crypto');
const http = require('http');
const { LMSClient } = require('./lms/client');
const { createLogger } = require('./lib/logger');
const { convertToRgb565 } = require('./knobs/rgb565');
const { advertise } = require('./lib/mdns');
const { getDataDir } = require('./lib/paths');

const PORT = parseInt(process.env.PORT, 10) || 9199;
const CONFIG_DIR = getDataDir();
const FIRMWARE_DIR = process.env.FIRMWARE_DIR || path.join(CONFIG_DIR, 'firmware');
const KNOB_CONFIG_FILE = path.join(CONFIG_DIR, 'knob_config.json');
const log = createLogger('LMS-Plugin');

log.info('Starting Unified Hi-Fi Control (LMS Plugin Mode)');

// Create LMS client (connects to parent LMS instance)
// Always use 127.0.0.1 in LMS plugin mode - we're always on the same host
const lms = new LMSClient({
  host: '127.0.0.1',
  port: parseInt(process.env.LMS_PORT, 10) || 9000,
  logger: createLogger('LMS'),
});

// Single knob state (LMS plugin mode only supports one knob)
let knobState = {
  id: null,
  version: null,
  last_seen: null,
  status: { battery_level: null, battery_charging: null, zone_id: null, ip: null },
};

// Load knob config from file (written by LMS plugin Settings.pm)
function loadKnobConfig() {
  try {
    if (fs.existsSync(KNOB_CONFIG_FILE)) {
      return JSON.parse(fs.readFileSync(KNOB_CONFIG_FILE, 'utf8'));
    }
  } catch (e) {
    log.warn('Failed to load knob config', { error: e.message });
  }
  // Return defaults matching firmware defaults
  return {
    name: '',
    rotation_charging: 180,
    rotation_not_charging: 0,
    art_mode_charging: { enabled: true, timeout_sec: 60 },
    dim_charging: { enabled: true, timeout_sec: 120 },
    sleep_charging: { enabled: false, timeout_sec: 0 },
    art_mode_battery: { enabled: true, timeout_sec: 30 },
    dim_battery: { enabled: true, timeout_sec: 30 },
    sleep_battery: { enabled: true, timeout_sec: 60 },
  };
}

function computeConfigSha(config) {
  return crypto.createHash('sha256').update(JSON.stringify(config)).digest('hex').substring(0, 8);
}

// Extract knob info from request headers
function extractKnob(req) {
  const headerId = req.headers['x-knob-id'] || req.headers['x-device-id'];
  const version = req.headers['x-knob-version'] || req.headers['x-device-version'];
  if (!headerId && !version) return null;
  return { id: headerId, version };
}

// Validate firmware filename to prevent path traversal
function sanitizeFirmwareFilename(filename) {
  if (!filename || typeof filename !== 'string') return 'roon_knob.bin';
  // Remove any path separators and parent directory references
  const sanitized = path.basename(filename);
  // Only allow alphanumeric, dots, hyphens, underscores
  if (!/^[\w.-]+$/.test(sanitized)) return 'roon_knob.bin';
  return sanitized;
}

// HTTP API - compatible with knob/phone/watch clients
const server = http.createServer(async (req, res) => {
  res.setHeader('Access-Control-Allow-Origin', '*');
  res.setHeader('Access-Control-Allow-Methods', 'GET, POST, PUT, OPTIONS');
  res.setHeader('Access-Control-Allow-Headers', 'Content-Type, x-knob-id, x-device-id, x-knob-version, x-device-version');

  if (req.method === 'OPTIONS') {
    res.writeHead(204);
    res.end();
    return;
  }

  const url = new URL(req.url, `http://localhost:${PORT}`);
  const pathname = url.pathname;

  try {
    // Root - minimal status page
    if (pathname === '/') {
      res.setHeader('Content-Type', 'text/html');
      const players = await lms.getPlayers().catch(() => []);
      const knobInfo = knobState.id ? `
        <h2>Knob</h2>
        <ul>
          <li><strong>ID:</strong> ${knobState.id}</li>
          <li><strong>Version:</strong> ${knobState.version || 'unknown'}</li>
          <li><strong>Last seen:</strong> ${knobState.last_seen || 'never'}</li>
          <li><strong>Battery:</strong> ${knobState.status.battery_level ?? '?'}% ${knobState.status.battery_charging ? '(charging)' : ''}</li>
        </ul>` : '<p>No knob connected yet.</p>';

      res.end(`<!DOCTYPE html>
<html><head><title>Unified Hi-Fi Control (LMS)</title>
<style>body{font-family:system-ui,sans-serif;max-width:600px;margin:2em auto;padding:0 1em}
h1{color:#333}h2{color:#666;border-bottom:1px solid #ddd}ul{list-style:none;padding:0}
li{padding:0.3em 0}a{color:#0066cc}</style></head>
<body>
<h1>Unified Hi-Fi Control</h1>
<p>LMS Plugin Mode</p>
<h2>Zones (${players.length})</h2>
<ul>${players.map(p => `<li><strong>${p.name}</strong> - ${p.model || 'Unknown'}</li>`).join('') || '<li>No players found</li>'}</ul>
${knobInfo}
<h2>API Endpoints</h2>
<ul>
  <li><a href="/health">/health</a> - Health check</li>
  <li><a href="/zones">/zones</a> - List zones</li>
  <li><a href="/api/knobs">/api/knobs</a> - Knob status</li>
</ul>
<h2>Firmware Updates</h2>
<p>To update knob firmware, visit <a href="https://roon-knob.muness.com/" target="_blank">roon-knob.muness.com</a></p>
</body></html>`);
      return;
    }

    // Health check
    if (pathname === '/health' || pathname === '/status') {
      res.setHeader('Content-Type', 'application/json');
      res.end(JSON.stringify({
        status: 'ok',
        mode: 'lms-plugin',
        service: 'unified-hifi-control',
        version: process.env.APP_VERSION || 'dev',
      }));
      return;
    }

    // GET /zones - list all LMS players as zones
    if (pathname === '/zones') {
      res.setHeader('Content-Type', 'application/json');
      const players = await lms.getPlayers();
      const zones = players.map(p => ({
        zone_id: `lms:${p.playerid}`,
        zone_name: p.name,
        output_name: p.model || 'Squeezebox',
        device_name: p.ip,
      }));
      res.end(JSON.stringify({ zones }));
      return;
    }

    // GET /now_playing - get current playback for a zone
    if (pathname === '/now_playing') {
      res.setHeader('Content-Type', 'application/json');
      const zoneId = url.searchParams.get('zone_id');
      const knob = extractKnob(req);

      if (!zoneId) {
        res.statusCode = 400;
        res.end(JSON.stringify({ error: 'zone_id required' }));
        return;
      }

      // Update knob status (single knob mode)
      if (knob?.id) {
        knobState.id = knob.id;
        knobState.version = knob.version || knobState.version;
        knobState.last_seen = new Date().toISOString();
        knobState.status.zone_id = zoneId;
        knobState.status.ip = req.socket.remoteAddress;
        const batteryLevel = url.searchParams.get('battery_level');
        const batteryCharging = url.searchParams.get('battery_charging');
        if (batteryLevel) knobState.status.battery_level = parseInt(batteryLevel, 10);
        if (batteryCharging !== null) knobState.status.battery_charging = batteryCharging === '1' || batteryCharging === 'true';
      }

      const playerId = zoneId.replace(/^lms:/, '');
      const status = await lms.getPlayerStatus(playerId);

      if (!status) {
        res.statusCode = 404;
        res.end(JSON.stringify({ error: 'Zone not found' }));
        return;
      }

      const response = {
        zone_id: zoneId,
        line1: status.title || 'Stopped',
        line2: status.artist || '',
        line3: status.album || '',
        is_playing: status.mode === 'play',
        volume: status.volume,
        volume_type: 'number',
        image_key: status.artwork_track_id || status.coverid,
        image_url: `/now_playing/image?zone_id=${encodeURIComponent(zoneId)}`,
      };

      // Include zones list and config_sha for knob
      const players = await lms.getPlayers();
      response.zones = players.map(p => ({
        zone_id: `lms:${p.playerid}`,
        zone_name: p.name,
      }));

      if (knob?.id) {
        const config = loadKnobConfig();
        response.config_sha = computeConfigSha(config);
      }

      res.end(JSON.stringify(response));
      return;
    }

    // GET /now_playing/image - album artwork with optional resizing
    if (pathname === '/now_playing/image') {
      const zoneId = url.searchParams.get('zone_id');
      const width = parseInt(url.searchParams.get('width'), 10) || 360;
      const height = parseInt(url.searchParams.get('height'), 10) || 360;
      const format = url.searchParams.get('format');

      if (!zoneId) {
        res.statusCode = 400;
        res.end(JSON.stringify({ error: 'zone_id required' }));
        return;
      }

      const playerId = zoneId.replace(/^lms:/, '');
      const status = await lms.getPlayerStatus(playerId);
      const coverId = status?.artwork_track_id || status?.coverid;

      if (!coverId) {
        res.setHeader('Content-Type', 'image/svg+xml');
        res.end(`<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}">
          <rect width="100%" height="100%" fill="#333"/>
          <text x="50%" y="50%" fill="#888" text-anchor="middle" dy=".3em" font-family="sans-serif" font-size="24">No Image</text>
        </svg>`);
        return;
      }

      try {
        // Let LMS do the resizing via URL format (cover_WxH.jpg)
        const { contentType, body } = await lms.getArtwork(coverId, { width, height });

        if (format === 'rgb565') {
          // Use pure JS converter (no native deps for pkg bundle)
          const result = convertToRgb565(body, width, height);

          res.setHeader('Content-Type', 'application/octet-stream');
          res.setHeader('X-Image-Width', result.width.toString());
          res.setHeader('X-Image-Height', result.height.toString());
          res.setHeader('X-Image-Format', 'rgb565');
          res.end(result.data);
          return;
        }

        // Return JPEG directly (already resized by LMS)
        res.setHeader('Content-Type', contentType || 'image/jpeg');
        res.end(body);
        return;

      } catch (err) {
        log.warn('Artwork fetch failed', { error: err.message });
        res.setHeader('Content-Type', 'image/svg+xml');
        res.end(`<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}">
          <rect width="100%" height="100%" fill="#333"/>
        </svg>`);
        return;
      }
    }

    // POST /control - transport controls
    if (pathname === '/control' && req.method === 'POST') {
      res.setHeader('Content-Type', 'application/json');
      const body = await readBody(req);
      const { zone_id, action, value } = JSON.parse(body);

      if (!zone_id || !action) {
        res.statusCode = 400;
        res.end(JSON.stringify({ error: 'zone_id and action required' }));
        return;
      }

      const playerId = zone_id.replace(/^lms:/, '');
      log.info('Control command', { playerId, action, value });

      switch (action) {
        case 'play':
          await lms.command(playerId, ['play']);
          break;
        case 'pause':
          await lms.command(playerId, ['pause', '1']);
          break;
        case 'play_pause':
          await lms.command(playerId, ['pause']);
          break;
        case 'stop':
          await lms.command(playerId, ['stop']);
          break;
        case 'next':
          await lms.command(playerId, ['playlist', 'index', '+1']);
          break;
        case 'previous':
          await lms.command(playerId, ['playlist', 'index', '-1']);
          break;
        case 'volume':
          await lms.command(playerId, ['mixer', 'volume', String(value)]);
          break;
        case 'vol_rel': {
          const delta = value > 0 ? `+${value}` : String(value);
          await lms.command(playerId, ['mixer', 'volume', delta]);
          break;
        }
        case 'mute':
          await lms.command(playerId, ['mixer', 'muting', value ? '1' : '0']);
          break;
        default:
          res.statusCode = 400;
          res.end(JSON.stringify({ error: `Unknown action: ${action}` }));
          return;
      }

      res.end(JSON.stringify({ status: 'ok' }));
      return;
    }

    // GET /config/:knob_id - Get knob configuration (single knob mode - ignores ID)
    const configMatch = pathname.match(/^\/config\/([^/]+)$/);
    if (configMatch && req.method === 'GET') {
      const knobId = decodeURIComponent(configMatch[1]);
      const version = req.headers['x-knob-version'];

      // Update state if this is a new knob
      if (knobId && !knobState.id) {
        knobState.id = knobId;
        knobState.version = version;
        knobState.last_seen = new Date().toISOString();
      }

      const config = loadKnobConfig();
      res.setHeader('Content-Type', 'application/json');
      res.end(JSON.stringify({
        config: { knob_id: knobId, ...config },
        config_sha: computeConfigSha(config),
      }));
      return;
    }

    // PUT /config/:knob_id - Update knob configuration (read-only in LMS mode, config via LMS Settings)
    if (configMatch && req.method === 'PUT') {
      res.setHeader('Content-Type', 'application/json');
      res.statusCode = 400;
      res.end(JSON.stringify({
        error: 'Config is managed via LMS plugin settings. Update configuration in the LMS web interface.',
      }));
      return;
    }

    // GET /api/knobs - List known knob (single knob mode)
    if (pathname === '/api/knobs') {
      res.setHeader('Content-Type', 'application/json');
      const knobsList = [];
      if (knobState.id) {
        const config = loadKnobConfig();
        knobsList.push({
          knob_id: knobState.id,
          name: config.name || '',
          last_seen: knobState.last_seen,
          version: knobState.version,
          status: knobState.status,
        });
      }
      res.end(JSON.stringify({ knobs: knobsList }));
      return;
    }

    // GET /firmware/version - Check firmware version
    if (pathname === '/firmware/version') {
      const knob = extractKnob(req);
      log.info('Firmware version check', { knob, ip: req.socket.remoteAddress });

      if (!fs.existsSync(FIRMWARE_DIR)) {
        res.statusCode = 404;
        res.setHeader('Content-Type', 'application/json');
        res.end(JSON.stringify({ error: 'No firmware available' }));
        return;
      }

      const versionFile = path.join(FIRMWARE_DIR, 'version.json');
      if (!fs.existsSync(versionFile)) {
        res.statusCode = 404;
        res.setHeader('Content-Type', 'application/json');
        res.end(JSON.stringify({ error: 'No firmware version available' }));
        return;
      }

      const versionData = JSON.parse(fs.readFileSync(versionFile, 'utf8'));
      const firmwareFile = sanitizeFirmwareFilename(versionData.file);
      const firmwarePath = path.join(FIRMWARE_DIR, firmwareFile);

      if (!fs.existsSync(firmwarePath)) {
        res.statusCode = 404;
        res.setHeader('Content-Type', 'application/json');
        res.end(JSON.stringify({ error: 'Firmware file not found' }));
        return;
      }

      const stats = fs.statSync(firmwarePath);
      res.setHeader('Content-Type', 'application/json');
      res.end(JSON.stringify({
        version: versionData.version,
        size: stats.size,
        file: firmwareFile,
      }));
      return;
    }

    // GET /firmware/download - Download firmware binary
    if (pathname === '/firmware/download') {
      const knob = extractKnob(req);
      log.info('Firmware download requested', { knob, ip: req.socket.remoteAddress });

      if (!fs.existsSync(FIRMWARE_DIR)) {
        res.statusCode = 404;
        res.setHeader('Content-Type', 'application/json');
        res.end(JSON.stringify({ error: 'No firmware available' }));
        return;
      }

      let firmwareFile = 'roon_knob.bin';
      const versionFile = path.join(FIRMWARE_DIR, 'version.json');
      if (fs.existsSync(versionFile)) {
        const versionData = JSON.parse(fs.readFileSync(versionFile, 'utf8'));
        firmwareFile = sanitizeFirmwareFilename(versionData.file);
      }

      const firmwarePath = path.join(FIRMWARE_DIR, firmwareFile);
      if (!fs.existsSync(firmwarePath)) {
        res.statusCode = 404;
        res.setHeader('Content-Type', 'application/json');
        res.end(JSON.stringify({ error: 'Firmware file not found' }));
        return;
      }

      const stats = fs.statSync(firmwarePath);
      res.setHeader('Content-Type', 'application/octet-stream');
      res.setHeader('Content-Length', stats.size);
      res.setHeader('Content-Disposition', `attachment; filename="${firmwareFile}"`);
      fs.createReadStream(firmwarePath).pipe(res);
      return;
    }

    // Not found
    res.setHeader('Content-Type', 'application/json');
    res.statusCode = 404;
    res.end(JSON.stringify({ error: 'Not found' }));

  } catch (err) {
    log.error('Request error', { path: pathname, error: err.message });
    res.setHeader('Content-Type', 'application/json');
    res.statusCode = 500;
    res.end(JSON.stringify({ error: err.message }));
  }
});

function readBody(req) {
  return new Promise((resolve, reject) => {
    let body = '';
    req.on('data', chunk => body += chunk);
    req.on('end', () => resolve(body));
    req.on('error', reject);
  });
}

function getLocalIp() {
  const interfaces = os.networkInterfaces();
  for (const name of Object.keys(interfaces)) {
    for (const iface of interfaces[name]) {
      if (iface.family === 'IPv4' && !iface.internal) {
        return iface.address;
      }
    }
  }
  return 'localhost';
}

const localIp = getLocalIp();
let mdnsService;

server.on('error', (err) => {
  if (err.code === 'EADDRINUSE') {
    log.error(`Port ${PORT} is already in use. Another instance may be running.`, { port: PORT, error: err.message });
  } else {
    log.error('Server error', { error: err.message, code: err.code });
  }
  process.exit(1);
});

server.listen(PORT, async () => {
  log.info(`LMS plugin API listening on port ${PORT}`);

  // Connect to parent LMS instance
  try {
    await lms.start();
    log.info('Connected to LMS', { host: lms.host, port: lms.port, players: lms.players.size });
  } catch (err) {
    log.error('Failed to connect to LMS', { error: err.message });
  }

  mdnsService = advertise(PORT, {
    name: 'Unified Hi-Fi Control (LMS)',
    base: `http://${localIp}:${PORT}`,
  }, createLogger('mDNS'));
});

process.on('SIGTERM', () => {
  log.info('Shutting down...');
  if (mdnsService) mdnsService.stop();
  server.close();
  process.exit(0);
});

process.on('unhandledRejection', (err) => {
  log.error('Unhandled rejection', { error: err.message });
});
