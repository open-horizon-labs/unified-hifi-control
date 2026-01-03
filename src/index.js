const os = require('os');
const { createRoonClient } = require('./roon/client');
const { HQPClient } = require('./hqplayer/client');
const { createMqttService } = require('./mqtt');
const { createApp } = require('./server/app');
const { createLogger } = require('./lib/logger');
const { advertise } = require('./lib/mdns');
const { createKnobsStore } = require('./knobs/store');
const { createBus } = require('./bus');
const { RoonAdapter } = require('./bus/adapters/roon');
const { HQPAdapter } = require('./bus/adapters/hqp');
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

// Create Roon client
const roon = createRoonClient({
  logger: createLogger('Roon'),
  base_url: baseUrl,
});

// Create and configure bus
const bus = createBus({ logger: createLogger('Bus') });

const roonAdapter = new RoonAdapter(roon);
bus.registerBackend('roon', roonAdapter);

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
  const fs = require('fs');
  const path = require('path');
  const CONFIG_DIR = process.env.CONFIG_DIR || path.join(__dirname, '..', 'data');
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
        configs = [{
          name: data.name || 'default',
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
    configs = [{
      name: process.env.HQP_NAME || 'default',
      host: process.env.HQP_HOST,
      port: process.env.HQP_PORT || 8088,
      username: process.env.HQP_USER,
      password: process.env.HQP_PASS,
    }];
  }

  // Create instances
  configs.forEach(config => {
    const instanceName = config.name || 'default';
    const client = new HQPClient({
      host: config.host,
      port: config.port,
      username: config.username,
      password: config.password,
      logger: createLogger(`HQP:${instanceName}`),
    });

    const adapter = new HQPAdapter(client, { instanceName });

    hqpInstances.set(instanceName, { client, adapter });
    bus.registerBackend(`hqp:${instanceName}`, adapter);

    log.info('HQPlayer instance configured', {
      instance: instanceName,
      host: config.host,
      port: config.port
    });
  });

  return hqpInstances;
}

loadHQPInstances();

// For backward compatibility, expose first instance as 'hqp'
const firstHQP = Array.from(hqpInstances.values())[0];
const hqp = firstHQP ? firstHQP.client : null;

// Create MQTT service (opt-in via MQTT_BROKER env var)
const mqttService = createMqttService({
  hqp,
  logger: createLogger('MQTT'),
});

// Create HTTP server
const app = createApp({
  bus,     // Pass bus to app
  roon,    // Keep for backward compat during Phase 2 testing
  hqp,     // First instance for backward compat
  hqpInstances, // All instances for multi-instance support
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

// Export bus for other modules
// Don't export bus - causes circular dependency with routes
