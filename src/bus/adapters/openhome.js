/**
 * OpenHomeAdapter - Wraps OpenHome client for bus integration
 */

class OpenHomeAdapter {
  constructor(openHomeClient, { onZonesChanged } = {}) {
    this.client = openHomeClient;
    this.onZonesChanged = onZonesChanged;
  }

  async start() {
    return this.client.start();
  }

  async stop() {
    return this.client.stop();
  }

  getZones(opts = {}) {
    const zones = this.client.getZones(opts);
    return zones.map(zone => ({
      ...zone,
      zone_id: `openhome:${zone.zone_id}`,  // Add prefix
    }));
  }

  getNowPlaying(zone_id) {
    const id = zone_id.replace(/^openhome:/, '');
    const np = this.client.getNowPlaying(id);
    if (!np) return null;

    return {
      ...np,
      zone_id: `openhome:${np.zone_id}`,
    };
  }

  async control(zone_id, action, value) {
    const id = zone_id.replace(/^openhome:/, '');
    return this.client.control(id, action, value);
  }

  async getImage(image_key, opts = {}) {
    return this.client.getImage(image_key, opts);
  }

  getStatus() {
    return { ...this.client.getStatus() };
  }
}

module.exports = { OpenHomeAdapter };
