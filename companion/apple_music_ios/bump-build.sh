#!/bin/sh
# Increment the build number for the next TestFlight upload.
# App Store Connect refuses an upload whose build number it has already seen.
set -eu
CONFIG="$(dirname "$0")/Xcode/Config/Version.xcconfig"
CURRENT=$(sed -n 's/^CURRENT_PROJECT_VERSION = //p' "$CONFIG")
NEXT=$((CURRENT + 1))
sed -i '' "s/^CURRENT_PROJECT_VERSION = .*/CURRENT_PROJECT_VERSION = ${NEXT}/" "$CONFIG"
echo "build ${CURRENT} -> ${NEXT}"
