/**
 * LMS Client - Logitech Media Server JSON-RPC protocol implementation
 *
 * LMS uses JSON-RPC over HTTP (port 9000 default)
 * Documentation: http://HOST:9000/html/docs/cli-api.html
 */

const fs = require('fs');
const path = require('path');

// Use node-fetch for pkg compatibility (native fetch doesn't work in pkg bundles)
const fetch = require('node-fetch');
const { getDataDir } = require('../lib/paths');

const CONFIG_DIR = getDataDir();
const LMS_CONFIG_FILE = path.join(CONFIG_DIR, 'lms-config.json');

class LMSClient {
  constructor(opts = {}) {
    this.host = opts.host || null;
    this.port = opts.port || 9000;
    this.username = opts.username || null;
    this.password = opts.password || null;
    this.logger = opts.logger || console;
    this.players = new Map();
    this.pollInterval = opts.pollInterval || 2000;
    this.pollTimer = null;
    this.connected = false;
    this.onZonesChanged = opts.onZonesChanged || null;

    // Load saved config on startup
    this._loadConfig();

    // Set baseUrl after loading config
    if (this.host) {
      this.baseUrl = `http://${this.host}:${this.port}`;
    }
  }

  _loadConfig() {
    try {
      if (fs.existsSync(LMS_CONFIG_FILE)) {
        const saved = JSON.parse(fs.readFileSync(LMS_CONFIG_FILE, 'utf8'));
        if (saved.host) this.host = saved.host;
        if (saved.port) this.port = Number(saved.port);
        if (saved.username) this.username = saved.username;
        if (saved.password) this.password = saved.password;
        this.logger.info('Loaded Lyrion config from disk', { host: this.host, port: this.port, hasAuth: !!this.username });
      }
    } catch (e) {
      this.logger.warn('Failed to load Lyrion config', { error: e.message });
    }
  }

  _saveConfig() {
    try {
      if (!fs.existsSync(CONFIG_DIR)) {
        fs.mkdirSync(CONFIG_DIR, { recursive: true });
      }
      const config = {
        host: this.host,
        port: this.port,
      };
      if (this.username) config.username = this.username;
      if (this.password) config.password = this.password;
      fs.writeFileSync(LMS_CONFIG_FILE, JSON.stringify(config, null, 2));
      this.logger.info('Saved Lyrion config to disk');
    } catch (e) {
      this.logger.error('Failed to save Lyrion config', { error: e.message });
    }
  }

  isConfigured() {
    return !!this.host;
  }

  configure({ host, port, username, password }) {
    this.host = host || this.host;
    this.port = Number(port) || this.port;
    // Allow clearing auth by passing empty string
    if (username !== undefined) this.username = username || null;
    if (password !== undefined) this.password = password || null;
    if (this.host) {
      this.baseUrl = `http://${this.host}:${this.port}`;
    }
    // Persist config to disk
    this._saveConfig();
    // If already connected, stop and allow restart
    if (this.connected) {
      this.stop();
    }
  }

  /**
   * Start polling for players and their status
   */
  async start() {
    if (!this.host) {
      throw new Error('LMS_HOST not configured');
    }

    try {
      await this.updatePlayers();
      this.connected = true;
      this.logger.info('LMS client connected', { host: this.host, port: this.port, players: this.players.size });

      // Force initial zone notification after successful connection
      if (this.onZonesChanged) {
        this.onZonesChanged();
      }

      // Start polling
      this.pollTimer = setInterval(() => {
        this.updatePlayers().catch(err => {
          this.logger.error('Failed to update players', { error: err.message });
        });
      }, this.pollInterval);

    } catch (err) {
      this.logger.error('Failed to connect to LMS', { error: err.message });
      throw err;
    }
  }

  /**
   * Stop polling
   */
  async stop() {
    if (this.pollTimer) {
      clearInterval(this.pollTimer);
      this.pollTimer = null;
    }
    this.connected = false;
  }

