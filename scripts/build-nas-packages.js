#!/usr/bin/env node

/**
 * Build NAS packages (Synology SPK and QNAP QPKG)
 *
 * Usage: npm run build:nas
 *
 * Prerequisites: Run npm run build:binaries first
 */

const { execSync } = require('child_process');
const fs = require('fs');
const path = require('path');

const ROOT = path.resolve(__dirname, '..');
const DIST = path.join(ROOT, 'dist');
const BINARIES = path.join(DIST, 'bin');
const INSTALLERS = path.join(DIST, 'installers');
const BUILD = path.join(ROOT, 'build');
const PKG_JSON = require(path.join(ROOT, 'package.json'));

const VERSION = PKG_JSON.version;

// Architecture mappings
const SYNOLOGY_ARCHS = {
  'linux-x64': 'x86_64',
  'linux-arm64': 'aarch64'
};

const QNAP_ARCHS = {
  'linux-x64': 'x86_64',
  'linux-arm64': 'arm_64'
};

async function main() {
  console.log(`\n${'='.repeat(50)}`);
  console.log(`Building NAS packages v${VERSION}`);
  console.log(`${'='.repeat(50)}\n`);

  fs.mkdirSync(INSTALLERS, { recursive: true });

  const results = [];

  // Find available Linux binaries
  const binaries = [];
  const x64Binary = path.join(BINARIES, 'unified-hifi-linux-x64');
  const arm64Binary = path.join(BINARIES, 'unified-hifi-linux-arm64');

  if (fs.existsSync(x64Binary)) {
    binaries.push({ path: x64Binary, platform: 'linux-x64' });
  }
  if (fs.existsSync(arm64Binary)) {
    binaries.push({ path: arm64Binary, platform: 'linux-arm64' });
  }

  if (binaries.length === 0) {
    console.error('No Linux binaries found. Run npm run build:binaries first.');
    process.exit(1);
  }

  // Build packages for each architecture
  for (const binary of binaries) {
    results.push(await buildSynologyPackage(binary));
    results.push(await buildQnapPackage(binary));
  }

  // Summary
  console.log(`\n${'='.repeat(50)}`);
  console.log('Build Summary');
  console.log(`${'='.repeat(50)}`);

  for (const result of results) {
    const status = result.success ? '✓' : '✗';
    const size = result.size ? ` (${result.size})` : '';
    console.log(`${status} ${result.name}${size}`);
    if (!result.success && result.error) {
      console.log(`  Error: ${result.error}`);
    }
  }

  console.log(`\nOutput: ${INSTALLERS}`);
}

async function buildSynologyPackage(binary) {
  const arch = SYNOLOGY_ARCHS[binary.platform];
  const result = { name: `Synology SPK (${arch})`, success: false };

  console.log(`Building Synology package for ${arch}...`);

  try {
    const tempDir = fs.mkdtempSync(path.join(DIST, 'synology-'));
    const packageDir = path.join(tempDir, 'package');

    // Create package directory structure
    fs.mkdirSync(packageDir, { recursive: true });

    // Copy binary
    fs.copyFileSync(binary.path, path.join(packageDir, 'unified-hifi-control'));
    fs.chmodSync(path.join(packageDir, 'unified-hifi-control'), 0o755);

    // Create package.tgz
    const packageTgz = path.join(tempDir, 'package.tgz');
    execSync(`tar -czf "${packageTgz}" -C "${packageDir}" .`, { stdio: 'pipe' });

    // Copy and process INFO file
    let infoContent = fs.readFileSync(path.join(BUILD, 'synology', 'INFO'), 'utf8');
    infoContent = infoContent.replace(/\{\{VERSION\}\}/g, VERSION);
    infoContent = infoContent.replace(/\{\{ARCH\}\}/g, arch);
    fs.writeFileSync(path.join(tempDir, 'INFO'), infoContent);

    // Copy scripts
    const scriptsDir = path.join(tempDir, 'scripts');
    fs.mkdirSync(scriptsDir, { recursive: true });

    const scriptFiles = ['start-stop-status', 'postinst', 'preuninst'];
    for (const script of scriptFiles) {
      const srcPath = path.join(BUILD, 'synology', 'scripts', script);
      if (fs.existsSync(srcPath)) {
        fs.copyFileSync(srcPath, path.join(scriptsDir, script));
        fs.chmodSync(path.join(scriptsDir, script), 0o755);
      }
    }

    // Copy conf
    const confDir = path.join(tempDir, 'conf');
    fs.mkdirSync(confDir, { recursive: true });
    const resourceSrc = path.join(BUILD, 'synology', 'conf', 'resource');
    if (fs.existsSync(resourceSrc)) {
      fs.copyFileSync(resourceSrc, path.join(confDir, 'resource'));
    }

    // Create placeholder icons (72x72 and 256x256 PNG)
    // In production, these should be actual icons
    createPlaceholderIcon(path.join(tempDir, 'PACKAGE_ICON.PNG'), 72);
    createPlaceholderIcon(path.join(tempDir, 'PACKAGE_ICON_256.PNG'), 256);

    // Build SPK (tar archive)
    const spkName = `unified-hifi-control-${VERSION}-${arch}.spk`;
    const spkPath = path.join(INSTALLERS, spkName);

    execSync(`tar -cf "${spkPath}" -C "${tempDir}" INFO package.tgz scripts conf PACKAGE_ICON.PNG PACKAGE_ICON_256.PNG`, {
      stdio: 'pipe'
    });

    // Cleanup
    fs.rmSync(tempDir, { recursive: true });

    const stats = fs.statSync(spkPath);
    result.success = true;
    result.size = `${(stats.size / 1024 / 1024).toFixed(1)} MB`;
    result.path = spkPath;

    console.log(`  ✓ ${spkName}`);

  } catch (err) {
    result.error = err.message;
    console.error(`  ✗ Synology (${arch}): ${err.message}`);
  }

  return result;
}

