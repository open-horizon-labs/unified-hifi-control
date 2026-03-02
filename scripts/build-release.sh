#!/usr/bin/env bash
# Build the release binary with embedded WASM assets.
#
# Two-step process (see AGENTS.md § "Single-binary build"):
#   1. dx build — compiles WASM/JS assets into target/dx/
#   2. cargo build — compiles the server and embeds those assets
#
# Usage:
#   ./scripts/build-release.sh          # full build (WASM + server)
#   ./scripts/build-release.sh --server # server-only (skip WASM, reuse cached assets)
#
# The resulting binary is at: ./target/release/unified-hifi-control
set -euo pipefail

SERVER_ONLY=false
for arg in "$@"; do
  case "$arg" in
    --server) SERVER_ONLY=true ;;
    *) echo "Unknown arg: $arg"; exit 1 ;;
  esac
done

# Step 1: WASM/JS assets
if [ "$SERVER_ONLY" = false ]; then
  echo "==> Step 1/2: Building WASM assets (dx build)..."
  dx build --fullstack --release \
    '@client' --no-default-features --features web \
    '@server' --features server
else
  # Verify cached assets exist
  if [ ! -d "target/dx/unified-hifi-control/release/web/public" ]; then
    echo "ERROR: No cached WASM assets found. Run without --server first."
    exit 1
  fi
  echo "==> Step 1/2: Skipping WASM (--server, using cached assets)"
fi

# Step 2: Server binary with embedded assets
echo "==> Step 2/2: Building server binary (cargo build)..."
SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk \
  cargo build --release --bin unified-hifi-control \
  --features server --no-default-features

echo ""
echo "Done. Binary at: ./target/release/unified-hifi-control"
