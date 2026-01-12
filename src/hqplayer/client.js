const http = require('http');
const crypto = require('crypto');
const fs = require('fs');
const path = require('path');
const { HQPNativeClient, discoverHQPlayers } = require('./native-client');
const { getDataDir } = require('../lib/paths');

const PROFILE_PATH = '/config/profile/load';
const CONFIG_DIR = getDataDir();
const HQP_CONFIG_FILE = path.join(CONFIG_DIR, 'hqp-config.json');

class HQPClient {
  constructor({ host, port = 8088, username, password, logger } = {}) {
    this.host = host || null;
    this.port = Number(port) || 8088;  // Web UI port
    this.username = username || '';
    this.password = password || '';
    this.log = logger || console;
    this.cookies = {};
    this.digest = null;
    this.lastHiddenFields = {};
    this.lastProfiles = [];
    this.lastConfigTitle = null;

    // Native protocol client (port 4321) - used for pipeline control
    this.native = new HQPNativeClient({ logger: this.log });

    // Handle native client errors gracefully (don't crash if HQPlayer unavailable)
    this.native.on('error', (err) => {
      this.log.warn('HQPlayer native client error (non-fatal)', { error: err.message });
    });

    // Load saved config on startup
    this._loadConfig();
  }

  _loadConfig() {
    try {
      if (fs.existsSync(HQP_CONFIG_FILE)) {
        const saved = JSON.parse(fs.readFileSync(HQP_CONFIG_FILE, 'utf8'));
        if (saved.host) this.host = saved.host;
        if (saved.port) this.port = Number(saved.port);
        if (saved.username) this.username = saved.username;
        if (saved.password) this.password = saved.password;
        // Configure native client with same host
        if (this.host) {
          this.native.configure({ host: this.host });
        }
        this.log.info('Loaded HQPlayer config from disk', { host: this.host, port: this.port });
      }
    } catch (e) {
      this.log.warn('Failed to load HQPlayer config', { error: e.message });
    }
  }

  _saveConfig() {
    try {
      if (!fs.existsSync(CONFIG_DIR)) {
        fs.mkdirSync(CONFIG_DIR, { recursive: true });
      }
      fs.writeFileSync(HQP_CONFIG_FILE, JSON.stringify({
        host: this.host,
        port: this.port,
        username: this.username,
        password: this.password,
      }, null, 2));
      this.log.info('Saved HQPlayer config to disk');
    } catch (e) {
      this.log.error('Failed to save HQPlayer config', { error: e.message });
    }
  }

  isConfigured() {
    // Native protocol only needs host; web creds optional (for profiles)
    return !!this.host;
  }

  hasWebCredentials() {
    return !!(this.host && this.username && this.password);
  }

  configure({ host, port, username, password }) {
    this.host = host || this.host;
    this.port = Number(port) || this.port;
    this.username = username || this.username;
    this.password = password || this.password;
    // Configure native client
    if (this.host) {
      this.native.configure({ host: this.host });
    }
    // Reset auth state when reconfiguring
    this.cookies = {};
    this.digest = null;
    // Persist config to disk
    this._saveConfig();
  }

  baseHeaders() {
    return {
      Accept: 'text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8',
      Connection: 'keep-alive',
      'User-Agent': 'UnifiedHiFiControl/1.0',
    };
  }

  serializeCookies() {
    const entries = Object.entries(this.cookies);
    if (!entries.length) return '';
    return entries.map(([name, value]) => `${name}=${value}`).join('; ');
  }

  collectCookies(headers) {
    const setCookie = headers['set-cookie'];
    if (!setCookie) return;
    setCookie.forEach((raw) => {
      const [cookie] = raw.split(';');
      const [name, value] = cookie.split('=');
      if (name && value !== undefined) {
        this.cookies[name.trim()] = value.trim();
      }
    });
  }

  parseDigest(header) {
    const challenge = header.replace(/^Digest\s+/i, '');
    const parts = {};
    challenge.split(/,\s*/).forEach((chunk) => {
      const eq = chunk.indexOf('=');
      if (eq === -1) return;
      const key = chunk.slice(0, eq).trim();
      let value = chunk.slice(eq + 1).trim();
      value = value.replace(/^"|"$/g, '');
      parts[key] = value;
    });
    this.digest = {
      realm: parts.realm || '',
      nonce: parts.nonce || '',
      qop: parts.qop || '',
      opaque: parts.opaque || '',
      algorithm: (parts.algorithm || 'MD5').toUpperCase(),
      nc: 0,
    };
  }

  md5(value) {
    return crypto.createHash('md5').update(value).digest('hex');
  }

