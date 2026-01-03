/**
 * UPnP Client - Discovers and controls UPnP/DLNA Media Renderers
 *
 * Uses SSDP for discovery and MediaRenderer client for control
 */

const { Client: SSDPClient } = require('node-ssdp');
const MediaRendererClient = require('upnp-mediarenderer-client');

const SSDP_SEARCH_INTERVAL_MS = 30000; // Search every 30 seconds
const RENDERER_SERVICE_TYPE = 'urn:schemas-upnp-org:device:MediaRenderer:1';

function createUPnPClient(opts = {}) {
  const log = opts.logger || console;
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

  // Handle discovered devices
  ssdpClient.on('response', (headers, statusCode, rinfo) => {
    // Only process MediaRenderer devices
    if (headers.ST !== RENDERER_SERVICE_TYPE && headers.NT !== RENDERER_SERVICE_TYPE) {
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
      // Create MediaRenderer client for this device
      const client = new MediaRendererClient(location);

      // Store renderer info
      state.renderers.set(uuid, {
        uuid,
        location,
        usn,
        client,
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

      // Set up event listeners for this renderer
      setupRendererListeners(uuid, client);

      // Try to get device-friendly name
      // MediaRendererClient doesn't expose device description directly,
      // so we'll use the UUID as the name for now
      state.renderers.get(uuid).info.name = `Renderer ${uuid.substring(0, 8)}`;

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
        state.renderers.delete(uuid);
      }
    }
  }

  function getZones() {
    return Array.from(state.renderers.values()).map(renderer => ({
      zone_id: renderer.uuid,
      zone_name: renderer.info.name,
      source: 'upnp',
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
    }));
  }

  function getNowPlaying(uuid) {
    const renderer = state.renderers.get(uuid);
    if (!renderer) return null;

    return {
      zone_id: uuid,
      line1: renderer.info.name,
      line2: '', // UPnP doesn't provide track info easily without polling
      line3: '',
      is_playing: renderer.info.state === 'playing',
      volume: renderer.info.volume,
      volume_type: 'number',
      volume_min: 0,
      volume_max: 100,
      volume_step: 1,
      seek_position: null,
      length: null,
    };
  }

  async function control(uuid, action, value) {
    const renderer = state.renderers.get(uuid);
    if (!renderer) {
      throw new Error('Renderer not found');
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
        // Not all renderers support next/previous
        throw new Error('Next track not supported');
      case 'previous':
      case 'prev':
        throw new Error('Previous track not supported');
      case 'vol_abs':
        const volume = Math.max(0, Math.min(100, Number(value) || 0));
        await promisify(client, 'setVolume', volume);
        renderer.info.volume = volume;
        break;
      case 'vol_rel':
        // Get current volume first
        const currentVolume = await promisify(client, 'getVolume');
        const newVolume = Math.max(0, Math.min(100, currentVolume + (Number(value) || 0)));
        await promisify(client, 'setVolume', newVolume);
        renderer.info.volume = newVolume;
        break;
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
    if (state.ssdpClient) {
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

  return {
    start,
    stop,
    getZones,
    getNowPlaying,
    control,
    getStatus,
  };
}

module.exports = { createUPnPClient };