async function buildQnapPackage(binary) {
  const arch = QNAP_ARCHS[binary.platform];
  const result = { name: `QNAP QPKG (${arch})`, success: false };

  console.log(`Building QNAP package for ${arch}...`);

  try {
    const tempDir = fs.mkdtempSync(path.join(DIST, 'qnap-'));
    const payloadDir = path.join(tempDir, 'payload');

    // Create payload directory (contents that will be installed)
    fs.mkdirSync(payloadDir, { recursive: true });

    // Copy binary
    fs.copyFileSync(binary.path, path.join(payloadDir, 'unified-hifi-control'));
    fs.chmodSync(path.join(payloadDir, 'unified-hifi-control'), 0o755);

    // Copy scripts from build/qnap/shared
    const qnapSharedSrc = path.join(BUILD, 'qnap', 'shared');
    const sharedFiles = fs.readdirSync(qnapSharedSrc);
    for (const file of sharedFiles) {
      fs.copyFileSync(path.join(qnapSharedSrc, file), path.join(payloadDir, file));
      fs.chmodSync(path.join(payloadDir, file), 0o755);
    }

    // Create payload.tar.gz
    const payloadTar = path.join(tempDir, 'payload.tar.gz');
    execSync(`tar -czf "${payloadTar}" -C "${payloadDir}" .`, { stdio: 'pipe' });

    // Read and process header template
    let headerContent = fs.readFileSync(path.join(BUILD, 'qnap', 'qpkg_header.sh'), 'utf8');
    headerContent = headerContent.replace(/\{\{VERSION\}\}/g, VERSION);
    const headerPath = path.join(tempDir, 'header.sh');
    fs.writeFileSync(headerPath, headerContent);

    // Build QPKG: concatenate header + payload
    // QPKG is a self-extracting shell script with embedded tar.gz
    const qpkgName = `unified-hifi-control_${VERSION}_${arch}.qpkg`;
    const qpkgPath = path.join(INSTALLERS, qpkgName);

    // Concatenate: header.sh + payload.tar.gz
    const headerData = fs.readFileSync(headerPath);
    const payloadData = fs.readFileSync(payloadTar);
    const qpkgData = Buffer.concat([headerData, payloadData]);
    fs.writeFileSync(qpkgPath, qpkgData);
    fs.chmodSync(qpkgPath, 0o755);

    // Cleanup
    fs.rmSync(tempDir, { recursive: true });

    const stats = fs.statSync(qpkgPath);
    result.success = true;
    result.size = `${(stats.size / 1024 / 1024).toFixed(1)} MB`;
    result.path = qpkgPath;

    console.log(`  ✓ ${qpkgName}`);

  } catch (err) {
    result.error = err.message;
    console.error(`  ✗ QNAP (${arch}): ${err.message}`);
  }

  return result;
}

function createPlaceholderIcon(filePath, size) {
  // Create a minimal valid PNG (1x1 transparent pixel, will be stretched)
  // In production, replace with actual icon generation using sharp or similar
  const PNG_HEADER = Buffer.from([
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
    0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, // IHDR chunk header
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // 1x1 dimensions
    0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, // bit depth, color type, etc.
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, // IDAT chunk
    0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
    0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, // IEND chunk
    0x42, 0x60, 0x82
  ]);

  fs.writeFileSync(filePath, PNG_HEADER);
}

main().catch((err) => {
  console.error('Build failed:', err);
  process.exit(1);
});
