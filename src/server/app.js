const express = require('express');
const cors = require('cors');
const morgan = require('morgan');
const path = require('path');
const { createKnobRoutes } = require('../knobs/routes');
const { loadAppSettings, saveAppSettings } = require('../lib/settings');

function createApp(opts = {}) {
  const { bus, roon, hqp, hqpInstances, hqpService, lms, knobs, adapterFactory, logger } = opts;
  const log = logger || console;
  const app = express();

  // Trust proxy for correct req.ip behind reverse proxy (Docker, nginx, traefik)
  app.set('trust proxy', true);

  app.use(cors());
  app.use(express.json());
  app.use(morgan('combined'));

  // Static files
  app.use('/ui', express.static(path.join(__dirname, '..', 'ui')));

  // Health check
  app.get('/status', (req, res) => {
    res.json({
      service: 'unified-hifi-control',
      version: process.env.APP_VERSION || 'dev',
      uptime: process.uptime(),
    });
  });

  // Roon routes
  app.get('/roon/status', (req, res) => {
    res.json(roon.getStatus());
  });

  app.get('/roon/zones', (req, res) => {
    const debug = req.query.debug === 'true';
    res.json(roon.getZones({ debug }));
  });

  app.get('/roon/now_playing', (req, res) => {
    const zone_id = req.query.zone_id;
    if (!zone_id) {
      return res.status(400).json({ error: 'zone_id required' });
    }
    const np = roon.getNowPlaying(zone_id);
    if (!np) {
      return res.status(404).json({ error: 'Zone not found or not connected' });
    }
    res.json({
      ...np,
      zones: roon.getZones(),
    });
  });

  app.get('/roon/image', async (req, res) => {
    const { image_key, width, height, format } = req.query;
    if (!image_key) {
      return res.status(400).json({ error: 'image_key required' });
    }
    try {
      const { contentType, body } = await roon.getImage(image_key, {
        width: width || 300,
        height: height || 300,
        format: format || 'image/jpeg',
      });
      res.set('Content-Type', contentType);
      res.send(body);
    } catch (err) {
      log.warn('Image fetch failed', { error: err.message });
      res.status(500).json({ error: err.message });
    }
  });

  app.post('/roon/control', async (req, res) => {
    const { zone_id, action, value } = req.body;
    if (!zone_id || !action) {
      return res.status(400).json({ error: 'zone_id and action required' });
    }
    try {
      await roon.control(zone_id, action, value);
      res.json({ ok: true });
    } catch (err) {
      log.warn('Control failed', { error: err.message, zone_id, action });
      res.status(500).json({ error: err.message });
    }
  });

  // HQPlayer routes
  app.get('/hqp/status', async (req, res) => {
    try {
      const status = await hqp.getStatus();
      res.json(status);
    } catch (err) {
      log.warn('HQP status failed', { error: err.message });
      res.json({ enabled: false, error: err.message });
    }
  });

  app.get('/hqp/profiles', async (req, res) => {
    // Get first instance (backward compat)
    const firstInstance = hqpInstances.size > 0 ? Array.from(hqpInstances.values())[0] : null;
    if (!firstInstance || !firstInstance.client) {
      return res.json({ enabled: false, profiles: [], message: 'HQPlayer not configured' });
    }

    if (!firstInstance.client.hasWebCredentials()) {
      return res.json({ enabled: false, profiles: [], message: 'Web credentials required for profiles' });
    }
    try {
      const profiles = await firstInstance.client.fetchProfiles();
      res.json({ enabled: true, profiles });
    } catch (err) {
      log.warn('HQP profiles failed', { error: err.message });
      res.status(500).json({ error: err.message });
    }
  });

  app.post('/hqp/profiles/load', async (req, res) => {
    const { profile } = req.body;
    if (!profile) {
      return res.status(400).json({ error: 'profile required' });
    }

    // Get first instance (backward compat)
    const firstInstance = hqpInstances.size > 0 ? Array.from(hqpInstances.values())[0] : null;
    if (!firstInstance || !firstInstance.client) {
      return res.status(400).json({ error: 'HQPlayer not configured' });
    }

    if (!firstInstance.client.hasWebCredentials()) {
      return res.status(400).json({ error: 'Web credentials required for profile loading' });
    }
    try {
      await firstInstance.client.loadProfile(profile);
      res.json({ ok: true, message: 'Profile loading, HQPlayer will restart' });
    } catch (err) {
      log.warn('HQP load profile failed', { error: err.message, profile });
      res.status(500).json({ error: err.message });
    }
  });

  app.get('/hqp/pipeline', async (req, res) => {
    // Get first instance (backward compat)
    const firstInstance = hqpInstances.size > 0 ? Array.from(hqpInstances.values())[0] : null;
    if (!firstInstance || !firstInstance.client || !firstInstance.client.isConfigured()) {
      return res.json({ enabled: false });
    }
    try {
      const pipeline = await firstInstance.client.fetchPipeline();
      res.json({ enabled: true, ...pipeline });
    } catch (err) {
      log.warn('HQP pipeline failed', { error: err.message });
      res.status(500).json({ error: err.message });
    }
  });

  app.post('/hqp/pipeline', async (req, res) => {
    const { setting, value } = req.body;
    if (!setting || value === undefined) {
      return res.status(400).json({ error: 'setting and value required' });
    }
    const validSettings = ['mode', 'samplerate', 'filter1x', 'filterNx', 'shaper', 'dither'];
    if (!validSettings.includes(setting)) {
      return res.status(400).json({ error: `Invalid setting. Valid: ${validSettings.join(', ')}` });
    }

    // Get first instance (backward compat)
    const firstInstance = hqpInstances.size > 0 ? Array.from(hqpInstances.values())[0] : null;
    if (!firstInstance || !firstInstance.client || !firstInstance.client.isConfigured()) {
      return res.status(400).json({ error: 'HQPlayer not configured' });
    }

    try {
      await firstInstance.client.setPipelineSetting(setting, value);
      res.json({ ok: true });
    } catch (err) {
      log.warn('HQP set pipeline failed', { error: err.message, setting, value });
      res.status(500).json({ error: err.message });
    }
  });

  app.post('/hqp/configure', async (req, res) => {
    const { host, port, username, password } = req.body;
    if (!host) {
      return res.status(400).json({ error: 'host required' });
    }

    const fs = require('fs');
    const path = require('path');
    const { HQPClient } = require('../hqplayer/client');
    const configDir = process.env.CONFIG_DIR || path.join(__dirname, '..', '..', 'data');
    const configFile = path.join(configDir, 'hqp-config.json');

    // Use host as instance name
    const instanceName = host;

    // Create and validate the client BEFORE persisting
    const client = new HQPClient({
      host,
      port: port || 8088,
      username: username || '',
      password: password || '',
      logger: log,
    });

    try {
      // Validate connection by checking status
      const status = await client.getStatus();
      if (!status.enabled && status.error) {
        return res.status(400).json({
          error: `Cannot reach HQPlayer at ${host}:${port || 8088}`,
          details: status.error
        });
      }
    } catch (err) {
      return res.status(400).json({
        error: `Cannot reach HQPlayer at ${host}:${port || 8088}. Check host/port.`,
        details: err.message
      });
    }

    // Load existing config
    let configs = [];
    try {
      if (fs.existsSync(configFile)) {
        const data = JSON.parse(fs.readFileSync(configFile, 'utf8'));
        configs = Array.isArray(data) ? data : [data];
      }
    } catch (err) {
      log.warn('Failed to load HQP config', { error: err.message });
    }

    // Update or add instance config
    const instanceConfig = {
      name: instanceName,
      host,
      port: port || 8088,
      username: username || '',
      password: password || '',
    };

    const existingIndex = configs.findIndex(c => c.name === instanceName || c.host === host);
    if (existingIndex >= 0) {
      configs[existingIndex] = instanceConfig;
    } else {
      configs.push(instanceConfig);
    }

    // Write config file for persistence
    try {
      if (!fs.existsSync(configDir)) {
        fs.mkdirSync(configDir, { recursive: true });
      }
      fs.writeFileSync(configFile, JSON.stringify(configs, null, 2));
    } catch (err) {
      log.error('Failed to save HQP config', { error: err.message });
      return res.status(500).json({ error: 'Failed to save configuration: ' + err.message });
    }

    // Hot reload: Update runtime state
    hqpInstances.set(instanceName, { client });

    // If HQP service exists, it will automatically see the new instance
    // (hqpInstances Map is passed by reference)

    log.info('HQPlayer configured (hot-reload)', { host, port, instance: instanceName });
    res.json({ ok: true, instance: instanceName });
  });

  // HQP zone linking routes - DSP service enrichment
  app.get('/hqp/zones/links', (req, res) => {
    if (!hqpService) {
      return res.status(503).json({ error: 'HQP service not available' });
    }
    res.json({ links: hqpService.getLinks() });
  });

  app.post('/hqp/zones/link', (req, res) => {
    if (!hqpService) {
      return res.status(503).json({ error: 'HQP service not available' });
    }

    const { zone_id, instance } = req.body;
    if (!zone_id || !instance) {
      return res.status(400).json({ error: 'zone_id and instance required' });
    }

    try {
      hqpService.linkZone(zone_id, instance);

      // Save to settings
      const settings = loadAppSettings();
      settings.hqp = settings.hqp || {};
      settings.hqp.zoneLinks = hqpService.saveLinks();
      saveAppSettings(settings);

      log.info('Zone linked to HQP', { zone_id, instance });
      res.json({ ok: true, zone_id, instance });
    } catch (err) {
      log.warn('Zone link failed', { error: err.message, zone_id, instance });
      res.status(400).json({ error: err.message });
    }
  });

  app.post('/hqp/zones/unlink', (req, res) => {
    if (!hqpService) {
      return res.status(503).json({ error: 'HQP service not available' });
    }

    const { zone_id } = req.body;
    if (!zone_id) {
      return res.status(400).json({ error: 'zone_id required' });
    }

    const wasLinked = hqpService.unlinkZone(zone_id);
    if (wasLinked) {
      // Save to settings
      const settings = loadAppSettings();
      settings.hqp = settings.hqp || {};
      settings.hqp.zoneLinks = hqpService.saveLinks();
      saveAppSettings(settings);

      log.info('Zone unlinked from HQP', { zone_id });
    }
    res.json({ ok: true, zone_id, was_linked: wasLinked });
  });

  // HQP instances route
  app.get('/hqp/instances', (req, res) => {
    if (!hqpInstances || hqpInstances.size === 0) {
      return res.json({ instances: [] });
    }
    const instances = Array.from(hqpInstances.entries()).map(([name, { client }]) => ({
      name,
      host: client.host,
      port: client.port,
      configured: client.isConfigured(),
    }));
    res.json({ instances });
  });

  // Lyrion routes
  app.get('/lms/status', (req, res) => {
    if (!lms) {
      return res.json({ enabled: false, error: 'Lyrion not available' });
    }
    res.json({
      enabled: true,
      ...lms.getStatus(),
    });
  });

  app.post('/lms/configure', (req, res) => {
    const { host, port, username, password } = req.body;
    if (!host) {
      return res.status(400).json({ error: 'host required' });
    }
    if (!lms) {
      return res.status(400).json({ error: 'Lyrion not available' });
    }
    lms.configure({ host, port, username, password });
    log.info('Lyrion configured', { host, port, hasAuth: !!username });

    // Start Lyrion if not already started
    if (!lms.connected && lms.isConfigured()) {
      lms.start().catch(err => {
        log.warn('Failed to start Lyrion after configure', { error: err.message });
      });
    }

    res.json({ ok: true });
  });

  // Combined status for dashboard
  app.get('/api/status', async (req, res) => {
    const hqpStatus = await hqp.getStatus().catch(() => ({ enabled: false }));
    res.json({
      roon: roon.getStatus(),
      hqplayer: hqpStatus,
    });
  });

  // Knob-compatible routes (mounted at root for firmware compatibility)
  if (knobs) {
    const knobRoutes = createKnobRoutes({ bus, roon, knobs, adapterFactory, logger: log });
    app.use('/', knobRoutes);
  }

  // Error handler
  app.use((err, req, res, _next) => {
    log.error('Unhandled error', { error: err.message, stack: err.stack });
    res.status(500).json({ error: 'Internal server error' });
  });

  return app;
}

module.exports = { createApp };
