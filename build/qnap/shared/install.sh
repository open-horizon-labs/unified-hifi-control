#!/bin/bash

CONF=/etc/config/qpkg.conf
QPKG_NAME="unified-hifi-control"
QPKG_ROOT=$(/sbin/getcfg $QPKG_NAME Install_Path -f $CONF)

# Check if service was running (for restart after upgrade)
WAS_RUNNING=false
if [ -x "${QPKG_ROOT}/unified-hifi-control.sh" ]; then
    if "${QPKG_ROOT}/unified-hifi-control.sh" status 2>/dev/null | grep -q "running"; then
        WAS_RUNNING=true
    fi
    echo "Stopping existing service for upgrade..."
    "${QPKG_ROOT}/unified-hifi-control.sh" stop 2>/dev/null || true
    sleep 1
fi

# Set executable permissions
chmod +x "${QPKG_ROOT}/unified-hifi-control"
chmod +x "${QPKG_ROOT}/unified-hifi-control.sh"

# Create log file
touch "${QPKG_ROOT}/unified-hifi-control.log"

# Restart service if it was running before upgrade
if [ "$WAS_RUNNING" = true ]; then
    echo "Restarting service after upgrade..."
    "${QPKG_ROOT}/unified-hifi-control.sh" start
fi

echo "Unified Hi-Fi Control installed successfully"
echo "Access the web UI at http://$(hostname):8088"

exit 0