  /**
   * Execute JSON-RPC command
   */
  async execute(playerId, params) {
    const url = `${this.baseUrl}/jsonrpc.js`;

    const body = {
      id: 1,
      method: 'slim.request',
      params: playerId ? [playerId, params] : ['', params],
    };

    const headers = { 'Content-Type': 'application/json' };
    if (this.username && this.password) {
      const auth = Buffer.from(`${this.username}:${this.password}`).toString('base64');
      headers['Authorization'] = `Basic ${auth}`;
    }

    const response = await fetch(url, {
      method: 'POST',
      headers,
      body: JSON.stringify(body),
    });

    if (!response.ok) {
      throw new Error(`Lyrion request failed: ${response.status}`);
    }

    const data = await response.json();
    if (data.error) {
      throw new Error(`LMS error: ${JSON.stringify(data.error)}`);
    }

    return data.result;
  }

  /**
   * Get list of all players
   */
  async getPlayers() {
    const result = await this.execute(null, ['players', 0, 100]);
    const players = result.players_loop || [];

    return players.map(p => ({
      playerid: p.playerid,
      name: p.name,
      model: p.model || 'Unknown',
      connected: p.connected === 1,
      power: p.power === 1,
      ip: p.ip,
    }));
  }

  /**
   * Get player status (track, state, volume)
   */
  async getPlayerStatus(playerId) {
    const result = await this.execute(playerId, [
      'status',
      '-',
      1,
      'tags:aAdltKc',  // a=artist, A=album, d=duration, l=album_id, t=tracknum, K=artwork_url, c=coverid
    ]);

    const playlist_loop = result.playlist_loop?.[0] || {};
    const isPlaying = result.mode === 'play';
    const isPaused = result.mode === 'pause';

    // For artwork, LMS returns artwork_url as a relative path like:
    // /imageproxy/https%3A%2F%2Fstatic.qobuz.com%2F.../image.jpg
    // We need to make it absolute by prepending baseUrl
    let artworkUrl = playlist_loop.artwork_url || null;
    if (artworkUrl && artworkUrl.startsWith('/')) {
      artworkUrl = `${this.baseUrl}${artworkUrl}`;
    }

    // coverid can be used for local content: /music/<coverid>/cover
    const artworkId = playlist_loop.coverid || playlist_loop.artwork_track_id || playlist_loop.id;

    return {
      playerid: playerId,
      state: isPlaying ? 'playing' : isPaused ? 'paused' : 'stopped',
      mode: result.mode,
      power: result.power === 1,
      volume: result['mixer volume'],
      playlist_tracks: result.playlist_tracks || 0,
      playlist_cur_index: result.playlist_cur_index,
      time: result.time || 0,
      duration: playlist_loop.duration || 0,
      title: playlist_loop.title || '',
      artist: playlist_loop.artist || '',
      album: playlist_loop.album || '',
      artwork_track_id: artworkId,
      coverid: artworkId,
      artwork_url: artworkUrl,  // Full URL to artwork (via LMS imageproxy for streaming)
    };
  }

  /**
   * Update cached player information
   */
  async updatePlayers() {
    try {
      const players = await this.getPlayers();
      const previousIds = new Set(this.players.keys());

      for (const player of players) {
        try {
          const status = await this.getPlayerStatus(player.playerid);
          this.players.set(player.playerid, {
            ...player,
            ...status,
          });
        } catch (err) {
          this.logger.warn('Failed to get player status', {
            playerId: player.playerid,
            error: err.message,
          });
        }
      }

      // Remove players no longer reported by LMS
      const activeIds = new Set(players.map(p => p.playerid));
      for (const [id] of this.players) {
        if (!activeIds.has(id)) {
          this.players.delete(id);
        }
      }

      // Only notify bus if player set changed (not on every poll)
      const currentIds = new Set(this.players.keys());
      const setChanged = previousIds.size !== currentIds.size ||
        [...previousIds].some(id => !currentIds.has(id)) ||
        [...currentIds].some(id => !previousIds.has(id));

      if (setChanged && this.onZonesChanged) {
        this.onZonesChanged();
      }
    } catch (err) {
      this.logger.error('Failed to update players', { error: err.message });
      throw err;
    }
  }