  buildDigestHeader(method, uri) {
    if (!this.digest || !this.digest.nonce) return '';

    const { realm, nonce, qop, opaque, algorithm } = this.digest;
    const { username, password } = this;

    this.digest.nc += 1;
    const nc = this.digest.nc.toString(16).padStart(8, '0');
    const cnonce = crypto.randomBytes(8).toString('hex');

    let ha1;
    if (algorithm === 'MD5-SESS') {
      const initial = this.md5(`${username}:${realm}:${password}`);
      ha1 = this.md5(`${initial}:${nonce}:${cnonce}`);
    } else {
      ha1 = this.md5(`${username}:${realm}:${password}`);
    }

    const ha2 = this.md5(`${method}:${uri}`);
    let response;

    if (qop) {
      const qopValue = qop.split(',')[0].trim();
      response = this.md5(`${ha1}:${nonce}:${nc}:${cnonce}:${qopValue}:${ha2}`);
      return [
        `Digest username="${username}"`,
        `realm="${realm}"`,
        `nonce="${nonce}"`,
        `uri="${uri}"`,
        `algorithm=${algorithm}`,
        `response="${response}"`,
        `qop=${qopValue}`,
        `nc=${nc}`,
        `cnonce="${cnonce}"`,
        opaque ? `opaque="${opaque}"` : null,
      ]
        .filter(Boolean)
        .join(', ');
    }

    response = this.md5(`${ha1}:${nonce}:${ha2}`);
    return [
      `Digest username="${username}"`,
      `realm="${realm}"`,
      `nonce="${nonce}"`,
      `uri="${uri}"`,
      `algorithm=${algorithm}`,
      `response="${response}"`,
      opaque ? `opaque="${opaque}"` : null,
    ]
      .filter(Boolean)
      .join(', ');
  }

  makeRequest(path, { method = 'GET', headers = {}, body } = {}) {
    if (!this.host) {
      return Promise.reject(new Error('HQPlayer host not configured'));
    }
    const options = {
      hostname: this.host,
      port: this.port,
      path,
      method,
      headers,
    };

    return new Promise((resolve, reject) => {
      const req = http.request(options, (res) => {
        const chunks = [];
        res.on('data', (chunk) => chunks.push(chunk));
        res.on('end', () => {
          const buffer = Buffer.concat(chunks);
          resolve({
            statusCode: res.statusCode || 0,
            headers: res.headers,
            body: buffer.toString('utf8'),
          });
        });
      });

      req.on('error', reject);
      if (body) req.write(body);
      req.end();
    });
  }

  async request(path, { method = 'GET', headers = {}, body } = {}) {
    const payload = body || null;
    let mergedHeaders = { ...this.baseHeaders(), ...headers };
    const cookieHeader = this.serializeCookies();
    if (cookieHeader) mergedHeaders.Cookie = cookieHeader;

    if (payload && !mergedHeaders['Content-Length']) {
      mergedHeaders['Content-Length'] = Buffer.byteLength(payload);
    }

    if (this.digest && this.digest.nonce) {
      const authHeader = this.buildDigestHeader(method, path);
      if (authHeader) mergedHeaders.Authorization = authHeader;
    }

    let response = await this.makeRequest(path, { method, headers: mergedHeaders, body: payload });
    this.collectCookies(response.headers);

    if (response.statusCode === 401) {
      const authHeader = response.headers['www-authenticate'];
      if (authHeader && /digest/i.test(authHeader)) {
        this.parseDigest(authHeader);
        const retryHeaders = { ...headers };
        const cookies = this.serializeCookies();
        mergedHeaders = { ...this.baseHeaders(), ...retryHeaders };
        if (cookies) mergedHeaders.Cookie = cookies;
        const digestHeader = this.buildDigestHeader(method, path);
        if (digestHeader) mergedHeaders.Authorization = digestHeader;
        if (payload && !mergedHeaders['Content-Length']) {
          mergedHeaders['Content-Length'] = Buffer.byteLength(payload);
        }
        response = await this.makeRequest(path, { method, headers: mergedHeaders, body: payload });
        this.collectCookies(response.headers);
      }
    }

    return response;
  }

  getAttribute(tag, attribute) {
    const regex = new RegExp(`${attribute}\\s*=\\s*(?:"([^"]*)"|'([^']*)'|([^\\s>]+))`, 'i');
    const match = tag.match(regex);
    if (!match) return '';
    return match[1] || match[2] || match[3] || '';
  }

  parseHiddenInputs(html) {
    const payload = {};
    const inputRegex = /<input[^>]*name\s*=\s*["']([^"'>\s]+)["'][^>]*>/gi;
    let match;
    while ((match = inputRegex.exec(html)) !== null) {
      const tag = match[0];
      const name = match[1];
      const type = this.getAttribute(tag, 'type').toLowerCase();
      if (type === 'hidden' || name === '_xsrf') {
        payload[name] = this.getAttribute(tag, 'value') || '';
      }
    }
    return payload;
  }

