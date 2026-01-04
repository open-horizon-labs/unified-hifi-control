const os = require('os');
const { createRoonClient } = require('./roon/client');
const { createUPnPClient } = require('./upnp/client');
const { createOpenHomeClient } = require('./openhome/client');
const { HQPClient } = require('./hqplayer/client');
const { LMSClient } = require('./lms/client');
const { createMqttService } = require('./mqtt');
const { createApp } = require('./server/app');
const { createLogger } = require('./lib/logger');
const { loadAppSettings } = require('./lib/settings');
const { advertise } = require('./lib/mdns');
const { createKnobsStore } = require('./knobs/store');
const { createBus } = require('./bus');
const { RoonAdapter } = require('./bus/adapters/roon');
const { UPnPAdapter } = require('./bus/adapters/upnp');
const { OpenHomeAdapter } = require('./bus/adapters/openhome');
const { LMSAdapter } = require('./bus/adapters/lms');
const busDebug = require('./bus/debug');

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

// Load settings for adapter configuration
const appSettings = loadAppSettings();
const adapterConfig = appSettings.adapters || { roon: true, upnp: false, openhome: false, lms: false };

log.info('Adapter configuration', adapterConfig);

// Create bus first so we can reference it in callbacks
const bus = createBus({ logger: createLogger('Bus') });

// Create Lyrion client (shared for config API and adapter)
const lms = new LMSClient({
  host: process.env.LMS_HOST,
  port: parseInt(process.env.LMS_PORT, 10) || 9000,
  username: process.env.LMS_USERNAME,
  password: process.env.LMS_PASSWORD,
  logger: createLogger('Lyrion'),
  onZonesChanged: () => bus.refreshZones('lms'),
});

// Adapter factory - creates adapters on demand for dynamic enable/disable
const adapterFactory = {
  createRoon() {
    const client = createRoonClient({
      logger: createLogger('Roon'),
      base_url: baseUrl,
      onZonesChanged: () => bus.refreshZones('roon'),
    });
    return new RoonAdapter(client);
  },
  createUPnP() {
    const client = createUPnPClient({
      logger: createLogger('UPnP'),
    });
    return new UPnPAdapter(client, {
      onZonesChanged: () => bus.refreshZones('upnp'),
    });
  },
  createOpenHome() {
    const client = createOpenHomeClient({
      logger: createLogger('OpenHome'),
      onZonesChanged: () => bus.refreshZones('openhome'),
    });
    return new OpenHomeAdapter(client, {
      onZonesChanged: () => bus.refreshZones('openhome'),
    });
  },
  createLMS() {
    // Use shared lms client so config API works
    return new LMSAdapter(lms, {
      onZonesChanged: () => bus.refreshZones('lms'),
    });
  },
};

// Conditionally create and register adapters based on settings
let roon = null;
if (adapterConfig.roon !== false) {
  roon = adapterFactory.createRoon();
  bus.registerBackend('roon', roon);
  log.info('Roon adapter enabled');
}

if (adapterConfig.upnp) {
  bus.registerBackend('upnp', adapterFactory.createUPnP());
  log.info('UPnP adapter enabled');
}

if (adapterConfig.openhome) {
  bus.registerBackend('openhome', adapterFactory.createOpenHome());
  log.info('OpenHome adapter enabled');
}

// Enable Lyrion if configured via settings or env var
if (adapterConfig.lms || process.env.LMS_HOST) {
  bus.registerBackend('lms', adapterFactory.createLMS());
  log.info('Lyrion adapter enabled', { host: process.env.LMS_HOST || 'via settings' });
}

// Create HQPlayer client (unconfigured initially, configured via API or env vars)
const hqp = new HQPClient({
  logger: createLogger('HQP'),
});

// Initialize debug consumer
busDebug.init(bus);

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
  bus,
  roon,    // Keep for backward compat during Phase 2 testing
  hqp,
  lms,     // Lyrion client for configuration API
  knobs,
  adapterFactory,
  logger: createLogger('Server'),
});

// Start services
bus.start();  // Starts all registered backends (including roon)
mqttService.connect();

let mdnsService;

app.listen(PORT, () => {
  log.info(`HTTP server listening on port ${PORT}`);
  if (adapterConfig.roon !== false) {
    log.info('Waiting for Roon Core authorization...');
  }

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

// Export bus for other modules
// Don't export bus - causes circular dependency with routes
