/**
 * UPnP Client - Discovers and controls UPnP/DLNA Media Renderers
 *
 * Uses SSDP for discovery and MediaRenderer client for control
 */

const { Client: SSDPClient } = require('node-ssdp');
const MediaRendererClient = require('upnp-mediarenderer-client');
const http = require('http');
const https = require('https');
const { parseString } = require('xml2js');

const SSDP_SEARCH_INTERVAL_MS = 30000; // Search every 30 seconds
const RENDERER_SERVICE_TYPE = 'urn:schemas-upnp-org:device:MediaRenderer:1';

function createUPnPClient(opts = {}) {
  const log = opts.logger || console;
  let onZonesChanged = opts.onZonesChanged || (() => {});
  const state = {
    renderers: new Map(), // uuid -> { device, client, info }
    ssdpClient: null,
    searchInterval: null,
  };

  // Create SSDP client for discovery
  const ssdpClient = new SSDPClient({
    customLogger: (...args) => {
      // Forward SSDP logs to our logger at debug level
      if (log.debug) log.debug(args[0], ...args.slice(1));
    },
  });

  state.ssdpClient = ssdpClient;

  // Fetch device friendly name from description XML
  async function fetchDeviceName(uuid, location) {
    try {
      const urlObj = new URL(location);
      const protocol = urlObj.protocol === 'https:' ? https : http;

      return new Promise((resolve, reject) => {
        let req;
        const timeout = setTimeout(() => {
          if (req) req.destroy();
          reject(new Error('Device info fetch timeout'));
        }, 5000);

        req = protocol.get(location, (res) => {
          let xml = '';
          res.on('data', chunk => { xml += chunk; });
          res.on('end', () => {
            clearTimeout(timeout);
            parseString(xml, { explicitArray: false }, (err, result) => {
              if (err) return reject(err);

              try {
                const device = result.root?.device;
                const friendlyName = device?.friendlyName;
                const modelName = device?.modelName;
                const manufacturer = device?.manufacturer;

                const renderer = state.renderers.get(uuid);
                if (renderer) {
                  renderer.info.name = friendlyName || `Renderer ${uuid.substring(0, 8)}`;
                  renderer.info.manufacturer = manufacturer || null;
                  renderer.info.model = modelName || null;

                  log.info('Got device info', {
                    uuid,
                    name: renderer.info.name,
                    model: modelName,
                  });
                  onZonesChanged();
                }
                resolve();
              } catch (e) {
                reject(e);
              }
            });
          });
        }).on('error', (err) => {
          clearTimeout(timeout);
          reject(err);
        });
      });
    } catch (err) {
      log.error('Failed to parse device location', { uuid, location, error: err.message });
    }
  }


  // Handle discovered devices
  ssdpClient.on('response', (headers, statusCode, rinfo) => {
    const st = headers.ST || headers.NT;

    // Only process MediaRenderer devices
    if (st !== RENDERER_SERVICE_TYPE) {
      return;
    }

    const location = headers.LOCATION;
    const usn = headers.USN; // Unique service name: uuid:device-UUID::urn:...

    if (!location || !usn) return;

    // Extract UUID from USN (format: uuid:XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX::...)
    const uuidMatch = usn.match(/uuid:([a-f0-9-]+)/i);
    if (!uuidMatch) return;

    const uuid = uuidMatch[1];

    // Update lastSeen for existing renderers
    if (state.renderers.has(uuid)) {
      const renderer = state.renderers.get(uuid);
      if (renderer) {
        renderer.lastSeen = Date.now();
      }
      return;
    }

    log.info('Discovered UPnP MediaRenderer', { uuid, location, usn });

    try {
      // Store renderer info (client created lazily on control to avoid errors)
      state.renderers.set(uuid, {
        uuid,
        location,
        usn,
        client: null,  // Created lazily in control()
        info: {
          uuid,
          name: null, // Will be populated when we get device description
          manufacturer: null,
          model: null,
          state: 'stopped',
          volume: null,
        },
        lastSeen: Date.now(),
      });

      // Fetch device description to get friendly name
      fetchDeviceName(uuid, location).catch(err => {
        log.warn('Failed to fetch device name, using UUID', { uuid, error: err.message });
        // Fallback to UUID-based name
        const renderer = state.renderers.get(uuid);
        if (renderer) {
          renderer.info.name = `Renderer ${uuid.substring(0, 8)}`;
        }
        onZonesChanged();
      });

    } catch (err) {
      log.error('Failed to create MediaRenderer client', { uuid, error: err.message });
    }
  });

  function setupRendererListeners(uuid, client) {
    client.on('status', (status) => {
      const renderer = state.renderers.get(uuid);
      if (!renderer) return;

      // Update state based on status events
      if (status.TransportState) {
        const transportState = status.TransportState;
        renderer.info.state = transportState.toLowerCase();
      }

      renderer.lastSeen = Date.now();
    });

    client.on('loading', () => {
      const renderer = state.renderers.get(uuid);
      if (renderer) {
        renderer.info.state = 'loading';
        renderer.lastSeen = Date.now();
      }
    });

    client.on('playing', () => {
      const renderer = state.renderers.get(uuid);
      if (renderer) {
        renderer.info.state = 'playing';
        renderer.lastSeen = Date.now();
      }
    });

    client.on('paused', () => {
      const renderer = state.renderers.get(uuid);
      if (renderer) {
        renderer.info.state = 'paused';
        renderer.lastSeen = Date.now();
      }
    });

    client.on('stopped', () => {
      const renderer = state.renderers.get(uuid);
      if (renderer) {
        renderer.info.state = 'stopped';
        renderer.lastSeen = Date.now();
      }
    });

    client.on('error', (err) => {
      log.error('MediaRenderer client error', { uuid, error: err.message });
    });
  }

  function startDiscovery() {
    // Perform initial search
    performSearch();

    // Set up periodic search
    state.searchInterval = setInterval(() => {
      performSearch();
      cleanupStaleRenderers();
    }, SSDP_SEARCH_INTERVAL_MS);

    log.info('UPnP discovery started');
  }

  function performSearch() {
    try {
      state.ssdpClient.search(RENDERER_SERVICE_TYPE);
    } catch (err) {
      log.error('SSDP search failed', { error: err.message });
    }
  }

  function cleanupStaleRenderers() {
    const now = Date.now();
    const staleThreshold = SSDP_SEARCH_INTERVAL_MS * 3; // 3 missed searches

    for (const [uuid, renderer] of state.renderers.entries()) {
      if (now - renderer.lastSeen > staleThreshold) {
        log.info('Removing stale renderer', { uuid });

        // Clean up event listeners before deletion
        if (renderer.client && renderer.client.removeAllListeners) {
          renderer.client.removeAllListeners();
        }

        state.renderers.delete(uuid);
        // Notify that zones changed (renderer removed)
        onZonesChanged();
      }
    }
  }

  function getZones() {
    return Array.from(state.renderers.values()).map(renderer => ({
      zone_id: renderer.uuid,
      zone_name: renderer.info.name,
      state: renderer.info.state,
      output_count: 1,
      output_name: renderer.info.name,
      device_name: renderer.info.manufacturer && renderer.info.model
        ? `${renderer.info.manufacturer} ${renderer.info.model}`
        : null,
      volume_control: renderer.info.volume !== null ? {
        type: 'number',
        min: 0,
        max: 100,
        is_muted: false,
      } : null,
      // Pure UPnP doesn't support these features
      unsupported: ['next', 'previous', 'track_metadata', 'album_art'],
    }));
  }

  function getNowPlaying(uuid) {
    const renderer = state.renderers.get(uuid);
    if (!renderer) return null;

    // Pure UPnP doesn't provide track metadata
    return {
      zone_id: uuid,
      line1: renderer.info.name,
      line2: '',
      line3: '',
      is_playing: renderer.info.state === 'playing',
      volume: renderer.info.volume,
      volume_type: 'number',
      volume_min: 0,
      volume_max: 100,
      volume_step: 1,
      seek_position: null,
      length: null,
      image_key: null,
    };
  }

  async function control(uuid, action, value) {
    const renderer = state.renderers.get(uuid);
    if (!renderer) {
      throw new Error('Renderer not found');
    }

    // Create MediaRenderer client lazily
    if (!renderer.client) {
      renderer.client = new MediaRendererClient(renderer.location);
      setupRendererListeners(uuid, renderer.client);
      log.info('Created MediaRenderer client for control', { uuid, name: renderer.info.name });
    }

    const client = renderer.client;

    switch (action) {
      case 'play_pause':
        if (renderer.info.state === 'playing') {
          await promisify(client, 'pause');
        } else {
          await promisify(client, 'play');
        }
        break;
      case 'play':
        await promisify(client, 'play');
        break;
      case 'pause':
        await promisify(client, 'pause');
        break;
      case 'stop':
        await promisify(client, 'stop');
        break;
      case 'next':
        throw new Error('Next track not supported');
      case 'previous':
      case 'prev':
        throw new Error('Previous track not supported');
      case 'vol_abs': {
        const volume = Math.max(0, Math.min(100, Number(value) || 0));
        await promisify(client, 'setVolume', volume);
        renderer.info.volume = volume;
        break;
      }
      case 'vol_rel': {
        const currentVolume = await promisify(client, 'getVolume');
        const newVolume = Math.max(0, Math.min(100, currentVolume + (Number(value) || 0)));
        await promisify(client, 'setVolume', newVolume);
        renderer.info.volume = newVolume;
        break;
      }
      default:
        throw new Error('Unknown action');
    }
  }


  // Helper to promisify MediaRendererClient methods
  function promisify(client, method, ...args) {
    return new Promise((resolve, reject) => {
      client[method](...args, (err, result) => {
        if (err) return reject(new Error(String(err)));
        resolve(result);
      });
    });
  }

  function start() {
    startDiscovery();
  }

  function stop() {
    if (state.searchInterval) {
      clearInterval(state.searchInterval);
      state.searchInterval = null;
    }

    // Clean up all renderer clients
    for (const renderer of state.renderers.values()) {
      if (renderer.client && renderer.client.removeAllListeners) {
        renderer.client.removeAllListeners();
      }
    }
    state.renderers.clear();

    // Clean up SSDP client
    if (state.ssdpClient) {
      state.ssdpClient.removeAllListeners();
      state.ssdpClient.stop();
    }

    log.info('UPnP client stopped');
  }

  function getStatus() {
    return {
      connected: state.renderers.size > 0,
      renderer_count: state.renderers.size,
      renderers: getZones(),
    };
  }

  function setOnZonesChanged(callback) {
    onZonesChanged = callback || (() => {});
  }

  return {
    start,
    stop,
    getZones,
    getNowPlaying,
    control,
    getStatus,
    setOnZonesChanged,
  };
}

module.exports = { createUPnPClient };
