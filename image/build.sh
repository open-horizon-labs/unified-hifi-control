#!/usr/bin/env bash
#
# Build a flashable Raspberry Pi image with UHC pre-installed.
#
# Usage:
#   ./image/build.sh [--arch arm64|armv7] [--binary PATH]
#
# Prerequisites:
#   - Docker (pi-gen runs inside containers)
#   - A pre-built UHC binary for the target architecture
#     (or it will be built via cross if available)
#
# Output:
#   image/deploy/*.img.gz
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
PIGEN_DIR="$SCRIPT_DIR/pi-gen"
DEPLOY_DIR="$SCRIPT_DIR/deploy"

# Defaults
ARCH="arm64"
UHC_BINARY=""
PIGEN_REPO="https://github.com/RPi-Distro/pi-gen.git"
PIGEN_BRANCH="bookworm"
IMAGE_NAME="uhc-pi"

usage() {
    cat <<EOF
Usage: $(basename "$0") [OPTIONS]

Build a flashable Raspberry Pi image with UHC pre-installed.

Options:
  --arch ARCH       Target architecture: arm64 (default) or armv7
  --binary PATH     Path to pre-built UHC binary (skip cross-compilation)
  --clean           Remove pi-gen work directories before building
  -h, --help        Show this help

Examples:
  # Build arm64 image (Pi 3/4/5)
  ./image/build.sh --arch arm64

  # Build armv7 image (Pi Zero 2 W, Pi 2/3)
  ./image/build.sh --arch armv7

  # Use a pre-built binary
  ./image/build.sh --arch arm64 --binary ./target/aarch64-unknown-linux-musl/release/unified-hifi-control
EOF
    exit 0
}

CLEAN=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --arch)
            ARCH="$2"
            shift 2
            ;;
        --binary)
            UHC_BINARY="$2"
            shift 2
            ;;
        --clean)
            CLEAN=true
            shift
            ;;
        -h|--help)
            usage
            ;;
        *)
            echo "Unknown option: $1" >&2
            usage
            ;;
    esac
done

# Validate architecture
case "$ARCH" in
    arm64|aarch64)
        ARCH="arm64"
        CROSS_TARGET="aarch64-unknown-linux-musl"
        PIGEN_ARCH="arm64"
        ;;
    armv7|armhf)
        ARCH="armv7"
        CROSS_TARGET="armv7-unknown-linux-musleabihf"
        PIGEN_ARCH="armhf"
        ;;
    *)
        echo "Error: unsupported architecture '$ARCH'. Use arm64 or armv7." >&2
        exit 1
        ;;
esac

echo "==> Building UHC Pi image"
echo "    Architecture: $ARCH ($PIGEN_ARCH)"
echo "    Cross target: $CROSS_TARGET"

# --- Step 1: Obtain UHC binary ---

if [[ -n "$UHC_BINARY" ]]; then
    if [[ ! -f "$UHC_BINARY" ]]; then
        echo "Error: binary not found at $UHC_BINARY" >&2
        exit 1
    fi
    echo "==> Using pre-built binary: $UHC_BINARY"
else
    BINARY_PATH="$REPO_ROOT/target/$CROSS_TARGET/release/unified-hifi-control"
    if [[ -f "$BINARY_PATH" ]]; then
        echo "==> Found existing binary: $BINARY_PATH"
        UHC_BINARY="$BINARY_PATH"
    else
        echo "==> Building UHC binary for $CROSS_TARGET via cross..."
        if ! command -v cross &>/dev/null; then
            echo "Error: 'cross' not found. Install with: cargo install cross" >&2
            echo "Or provide a pre-built binary with --binary PATH" >&2
            exit 1
        fi
        (cd "$REPO_ROOT" && cross build --release --target "$CROSS_TARGET" --no-default-features --features server)
        UHC_BINARY="$BINARY_PATH"
    fi
fi

# Verify the binary is the correct architecture
if command -v readelf &>/dev/null; then
    ELF_MACHINE=$(readelf -h "$UHC_BINARY" 2>/dev/null | grep 'Machine:' || true)
    case "$ARCH" in
        arm64)
            if ! echo "$ELF_MACHINE" | grep -qi "AArch64"; then
                echo "Error: binary is not arm64: $ELF_MACHINE" >&2
                exit 1
            fi
            ;;
        armv7)
            if ! echo "$ELF_MACHINE" | grep -qi "ARM"; then
                echo "Error: binary is not armv7: $ELF_MACHINE" >&2
                exit 1
            fi
            ;;
    esac
else
    echo "WARNING: readelf not available, skipping architecture validation" >&2
