/**
 * HQPService - Manages zone linkage between primary zones and HQPlayer instances
 *
 * HQPlayer is a DSP service, not a zone backend. This service:
 * - Maps primary zones (roon:*, lms:*, openhome:*) to HQP instances
 * - Enriches zone state with HQP pipeline data
 *
 * Does NOT manage HQP instances (that's in index.js via hqpInstances Map)
 * Persistence is handled by caller (settings can be added later)
 */

class HQPService {
  constructor({ instances, logger } = {}) {
    if (!instances) {
      throw new Error('HQPService requires instances Map');
    }
    this.instances = instances; // Reference to hqpInstances Map from index.js
    this.zoneLinks = new Map(); // zone_id -> instanceName
    this.log = logger || console;
  }

  /**
   * Link a primary zone to an HQP instance
   */
  linkZone(zone_id, instanceName) {
    if (!this.instances.has(instanceName)) {
      throw new Error(`Unknown HQP instance: ${instanceName}`);
    }
    this.zoneLinks.set(zone_id, instanceName);
    this.log.info('Zone linked to HQP instance', { zone_id, instance: instanceName });
  }

  /**
   * Unlink a zone from HQP
   */
  unlinkZone(zone_id) {
    const wasLinked = this.zoneLinks.delete(zone_id);
    if (wasLinked) {
      this.log.info('Zone unlinked from HQP', { zone_id });
    }
    return wasLinked;
  }

  /**
   * Get HQP instance name for a zone
   */
  getInstanceForZone(zone_id) {
    return this.zoneLinks.get(zone_id);
  }

  /**
   * Get all zone links
   */
  getLinks() {
    return Array.from(this.zoneLinks.entries()).map(([zone_id, instanceName]) => ({
      zone_id,
      instance: instanceName,
    }));
  }

  /**
   * Fetch HQP pipeline data for a linked zone
   * Returns null if zone not linked or HQP not configured
   */
  async getPipelineForZone(zone_id) {
    const instanceName = this.zoneLinks.get(zone_id);
    if (!instanceName) {
      return null;
    }

    const instance = this.instances.get(instanceName);
    if (!instance) {
      this.log.warn('Zone linked to unknown HQP instance', { zone_id, instance: instanceName });
      return null;
    }

    const { client } = instance;
    if (!client?.isConfigured()) {
      return null;
    }

    try {
      const pipeline = await client.fetchPipeline();
      return {
        instance: instanceName,
        ...pipeline,
      };
    } catch (err) {
      this.log.error('Failed to fetch HQP pipeline', {
        zone_id,
        instance: instanceName,
        error: err.message,
      });
      return null;
    }
  }

  /**
   * Load zone links from object (for settings integration)
   */
  loadLinks(links) {
    this.zoneLinks.clear();
    if (!links) return;

    Object.entries(links).forEach(([zone_id, instanceName]) => {
      if (this.instances.has(instanceName)) {
        this.zoneLinks.set(zone_id, instanceName);
      } else {
        this.log.warn('Skipping link to unknown HQP instance', { zone_id, instance: instanceName });
      }
    });
    this.log.info('Loaded zone links', { count: this.zoneLinks.size });
  }

  /**
   * Get zone links as plain object (for settings serialization)
   */
  saveLinks() {
    return Object.fromEntries(this.zoneLinks);
  }
}

module.exports = { HQPService };
