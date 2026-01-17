#!/bin/bash

CONF=/etc/config/qpkg.conf
QPKG_NAME="unified-hifi-control"
QPKG_ROOT=$(/sbin/getcfg $QPKG_NAME Install_Path -f $CONF)

# Set executable permissions
chmod +x "${QPKG_ROOT}/unified-hifi-control"
chmod +x "${QPKG_ROOT}/unified-hifi-control.sh"

# Create log file
touch "${QPKG_ROOT}/unified-hifi-control.log"

echo "Unified Hi-Fi Control installed successfully"
echo "Access the web UI at http://$(hostname):8088"

exit 0
