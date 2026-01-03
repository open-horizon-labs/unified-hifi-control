/**
 * LMSAdapter - Wraps LMSClient to implement bus adapter interface
 *
 * Pattern follows RoonAdapter (src/bus/adapters/roon.js):
 * - getZones() returns flattened structure with lms: prefix
 * - getNowPlaying() returns summary with image_key
 * - getImage() takes image_key (coverid), returns {contentType, body}
 * - control() maps bus actions to LMS commands
 */

class LMSAdapter {
  constructor(lmsClient, { onZonesChanged } = {}) {
    this.lms = lmsClient;
    this.onZonesChanged = onZonesChanged;
  }

  async start() {
    return this.lms.start();
  }

  async stop() {
    return this.lms.stop();
  }

  getZones(opts = {}) {
    const players = this.lms.getCachedPlayers();
    return players.map(player => ({
      zone_id: `lms:${player.playerid}`,
      zone_name: player.name,
      source: 'lms',
      state: player.state || 'stopped',
      output_count: 1,
      output_name: player.model,
      device_name: player.name,
      volume_control: {
        type: 'number',
        min: 0,
        max: 100,
        is_muted: false,
      },
      supports_grouping: false,
    }));
  }

  getNowPlaying(zone_id) {
    const playerId = zone_id.replace(/^lms:/, '');
    const player = this.lms.getCachedPlayer(playerId);
    if (!player) return null;

    return {
      zone_id: `lms:${player.playerid}`,
      line1: player.title || 'No track',
      line2: player.artist || '',
      line3: player.album || '',
      is_playing: player.state === 'playing',
      volume: player.volume,
      volume_type: 'number',
      volume_min: 0,
      volume_max: 100,
      volume_step: 1,
      seek_position: player.time || 0,
      length: player.duration || 0,
      image_key: player.coverid || player.artwork_track_id || null,
    };
  }

  async control(zone_id, action, value) {
    const playerId = zone_id.replace(/^lms:/, '');
    return this.lms.control(playerId, action, value);
  }

  async getImage(image_key, opts = {}) {
    // image_key is coverid - we need to find the player
    // For simplicity, use the first player or extract from context
    const players = this.lms.getCachedPlayers();
    if (players.length === 0) {
      throw new Error('No players available');
    }

    // Use first player's ID for artwork URL construction
    const playerId = players[0].playerid;
    return this.lms.getArtwork(playerId, image_key, opts);
  }

  getStatus() {
    const status = this.lms.getStatus();
    return {
      ...status,
      zones: this.getZones(),
      now_playing: this.lms.getCachedPlayers().map(player => ({
        zone_id: `lms:${player.playerid}`,
        line1: player.title || 'No track',
        line2: player.artist || '',
        is_playing: player.state === 'playing',
      })),
    };
  }
}

module.exports = { LMSAdapter };