  parseProfiles(html) {
    const selectMatch = html.match(
      /<select[^>]*name\s*=\s*["']profile["'][^>]*>([\s\S]*?)<\/select>/i
    );
    if (!selectMatch) return [];

    const content = selectMatch[1];
    const options = [];
    const optionRegex = /<option([^>]*)>([\s\S]*?)<\/option>/gi;
    let optionMatch;
    while ((optionMatch = optionRegex.exec(content)) !== null) {
      const text = optionMatch[2].replace(/\s+/g, ' ').trim();
      const rawValue = this.getAttribute(optionMatch[0], 'value') || text;
      const value = rawValue ? String(rawValue).trim() : '';
      options.push({
        value,
        title: text || value || 'Unnamed profile',
      });
    }
    return this.sanitizeProfiles(options);
  }

  sanitizeProfiles(list) {
    if (!Array.isArray(list)) return [];
    return list
      .map((entry) => {
        if (!entry || typeof entry !== 'object') return null;
        const raw = entry.value != null ? String(entry.value) : '';
        const value = raw.trim();
        const slug = value.toLowerCase().replace(/[^a-z0-9]+/g, '');
        if (!value || !slug.length || slug === 'default') {
          return null;
        }
        const title = entry.title && String(entry.title).trim();
        return { value, title: title || value || 'Unnamed profile' };
      })
      .filter(Boolean);
  }

  async fetchProfiles() {
    const response = await this.request(PROFILE_PATH);
    if (response.statusCode >= 400) {
      throw new Error(`Failed to load profile form (${response.statusCode}).`);
    }

    this.lastHiddenFields = this.parseHiddenInputs(response.body);
    this.lastProfiles = this.parseProfiles(response.body);

    return this.lastProfiles;
  }

  parseSelectOptions(html, selectName) {
    const selectRegex = new RegExp(
      `<select[^>]*name\\s*=\\s*["']${selectName}["'][^>]*>([\\s\\S]*?)<\\/select>`,
      'i'
    );
    const selectMatch = html.match(selectRegex);
    if (!selectMatch) return { selected: { value: '', label: '' }, options: [] };

    const content = selectMatch[1];
    const options = [];
    let selected = { value: '', label: '' };

    const optionRegex = /<option([^>]*)>([\s\S]*?)<\/option>/gi;
    let match;
    while ((match = optionRegex.exec(content)) !== null) {
      const attrs = match[1];
      const label = match[2].replace(/\s+/g, ' ').trim();
      const value = this.getAttribute(match[0], 'value') || label;
      const opt = { value, label };
      options.push(opt);
      if (/selected/i.test(attrs)) {
        selected = opt;
      }
    }

    if (!selected.value && options.length > 0) {
      selected = options[0];
    }

    return { selected, options };
  }

  parseStatusTable(html) {
    const tableRegex = /<tr><th>State<\/th>.*?<\/tr>\s*<tr><td>(.*?)<\/td><td>(.*?)<\/td><td>(.*?)<\/td><td>(.*?)<\/td><td>(.*?)<\/td><td>(.*?)<\/td><td>(.*?)<\/td><td>(.*?)<\/td><td>(.*?)<\/td><\/tr>/is;
    const match = html.match(tableRegex);
    if (!match) return null;
    return {
      state: match[1].trim(),
      track: match[2].trim(),
      tracks: match[3].trim(),
      limits: match[4].trim(),
      activeMode: match[5].trim(),
      activeFilter: match[6].trim(),
      activeShaper: match[7].trim(),
      output: match[8].trim(),
      offload: match[9].trim(),
    };
  }

  parseVolume(html) {
    const volumeRegex = /<input[^>]*name\s*=\s*["']volume["'][^>]*>/i;
    const match = html.match(volumeRegex);
    if (!match) return null;

    const tag = match[0];
    const value = parseFloat(this.getAttribute(tag, 'value')) || 0;
    const min = parseFloat(this.getAttribute(tag, 'min')) || -60;
    const max = parseFloat(this.getAttribute(tag, 'max')) || 0;
    const isFixed = (max - min) <= 6;

    return { value, min, max, isFixed };
  }

  async fetchConfigTitle() {
    try {
      const response = await this.request('/config');
      if (response.statusCode >= 400) {
        throw new Error(`Failed to load config page (${response.statusCode}).`);
      }

      const titleMatch = response.body.match(/<input[^>]*name\s*=\s*["']title["'][^>]*>/i);
      if (!titleMatch) return this.lastConfigTitle;

      const title = this.getAttribute(titleMatch[0], 'value') || null;
      if (title) {
        this.lastConfigTitle = title;
      }
      return title;
    } catch (err) {
      // Return cached value on error (e.g., during HQPlayer restart)
      return this.lastConfigTitle;
    }
  }

  async fetchPipeline() {
    // Use native protocol (port 4321) for pipeline status
    return this.native.getPipelineStatus();
  }

  async setPipelineSetting(name, value) {
    // Use native protocol for pipeline changes
    // Note: UI sends VALUES (from option.value), but native protocol expects INDICES
    // (array positions). We need to convert value → index for mode/filter/shaper.
    // Samplerate is the exception - UI already sends index.
    const numValue = Number(value);

    switch (name) {
      case 'mode': {
        // Convert value (-1, 0, 1) to index (0, 1, 2)
        const modes = await this.native.getModes();
        const mode = modes.find(m => m.value === numValue);
        if (!mode) throw new Error(`Invalid mode value: ${value}`);
        return this.native.setMode(mode.index);
      }

      case 'filter1x': {
        // Convert value to index, keep current Nx filter
        const [filters, state] = await Promise.all([
          this.native.getFilters(),
          this.native.getState(),
        ]);
        const filter = filters.find(f => f.value === numValue);
        if (!filter) throw new Error(`Invalid filter value: ${value}`);
        return this.native.setFilter(state.filterNx ?? state.filter, filter.index);
      }

      case 'filterNx': {
        // Convert value to index, keep current 1x filter
        const [filters, state] = await Promise.all([
          this.native.getFilters(),
          this.native.getState(),
        ]);
        const filter = filters.find(f => f.value === numValue);
        if (!filter) throw new Error(`Invalid filter value: ${value}`);
        return this.native.setFilter(filter.index, state.filter1x ?? state.filter);
      }

      case 'shaper': {
        // Convert value to index
        const shapers = await this.native.getShapers();
        const shaper = shapers.find(s => s.value === numValue);
        if (!shaper) throw new Error(`Invalid shaper value: ${value}`);
        return this.native.setShaping(shaper.index);
      }

      case 'samplerate':
        // Samplerate UI already sends index, not value
        return this.native.setRate(numValue);

      default:
        throw new Error(`Unknown setting: ${name}`);
    }
  }

  async getMatrixProfiles() {
    return this.native.getMatrixProfiles();
  }

  async getMatrixProfile() {
    return this.native.getMatrixProfile();
  }

  async setMatrixProfile(value) {
    return this.native.setMatrixProfile(value);
  }

  async setVolume(value) {
    const pipeline = await this.fetchPipeline();
    if (!pipeline) {
      throw new Error('Pipeline unavailable');
    }
    if (pipeline.volume?.isFixed) {
      throw new Error('Volume is fixed in current profile');
    }
    return this.native.setVolume(Number(value));
  }

  async loadProfile(profileValue) {
    if (!profileValue || String(profileValue).trim().toLowerCase() === 'default') {
      throw new Error('Profile value is required');
    }

    if (!this.lastProfiles.length || !Object.keys(this.lastHiddenFields).length) {
      await this.fetchProfiles();
    }

    const payload = { ...this.lastHiddenFields, profile: profileValue };
    const encoded = new URLSearchParams(payload).toString();

    const response = await this.request(PROFILE_PATH, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/x-www-form-urlencoded',
        Origin: `http://${this.host}:${this.port}`,
        Referer: `http://${this.host}:${this.port}${PROFILE_PATH}`,
      },
      body: encoded,
    });

    if (response.statusCode >= 400) {
      throw new Error(`Profile load failed (${response.statusCode}).`);
    }

    return true;
  }

  async getStatus() {
    if (!this.isConfigured()) {
      return { enabled: false, message: 'HQPlayer not configured' };
    }

    try {
      // Get info and pipeline via native protocol
      const [info, pipeline] = await Promise.all([
        this.native.getInfo().catch(() => null),
        this.fetchPipeline().catch(() => null),
      ]);

      const isEmbedded = info?.product?.toLowerCase().includes('embedded');

      // Fetch profiles and current config name if we have web creds AND it's Embedded
      let profiles = [];
      let configName = null;
      if (isEmbedded && this.hasWebCredentials()) {
        [profiles, configName] = await Promise.all([
          this.fetchProfiles().catch(() => this.lastProfiles),
          this.fetchConfigTitle().catch(() => this.lastConfigTitle),
        ]);
      }

      return {
        enabled: true,
        connected: true,
        host: this.host,
        port: this.port,
        product: info?.product || null,
        version: info?.version || null,
        isEmbedded,
        supportsProfiles: isEmbedded && this.hasWebCredentials(),
        configName,
        profiles,
        pipeline,
      };
    } catch (err) {
      return {
        enabled: true,
        connected: false,
        host: this.host,
        port: this.port,
        error: err.message,
      };
    }
  }

  // Expose discovery for finding HQPlayers on the network
  static discover(timeout = 3000) {
    return discoverHQPlayers(timeout);
  }
}

module.exports = { HQPClient };
