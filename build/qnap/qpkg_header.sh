#!/bin/sh
# QPKG self-extracting package for Unified Hi-Fi Control
# This header is concatenated with payload archives

QPKG_NAME="unified-hifi-control"
QPKG_DISPLAYNAME="Unified Hi-Fi Control"

# Installation directory
QPKG_INSTALL_PATH="/share/CACHEDEV1_DATA/.qpkg"
QPKG_DIR="${QPKG_INSTALL_PATH}/${QPKG_NAME}"

# Find script location and calculate payload offset
SCRIPT_PATH="$0"
SCRIPT_SIZE=$(sed '/^exit 1$/q' "$SCRIPT_PATH" | wc -c)

# Check if running as root
if [ "$(id -u)" != "0" ]; then
    echo "Error: This script must be run as root"
    exit 1
fi

# Create installation directory
mkdir -p "$QPKG_DIR"

# Extract payload (everything after 'exit 1')
echo "Extracting ${QPKG_DISPLAYNAME}..."
tail -c +$((SCRIPT_SIZE + 1)) "$SCRIPT_PATH" > /tmp/qpkg_payload.tar.gz

# Extract the archive
tar -xzf /tmp/qpkg_payload.tar.gz -C "$QPKG_DIR"
rm -f /tmp/qpkg_payload.tar.gz

# Make binary executable
chmod +x "${QPKG_DIR}/unified-hifi-control"
chmod +x "${QPKG_DIR}/unified-hifi-control.sh"

# Run install script if present
if [ -f "${QPKG_DIR}/install.sh" ]; then
    chmod +x "${QPKG_DIR}/install.sh"
    "${QPKG_DIR}/install.sh"
fi

# Register with QNAP
echo "Registering package..."
/sbin/setcfg "${QPKG_NAME}" Name "${QPKG_NAME}" -f /etc/config/qpkg.conf
/sbin/setcfg "${QPKG_NAME}" Display_Name "${QPKG_DISPLAYNAME}" -f /etc/config/qpkg.conf
/sbin/setcfg "${QPKG_NAME}" Version "{{VERSION}}" -f /etc/config/qpkg.conf
/sbin/setcfg "${QPKG_NAME}" Author "Muness Castle" -f /etc/config/qpkg.conf
/sbin/setcfg "${QPKG_NAME}" Install_Path "${QPKG_DIR}" -f /etc/config/qpkg.conf
/sbin/setcfg "${QPKG_NAME}" Enable TRUE -f /etc/config/qpkg.conf
/sbin/setcfg "${QPKG_NAME}" Service_Port 8088 -f /etc/config/qpkg.conf

# Start the service
echo "Starting ${QPKG_DISPLAYNAME}..."
"${QPKG_DIR}/unified-hifi-control.sh" start

echo "Installation complete!"
exit 0
exit 1
