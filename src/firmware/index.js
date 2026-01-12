const https = require('https');
const fs = require('fs');
const path = require('path');
const { MAX_REDIRECTS } = require('../lib/constants');
const { getDataDir } = require('../lib/paths');

const DEFAULT_POLL_INTERVAL_MINUTES = 360; // 6 hours
const DEFAULT_POLL_INTERVAL_MS = DEFAULT_POLL_INTERVAL_MINUTES * 60 * 1000;
const REQUEST_TIMEOUT_MS = 30000; // 30 seconds

/**
 * FirmwareService - Polls GitHub for new firmware releases and notifies via bus
 */
function createFirmwareService({ logger, pollIntervalMs } = {}) {
  const log = logger || console;
  let pollTimer = null;
  let lastKnownVersion = null;
  let isStarted = false;
  let checkInProgress = false;
  const eventListeners = [];

  // Config from environment
  const CONFIG_DIR = getDataDir();
  const FIRMWARE_DIR = process.env.FIRMWARE_DIR || path.join(CONFIG_DIR, 'firmware');
  const GITHUB_REPO = process.env.FIRMWARE_REPO || 'muness/roon-knob';

  // Support both minutes (new, preferred) and milliseconds (legacy)
  let POLL_INTERVAL;
  let configSource;

  if (pollIntervalMs) {
    POLL_INTERVAL = pollIntervalMs;
    configSource = 'constructor';
  } else if (process.env.FIRMWARE_POLL_INTERVAL_MINUTES) {
    const minutes = parseInt(process.env.FIRMWARE_POLL_INTERVAL_MINUTES, 10);
    if (isNaN(minutes) || minutes <= 0) {
      log.warn(`Invalid FIRMWARE_POLL_INTERVAL_MINUTES: "${process.env.FIRMWARE_POLL_INTERVAL_MINUTES}", using default: ${DEFAULT_POLL_INTERVAL_MINUTES} minutes`);
      POLL_INTERVAL = DEFAULT_POLL_INTERVAL_MS;
      configSource = 'default';
    } else {
      POLL_INTERVAL = minutes * 60 * 1000;
      configSource = `env (${minutes} minutes)`;
    }
  } else if (process.env.FIRMWARE_POLL_INTERVAL_MS) {
    // Legacy: still support milliseconds for backward compatibility
    const ms = parseInt(process.env.FIRMWARE_POLL_INTERVAL_MS, 10);
    if (isNaN(ms) || ms <= 0) {
      log.warn(`Invalid FIRMWARE_POLL_INTERVAL_MS: "${process.env.FIRMWARE_POLL_INTERVAL_MS}", using default: ${DEFAULT_POLL_INTERVAL_MS}ms`);
      POLL_INTERVAL = DEFAULT_POLL_INTERVAL_MS;
      configSource = 'default';
    } else {
      POLL_INTERVAL = ms;
      configSource = `env legacy (${ms}ms)`;
    }
  } else {
    POLL_INTERVAL = DEFAULT_POLL_INTERVAL_MS;
    configSource = `default (${DEFAULT_POLL_INTERVAL_MINUTES} minutes)`;
  }

  log.info('Firmware poll interval configured', {
    interval: `${Math.round(POLL_INTERVAL / 1000 / 60)} minutes`,
    source: configSource
  });

  /**
   * Subscribe to firmware events
   */
  function on(event, callback) {
    eventListeners.push({ event, callback });
    return () => {
      const idx = eventListeners.findIndex(l => l.event === event && l.callback === callback);
      if (idx >= 0) eventListeners.splice(idx, 1);
    };
  }

  function emit(event, data) {
    eventListeners
      .filter(l => l.event === event)
      .forEach(l => {
        try {
          l.callback(data);
        } catch (err) {
          log.error('Firmware event listener error', { event, error: err.message });
        }
      });
  }

  /**
   * Get currently installed firmware version from version.json
   */
  function getCurrentVersion() {
    const versionFile = path.join(FIRMWARE_DIR, 'version.json');
    if (fs.existsSync(versionFile)) {
      try {
        const data = JSON.parse(fs.readFileSync(versionFile, 'utf8'));
        return data.version || null;
      } catch (e) {
        log.warn('Failed to read version.json', { error: e.message });
      }
    }
    return null;
  }

  /**
   * Fetch latest release info from GitHub API
   */
  async function fetchLatestRelease() {
    return new Promise((resolve, reject) => {
      let settled = false;
      const options = {
        hostname: 'api.github.com',
        path: `/repos/${GITHUB_REPO}/releases/latest`,
        headers: { 'User-Agent': 'unified-hifi-control' }
      };

      const req = https.get(options, (response) => {
        let data = '';
        response.on('data', chunk => data += chunk);
        response.on('end', () => {
          if (settled) return;
          settled = true;
          if (response.statusCode === 200) {
            try {
              resolve(JSON.parse(data));
            } catch (e) {
              reject(new Error(`Failed to parse GitHub response: ${e.message}`));
            }
          } else if (response.statusCode === 404) {
            resolve(null); // No releases yet
          } else if (response.statusCode === 403) {
            reject(new Error('GitHub API rate limit exceeded'));
          } else {
            reject(new Error(`GitHub API error: ${response.statusCode}`));
          }
        });
      });

      req.on('error', (err) => {
        if (settled) return;
        settled = true;
        reject(err);
      });

      req.setTimeout(REQUEST_TIMEOUT_MS, () => {
        if (settled) return;
        settled = true;
        req.destroy();
        reject(new Error('GitHub API request timed out'));
      });
    });
  }

  /**
   * Download firmware binary from GitHub release
   */
  async function downloadFirmware(asset, version, releaseUrl) {
    const downloadUrl = asset.browser_download_url;
    log.info('Downloading firmware', { version, url: downloadUrl });

    if (!fs.existsSync(FIRMWARE_DIR)) {
      fs.mkdirSync(FIRMWARE_DIR, { recursive: true });
    }

    // Download to temp file first, rename on success
    const firmwarePath = path.join(FIRMWARE_DIR, 'roon_knob.bin');
    const tempPath = path.join(FIRMWARE_DIR, 'roon_knob.bin.tmp');
    const file = fs.createWriteStream(tempPath);

    await new Promise((resolve, reject) => {
      const DOWNLOAD_TIMEOUT_MS = 120000; // 2 minutes for download
      let redirectCount = 0;
      let settled = false;
      let currentReq = null;

      const cleanup = () => {
        if (currentReq) {
          currentReq.destroy();
          currentReq = null;
        }
        try {
          file.close();
          if (fs.existsSync(tempPath)) fs.unlinkSync(tempPath);
        } catch (e) {
          // Ignore cleanup errors
        }
      };

      file.on('error', (err) => {
        if (settled) return;
        settled = true;
        cleanup();
        reject(new Error(`File write error: ${err.message}`));
      });

      const download = (url) => {
        currentReq = https.get(url, (response) => {
          if (response.statusCode === 302 || response.statusCode === 301) {
            redirectCount++;
            if (redirectCount > MAX_REDIRECTS) {
              if (settled) return;
              settled = true;
              cleanup();
              reject(new Error('Too many redirects'));
              return;
            }
            response.resume(); // Consume response to free up memory
            download(response.headers.location);
            return;
          }
          if (response.statusCode !== 200) {
            if (settled) return;
            settled = true;
            cleanup();
            reject(new Error(`Download failed: ${response.statusCode}`));
            return;
          }
          response.pipe(file);
          file.on('finish', () => {
            if (settled) return;
            settled = true;
            file.close();
            // Rename temp to final on success
            fs.renameSync(tempPath, firmwarePath);
            resolve();
          });
        });

        currentReq.on('error', (err) => {
          if (settled) return;
          settled = true;
          cleanup();
          reject(err);
        });

        currentReq.setTimeout(DOWNLOAD_TIMEOUT_MS, () => {
          if (settled) return;
          settled = true;
          cleanup();
          reject(new Error('Download timed out'));
        });
      };

      download(downloadUrl);
    });

    const versionPath = path.join(FIRMWARE_DIR, 'version.json');
    fs.writeFileSync(versionPath, JSON.stringify({
      version,
      file: 'roon_knob.bin',
      fetched_at: new Date().toISOString(),
      release_url: releaseUrl
    }, null, 2));

    const stats = fs.statSync(firmwarePath);
    log.info('Firmware downloaded successfully', { version, size: stats.size });

    return { version, size: stats.size, file: 'roon_knob.bin' };
  }

  /**
   * Compare semver versions: returns true if remote > local
   * Handles pre-release suffixes (e.g., 1.0.0-beta) by stripping them
   */
  function isNewerVersion(remoteVersion, localVersion) {
    if (!localVersion) return true;
    if (!remoteVersion) return false;

    // Strip leading 'v' and any pre-release suffix for comparison
    const parseVersion = (v) => v.replace(/^v/, '').split('-')[0].split('.').map(n => parseInt(n, 10) || 0);
    const remote = parseVersion(remoteVersion);
    const local = parseVersion(localVersion);

    for (let i = 0; i < 3; i++) {
      const r = remote[i] || 0;
      const l = local[i] || 0;
      if (r > l) return true;
      if (r < l) return false;
    }
    return false;
  }

  /**
   * Check for updates and optionally download
   */
  async function checkForUpdates({ autoDownload = true } = {}) {
    try {
      const releaseData = await fetchLatestRelease();

      if (!releaseData) {
        log.debug('No releases found on GitHub');
        return { updateAvailable: false, currentVersion: getCurrentVersion() };
      }

      const latestVersion = releaseData.tag_name.replace(/^v/, '');
      const currentVersion = getCurrentVersion();
      // Guard against undefined assets array
      const assets = Array.isArray(releaseData.assets) ? releaseData.assets : [];
      const asset = assets.find(a => a.name === 'roon_knob.bin');

      const status = {
        currentVersion,
        latestVersion,
        updateAvailable: isNewerVersion(latestVersion, currentVersion),
        releaseUrl: releaseData.html_url,
        hasAsset: !!asset,
        timestamp: Date.now()
      };

      if (status.updateAvailable && asset) {
        log.info('New firmware version available', {
          current: currentVersion,
          latest: latestVersion
        });

        // Emit event before download
        emit('update_available', {
          currentVersion,
          latestVersion,
          releaseUrl: releaseData.html_url
        });

        if (autoDownload) {
          const downloaded = await downloadFirmware(asset, latestVersion, releaseData.html_url);
          status.downloaded = true;
          status.size = downloaded.size;

          // Emit event after download
          emit('firmware_downloaded', {
            version: latestVersion,
            size: downloaded.size,
            releaseUrl: releaseData.html_url
          });
        }
      } else if (status.updateAvailable && !asset) {
        log.warn('New version available but no firmware binary found', { version: latestVersion });
      } else {
        log.debug('Firmware is up to date', { version: currentVersion });
      }

      lastKnownVersion = getCurrentVersion();
      return status;
    } catch (err) {
      log.error('Failed to check for firmware updates', { error: err.message });
      return {
        error: err.message,
        currentVersion: getCurrentVersion(),
        timestamp: Date.now()
      };
    }
  }

  /**
   * Run check with guard against overlapping calls
   */
  async function runCheck() {
    if (checkInProgress) {
      log.debug('Skipping firmware check, previous check still in progress');
      return;
    }
    checkInProgress = true;
    try {
      await checkForUpdates();
    } catch (err) {
      log.error('Firmware check failed', { error: err.message });
    } finally {
      checkInProgress = false;
    }
  }

  /**
   * Start periodic polling
   */
  function start() {
    if (isStarted) {
      log.warn('FirmwareService already started');
      return;
    }

    isStarted = true;
    const intervalMinutes = Math.round(POLL_INTERVAL / 1000 / 60);
    const intervalDisplay = intervalMinutes >= 60
      ? `${Math.round(intervalMinutes / 60)}h`
      : `${intervalMinutes}m`;
    log.info('FirmwareService started', {
      repo: GITHUB_REPO,
      pollInterval: intervalDisplay,
      firmwareDir: FIRMWARE_DIR
    });

    // Check immediately on start
    runCheck();

    // Then poll at interval
    pollTimer = setInterval(() => {
      runCheck();
    }, POLL_INTERVAL);
  }

  /**
   * Stop periodic polling
   */
  function stop() {
    if (pollTimer) {
      clearInterval(pollTimer);
      pollTimer = null;
    }
    isStarted = false;
    log.info('FirmwareService stopped');
  }

  /**
   * Get current status
   */
  function getStatus() {
    return {
      currentVersion: getCurrentVersion(),
      lastKnownVersion,
      isPolling: isStarted,
      pollInterval: POLL_INTERVAL,
      repo: GITHUB_REPO
    };
  }

  return {
    start,
    stop,
    checkForUpdates,
    getStatus,
    getCurrentVersion,
    on,
  };
}

module.exports = { createFirmwareService };
