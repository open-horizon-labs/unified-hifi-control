const express = require('express');
const cors = require('cors');
const morgan = require('morgan');
const path = require('path');

function createApp(opts = {}) {
  const { roon, logger } = opts;
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

  // Placeholder for HQPlayer routes (Phase 3)
  app.get('/hqp/status', (req, res) => {
    res.json({ enabled: false, message: 'HQPlayer integration not configured' });
  });

  // Combined status for dashboard
  app.get('/api/status', (req, res) => {
    res.json({
      roon: roon.getStatus(),
      hqplayer: { enabled: false },
    });
  });

  // Error handler
  app.use((err, req, res, _next) => {
    log.error('Unhandled error', { error: err.message, stack: err.stack });
    res.status(500).json({ error: 'Internal server error' });
  });

  return app;
}

module.exports = { createApp };
