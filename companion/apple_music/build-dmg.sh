#!/usr/bin/env bash
set -euo pipefail

# Build the arm64-only (Apple Silicon) macOS companion app from the Xcode
# project (XcodeMac/AppleMusicCompanionMac.xcworkspace) and wrap it in a
# distributable .dmg. This reproduces what CI's companion-dmg job produces
# (see .github/workflows/build.yml) so it can be verified locally on a
# Mac before/without CI.
#
# ARM64 (Apple Silicon) only, by user decision — this script never builds
# x86_64 or a universal binary.
#
# By default the app is built unsigned/ad-hoc (no Developer ID required),
# matching the CI environment which has no signing identity available. Set
# CODE_SIGN_IDENTITY (and DEVELOPMENT_TEAM) to sign with a local identity
# instead. See companion/apple_music/README.md for the right-click-Open /
# xattr workaround unsigned builds require on first launch.

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
WORKSPACE="$SCRIPT_DIR/XcodeMac/AppleMusicCompanionMac.xcworkspace"
SCHEME=${SCHEME:-AppleMusicCompanionMac}
CONFIGURATION=${CONFIGURATION:-Release}
APP_NAME=AppleMusicCompanionMac
VERSION=${VERSION:-0.0.0-dev}
VOLUME_NAME=${VOLUME_NAME:-"Apple Music Companion"}
OUTPUT_DIR=${OUTPUT_DIR:-"$SCRIPT_DIR/dist"}
DMG_NAME=${DMG_NAME:-"unified-hifi-applemusic-companion-macos-arm64-${VERSION}.dmg"}

# Signing overrides. Defaults produce an unsigned/ad-hoc build so this works
# without a Developer ID (matches CI). CODE_SIGN_IDENTITY=- is ad-hoc;
# DEVELOPMENT_TEAM must stay empty for ad-hoc signing.
CODE_SIGN_IDENTITY=${CODE_SIGN_IDENTITY:--}
CODE_SIGN_STYLE=${CODE_SIGN_STYLE:-Manual}
DEVELOPMENT_TEAM=${DEVELOPMENT_TEAM:-}
CODE_SIGNING_REQUIRED=${CODE_SIGNING_REQUIRED:-NO}
CODE_SIGNING_ALLOWED=${CODE_SIGNING_ALLOWED:-YES}

if ! command -v xcodebuild >/dev/null 2>&1; then
  echo "error: xcodebuild not found. Install Xcode (not just the Command Line Tools)." >&2
  exit 1
fi

if ! xcodebuild -version >/dev/null 2>&1; then
  echo "error: xcodebuild requires a full Xcode install, not just the Command Line" >&2
  echo "  Tools. If Xcode.app is installed but 'xcode-select' points elsewhere," >&2
  echo "  either run 'sudo xcode-select -s /Applications/Xcode.app/Contents/Developer'" >&2
  echo "  or re-run this script with DEVELOPER_DIR set, e.g.:" >&2
  echo "  DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer $0" >&2
  exit 1
fi

DERIVED_DATA_DIR=$(mktemp -d "${TMPDIR:-/tmp}/applemusic-companion-dmg.XXXXXX")
STAGING_DIR=$(mktemp -d "${TMPDIR:-/tmp}/applemusic-companion-staging.XXXXXX")
cleanup() {
  rm -rf "$DERIVED_DATA_DIR" "$STAGING_DIR"
}
trap cleanup EXIT

echo "==> Building $SCHEME ($CONFIGURATION, arm64) from $WORKSPACE"
xcodebuild \
  -workspace "$WORKSPACE" \
  -scheme "$SCHEME" \
  -configuration "$CONFIGURATION" \
  -derivedDataPath "$DERIVED_DATA_DIR" \
  -destination 'generic/platform=macOS' \
  ARCHS=arm64 \
  ONLY_ACTIVE_ARCH=NO \
  CODE_SIGN_IDENTITY="$CODE_SIGN_IDENTITY" \
  CODE_SIGN_STYLE="$CODE_SIGN_STYLE" \
  DEVELOPMENT_TEAM="$DEVELOPMENT_TEAM" \
  CODE_SIGNING_REQUIRED="$CODE_SIGNING_REQUIRED" \
  CODE_SIGNING_ALLOWED="$CODE_SIGNING_ALLOWED" \
  build

BUILT_APP="$DERIVED_DATA_DIR/Build/Products/$CONFIGURATION/$APP_NAME.app"
if [[ ! -d "$BUILT_APP" ]]; then
  echo "error: expected built app at $BUILT_APP" >&2
  exit 1
fi

EXECUTABLE="$BUILT_APP/Contents/MacOS/$APP_NAME"
ARCHES=$(lipo -archs "$EXECUTABLE")
echo "==> Executable architectures: $ARCHES"
if [[ "$ARCHES" != "arm64" ]]; then
  echo "error: expected an arm64-only executable, got '$ARCHES'" >&2
  exit 1
fi

rm -rf "$OUTPUT_DIR"
mkdir -p "$OUTPUT_DIR"

cp -R "$BUILT_APP" "$STAGING_DIR/"
ln -s /Applications "$STAGING_DIR/Applications"

DMG_PATH="$OUTPUT_DIR/$DMG_NAME"
rm -f "$DMG_PATH"

echo "==> Creating $DMG_PATH"
hdiutil create \
  -volname "$VOLUME_NAME" \
  -srcfolder "$STAGING_DIR" \
  -ov -format UDZO \
  "$DMG_PATH"

echo "==> Created $DMG_PATH"
if [[ "$CODE_SIGN_IDENTITY" == "-" ]]; then
  echo "==> This is an unsigned/ad-hoc build. First launch on another Mac"
  echo "    requires right-click > Open (or removing the quarantine"
  echo "    attribute with 'xattr -dr com.apple.quarantine'). See"
  echo "    companion/apple_music/README.md for details."
fi

echo "$DMG_PATH"
