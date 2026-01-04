/**
 * UPnP Client - Discovers and controls UPnP/DLNA Media Renderers
 *
 * Uses SSDP for discovery and MediaRenderer client for control
 */

const { Client: SSDPClient } = require('node-ssdp');
const MediaRendererClient = require('upnp-mediarenderer-client');
const DeviceClient = require('upnp-device-client');
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
    trackPollInterval: null,
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
        protocol.get(location, (res) => {
          let xml = '';
          res.on('data', chunk => { xml += chunk; });
          res.on('end', () => {
            parseString(xml, { explicitArray: false }, (err, result) => {
              if (err) return reject(err);

              try {
                const device = result.root?.device;
                const friendlyName = device?.friendlyName;
                const modelName = device?.modelName;
                const manufacturer = device?.manufacturer;

                // Check for OpenHome services
                const serviceList = device?.serviceList?.service || [];
                const services = Array.isArray(serviceList) ? serviceList : [serviceList];
                const hasOpenHomeInfo = services.some(s =>
                  s.serviceType?.includes('av-openhome-org:service:Info')
                );

                const renderer = state.renderers.get(uuid);
                if (renderer) {
                  renderer.info.name = friendlyName || `Renderer ${uuid.substring(0, 8)}`;
                  renderer.info.manufacturer = manufacturer || null;
                  renderer.info.model = modelName || null;
                  renderer.hasOpenHome = hasOpenHomeInfo;

                  log.info('Got device info', {
                    uuid,
                    name: renderer.info.name,
                    model: modelName,
                    openHome: hasOpenHomeInfo
                  });
                  onZonesChanged();
                }
                resolve();
              } catch (e) {
                reject(e);
              }
            });
          });
        }).on('error', reject);
      });
    } catch (err) {
      log.error('Failed to parse device location', { uuid, location, error: err.message });
    }
  }


  // Handle discovered devices
  ssdpClient.on('response', (headers, statusCode, rinfo) => {
    const st = headers.ST || headers.NT;

    // Process MediaRenderer OR OpenHome devices
    const isMediaRenderer = st === RENDERER_SERVICE_TYPE;
    const isOpenHome = st && st.includes('av-openhome-org');

    if (!isMediaRenderer && !isOpenHome) {
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

    // Poll track info periodically for playing devices
    state.trackPollInterval = setInterval(() => {
      pollTrackInfo();
    }, 2000);  // Poll every 2 seconds

    log.info('UPnP discovery started');
  }

  function performSearch() {
    try {
      // Search for both MediaRenderer and OpenHome devices
      state.ssdpClient.search(RENDERER_SERVICE_TYPE);
      state.ssdpClient.search('urn:av-openhome-org:service:Product:1');
    } catch (err) {
      log.error('SSDP search failed', { error: err.message });
    }
  }

  async function pollTrackInfo() {
    const now = Date.now();

    for (const [uuid, renderer] of state.renderers.entries()) {
      // Only poll OpenHome devices
      if (!renderer.hasOpenHome) {
        continue;
      }

      // Poll all OpenHome devices (get state + track info)

      // Rate limit: max once per 5 seconds per device
      if (renderer.lastTrackPoll && now - renderer.lastTrackPoll < 5000) {
        continue;
      }
      renderer.lastTrackPoll = now;

      // Reuse device client
      if (!renderer.deviceClient) {
        renderer.deviceClient = new DeviceClient(renderer.location);
      }

      try {
        // Query OpenHome Transport state first
        const transportState = await new Promise((resolve, reject) => {
          const timeout = setTimeout(() => reject(new Error('Timeout')), 3000);
          renderer.deviceClient.callAction(
            'urn:av-openhome-org:serviceId:Transport',
            'TransportState',
            {},
            (err, result) => {
              clearTimeout(timeout);
              if (err) return reject(err);
              resolve(result);
            }
          );
        });

        const newState = (transportState.State || '').toLowerCase();
        if (newState !== renderer.info.state) {
          renderer.info.state = newState;
          onZonesChanged();
        }

        // Query OpenHome Volume
        const volumeInfo = await new Promise((resolve, reject) => {
          const timeout = setTimeout(() => reject(new Error('Timeout')), 3000);
          renderer.deviceClient.callAction(
            'urn:av-openhome-org:serviceId:Volume',
            'Volume',
            {},
            (err, result) => {
              clearTimeout(timeout);
              if (err) return reject(err);
              resolve(result);
            }
          );
        });

        if (volumeInfo.Value !== undefined) {
          renderer.info.volume = volumeInfo.Value;
        }

        // Query OpenHome Info:Track service
        const trackInfo = await new Promise((resolve, reject) => {
          const timeout = setTimeout(() => reject(new Error('Timeout')), 3000);
          renderer.deviceClient.callAction(
            'urn:av-openhome-org:serviceId:Info',
            'Track',
            {},
            (err, result) => {
              clearTimeout(timeout);
              if (err) return reject(err);
              resolve(result);
            }
          );
        });

        // Only parse if URI changed
        if (trackInfo.Uri && trackInfo.Uri !== renderer.lastTrackUri) {
          renderer.lastTrackUri = trackInfo.Uri;

          if (trackInfo.Metadata) {
            const metadata = await new Promise((resolve, reject) => {
              parseString(trackInfo.Metadata, { explicitArray: false, trim: true }, (err, result) => {
                if (err) return reject(err);
                resolve(result);
              });
            });

            const item = metadata?.['DIDL-Lite']?.item;
            if (item) {
              renderer.openHomeTrackInfo = {
                title: item['dc:title'] || '',
                artist: (Array.isArray(item['upnp:artist']) ? item['upnp:artist'][0] : item['upnp:artist']) || '',
                album: item['upnp:album'] || '',
                albumArtUri: item['upnp:albumArtURI'] || '',
                genre: item['upnp:genre'] || '',
              };
              log.info('Updated OpenHome track info', {
                uuid,
                track: renderer.openHomeTrackInfo.title,
                hasArt: !!renderer.openHomeTrackInfo.albumArtUri
              });
            }
          }
        }
      } catch (err) {
        // Log occasionally (max once per minute per device)
        if (!renderer.lastPollError || now - renderer.lastPollError > 60000) {
          log.warn('Failed to poll OpenHome track info', { uuid, name: renderer.info.name, error: err.message });
          renderer.lastPollError = now;
        }
      }
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

    // Use OpenHome track info if available
    const track = renderer.openHomeTrackInfo || {};

    return {
      zone_id: uuid,
      line1: track.title || renderer.info.name,
      line2: track.artist || '',
      line3: track.album || '',
      is_playing: renderer.info.state === 'playing',
      volume: renderer.info.volume,
      volume_type: 'number',
      volume_min: 0,
      volume_max: 100,
      volume_step: 1,
      seek_position: null,
      length: null,
      image_key: track.albumArtUri || null,  // Album art URL
    };
  }

  async function control(uuid, action, value) {
    const renderer = state.renderers.get(uuid);
    if (!renderer) {
      throw new Error('Renderer not found');
    }

    // Create client lazily on first control
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

    if (state.trackPollInterval) {
      clearInterval(state.trackPollInterval);
      state.trackPollInterval = null;
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
