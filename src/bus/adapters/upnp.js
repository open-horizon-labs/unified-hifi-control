/**
 * UPnPAdapter - Wraps UPnPClient to implement bus adapter interface
 *
 * Follows RoonAdapter pattern (~80 lines):
 * - Wraps UPnP client
 * - Zone IDs: upnp:{uuid}
 * - Control point role (discover/control renderers)
 */

class UPnPAdapter {
  constructor(upnpClient, { onZonesChanged } = {}) {
    this.upnp = upnpClient;

    // Pass onZonesChanged to client if provided
    if (onZonesChanged && upnpClient.setOnZonesChanged) {
      upnpClient.setOnZonesChanged(onZonesChanged);
    }
  }

  async start() {
    return this.upnp.start();
  }

  async stop() {
    return this.upnp.stop();
  }

  getZones(opts = {}) {
    const zones = this.upnp.getZones(opts);
    return zones.map(zone => ({
      ...zone,
      zone_id: `upnp:${zone.zone_id}`,  // Prefix for routing
    }));
  }

  getNowPlaying(zone_id) {
    const upnpId = zone_id.replace(/^upnp:/, '');
    const state = this.upnp.getNowPlaying(upnpId);
    if (!state) return null;

    return {
      ...state,
      zone_id: `upnp:${state.zone_id}`,  // Prefix zone_id for routing
    };
  }

  async control(zone_id, action, value) {
    const upnpId = zone_id.replace(/^upnp:/, '');
    return this.upnp.control(upnpId, action, value);
  }

  async getImage(image_key, opts = {}, redirectCount = 0) {
    const MAX_REDIRECTS = 5;

    // For OpenHome devices, image_key is a direct URL
    if (image_key && (image_key.startsWith('http://') || image_key.startsWith('https://'))) {
      if (redirectCount >= MAX_REDIRECTS) {
        throw new Error('Too many redirects');
      }

      const https = require('https');
      const http = require('http');
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
            return this.getImage(redirectUrl.href, opts, redirectCount + 1).then(resolve).catch(reject);
          }

          // Check status
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

    throw new Error('Image retrieval not supported for basic UPnP renderers');
  }

  getStatus() {
    const status = { ...this.upnp.getStatus() };

    if (status.renderers) {
      status.renderers = status.renderers.map(r => ({
        ...r,
        zone_id: `upnp:${r.zone_id}`,
      }));
    }

    return status;
  }
}

module.exports = { UPnPAdapter };
