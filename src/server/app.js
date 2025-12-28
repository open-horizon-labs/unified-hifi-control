const express = require('express');
const cors = require('cors');
const morgan = require('morgan');
const path = require('path');
const { createKnobRoutes } = require('../knobs/routes');

function createApp(opts = {}) {
  const { roon, hqp, knobs, logger } = opts;
  const log = logger || console;
  const app = express();

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
    res.json(roon.getZones());
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
    if (!hqp.isConfigured()) {
      return res.json({ enabled: false, profiles: [] });
    }
    try {
      const profiles = await hqp.fetchProfiles();
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
    if (!hqp.isConfigured()) {
      return res.status(400).json({ error: 'HQPlayer not configured' });
    }
    try {
      await hqp.loadProfile(profile);
      res.json({ ok: true, message: 'Profile loading, HQPlayer will restart' });
    } catch (err) {
      log.warn('HQP load profile failed', { error: err.message, profile });
      res.status(500).json({ error: err.message });
    }
  });

  app.get('/hqp/pipeline', async (req, res) => {
    if (!hqp.isConfigured()) {
      return res.json({ enabled: false });
    }
    try {
      const pipeline = await hqp.fetchPipeline();
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
    if (!hqp.isConfigured()) {
      return res.status(400).json({ error: 'HQPlayer not configured' });
    }
    try {
      await hqp.setPipelineSetting(setting, value);
      res.json({ ok: true });
    } catch (err) {
      log.warn('HQP set pipeline failed', { error: err.message, setting, value });
      res.status(500).json({ error: err.message });
    }
  });

  app.post('/hqp/configure', (req, res) => {
    const { host, port, username, password } = req.body;
    if (!host) {
      return res.status(400).json({ error: 'host required' });
    }
    hqp.configure({ host, port, username, password });
    log.info('HQPlayer configured', { host, port });
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
    const knobRoutes = createKnobRoutes({ roon, knobs, logger: log });
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
