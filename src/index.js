const os = require('os');
const fs = require('fs');
const path = require('path');
const { getDataDir } = require('./lib/paths');
const { createRoonClient } = require('./roon/client');
const { createUPnPClient } = require('./upnp/client');
const { createOpenHomeClient } = require('./openhome/client');
const { HQPClient } = require('./hqplayer/client');
const { LMSClient } = require('./lms/client');
const { createMqttService } = require('./mqtt');
const { createApp } = require('./server/app');
const { createLogger } = require('./lib/logger');
const { loadAppSettings, saveAppSettings } = require('./lib/settings');
const { advertise } = require('./lib/mdns');
const { createKnobsStore } = require('./knobs/store');
const { createBus } = require('./bus');
const { createFirmwareService } = require('./firmware');
const { RoonAdapter } = require('./bus/adapters/roon');
const { UPnPAdapter } = require('./bus/adapters/upnp');
const { OpenHomeAdapter } = require('./bus/adapters/openhome');
const { LMSAdapter } = require('./bus/adapters/lms');
const { HQPService } = require('./hqplayer/service');
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

// Initialize debug consumer
busDebug.init(bus);

// Create knobs store for ESP32 knob configuration
const knobs = createKnobsStore({
  logger: createLogger('Knobs'),
});

// Load HQPlayer instances from config file or env vars
// Supports multiple instances (e.g., embedded + desktop simultaneously)
const hqpInstances = new Map(); // instanceName -> { client, adapter }

function loadHQPInstances() {
  const CONFIG_DIR = getDataDir();
  const HQP_CONFIG_FILE = path.join(CONFIG_DIR, 'hqp-config.json');

  let configs = [];

  // Try loading from file
  try {
    if (fs.existsSync(HQP_CONFIG_FILE)) {
      const data = JSON.parse(fs.readFileSync(HQP_CONFIG_FILE, 'utf8'));

      // Support both old single-instance and new multi-instance format
      if (Array.isArray(data)) {
        configs = data;
      } else if (data.host) {
        // Old format: single instance object
        // Use host as default name for predictability (e.g., "192.168.1.61")
        configs = [{
          name: data.name || data.host,
          host: data.host,
          port: data.port,
          username: data.username,
          password: data.password,
        }];
      }
    }
  } catch (e) {
    log.warn('Failed to load HQP config file', { error: e.message });
  }

  // Fallback to env vars if no config file
  if (configs.length === 0 && process.env.HQP_HOST) {
    // Use host as default name for predictability
    configs = [{
      name: process.env.HQP_NAME || process.env.HQP_HOST,
      host: process.env.HQP_HOST,
      port: process.env.HQP_PORT || 8088,
      username: process.env.HQP_USER,
      password: process.env.HQP_PASS,
    }];
  }

  // Create instances
  configs.forEach(config => {
    const instanceName = config.name || config.host || 'unknown';

    // Validate instance name
    if (!instanceName || typeof instanceName !== 'string') {
      log.error('Invalid instance name - skipping', { config });
      return;
    }
    if (hqpInstances.has(instanceName)) {
      log.error('Duplicate instance name - skipping', { name: instanceName });
      return;
    }
    if (instanceName.includes(':')) {
      log.error('Instance name cannot contain colons - skipping', { name: instanceName });
      return;
    }

    const client = new HQPClient({
      host: config.host,
      port: config.port,
      username: config.username,
      password: config.password,
      logger: createLogger(`HQP:${instanceName}`),
    });

    hqpInstances.set(instanceName, { client });

    log.info('HQPlayer instance configured (DSP service, not zone backend)', {
      instance: instanceName,
      host: config.host,
      port: config.port
    });
  });

  return hqpInstances;
}

loadHQPInstances();

// Create HQP service for zone linking and enrichment
const hqpService = new HQPService({
  instances: hqpInstances,
  logger: createLogger('HQP:Service'),
});

// Load existing zone links from settings
if (appSettings.hqp?.zoneLinks) {
  const loadResult = hqpService.loadLinks(appSettings.hqp.zoneLinks);

  log.info('Loaded HQP zone links from settings', { count: hqpService.getLinks().length });

  // Persist auto-corrections
  if (loadResult.corrected) {
    appSettings.hqp.zoneLinks = loadResult.links;
    try {
      saveAppSettings(appSettings);
      log.warn('Auto-corrected zone links persisted to app-settings.json');
    } catch (err) {
      log.error('Failed to persist auto-corrected zone links', { error: err.message });
    }
  }
}

// Register HQP service with bus for zone enrichment
bus.setHQPService(hqpService);

// For backward compatibility, expose first instance as 'hqp'
const firstHQP = Array.from(hqpInstances.values())[0];
const hqp = firstHQP ? firstHQP.client : null;

// Create firmware service for automatic update polling
const firmwareService = createFirmwareService({
  logger: createLogger('Firmware'),
});

// Create MQTT service (opt-in via MQTT_BROKER env var)
const mqttService = createMqttService({
  hqp,
  firmware: firmwareService,
  logger: createLogger('MQTT'),
});

// Check if firmware auto-update is enabled (default: true)
const FIRMWARE_AUTO_UPDATE = process.env.FIRMWARE_AUTO_UPDATE !== 'false';

// Create HTTP server
const app = createApp({
  bus,
  roon,    // Keep for backward compat during Phase 2 testing
  hqp,
  hqpInstances, // All HQP instances for multi-instance support
  hqpService,   // HQP service for zone linking
  lms,     // Lyrion client for configuration API
  knobs,
  adapterFactory,
  logger: createLogger('Server'),
});

// Start services
bus.start();  // Starts all registered backends (including roon)
mqttService.connect();

// Start firmware polling only if enabled
if (FIRMWARE_AUTO_UPDATE) {
  firmwareService.start();  // Start polling for firmware updates
  log.info('Firmware auto-update enabled');
} else {
  log.info('Firmware auto-update disabled (set FIRMWARE_AUTO_UPDATE=true to enable)');
}

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
  firmwareService.stop();
  mqttService.disconnect();
  await bus.stop();
  process.exit(0);
});

process.on('unhandledRejection', (err) => {
  log.error('Unhandled rejection', { error: err.message, stack: err.stack });
});

// Export bus for other modules
// Don't export bus - causes circular dependency with routes
