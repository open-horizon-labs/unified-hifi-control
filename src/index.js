const { createRoonClient } = require('./roon/client');
const { HQPClient } = require('./hqplayer/client');
const { createApp } = require('./server/app');
const { createLogger } = require('./lib/logger');

const PORT = process.env.PORT || 8088;
const log = createLogger('Main');

log.info('Starting Unified Hi-Fi Control');

// Create Roon client
const roon = createRoonClient({
  logger: createLogger('Roon'),
});

// Create HQPlayer client (unconfigured initially, configured via API or env vars)
const hqp = new HQPClient({
  logger: createLogger('HQP'),
});

// Pre-configure HQPlayer if env vars set
if (process.env.HQP_HOST) {
  hqp.configure({
    host: process.env.HQP_HOST,
    port: process.env.HQP_PORT || 8088,
    username: process.env.HQP_USER,
    password: process.env.HQP_PASS,
  });
  log.info('HQPlayer pre-configured from environment', { host: process.env.HQP_HOST });
}

// Create HTTP server
const app = createApp({
  roon,
  hqp,
  logger: createLogger('Server'),
});

// Start services
roon.start();

app.listen(PORT, () => {
  log.info(`HTTP server listening on port ${PORT}`);
  log.info('Waiting for Roon Core authorization...');
});

// Graceful shutdown
process.on('SIGTERM', () => {
  log.info('Shutting down...');
  process.exit(0);
});

process.on('unhandledRejection', (err) => {
  log.error('Unhandled rejection', { error: err.message, stack: err.stack });
});
