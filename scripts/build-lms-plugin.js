#!/usr/bin/env node

/**
 * Build LMS (Lyrion Music Server) plugin package
 *
 * Usage: npm run build:lms-plugin
 *
 * Prerequisites: Run npm run build:binaries first
 *
 * Output: dist/lms-unified-hifi-control-{version}.zip
 */

const fs = require('fs');
const path = require('path');
const crypto = require('crypto');
const archiver = require('archiver');

const ROOT = path.resolve(__dirname, '..');
const LMS_PLUGIN_SRC = path.join(ROOT, 'lms-plugin');
const BINARIES_DIR = path.join(ROOT, 'dist', 'bin');
const DIST = path.join(ROOT, 'dist');
const PKG_JSON = require(path.join(ROOT, 'package.json'));

const VERSION = PKG_JSON.version;

// Binary mappings for LMS plugin
// NOTE: Binaries are now downloaded on-demand, not bundled in the plugin ZIP
// This reduces plugin size from ~125 MB to ~50 KB
const BINARY_MAP = [
  { src: 'unified-hifi-lms-linux-x64', dest: 'unified-hifi-linux-x86_64' },
  { src: 'unified-hifi-lms-linux-arm64', dest: 'unified-hifi-linux-aarch64' },
  { src: 'unified-hifi-lms-macos-x64', dest: 'unified-hifi-darwin-x86_64' },
  { src: 'unified-hifi-lms-macos-arm64', dest: 'unified-hifi-darwin-arm64' },
  { src: 'unified-hifi-lms-win-x64.exe', dest: 'unified-hifi-win64.exe' },
];

// Set to true to bundle binaries (for testing), false for production (on-demand download)
const BUNDLE_BINARIES = process.env.LMS_BUNDLE_BINARIES === 'true';

async function main() {
  console.log(`\nBuilding LMS plugin v${VERSION}\n`);

  // Update install.xml version before archiving (it gets included in the zip)
  updateInstallXml(VERSION);

  if (BUNDLE_BINARIES) {
    // Verify binaries exist (only when bundling)
    console.log('Checking binaries (bundle mode)...');
    for (const { src } of BINARY_MAP) {
      const binPath = path.join(BINARIES_DIR, src);
      if (!fs.existsSync(binPath)) {
        console.error(`Missing binary: ${src}`);
        console.error('Run "npm run build:binaries" first.');
        process.exit(1);
      }
    }
    console.log('  ✓ All binaries found\n');
  } else {
    console.log('On-demand download mode: binaries will NOT be bundled');
    console.log('Binary will be downloaded when user first starts the bridge.\n');
  }

  // Verify LMS plugin source exists
  if (!fs.existsSync(LMS_PLUGIN_SRC)) {
    console.error(`LMS plugin source not found: ${LMS_PLUGIN_SRC}`);
    console.error('Create the lms-plugin/ directory with Plugin.pm, Helper.pm, etc.');
    process.exit(1);
  }

  // Create output zip
  fs.mkdirSync(DIST, { recursive: true });
  const zipName = `lms-unified-hifi-control-${VERSION}.zip`;
  const zipPath = path.join(DIST, zipName);

  console.log(`Creating ${zipName}...`);

  const output = fs.createWriteStream(zipPath);
  const archive = archiver('zip', { zlib: { level: 9 } });

  output.on('close', () => {
    const sizeMB = (archive.pointer() / 1024 / 1024).toFixed(1);
    console.log(`  ✓ Created ${zipName} (${sizeMB} MB)\n`);

    // Generate SHA1 checksum
    const sha1 = generateSha1(zipPath);
    console.log(`SHA1: ${sha1}\n`);

    // Update repo.xml with new version and SHA
    updateRepoXml(VERSION, sha1);

    console.log('=== LMS Plugin Build Complete ===\n');
    console.log(`Package: ${zipPath}`);
    console.log(`SHA1: ${sha1}`);
    console.log('\nNext steps:');
    console.log('1. Upload zip to GitHub release');
    console.log('2. Submit repo.xml to LMS-Community/lms-plugin-repository');
  });

  archive.on('error', (err) => {
    throw err;
  });

  archive.pipe(output);

  // Add plugin source files
  archive.directory(LMS_PLUGIN_SRC, 'UnifiedHiFi', {
    ignore: ['Bin/**'], // We'll add binaries separately (or not)
  });

  if (BUNDLE_BINARIES) {
    // Add binaries with correct names (bundle mode)
    for (const { src, dest } of BINARY_MAP) {
      const srcPath = path.join(BINARIES_DIR, src);
      archive.file(srcPath, { name: `UnifiedHiFi/Bin/${dest}` });
    }
  } else {
    // Create empty Bin directory with placeholder (on-demand mode)
    archive.append('# Binaries downloaded on first run\n', { name: 'UnifiedHiFi/Bin/.gitkeep' });
  }

  await archive.finalize();
}

function generateSha1(filePath) {
  const fileBuffer = fs.readFileSync(filePath);
  return crypto.createHash('sha1').update(fileBuffer).digest('hex');
}

function updateInstallXml(version) {
  const installPath = path.join(LMS_PLUGIN_SRC, 'install.xml');

  if (!fs.existsSync(installPath)) {
    console.log('Note: install.xml not found, skipping version update');
    return;
  }

  let content = fs.readFileSync(installPath, 'utf8');

  // Update version element
  content = content.replace(
    /<version>[^<]+<\/version>/,
    `<version>${version}</version>`
  );

  fs.writeFileSync(installPath, content);
  console.log('  ✓ Updated install.xml version');
}

function updateRepoXml(version, sha1) {
  const repoPath = path.join(LMS_PLUGIN_SRC, 'repo.xml');

  if (!fs.existsSync(repoPath)) {
    console.log('Note: repo.xml not found, skipping update');
    return;
  }

  let content = fs.readFileSync(repoPath, 'utf8');

  // Update plugin version (not XML declaration version)
  content = content.replace(
    /(<plugin[^>]*\s)version="[^"]+"/,
    `$1version="${version}"`
  );

  // Update SHA
  content = content.replace(
    /<sha>[^<]+<\/sha>/,
    `<sha>${sha1}</sha>`
  );

  // Update URL (assumes GitHub releases)
  const repoUrl = PKG_JSON.repository?.url?.replace(/\.git$/, '') || '';
  if (repoUrl) {
    const downloadUrl = `${repoUrl.replace('git+', '')}/releases/download/v${version}/lms-unified-hifi-control-${version}.zip`;
    content = content.replace(
      /<url>[^<]+<\/url>/,
      `<url>${downloadUrl}</url>`
    );
  }

  fs.writeFileSync(repoPath, content);
  console.log('  ✓ Updated repo.xml');
}

main().catch((err) => {
  console.error('Build failed:', err);
  process.exit(1);
});
