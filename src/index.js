const os = require('os');
const { createRoonClient } = require('./roon/client');
const { HQPClient } = require('./hqplayer/client');
const { createMqttService } = require('./mqtt');
const { createApp } = require('./server/app');
const { createLogger } = require('./lib/logger');
const { advertise } = require('./lib/mdns');
const { createKnobsStore } = require('./knobs/store');
const { initBus } = require('./bus');
const { RoonAdapter } = require('./bus/adapters/roon');
const { initDebug } = require('./bus/debug');

const PORT = process.env.PORT || 8088;
const log = createLogger('Main');

log.info('Starting Unified Hi-Fi Control');

// Get local IP for base URL
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
const baseUrl = `http://${localIp}:${PORT}`;

// Create Roon client
const roon = createRoonClient({
  logger: createLogger('Roon'),
  base_url: baseUrl,
});

// Create HQPlayer client (unconfigured initially, configured via API or env vars)
const hqp = new HQPClient({
  logger: createLogger('HQP'),
});

// Initialize bus and register backends
const bus = initBus({
  logger: createLogger('Bus'),
});

const roonAdapter = new RoonAdapter(roon);
bus.registerBackend('roon', roonAdapter);

// Initialize debug consumer
initDebug({ logger: createLogger('BusDebug') });

// Create knobs store for ESP32 knob configuration
const knobs = createKnobsStore({
  logger: createLogger('Knobs'),
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

// Create MQTT service (opt-in via MQTT_BROKER env var)
const mqttService = createMqttService({
  hqp,
  logger: createLogger('MQTT'),
});

// Create HTTP server
const app = createApp({
  roon,    // Keep for backward compat during Phase 2 testing
  hqp,
  knobs,
  logger: createLogger('Server'),
});

// Start services
bus.start();  // Starts all registered backends (including roon)
mqttService.connect();

let mdnsService;

app.listen(PORT, () => {
  log.info(`HTTP server listening on port ${PORT}`);
  log.info('Waiting for Roon Core authorization...');

  // Advertise via mDNS for knob discovery
  mdnsService = advertise(PORT, {
    name: 'Unified Hi-Fi Control',
    base: `http://${localIp}:${PORT}`,
  }, createLogger('mDNS'));
});

// Graceful shutdown
process.on('SIGTERM', async () => {
  log.info('Shutting down...');
  if (mdnsService) mdnsService.stop();
  mqttService.disconnect();
  await bus.stop();
  process.exit(0);
});

process.on('unhandledRejection', (err) => {
  log.error('Unhandled rejection', { error: err.message, stack: err.stack });
});