fi

echo "    Binary size: $(du -h "$UHC_BINARY" | cut -f1)"

# --- Step 2: Clone/update pi-gen ---

if [[ ! -d "$PIGEN_DIR" ]]; then
    echo "==> Cloning pi-gen ($PIGEN_BRANCH)..."
    git clone --depth 1 --branch "$PIGEN_BRANCH" "$PIGEN_REPO" "$PIGEN_DIR"
else
    echo "==> pi-gen already present"
fi

if [[ "$CLEAN" == "true" ]]; then
    echo "==> Cleaning pi-gen work directories..."
    (cd "$PIGEN_DIR" && sudo rm -rf work deploy)
fi

# --- Step 3: Configure pi-gen ---

# Copy UHC binary to a location accessible during build
STAGE_DIR="$PIGEN_DIR/stage-uhc"
rm -rf "$STAGE_DIR"
cp -r "$SCRIPT_DIR/stage-uhc" "$STAGE_DIR"

# Place binary where the stage can find it
mkdir -p "$STAGE_DIR/01-uhc-binary/files"
cp "$UHC_BINARY" "$STAGE_DIR/01-uhc-binary/files/unified-hifi-control"
chmod 755 "$STAGE_DIR/01-uhc-binary/files/unified-hifi-control"

# Write pi-gen config
cat > "$PIGEN_DIR/config" <<EOF
IMG_NAME="$IMAGE_NAME"
RELEASE="bookworm"
TARGET_HOSTNAME="uhc"
FIRST_USER_NAME="uhc"
FIRST_USER_PASSWORD="uhc"
ENABLE_SSH="1"
LOCALE_DEFAULT="en_US.UTF-8"
KEYBOARD_KEYMAP="us"
KEYBOARD_LAYOUT="English (US)"
TIMEZONE_DEFAULT="UTC"
STAGE_LIST="stage0 stage1 stage2 stage-uhc"
EOF

# Skip stages 3-5 (desktop, full desktop, testing)
touch "$PIGEN_DIR/stage3/SKIP" "$PIGEN_DIR/stage4/SKIP" "$PIGEN_DIR/stage5/SKIP"
touch "$PIGEN_DIR/stage3/SKIP_IMAGES" "$PIGEN_DIR/stage4/SKIP_IMAGES" "$PIGEN_DIR/stage5/SKIP_IMAGES"

# Only generate image at stage-uhc (our final stage)
touch "$PIGEN_DIR/stage2/SKIP_IMAGES"
touch "$PIGEN_DIR/stage-uhc/EXPORT_IMAGE"

echo "==> pi-gen configured"
echo "    Image name: $IMAGE_NAME"
echo "    Hostname: uhc"
echo "    Architecture: $PIGEN_ARCH"

# --- Step 4: Build image ---

echo "==> Building image (this may take 15-30 minutes)..."
(cd "$PIGEN_DIR" && sudo ./build-docker.sh)

# --- Step 5: Collect output ---

mkdir -p "$DEPLOY_DIR"

# pi-gen outputs to deploy/ inside its directory
PIGEN_DEPLOY="$PIGEN_DIR/deploy"
if [[ -d "$PIGEN_DEPLOY" ]]; then
    # Find the most recent image
    BUILT_IMAGE=$(find "$PIGEN_DEPLOY" \( -name "*.img.gz" -o -name "*.img.xz" \) | sort | tail -1)
    if [[ -n "$BUILT_IMAGE" ]]; then
        # Preserve original compression extension
        IMG_EXT="${BUILT_IMAGE##*.img.}"
        FINAL_NAME="uhc-pi-${ARCH}.img.${IMG_EXT}"
        cp "$BUILT_IMAGE" "$DEPLOY_DIR/$FINAL_NAME"
        echo ""
        echo "==> Build complete!"
        echo "    Image: $DEPLOY_DIR/$FINAL_NAME"
        echo "    Size:  $(du -h "$DEPLOY_DIR/$FINAL_NAME" | cut -f1)"
        echo ""
        echo "Flash with:"
        echo "    # macOS/Linux"
        echo "    gunzip -c $DEPLOY_DIR/$FINAL_NAME | sudo dd of=/dev/sdX bs=4M status=progress"
        echo ""
        echo "    # Or use Raspberry Pi Imager -> 'Use custom'"
    else
        echo "Error: no image file found in $PIGEN_DEPLOY" >&2
        ls -la "$PIGEN_DEPLOY" >&2
        exit 1
    fi
else
    echo "Error: pi-gen deploy directory not found" >&2
    exit 1
fi
