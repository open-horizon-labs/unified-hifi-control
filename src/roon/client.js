const RoonApi = require('node-roon-api');
const RoonApiStatus = require('node-roon-api-status');
const RoonApiTransport = require('node-roon-api-transport');
const RoonApiImage = require('node-roon-api-image');
const fs = require('fs');
const path = require('path');

const VERSION = process.env.APP_VERSION || 'dev';
const CONFIG_DIR = process.env.CONFIG_DIR || path.join(__dirname, '..', '..', 'data');
const CONFIG_FILE = path.join(CONFIG_DIR, 'roon-config.json');

if (!fs.existsSync(CONFIG_DIR)) {
  fs.mkdirSync(CONFIG_DIR, { recursive: true });
}

const RATE_LIMIT_INTERVAL_MS = 100;
const MAX_RELATIVE_STEP_PER_CALL = 25;
const MAX_VOLUME = 100;
const MIN_VOLUME = 0;
const CORE_LOSS_TIMEOUT_MS = 5 * 60 * 1000;
const TRANSPORT_GRACE_PERIOD_MS = 5 * 1000;

const fallbackSummary = (zone) => {
  const vol = zone?.outputs?.[0]?.volume;
  return {
    line1: 'Idle',
    line2: zone?.now_playing?.three_line?.line2 || zone?.display_name || '',
    is_playing: false,
    volume: vol?.value ?? null,
    volume_min: vol?.min ?? -80,
    volume_max: vol?.max ?? 0,
    volume_step: vol?.step ?? 2,
    seek_position: zone?.now_playing?.seek_position ?? null,
    length: zone?.now_playing?.length ?? null,
    zone_id: zone?.zone_id,
  };
};

