#!/usr/bin/env bash
set -euo pipefail

# Build the SwiftPM host and wrap it in a normal macOS .app bundle. Xcode can
# sign the resulting bundle during development; CODE_SIGN_IDENTITY may also
# be set for a local Developer ID or Apple Development identity.

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
CONFIGURATION=${CONFIGURATION:-debug}
APP_NAME=${APP_NAME:-Apple Music Companion}
IDENTITY=${CODE_SIGN_IDENTITY:--}
PRODUCT=AppleMusicCompanionApp

swift build \
  --package-path "$SCRIPT_DIR" \
  --configuration "$CONFIGURATION" \
  --product "$PRODUCT"

BIN_DIR=$(swift build --package-path "$SCRIPT_DIR" --configuration "$CONFIGURATION" --show-bin-path)
APP_DIR=${APP_DIR:-"$BIN_DIR/$APP_NAME.app"}
CONTENTS="$APP_DIR/Contents"

rm -rf "$APP_DIR"
mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources"
cp "$BIN_DIR/$PRODUCT" "$CONTENTS/MacOS/$PRODUCT"
cp "$SCRIPT_DIR/Host/Info.plist" "$CONTENTS/Info.plist"

/usr/libexec/PlistBuddy \
  -c 'Add :CFBundleIdentifier string com.openhorizonlabs.uhc.applemusiccompanion' \
  -c "Add :CFBundleExecutable string $PRODUCT" \
  -c 'Add :CFBundlePackageType string APPL' \
  -c 'Add :CFBundleVersion string 1' \
  -c 'Add :CFBundleShortVersionString string 0.1.0' \
  "$CONTENTS/Info.plist"

SIGN_ARGS=(--force --deep --sign "$IDENTITY")
if [[ -f "$SCRIPT_DIR/Host/AppleMusicCompanion.entitlements" ]]; then
  SIGN_ARGS+=(--entitlements "$SCRIPT_DIR/Host/AppleMusicCompanion.entitlements")
fi
codesign "${SIGN_ARGS[@]}" "$APP_DIR"

echo "$APP_DIR"
