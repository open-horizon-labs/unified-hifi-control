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
chmod +x "${QPKG_ROOT}/uhc-hiphi-pair"
chmod +x "${QPKG_ROOT}/unified-hifi-control.sh"

# Core UHC logging rotates daily here with bounded retention. The launcher log
# is truncated on every start and contains only pre-initialization failures.
mkdir -p "${QPKG_ROOT}/logs"
chmod 700 "${QPKG_ROOT}/logs"
touch "${QPKG_ROOT}/unified-hifi-control-launcher.log"
chmod 600 "${QPKG_ROOT}/unified-hifi-control-launcher.log"

# Keep provider credentials in the package-owned config volume.  The server's
# config resolver creates the `unified-hifi` subdirectory below this path;
# creating the parent here makes a fresh install deterministic and lets the
# service run without relying on HOME or an operator shell profile.
mkdir -p "${QPKG_ROOT}/config"
chmod 700 "${QPKG_ROOT}/config"

# Pairing writes only these four public connector bindings here.  The private
# installation key stays in the same owner-only config directory and is never
# copied to HiPhi Cloud.  Preserve both across QPKG upgrades.
if [ -L "${QPKG_ROOT}/config/hiphi.env" ]; then
    echo "Refusing symlinked HiPhi connector configuration" >&2
    exit 1
elif [ ! -e "${QPKG_ROOT}/config/hiphi.env" ]; then
    touch "${QPKG_ROOT}/config/hiphi.env"
elif [ ! -f "${QPKG_ROOT}/config/hiphi.env" ]; then
    echo "Refusing non-file HiPhi connector configuration" >&2
    exit 1
fi
chmod 600 "${QPKG_ROOT}/config/hiphi.env"

# Restart service if it was running before upgrade
if [ "$WAS_RUNNING" = true ]; then
    echo "Restarting service after upgrade..."
    "${QPKG_ROOT}/unified-hifi-control.sh" start
fi

echo "Unified Hi-Fi Control installed successfully"
echo "Access the web UI at http://$(hostname):8088"

exit 0
