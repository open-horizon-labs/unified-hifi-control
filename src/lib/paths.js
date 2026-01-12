/**
 * Path utilities for pkg-bundled and development environments
 *
 * When running from pkg, __dirname points inside the read-only snapshot.
 * This module provides paths that work in both environments.
 */

const path = require('path');

/**
 * Get the data directory for config files, firmware, etc.
 * - In pkg: .data directory next to the executable (QNAP convention)
 * - In dev: project's data/ directory
 * - Can be overridden with CONFIG_DIR env var
 */
function getDataDir() {
  if (process.env.CONFIG_DIR) {
    return process.env.CONFIG_DIR;
  }
  if (process.pkg) {
    // Running from pkg bundle - use dot-directory next to executable
    // QNAP packages use dot-dirs for config/logs in the package root
    return path.join(path.dirname(process.execPath), '.data');
  }
  // Development mode - use project data directory
  // __dirname is src/lib, so go up two levels
  return path.join(__dirname, '..', '..', 'data');
}

module.exports = {
  getDataDir,
};
