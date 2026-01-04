/**
 * App Settings - Shared settings module for UI preferences and adapter configuration
 */

const fs = require('fs');
const path = require('path');

const CONFIG_DIR = process.env.CONFIG_DIR || './data';
const SETTINGS_FILE = path.join(CONFIG_DIR, 'app-settings.json');

const DEFAULTS = {
  hideKnobsPage: false,
  adapters: {
    roon: true,      // enabled by default
    upnp: false,     // disabled by default
    openhome: false, // disabled by default
    lms: false       // disabled by default
  }
};

/**
 * Load app settings from disk, merging with defaults
 */
function loadAppSettings() {
  try {
    const data = JSON.parse(fs.readFileSync(SETTINGS_FILE, 'utf8'));
    // Deep merge adapters to ensure all keys exist
    return {
      ...DEFAULTS,
      ...data,
      adapters: { ...DEFAULTS.adapters, ...data.adapters }
    };
  } catch {
    return { ...DEFAULTS };
  }
}

/**
 * Save app settings to disk
 */
function saveAppSettings(settings) {
  fs.mkdirSync(CONFIG_DIR, { recursive: true });
  fs.writeFileSync(SETTINGS_FILE, JSON.stringify(settings, null, 2));
}

module.exports = { loadAppSettings, saveAppSettings, DEFAULTS };
