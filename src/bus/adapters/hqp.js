/**
 * HQPAdapter - Wraps HQPClient to implement bus adapter interface
 *
 * Supports multiple HQPlayer instances (e.g., Embedded + Desktop simultaneously).
 * Each instance has a name property used for zone identification.
 *
 * Zone IDs follow pattern: hqp:{instance_name} (e.g., hqp:embedded, hqp:desktop)
 */

class HQPAdapter {
  constructor(hqpClient, { instanceName, onZonesChanged } = {}) {
    if (!instanceName) {
      throw new Error('HQPAdapter requires instanceName');
    }
    this.hqp = hqpClient;
    this.instanceName = instanceName;
    this.onZonesChanged = onZonesChanged;
  }

  async start() {
    // HQPClient doesn't require explicit start
    // Connection happens on first request
  }

  async stop() {
    // HQPClient doesn't expose stop/disconnect
    // Cleanup happens via process exit
    if (this.hqp.native) {
      this.hqp.native.disconnect();
    }
  }

  getZones(opts = {}) {
    // HQP is a DSP service, not a zone backend
    // It doesn't expose standalone zones - instead, it enriches primary zones
    // Zone linkage is configured via settings and applied in bus.getNowPlaying()
    return [];
  }

  async getNowPlaying(zone_id) {
    const expectedZoneId = `hqp:${this.instanceName}`;
    if (zone_id !== expectedZoneId) {
      return null;
    }

    if (!this.hqp.isConfigured()) {
      return null;
    }

    // Fetch pipeline to get volume information
    const pipeline = await this.hqp.fetchPipeline();

    // Return now playing state with volume
    // (HQPlayer doesn't provide detailed track info via native protocol)
    return {
      zone_id,
      state: 'unknown',
      instance: this.instanceName,
      volume: pipeline?.volume ? {
        current: pipeline.volume.value,
        min: pipeline.volume.min ?? -80,
        max: pipeline.volume.max ?? 0,
        type: 'db',
        is_muted: false,
      } : undefined,
    };
  }

  async control(zone_id, action, value) {
    const expectedZoneId = `hqp:${this.instanceName}`;
    if (zone_id !== expectedZoneId) {
      throw new Error(`Zone mismatch: expected ${expectedZoneId}, got ${zone_id}`);
    }

    if (!this.hqp.isConfigured()) {
      throw new Error('HQPlayer not configured');
    }

    // Map actions to HQPClient methods
    switch (action) {
      case 'play':
        return this.hqp.native.play();
      case 'pause':
        return this.hqp.native.pause();
      case 'stop':
        return this.hqp.native.stop();
      case 'next':
        return this.hqp.native.next();
      case 'previous':
      case 'prev':
        return this.hqp.native.previous();
      case 'vol_abs':
        return this.hqp.setVolume(value);
      case 'vol_rel':
        // Relative volume change
        if (value > 0) {
          return this.hqp.native.volumeUp();
        } else if (value < 0) {
          return this.hqp.native.volumeDown();
        }
        return;
      default:
        throw new Error(`Unknown action: ${action}`);
    }
  }

  getStatus() {
    // Return sync status (async details fetched separately via getExtendedStatus)
    return {
      instance: this.instanceName,
      configured: this.hqp.isConfigured(),
      host: this.hqp.host || null,
      port: this.hqp.port || null,
    };
  }

  /**
   * Extended status with async pipeline/profile info
   * Not part of standard adapter interface, but useful for HQP-specific routes
   */
  async getExtendedStatus() {
    return this.hqp.getStatus();
  }
}

module.exports = { HQPAdapter };
