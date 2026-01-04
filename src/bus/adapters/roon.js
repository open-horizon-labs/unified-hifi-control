/**
 * RoonAdapter - Wraps RoonClient to implement bus adapter interface
 *
 * Evidence-based implementation:
 * - getZones() returns flattened structure (src/roon/client.js:203-235)
 * - getNowPlaying() returns summary with image_key (src/roon/client.js:178-192)
 * - getImage() takes image_key, returns {contentType, body} (src/roon/client.js:256-270)
 * - image_key is opaque content identifier, NOT prefixed (kept backend-specific)
 */

class RoonAdapter {
  constructor(roonClient, { onZonesChanged } = {}) {
    this.roon = roonClient;
    this.onZonesChanged = onZonesChanged;
  }

  async start() {
    return this.roon.start();
  }

  async stop() {
    // RoonClient doesn't expose stop/disconnect
    // Cleanup happens via process exit
  }

  getZones(opts = {}) {
    const zones = this.roon.getZones(opts);
    return zones.map(zone => ({
      ...zone,
      zone_id: `roon:${zone.zone_id}`,  // Prefix for routing
    }));
  }

  getNowPlaying(zone_id) {
    const roonId = zone_id.replace(/^roon:/, '');
    const state = this.roon.getNowPlaying(roonId);
    if (!state) return null;

    return {
      ...state,
      zone_id: `roon:${state.zone_id}`,  // Prefix zone_id for routing
      image_key: state.image_key,         // Keep image_key opaque
    };
  }

  async control(zone_id, action, value) {
    const roonId = zone_id.replace(/^roon:/, '');
    return this.roon.control(roonId, action, value);
  }

  async getImage(image_key, opts = {}) {
    // image_key is opaque - pass through unchanged
    return this.roon.getImage(image_key, opts);
  }

  getStatus() {
    const status = { ...this.roon.getStatus() };

    if (status.zones) {
      status.zones = status.zones.map(z => ({
        ...z,
        zone_id: `roon:${z.zone_id}`,
      }));
    }

    if (status.now_playing) {
      status.now_playing = status.now_playing.map(np => ({
        ...np,
        zone_id: `roon:${np.zone_id}`,
      }));
    }

    return status;
  }
}

module.exports = { RoonAdapter };