  /**
   * Control player (play/pause/stop/skip)
   */
  async control(playerId, command, value) {
    switch (command) {
      case 'play':
        await this.execute(playerId, ['play']);
        break;
      case 'pause':
        await this.execute(playerId, ['pause', 1]);
        break;
      case 'stop':
        await this.execute(playerId, ['stop']);
        break;
      case 'play_pause':
        await this.execute(playerId, ['pause']);  // Toggle
        break;
      case 'next':
        await this.execute(playerId, ['playlist', 'index', '+1']);
        break;
      case 'previous':
      case 'prev':
        await this.execute(playerId, ['playlist', 'index', '-1']);
        break;
      case 'volume':
      case 'vol_abs':
        await this.execute(playerId, ['mixer', 'volume', value]);
        break;
      case 'vol_rel':
        await this.execute(playerId, ['mixer', 'volume', `${value > 0 ? '+' : ''}${value}`]);
        break;
      default:
        throw new Error(`Unknown command: ${command}`);
    }

    // Update status after command
    setTimeout(() => {
      this.getPlayerStatus(playerId)
        .then(status => {
          const player = this.players.get(playerId);
          if (player) {
            this.players.set(playerId, { ...player, ...status });
          }
        })
        .catch(err => {
          this.logger.warn('Failed to refresh player after control', { error: err.message });
        });
    }, 100);
  }

  /**
   * Get artwork URL for a track
   * @param {string} coverid - The cover ID from player status
   * @param {object} opts - Options (width, height)
   *
   * LMS supports resizing via URL suffix: /music/{coverid}/cover_WxH.jpg
   * e.g., /music/123/cover_360x360.jpg
   */
  getArtworkUrl(coverid, opts = {}) {
    if (!coverid) return null;

    // Use LMS's built-in resizing with suffix format
    let suffix = 'cover';
    if (opts.width && opts.height) {
      suffix = `cover_${opts.width}x${opts.height}.jpg`;
    } else if (opts.width) {
      suffix = `cover_${opts.width}x${opts.width}.jpg`;
    }

    return `${this.baseUrl}/music/${coverid}/${suffix}`;
  }

  /**
   * Fetch artwork image
   * @param {string} coverid - The cover ID from player status
   * @param {object} opts - Options (width, height)
   */
  async getArtwork(coverid, opts = {}) {
    const url = this.getArtworkUrl(coverid, opts);
    if (!url) {
      throw new Error('No artwork available');
    }

    const fetchOpts = {};
    if (this.username && this.password) {
      const auth = Buffer.from(`${this.username}:${this.password}`).toString('base64');
      fetchOpts.headers = { 'Authorization': `Basic ${auth}` };
    }

    const response = await fetch(url, fetchOpts);
    if (!response.ok) {
      throw new Error(`Failed to fetch artwork: ${response.status}`);
    }

    const contentType = response.headers.get('content-type') || 'image/jpeg';
    const body = Buffer.from(await response.arrayBuffer());

    return { contentType, body };
  }

  /**
   * Get cached player info
   */
  getCachedPlayer(playerId) {
    return this.players.get(playerId) || null;
  }

  /**
   * Get all cached players
   */
  getCachedPlayers() {
    return Array.from(this.players.values());
  }

  /**
   * Get client status
   */
  getStatus() {
    return {
      connected: this.connected,
      host: this.host,
      port: this.port,
      player_count: this.players.size,
      players: Array.from(this.players.values()).map(p => ({
        playerid: p.playerid,
        name: p.name,
        state: p.state,
        connected: p.connected,
      })),
    };
  }
}

module.exports = { LMSClient };
