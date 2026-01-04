/**
 * OpenHome Client - Discovers and controls OpenHome Media Renderers
 *
 * Uses SSDP for discovery and DeviceClient for OpenHome service calls
 */

const { Client: SSDPClient } = require('node-ssdp');
const DeviceClient = require('upnp-device-client');
const http = require('http');
const https = require('https');
const { parseString } = require('xml2js');

const SSDP_SEARCH_INTERVAL_MS = 30000; // Search every 30 seconds
const OPENHOME_PRODUCT_SERVICE = 'urn:av-openhome-org:service:Product:1';

function createOpenHomeClient(opts = {}) {
  const log = opts.logger || console;
  const onZonesChanged = opts.onZonesChanged || (() => {});

  const state = {
    devices: new Map(), // uuid -> { device info, cached track info }
    ssdpClient: null,
    searchInterval: null,
    pollInterval: null,
  };

  // Create SSDP client for discovery
  const ssdpClient = new SSDPClient({
    customLogger: (...args) => {
      if (log.debug) log.debug(args[0], ...args.slice(1));
    },
  });

  state.ssdpClient = ssdpClient;

  // Fetch device info from description XML
  async function fetchDeviceInfo(uuid, location) {
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
                const deviceInfo = result.root?.device;
                const device = state.devices.get(uuid);
                if (device) {
                  device.name = deviceInfo?.friendlyName || `OpenHome ${uuid.substring(0, 8)}`;
                  device.manufacturer = deviceInfo?.manufacturer || null;
                  device.model = deviceInfo?.modelName || null;
                  log.info('Got OpenHome device info', {
                    uuid,
                    name: device.name,
                    model: device.model
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

  // Handle discovered OpenHome devices
  ssdpClient.on('response', (headers) => {
    const st = headers.ST || headers.NT;

    // Only process OpenHome devices
    if (!st || !st.includes('av-openhome-org')) {
      return;
    }

    const location = headers.LOCATION;
    const usn = headers.USN;

    if (!location || !usn) return;

    // Extract UUID
    const uuidMatch = usn.match(/uuid:([a-f0-9-]+)/i);
    if (!uuidMatch) return;

    const uuid = uuidMatch[1];

    // Update lastSeen for existing devices
    if (state.devices.has(uuid)) {
      const device = state.devices.get(uuid);
      if (device) {
        device.lastSeen = Date.now();
      }
      return;
    }

    log.info('Discovered OpenHome device', { uuid, location, usn });

    // Store device info (will be populated by fetchDeviceInfo)
    state.devices.set(uuid, {
      uuid,
      location,
      usn,
      deviceClient: null,  // Created on demand for control/polling
      name: null,
      manufacturer: null,
      model: null,
      state: 'stopped',
      volume: null,
      trackInfo: null,
      lastSeen: Date.now(),
    });

    // Fetch device details
    fetchDeviceInfo(uuid, location).catch(err => {
      log.warn('Failed to fetch device info', { uuid, error: err.message });
      const device = state.devices.get(uuid);
      if (device) {
        device.name = `OpenHome ${uuid.substring(0, 8)}`;
      }
      onZonesChanged();
    });
  });

  function startDiscovery() {
    // Initial search
    performSearch();

    // Periodic search
    state.searchInterval = setInterval(() => {
      performSearch();
      cleanupStaleDevices();
    }, SSDP_SEARCH_INTERVAL_MS);

    // Poll state/metadata for active devices
    state.pollInterval = setInterval(() => {
      pollDeviceInfo();
    }, 2000);

    log.info('OpenHome discovery started');
  }

  function performSearch() {
    try {
      state.ssdpClient.search(OPENHOME_PRODUCT_SERVICE);
    } catch (err) {
      log.error('SSDP search failed', { error: err.message });
    }
  }

  function cleanupStaleDevices() {
    const now = Date.now();
    const staleThreshold = SSDP_SEARCH_INTERVAL_MS * 3;

    for (const [uuid, device] of state.devices.entries()) {
      if (now - device.lastSeen > staleThreshold) {
        log.info('Removing stale OpenHome device', { uuid });
        if (device.deviceClient) {
          device.deviceClient = null;
        }
        state.devices.delete(uuid);
        onZonesChanged();
      }
    }
  }

  async function pollDeviceInfo() {
    const now = Date.now();

    for (const [uuid, device] of state.devices.entries()) {
      // Rate limit: max once per 2s per device
      if (device.lastPoll && now - device.lastPoll < 2000) {
        continue;
      }
      device.lastPoll = now;

      // Create device client if needed
      if (!device.deviceClient) {
        device.deviceClient = new DeviceClient(device.location);
      }

      try {
        // Query transport state
        const transportState = await new Promise((resolve, reject) => {
          const timeout = setTimeout(() => reject(new Error('Timeout')), 5000);
          device.deviceClient.callAction(
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
        if (newState !== device.state) {
          device.state = newState;
          onZonesChanged();
        }

        // Query volume
        const volumeInfo = await new Promise((resolve, reject) => {
          const timeout = setTimeout(() => reject(new Error('Timeout')), 5000);
          device.deviceClient.callAction(
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
          device.volume = Number(volumeInfo.Value);
        }

        // Query track info
        const trackInfo = await new Promise((resolve, reject) => {
          const timeout = setTimeout(() => reject(new Error('Timeout')), 5000);
          device.deviceClient.callAction(
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
        if (trackInfo.Uri && trackInfo.Uri !== device.lastTrackUri) {
          device.lastTrackUri = trackInfo.Uri;

          if (trackInfo.Metadata) {
            const metadata = await new Promise((resolve, reject) => {
              parseString(trackInfo.Metadata, { explicitArray: false, trim: true }, (err, result) => {
                if (err) return reject(err);
                resolve(result);
              });
            });

            const item = metadata?.['DIDL-Lite']?.item;
            if (item) {
              device.trackInfo = {
                title: item['dc:title'] || '',
                artist: (Array.isArray(item['upnp:artist']) ? item['upnp:artist'][0] : item['upnp:artist']) || '',
                album: item['upnp:album'] || '',
                albumArtUri: item['upnp:albumArtURI'] || '',
                genre: item['upnp:genre'] || '',
              };
              log.info('Updated OpenHome track info', {
                uuid,
                track: device.trackInfo.title,
                hasArt: !!device.trackInfo.albumArtUri
              });
            }
          }
        }
      } catch (err) {
        // Log occasionally
        if (!device.lastPollError || now - device.lastPollError > 60000) {
          log.warn('Failed to poll OpenHome device', { uuid, name: device.name, error: err.message });
          device.lastPollError = now;
        }
      }
    }
  }

  function getZones() {
    return Array.from(state.devices.values()).map(device => ({
      zone_id: device.uuid,  // NO prefix - adapter adds it
      zone_name: device.name,
      state: device.state,
      output_count: 1,
      output_name: device.name,
      device_name: device.manufacturer && device.model
        ? `${device.manufacturer} ${device.model}`
        : null,
      volume_control: {
        type: 'number',
        min: 0,
        max: 100,
        is_muted: false,
      },
      // No unsupported field - OpenHome supports everything
    }));
  }

  function getNowPlaying(uuid) {
    const device = state.devices.get(uuid);
    if (!device) return null;

    const track = device.trackInfo || {};

    return {
      zone_id: uuid,
      line1: track.title || device.name,
      line2: track.artist || '',
      line3: track.album || '',
      is_playing: device.state === 'playing',
      volume: device.volume,
      volume_type: 'number',
      volume_min: 0,
      volume_max: 100,
      volume_step: 1,
      seek_position: null,
      length: null,
      image_key: track.albumArtUri || null,
    };
  }

  async function control(uuid, action, value) {
    const device = state.devices.get(uuid);
    if (!device) {
      throw new Error('Device not found');
    }

    if (!device.deviceClient) {
      device.deviceClient = new DeviceClient(device.location);
    }

    const callTransport = (actionName) => new Promise((resolve, reject) => {
      const timeout = setTimeout(() => reject(new Error('Transport timeout')), 5000);
      device.deviceClient.callAction('urn:av-openhome-org:serviceId:Transport', actionName, {}, (err) => {
        clearTimeout(timeout);
        if (err) return reject(err);
        resolve();
      });
    });

    const callVolume = (actionName, params = {}) => new Promise((resolve, reject) => {
      const timeout = setTimeout(() => reject(new Error('Volume timeout')), 5000);
      device.deviceClient.callAction('urn:av-openhome-org:serviceId:Volume', actionName, params, (err, result) => {
        clearTimeout(timeout);
        if (err) return reject(err);
        resolve(result);
      });
    });

    switch (action) {
      case 'play':
        await callTransport('Play');
        break;
      case 'pause':
        await callTransport('Pause');
        break;
      case 'play_pause':
        await callTransport(device.state === 'playing' ? 'Pause' : 'Play');
        break;
      case 'stop':
        await callTransport('Stop');
        break;
      case 'next':
        await callTransport('SkipNext');
        break;
      case 'previous':
      case 'prev':
        await callTransport('SkipPrevious');
        break;
      case 'vol_abs': {
        const volume = Math.max(0, Math.min(100, Number(value) || 0));
        await callVolume('SetVolume', { Value: volume });
        device.volume = volume;
        break;
      }
      case 'vol_rel': {
        const delta = Number(value) || 0;
        const targetVolume = Math.max(0, Math.min(100, (device.volume || 0) + delta));
        await callVolume('SetVolume', { Value: targetVolume });
        device.volume = targetVolume;
        break;
      }
      default:
        throw new Error(`Unknown action: ${action}`);
    }

    // Trigger immediate poll
    setTimeout(() => {
      pollDeviceInfo().catch(err => log.warn('Immediate poll failed', { uuid, error: err.message }));
    }, 200);
  }

  const MAX_REDIRECTS = 5;

  async function getImage(image_key, redirectCount = 0) {
    // image_key is direct URL for OpenHome
    if (!image_key || (!image_key.startsWith('http://') && !image_key.startsWith('https://'))) {
      throw new Error('Invalid image URL');
    }

    if (redirectCount >= MAX_REDIRECTS) {
      throw new Error('Too many redirects');
    }

    const protocol = image_key.startsWith('https') ? https : http;

    return new Promise((resolve, reject) => {
      let req;
      const timeout = setTimeout(() => {
        if (req) req.destroy();
        reject(new Error('Image fetch timeout'));
      }, 5000);

      req = protocol.get(image_key, (res) => {
        clearTimeout(timeout);

        // Handle redirects
        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
          const redirectUrl = new URL(res.headers.location, image_key);
          return getImage(redirectUrl.href, redirectCount + 1).then(resolve).catch(reject);
        }

        if (res.statusCode !== 200) {
          return reject(new Error(`HTTP ${res.statusCode}`));
        }

        const chunks = [];
        let size = 0;
        const maxSize = 10 * 1024 * 1024;

        res.on('data', chunk => {
          size += chunk.length;
          if (size > maxSize) {
            req.destroy();
            return reject(new Error('Image too large'));
          }
          chunks.push(chunk);
        });

        res.on('end', () => {
          resolve({
            contentType: res.headers['content-type'] || 'image/jpeg',
            body: Buffer.concat(chunks),
          });
        });
      });

      req.on('error', (err) => {
        clearTimeout(timeout);
        reject(err);
      });
    });
  }

  function getStatus() {
    return {
      connected: state.devices.size > 0,
      device_count: state.devices.size,
      devices: getZones(),
    };
  }

  function start() {
    startDiscovery();
  }

  function stop() {
    if (state.searchInterval) {
      clearInterval(state.searchInterval);
      state.searchInterval = null;
    }

    if (state.pollInterval) {
      clearInterval(state.pollInterval);
      state.pollInterval = null;
    }

    // Clean up device clients
    for (const device of state.devices.values()) {
      if (device.deviceClient) {
        device.deviceClient = null;
      }
    }
    state.devices.clear();

    // Clean up SSDP client
    if (state.ssdpClient) {
      state.ssdpClient.removeAllListeners();
      state.ssdpClient.stop();
    }

    log.info('OpenHome client stopped');
  }

  return {
    start,
    stop,
    getZones,
    getNowPlaying,
    control,
    getStatus,
    getImage,
  };
}

module.exports = { createOpenHomeClient };
