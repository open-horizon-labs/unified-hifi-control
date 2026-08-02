#!/bin/bash

CONF=/etc/config/qpkg.conf
QPKG_NAME="unified-hifi-control"
QPKG_ROOT=$(/sbin/getcfg $QPKG_NAME Install_Path -f $CONF)
PID_FILE="${QPKG_ROOT}/unified-hifi-control.pid"

# A stale PID file may name an unrelated process after PID reuse. Fail closed when
# the executable identity cannot be proven, and re-check after the grace period.
is_our_pid() {
    PID_TO_CHECK=$1
    case "$PID_TO_CHECK" in
        ''|*[!0-9]*) return 1 ;;
    esac
    [ "$PID_TO_CHECK" -gt 1 ] 2>/dev/null || return 1
    kill -0 "$PID_TO_CHECK" 2>/dev/null || return 1
    EXPECTED_EXE=$(readlink -f "${QPKG_ROOT}/unified-hifi-control" 2>/dev/null) || return 1
    ACTUAL_EXE=$(readlink -f "/proc/${PID_TO_CHECK}/exe" 2>/dev/null) || return 1
    [ -n "$EXPECTED_EXE" ] && [ "$ACTUAL_EXE" = "$EXPECTED_EXE" ]
}

# Stop the service if running
if [ -f "$PID_FILE" ]; then
    PID=$(cat "$PID_FILE")
    if is_our_pid "$PID"; then
        kill "$PID" 2>/dev/null
        sleep 2
        is_our_pid "$PID" && kill -9 "$PID" 2>/dev/null
    fi
    rm -f "$PID_FILE"
fi

echo "Unified Hi-Fi Control uninstalled"
exit 0
