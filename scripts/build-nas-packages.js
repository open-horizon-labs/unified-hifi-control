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
  const qdkArch = arch === 'x86_64' ? 'x86_64' : 'arm_64';
  const result = { name: `QNAP QPKG (${arch})`, success: false };
  let tempDir = null;

  console.log(`Building QNAP package for ${arch} using qbuild...`);

  try {
    tempDir = fs.mkdtempSync(path.join(DIST, 'qnap-'));

    // Create QDK2 directory structure
    const sharedDir = path.join(tempDir, 'shared');
    const archDir = path.join(tempDir, qdkArch);
    fs.mkdirSync(sharedDir, { recursive: true });
    fs.mkdirSync(archDir, { recursive: true });

    // Copy shared scripts
    const qnapSharedSrc = path.join(BUILD, 'qnap', 'shared');
    const sharedFiles = fs.readdirSync(qnapSharedSrc);
    for (const file of sharedFiles) {
      fs.copyFileSync(path.join(qnapSharedSrc, file), path.join(sharedDir, file));
      fs.chmodSync(path.join(sharedDir, file), 0o755);
    }

    // Copy binary to arch-specific directory
    fs.copyFileSync(binary.path, path.join(archDir, 'unified-hifi-control'));
    fs.chmodSync(path.join(archDir, 'unified-hifi-control'), 0o755);

    // Copy and process qpkg.cfg
    let cfgContent = fs.readFileSync(path.join(BUILD, 'qnap', 'qpkg.cfg'), 'utf8');
    cfgContent = cfgContent.replace(/\{\{VERSION\}\}/g, VERSION);
    fs.writeFileSync(path.join(tempDir, 'qpkg.cfg'), cfgContent);

    // Create package_routines (empty but required by qbuild)
    fs.writeFileSync(path.join(tempDir, 'package_routines'), '#!/bin/sh\n');

    // Build QPKG using Docker with qbuild (owncloudci/qnap-qpkg-builder - proven in production)
    // Run qbuild, copy result to /src/output, and fix permissions so host can delete
    const qpkgName = `unified-hifi-control_${VERSION}_${arch}.qpkg`;
    const dockerCmd = `docker run --rm --platform linux/amd64 -v "${tempDir}:/src" -w /src owncloudci/qnap-qpkg-builder:latest sh -c "/usr/share/qdk2/QDK/bin/qbuild --build-dir /src/build ${qdkArch} && cp /src/build/*.qpkg /src/ && chmod 666 /src/*.qpkg && rm -rf /src/build"`;
    console.log(`  Running qbuild...`);
    execSync(dockerCmd, { stdio: 'inherit' });

    // Find the generated QPKG file in temp dir (copied there by docker command)
    const qpkgFiles = fs.readdirSync(tempDir).filter(f => f.endsWith('.qpkg'));
    if (qpkgFiles.length === 0) {
      throw new Error('qbuild did not produce a QPKG file');
    }

    // Copy to installers directory with our naming convention
    const qpkgPath = path.join(INSTALLERS, qpkgName);
    fs.copyFileSync(path.join(tempDir, qpkgFiles[0]), qpkgPath);

    const stats = fs.statSync(qpkgPath);
    result.success = true;
    result.size = `${(stats.size / 1024 / 1024).toFixed(1)} MB`;
    result.path = qpkgPath;

    console.log(`  ✓ ${qpkgName}`);

  } catch (err) {
    result.error = err.message;
    console.error(`  ✗ QNAP (${arch}): ${err.message}`);
  } finally {
    // Always cleanup temp directory - use Docker to remove root-owned files
    if (tempDir && fs.existsSync(tempDir)) {
      try {
        fs.rmSync(tempDir, { recursive: true });
      } catch {
        // If Node can't delete (root-owned files), use Docker
        execSync(`docker run --rm -v "${tempDir}:/src" alpine rm -rf /src/*`, { stdio: 'pipe' });
        fs.rmSync(tempDir, { recursive: true });
      }
    }
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
