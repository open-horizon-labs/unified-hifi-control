const { validateAdapter } = require('./adapter');
const crypto = require('crypto');

/**
 * Bus - Routes commands to backend adapters
 */
function createBus({ logger } = {}) {
  const log = logger || console;
  const backends = new Map();
  const zones = new Map();
  const observers = [];
  let zonesSha = null; // Cached SHA of zone IDs

  function subscribe(callback) {
    if (typeof callback !== 'function') {
      throw new Error('Observer must be a function');
    }
    observers.push(callback);
    return () => {
      const idx = observers.indexOf(callback);
      if (idx >= 0) observers.splice(idx, 1);
    };
  }

  function notifyObservers(activity) {
    observers.forEach(observer => {
      try {
        observer(activity);
      } catch (err) {
        log.error('Observer error:', err);
      }
    });
  }

  function registerBackend(source, adapter) {
    validateAdapter(adapter, source);
    backends.set(source, adapter);

    // Log capabilities
    const caps = {
      image: typeof adapter.getImage === 'function',
      events: typeof adapter.on === 'function',
    };
    log.info(`Registered ${source} backend`, caps);

    // Note: Don't refresh zones yet - state.zones is empty until start() completes
    // Zones will be refreshed after bus.start() (see start() method)
  }

  async function unregisterBackend(source) {
    const adapter = backends.get(source);
    if (!adapter) {
      log.warn(`Cannot unregister: ${source} not found`);
      return;
    }

    // Stop the adapter
    try {
      await adapter.stop();
      log.info(`${source} stopped`);
    } catch (err) {
      log.error(`${source} stop failed:`, err);
    }

    // Remove from backends
    backends.delete(source);

    // Clear zones for this source
    for (const [zid] of zones) {
      if (zid.startsWith(`${source}:`)) zones.delete(zid);
    }

    // Invalidate cached SHA when zones change
    zonesSha = null;

    log.info(`Unregistered ${source} backend`);
  }

  async function enableBackend(source, adapter) {
    if (backends.has(source)) {
      log.warn(`Backend ${source} already registered`);
      return;
    }

    registerBackend(source, adapter);

    // Start immediately
    try {
      await adapter.start();
      log.info(`${source} started`);
    } catch (err) {
      log.error(`${source} start failed:`, err);
    }

    // Refresh zones for this backend
    refreshZones(source);
  }

  function computeZonesSha() {
    // Compute SHA from sorted zone IDs for stable hash
    const zoneIds = Array.from(zones.keys()).sort();
    const hash = crypto.createHash('sha256').update(JSON.stringify(zoneIds)).digest('hex');
    return hash.substring(0, 8);
  }

  function refreshZones(source = null) {
    if (source) {
      for (const [zid] of zones) {
        if (zid.startsWith(`${source}:`)) zones.delete(zid);
      }

      const adapter = backends.get(source);
      if (adapter) {
        adapter.getZones().forEach(z => {
          zones.set(z.zone_id, { zone: z, adapter });
        });
      }
    } else {
      zones.clear();
      for (const [src, adapter] of backends) {
        adapter.getZones().forEach(z => {
          zones.set(z.zone_id, { zone: z, adapter });
        });
      }
    }
    // Invalidate cached SHA when zones change
    zonesSha = null;
  }

  function getZones() {
    // Auto-refresh if zones empty (e.g., Roon paired after start)
    if (zones.size === 0 && backends.size > 0) {
      refreshZones();
    }
    return Array.from(zones.values()).map(({ zone }) => zone);
  }

  function getZonesSha() {
    // Lazy compute and cache
    if (zonesSha === null) {
      zonesSha = computeZonesSha();
    }
    return zonesSha;
  }

  function getZone(zone_id) {
    return zones.get(zone_id)?.zone || null;
  }

  function getAdapterForZone(zone_id) {
    const entry = zones.get(zone_id);
    if (entry) return entry.adapter;

    const backend = zone_id.split(':')[0];
    return backends.get(backend) || null;
  }

  function getNowPlaying(zone_id, opts = {}) {
    const adapter = getAdapterForZone(zone_id);
    const sender = opts.sender || {};

    if (!adapter) {
      log.warn(`No adapter for zone: ${zone_id}`);
      notifyObservers({ type: 'getNowPlaying', zone_id, error: 'No adapter found', sender, timestamp: Date.now() });
      return null;
    }
    const result = adapter.getNowPlaying(zone_id);
    notifyObservers({ type: 'getNowPlaying', zone_id, backend: zone_id.split(':')[0], has_data: !!result, sender, timestamp: Date.now() });
    return result;
  }

  async function control(zone_id, action, value, opts = {}) {
    const adapter = getAdapterForZone(zone_id);
    const sender = opts.sender || {};

    if (!adapter) {
      notifyObservers({ type: 'control', zone_id, action, value, error: 'Zone not found', sender, timestamp: Date.now() });
      throw new Error(`Zone not found: ${zone_id}`);
    }
    notifyObservers({ type: 'control', zone_id, backend: zone_id.split(':')[0], action, value, sender, timestamp: Date.now() });
    return adapter.control(zone_id, action, value);
  }

  async function getImage(image_key, opts = {}) {
    // Route by zone_id context (routes have zone_id when requesting images)
    const zone_id = opts.zone_id;
    if (!zone_id) {
      notifyObservers({ type: 'getImage', image_key, error: 'zone_id required', timestamp: Date.now() });
      throw new Error('zone_id required in opts for image routing');
    }

    const adapter = getAdapterForZone(zone_id);
    if (!adapter) {
      notifyObservers({ type: 'getImage', image_key, zone_id, error: 'Backend not found', timestamp: Date.now() });
      throw new Error(`Backend not found for zone: ${zone_id}`);
    }

    if (!adapter.getImage) {
      notifyObservers({ type: 'getImage', image_key, zone_id, error: 'Images not supported', timestamp: Date.now() });
      throw new Error(`${adapter.constructor.name} does not support images`);
    }

    notifyObservers({ type: 'getImage', image_key, zone_id, backend: zone_id.split(':')[0], timestamp: Date.now() });
    return adapter.getImage(image_key, opts);
  }

  function getStatus() {
    const status = {};
    for (const [source, adapter] of backends) {
      status[source] = adapter.getStatus();
    }
    return status;
  }

  async function start() {
    log.info('Starting bus...');
    for (const [source, adapter] of backends) {
      try {
        await adapter.start();
        log.info(`${source} started`);
      } catch (err) {
        log.error(`${source} start failed:`, err);
      }
    }
    refreshZones();
  }

  async function stop() {
    log.info('Stopping bus...');
    for (const [source, adapter] of backends) {
      try {
        await adapter.stop();
        log.info(`${source} stopped`);
      } catch (err) {
        log.error(`${source} stop failed:`, err);
      }
    }
    zones.clear();
  }

  return {
    registerBackend,
    unregisterBackend,
    enableBackend,
    refreshZones,
    getZones,
    getZonesSha,
    getZone,
    getNowPlaying,
    control,
    getImage,
    getStatus,
    start,
    stop,
    subscribe,
    hasBackend: (source) => backends.has(source),
  };
}

module.exports = { createBus };
