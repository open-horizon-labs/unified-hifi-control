const { createRoonClient } = require('./roon/client');
const { createApp } = require('./server/app');
const { createLogger } = require('./lib/logger');

const PORT = process.env.PORT || 8088;
const log = createLogger('Main');

log.info('Starting Unified Hi-Fi Control');

// Create Roon client
const roon = createRoonClient({
  logger: createLogger('Roon'),
});

// Create HTTP server
const app = createApp({
  roon,
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
