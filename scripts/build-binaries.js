#!/usr/bin/env node

/**
 * Build standalone binaries for all platforms using pkg
 *
 * Usage: npm run build:binaries
 *        npm run build:binaries -- --lms-only
 *
 * Output: dist/bin/
 *   Full build:
 *   - unified-hifi-linux-x64
 *   - unified-hifi-linux-arm64
 *   - unified-hifi-macos-x64
 *   - unified-hifi-macos-arm64
 *   - unified-hifi-win-x64.exe
 *
 *   LMS-only build (smaller, HQPlayer only):
 *   - unified-hifi-lms-linux-x64
 *   - unified-hifi-lms-linux-arm64
 *   - unified-hifi-lms-macos-x64
 *   - unified-hifi-lms-macos-arm64
 *   - unified-hifi-lms-win-x64.exe
 */

const { execSync } = require('child_process');
const fs = require('fs');
const path = require('path');

const ROOT = path.resolve(__dirname, '..');
const DIST = path.join(ROOT, 'dist', 'bin');
const PKG_JSON = require(path.join(ROOT, 'package.json'));

// Check for --lms-only flag
const lmsOnly = process.argv.includes('--lms-only');
const buildBoth = process.argv.includes('--all') || !process.argv.slice(2).length;

// Platform targets and output names
const FULL_TARGETS = [
  { target: 'node18-linux-x64', output: 'unified-hifi-linux-x64' },
  { target: 'node18-linux-arm64', output: 'unified-hifi-linux-arm64' },
  { target: 'node18-macos-x64', output: 'unified-hifi-macos-x64' },
  { target: 'node18-macos-arm64', output: 'unified-hifi-macos-arm64' },
  { target: 'node18-win-x64', output: 'unified-hifi-win-x64.exe' },
];

// LMS builds use same naming as GitHub releases (for on-demand download)
const LMS_TARGETS = [
  { target: 'node18-linux-x64', output: 'unified-hifi-linux-x86_64' },
  { target: 'node18-linux-arm64', output: 'unified-hifi-linux-aarch64' },
  { target: 'node18-macos-x64', output: 'unified-hifi-darwin-x86_64' },
  { target: 'node18-macos-arm64', output: 'unified-hifi-darwin-arm64' },
  { target: 'node18-win-x64', output: 'unified-hifi-win64.exe' },
];

async function main() {
  console.log(`\nBuilding unified-hifi-control v${PKG_JSON.version}\n`);

  // Create dist directory
  fs.mkdirSync(DIST, { recursive: true });

  // Check for native modules that need special handling
  checkNativeModules();

  const allOutputs = [];

  // Build full binaries (unless --lms-only)
  if (!lmsOnly) {
    console.log('=== Building Full Binaries ===\n');
    for (const { target, output } of FULL_TARGETS) {
      await buildTarget(target, output, 'src/index.js');
      allOutputs.push(output);
    }
  }

  // Build LMS-only binaries (if --lms-only or --all/default)
  if (lmsOnly || buildBoth) {
    console.log('\n=== Building LMS Plugin Binaries (minimal, no sharp) ===\n');
    for (const { target, output } of LMS_TARGETS) {
      await buildTarget(target, output, 'src/lms-entry.js', true);  // excludeSharp=true
      allOutputs.push(output);
    }
  }

  // Print summary
  console.log('\n=== Build Complete ===\n');
  console.log('Binaries:');
  for (const output of allOutputs) {
    const filePath = path.join(DIST, output);
    if (fs.existsSync(filePath)) {
      const stats = fs.statSync(filePath);
      const sizeMB = (stats.size / 1024 / 1024).toFixed(1);
      console.log(`  ${output} (${sizeMB} MB)`);
    }
  }
  console.log(`\nOutput directory: ${DIST}`);
}

async function buildTarget(target, output, entryPoint, excludeSharp = false) {
  console.log(`Building ${output}...`);

  const outputPath = path.join(DIST, output);

  // LMS builds: minimal config, exclude sharp and mcp
  // Full builds: use package.json config for all assets
  let pkgArgs;
  if (excludeSharp) {
    // LMS build - minimal, just include src/**/*.js
    pkgArgs = `"${entryPoint}" --target ${target} --output "${outputPath}" --ignore sharp --ignore mcp`;
  } else {
    // Full build - use package.json config for assets
    pkgArgs = `"${entryPoint}" --target ${target} --output "${outputPath}" --config package.json`;
  }

  try {
    execSync(`npx pkg ${pkgArgs}`, {
      cwd: ROOT,
      stdio: 'inherit',
    });
    console.log(`  ✓ ${output}\n`);
  } catch (error) {
    console.error(`  ✗ Failed to build ${output}\n`);
    process.exit(1);
  }
}

function checkNativeModules() {
  // sharp is a native module - pkg bundles it but we need platform-specific builds
  // The @yao-pkg/pkg handles this automatically for common native modules
  // LMS builds don't need sharp (no image processing)
  console.log('Note: Native modules (sharp) will be bundled for full builds.\n');
  console.log('LMS plugin builds are minimal (no sharp, no web UI).\n');
}

main().catch((err) => {
  console.error('Build failed:', err);
  process.exit(1);
});
