#!/bin/bash

CONF=/etc/config/qpkg.conf
QPKG_NAME="unified-hifi-control"
QPKG_ROOT=$(/sbin/getcfg $QPKG_NAME Install_Path -f $CONF)
PID_FILE="${QPKG_ROOT}/unified-hifi-control.pid"

# Stop the service if running
if [ -f "$PID_FILE" ]; then
    PID=$(cat "$PID_FILE")
    if kill -0 "$PID" 2>/dev/null; then
        kill "$PID"
        sleep 2
        kill -9 "$PID" 2>/dev/null
    fi
    rm -f "$PID_FILE"
fi

echo "Unified Hi-Fi Control by Open Horizon Labs uninstalled"
exit 0
