/**
 * LMS Client - Logitech Media Server JSON-RPC protocol implementation
 *
 * LMS uses JSON-RPC over HTTP (port 9000 default)
 * Documentation: http://HOST:9000/html/docs/cli-api.html
 */

class LMSClient {
  constructor(opts = {}) {
    this.host = opts.host;
    this.port = opts.port || 9000;
    this.logger = opts.logger || console;
    this.baseUrl = `http://${this.host}:${this.port}`;
    this.players = new Map();
    this.pollInterval = opts.pollInterval || 2000;
    this.pollTimer = null;
    this.connected = false;
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
      this.logger.info('LMS client connected', { host: this.host, port: this.port });

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
    const url = playerId
      ? `${this.baseUrl}/jsonrpc.js`
      : `${this.baseUrl}/jsonrpc.js`;

    const body = {
      id: 1,
      method: 'slim.request',
      params: playerId ? [playerId, params] : ['', params],
    };

    const response = await fetch(url, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });

    if (!response.ok) {
      throw new Error(`LMS request failed: ${response.status}`);
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
      'tags:aAldtK',  // Request album, artist, duration, title, artwork
    ]);

    const playlist_loop = result.playlist_loop?.[0] || {};
    const isPlaying = result.mode === 'play';
    const isPaused = result.mode === 'pause';
    const isStopped = result.mode === 'stop';

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
      artwork_track_id: playlist_loop.artwork_track_id || playlist_loop.id,
      coverid: playlist_loop.coverid,
    };
  }

  /**
   * Update cached player information
   */
  async updatePlayers() {
    try {
      const players = await this.getPlayers();

      for (const player of players) {
        if (!player.connected) continue;

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

      // Remove disconnected players
      const activeIds = new Set(players.filter(p => p.connected).map(p => p.playerid));
      for (const [id] of this.players) {
        if (!activeIds.has(id)) {
          this.players.delete(id);
        }
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
   */
  getArtworkUrl(playerId, coverid, opts = {}) {
    if (!coverid) return null;

    const params = new URLSearchParams();
    if (opts.width) params.set('width', opts.width);
    if (opts.height) params.set('height', opts.height);

    const query = params.toString();
    const url = `${this.baseUrl}/music/${coverid}/cover${query ? '?' + query : ''}`;

    return url;
  }

  /**
   * Fetch artwork image
   */
  async getArtwork(playerId, coverid, opts = {}) {
    const url = this.getArtworkUrl(playerId, coverid, opts);
    if (!url) {
      throw new Error('No artwork available');
    }

    const response = await fetch(url);
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
