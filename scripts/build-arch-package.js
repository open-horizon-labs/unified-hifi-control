#!/usr/bin/env node
/**
 * Build Arch Linux package (.pkg.tar.zst)
 *
 * Creates a PKGBUILD from the template and builds the package.
 * Requires: makepkg (from pacman)
 */

const fs = require('fs');
const path = require('path');
const { execSync } = require('child_process');
const crypto = require('crypto');

const ROOT = path.join(__dirname, '..');
const BUILD_DIR = path.join(ROOT, 'build', 'arch');
const DIST_DIR = path.join(ROOT, 'dist');
const BINARIES_DIR = path.join(ROOT, 'dist', 'bin');

// Read version from package.json
const packageJson = JSON.parse(fs.readFileSync(path.join(ROOT, 'package.json'), 'utf8'));
const VERSION = packageJson.version;

function sha256File(filePath) {
  const content = fs.readFileSync(filePath);
  return crypto.createHash('sha256').update(content).digest('hex');
}

function buildArchPackage() {
  console.log(`Building Arch package v${VERSION}...`);

  // Ensure dist directory exists
  if (!fs.existsSync(DIST_DIR)) {
    fs.mkdirSync(DIST_DIR, { recursive: true });
  }

  // Create working directory
  const workDir = path.join(DIST_DIR, 'arch-build');
  if (fs.existsSync(workDir)) {
    fs.rmSync(workDir, { recursive: true });
  }
  fs.mkdirSync(workDir, { recursive: true });

  // Check for binaries
  const x86Binary = path.join(BINARIES_DIR, 'unified-hifi-linux-x64');
  const armBinary = path.join(BINARIES_DIR, 'unified-hifi-linux-arm64');

  if (!fs.existsSync(x86Binary)) {
    console.error('Error: x86_64 binary not found. Run build:binaries first.');
    process.exit(1);
  }

  // Copy binaries with Arch naming convention
  fs.copyFileSync(x86Binary, path.join(workDir, 'unified-hifi-linux-x86_64'));

  let sha256Arm = 'SKIP';
  if (fs.existsSync(armBinary)) {
    fs.copyFileSync(armBinary, path.join(workDir, 'unified-hifi-linux-aarch64'));
    sha256Arm = sha256File(armBinary);
  }

  const sha256X86 = sha256File(x86Binary);

  // Copy systemd service
  fs.copyFileSync(
    path.join(BUILD_DIR, 'unified-hifi-control.service'),
    path.join(workDir, 'unified-hifi-control.service')
  );

  // Generate PKGBUILD from template
  let pkgbuild = fs.readFileSync(path.join(BUILD_DIR, 'PKGBUILD.template'), 'utf8');
  pkgbuild = pkgbuild
    .replace(/\${VERSION}/g, VERSION)
    .replace(/\${SHA256_X86_64}/g, sha256X86)
    .replace(/\${SHA256_AARCH64}/g, sha256Arm);

  // Fix source path to use local files
  pkgbuild = pkgbuild
    .replace(/source_x86_64=\([^)]+\)/, 'source_x86_64=("unified-hifi-linux-x86_64" "unified-hifi-control.service")')
    .replace(/source_aarch64=\([^)]+\)/, 'source_aarch64=("unified-hifi-linux-aarch64" "unified-hifi-control.service")')
    .replace(/sha256sums_x86_64=\([^)]+\)/, `sha256sums_x86_64=('${sha256X86}' 'SKIP')`)
    .replace(/sha256sums_aarch64=\([^)]+\)/, `sha256sums_aarch64=('${sha256Arm}' 'SKIP')`);

  fs.writeFileSync(path.join(workDir, 'PKGBUILD'), pkgbuild);

  console.log('PKGBUILD generated');
  console.log(`  x86_64 sha256: ${sha256X86}`);
  console.log(`  aarch64 sha256: ${sha256Arm}`);

  // Check if makepkg is available
  try {
    execSync('which makepkg', { stdio: 'ignore' });
  } catch {
    console.log('\nmakepkg not found - PKGBUILD ready for manual build');
    console.log(`cd ${workDir} && makepkg -s`);

    // Copy PKGBUILD to dist for release
    fs.copyFileSync(
      path.join(workDir, 'PKGBUILD'),
      path.join(DIST_DIR, 'PKGBUILD')
    );
    console.log(`\nPKGBUILD copied to ${path.join(DIST_DIR, 'PKGBUILD')}`);
    return;
  }

  // Build the package
  console.log('\nBuilding package with makepkg...');
  try {
    execSync('makepkg -sf --noconfirm', {
      cwd: workDir,
      stdio: 'inherit',
    });

    // Find and copy the built package
    const files = fs.readdirSync(workDir);
    const pkgFile = files.find(f => f.endsWith('.pkg.tar.zst') || f.endsWith('.pkg.tar.xz'));
    if (pkgFile) {
      fs.copyFileSync(
        path.join(workDir, pkgFile),
        path.join(DIST_DIR, pkgFile)
      );
      console.log(`\nPackage built: ${path.join(DIST_DIR, pkgFile)}`);
    }
  } catch (err) {
    console.error('makepkg failed:', err.message);
    process.exit(1);
  }
}

buildArchPackage();