function createRoonClient(opts = {}) {
  const log = opts.logger || console;
  const baseUrl = opts.base_url || '';
  const state = {
    core: null,
    coreInfo: null,
    transport: null,
    image: null,
    zones: [],
    nowPlayingByZone: new Map(),
    lastVolumeTick: new Map(),
    pendingRelative: new Map(),
    lastCoreSeen: 0,
    coreLossTimer: null,
    transportDisconnectedAt: null,
  };

  const roon = new RoonApi({
    extension_id: opts.extension_id || 'com.muness.unified-hifi-control',
    display_name: opts.display_name || 'Unified Hi-Fi Control',
    display_version: opts.display_version || VERSION,
    publisher: 'Muness Castle',
    email: 'support@example.com',
    website: 'https://github.com/muness/unified-hifi-control',
    log_level: 'none',
    core_paired(core) {
      log.info('Roon core paired', { id: core.core_id, name: core.display_name });
      state.core = core;
      state.transport = core.services.RoonApiTransport;
      state.image = core.services.RoonApiImage || null;
      state.coreInfo = {
        id: core.core_id,
        name: core.display_name,
        version: core.display_version,
      };
      state.lastCoreSeen = Date.now();
      state.transportDisconnectedAt = null;
      if (state.coreLossTimer) {
        clearTimeout(state.coreLossTimer);
        state.coreLossTimer = null;
      }
      subscribe(core);
    },
    core_unpaired() {
      log.warn('Roon core disconnected');
      state.core = null;
      state.transport = null;
      state.image = null;
      state.coreInfo = null;
      svc_status.set_status('Waiting for Roon core', true);
      if (state.coreLossTimer) {
        clearTimeout(state.coreLossTimer);
      }
      state.coreLossTimer = setTimeout(() => {
        if (state.core) return;
        log.warn('Core offline for prolonged period, clearing zone cache');
        state.zones = [];
        state.nowPlayingByZone.clear();
        state.pendingRelative.clear();
      }, CORE_LOSS_TIMEOUT_MS);
    },
  });

  // Config persistence for Docker
  roon.save_config = function(k, v) {
    try {
      let config = {};
      try {
        config = JSON.parse(fs.readFileSync(CONFIG_FILE, 'utf8')) || {};
      } catch (e) { /* start fresh */ }
      if (v === undefined || v === null) {
        delete config[k];
      } else {
        config[k] = v;
      }
      fs.writeFileSync(CONFIG_FILE, JSON.stringify(config, null, 2));
    } catch (e) {
      log.error('Failed to save config', { error: e.message });
    }
  };

  roon.load_config = function(k) {
    try {
      const config = JSON.parse(fs.readFileSync(CONFIG_FILE, 'utf8')) || {};
      return config[k];
    } catch (e) {
      return undefined;
    }
  };

  roon.service_port = opts.service_port || 9330;

  const svc_status = new RoonApiStatus(roon);
  svc_status.set_status('Waiting for authorization in Roon → Settings → Extensions', false);

  function subscribe(core) {
    const transport = core.services.RoonApiTransport;
    if (!transport) {
      svc_status.set_status('Transport service unavailable', true);
      return;
    }

    transport.subscribe_zones((msg, data) => {
      if (msg === 'Subscribed' && data?.zones) {
        state.zones = data.zones;
        data.zones.forEach(updateZone);
      } else if (msg === 'Changed') {
        if (Array.isArray(data?.zones_removed)) {
          data.zones_removed.forEach((zone_id) => {
            state.nowPlayingByZone.delete(zone_id);
            state.zones = state.zones.filter((z) => z.zone_id !== zone_id);
          });
        }
        if (Array.isArray(data?.zones_changed)) {
          data.zones_changed.forEach(updateZone);
        }
        if (Array.isArray(data?.zones_seek_changed)) {
          data.zones_seek_changed.forEach((e) => {
            const cached = state.nowPlayingByZone.get(e.zone_id);
            if (cached && e.seek_position != null) {
              cached.seek_position = e.seek_position;
            }
          });
        }
      } else if (msg === 'NetworkError') {
        if (!state.transportDisconnectedAt) {
          state.transportDisconnectedAt = Date.now();
        }
      }
    });

    const statusMsg = baseUrl ? `Connected • ${baseUrl}` : 'Connected to Roon';
    svc_status.set_status(statusMsg, false);
  }

  function updateZone(zone) {
    if (!zone || !zone.zone_id) return;
    const vol = zone.outputs?.[0]?.volume;
    const three = zone.now_playing?.three_line || {};
    const summary = {
      line1: three.line1 || zone.display_name || 'Unknown zone',
      line2: three.line2 || '',
      line3: three.line3 || '',
      is_playing: zone.state === 'playing',
      volume: vol?.value ?? null,
      volume_type: vol?.type || null,
      volume_min: vol?.min ?? -80,
      volume_max: vol?.max ?? 0,
      volume_step: vol?.step ?? 2,
      seek_position: zone.now_playing?.seek_position ?? null,
      length: zone.now_playing?.length ?? null,
      zone_id: zone.zone_id,
      image_key: zone.now_playing?.image_key || null,
    };
    state.nowPlayingByZone.set(zone.zone_id, summary);

    const idx = state.zones.findIndex((z) => z.zone_id === zone.zone_id);
    if (idx >= 0) {
      state.zones[idx] = zone;
    } else {
      state.zones.push(zone);
    }
  }

  function getZones(opts = {}) {
    return state.zones.map((zone) => {
      const output = zone.outputs?.[0];
      const sourceControl = output?.source_controls?.[0];
      const vol = output?.volume;
      const canGroup = output?.can_group_with_output_ids || [];
      const result = {
        zone_id: zone.zone_id,
        zone_name: zone.display_name,
        source: 'roon',
        state: zone.state || 'stopped',
        output_count: zone.outputs?.length || 0,
        output_name: output?.display_name || null,
        device_name: sourceControl?.display_name || null,
        source_control: sourceControl ? {
          status: sourceControl.status,
          supports_standby: sourceControl.supports_standby || false,
          control_key: sourceControl.control_key,
        } : null,
        volume_control: vol ? {
          type: vol.type,
          min: vol.min,
          max: vol.max,
          is_muted: vol.is_muted || false,
        } : null,
        supports_grouping: canGroup.length > 1,
      };
      if (opts.debug) {
        result._raw_output = output || null;
      }
      return result;
    });
  }

  function getNowPlaying(zone_id) {
    if (!zone_id) return null;
    const transportUnavailable = !state.core || !state.transport;
    const withinGracePeriod = state.transportDisconnectedAt &&
      (Date.now() - state.transportDisconnectedAt) < TRANSPORT_GRACE_PERIOD_MS;

    if (transportUnavailable && !withinGracePeriod) {
      return null;
    }

    const cached = state.nowPlayingByZone.get(zone_id);
    if (cached) return cached;
    const zone = state.zones.find((z) => z.zone_id === zone_id);
    if (!zone) return null;
    const fallback = fallbackSummary(zone);
    state.nowPlayingByZone.set(zone_id, fallback);
    return fallback;
  }

  function getImage(image_key, opts = {}) {
    return new Promise((resolve, reject) => {
      if (!state.core || !state.image || !image_key) {
        return reject(new Error('image service unavailable'));
      }
      const options = {};
      if (opts.scale) options.scale = opts.scale;
      if (opts.width) options.width = Number(opts.width);
      if (opts.height) options.height = Number(opts.height);
      if (opts.format) options.format = opts.format;
      state.image.get_image(image_key, options, (err, contentType, body) => {
        if (err) return reject(new Error(String(err)));
        resolve({ contentType, body });
      });
    });
  }

  async function control(zone_id, action, value) {
    if (!state.transport) throw new Error('Roon core unavailable');
    const zone = state.zones.find((z) => z.zone_id === zone_id);
    if (!zone) throw new Error('Zone not found');
    const output = zone.outputs?.[0];
    if (!output) throw new Error('No output for zone');

    switch (action) {
      case 'play_pause':
        await callTransport('control', zone_id, 'playpause');
        break;
      case 'next':
        await callTransport('control', zone_id, 'next');
        break;
      case 'previous':
      case 'prev':
        await callTransport('control', zone_id, 'previous');
        break;
      case 'vol_rel':
        await enqueueRelativeVolume(output.output_id, Number(value) || 0);
        break;
      case 'vol_abs':
        await callTransport('change_volume', output.output_id, 'absolute', clamp(Number(value), MIN_VOLUME, MAX_VOLUME));
        break;
      default:
        throw new Error('Unknown action');
    }
  }

  async function enqueueRelativeVolume(output_id, delta) {
    if (!delta) return;
    const current = state.pendingRelative.get(output_id) || 0;
    state.pendingRelative.set(output_id, clamp(current + delta, -MAX_RELATIVE_STEP_PER_CALL, MAX_RELATIVE_STEP_PER_CALL));
    await flushRelativeQueue(output_id);
  }

  async function flushRelativeQueue(output_id) {
    const pending = state.pendingRelative.get(output_id) || 0;
    if (!pending) return;

    const now = Date.now();
    const lastTick = state.lastVolumeTick.get(output_id) || 0;
    if (now - lastTick < RATE_LIMIT_INTERVAL_MS) {
      setTimeout(() => flushRelativeQueue(output_id), RATE_LIMIT_INTERVAL_MS - (now - lastTick));
      return;
    }

    const step = clamp(pending, -MAX_RELATIVE_STEP_PER_CALL, MAX_RELATIVE_STEP_PER_CALL);
    state.pendingRelative.set(output_id, pending - step);
    state.lastVolumeTick.set(output_id, now);

    await callTransport('change_volume', output_id, 'relative_step', step);

    if (state.pendingRelative.get(output_id)) {
      setTimeout(() => flushRelativeQueue(output_id), RATE_LIMIT_INTERVAL_MS);
    }
  }

  function clamp(value, min, max) {
    if (Number.isNaN(value)) return min;
    return Math.max(min, Math.min(max, value));
  }

  function callTransport(method, ...args) {
    return new Promise((resolve, reject) => {
      state.transport[method](...args, (err) => {
        if (err) return reject(new Error(err));
        resolve();
      });
    });
  }

  function start() {
    roon.init_services({
      required_services: [RoonApiTransport, RoonApiImage],
      provided_services: [svc_status],
    });
    roon.start_discovery();
  }

  return {
    start,
    getZones,
    getNowPlaying,
    getImage,
    control,
    getStatus: () => ({
      connected: !!state.core,
      core: state.coreInfo,
      zone_count: state.zones.length,
      zones: getZones(),
      now_playing: Array.from(state.nowPlayingByZone.entries()).map(([zone_id, np]) => ({ zone_id, ...np })),
    }),
  };
}

module.exports = { createRoonClient };
