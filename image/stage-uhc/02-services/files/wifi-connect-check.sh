#!/bin/bash
#
# Check if WiFi is connected. Used by wifi-connect.service to decide
# whether captive portal is needed.
#
# Exit 0 = no WiFi configured (start captive portal)
# Exit 1 = WiFi is connected (skip captive portal)
#

# If ethernet is connected, don't start captive portal
if nmcli -t -f TYPE,STATE dev | grep -q "ethernet:connected"; then
    exit 1
fi

# If WiFi is connected, don't start captive portal
if nmcli -t -f TYPE,STATE dev | grep -q "wifi:connected"; then
    exit 1
fi

# If there are saved WiFi connections, try to connect first
SAVED_WIFI=$(nmcli -t -f TYPE con show | grep -c "802-11-wireless" || true)
if [ "$SAVED_WIFI" -gt 0 ]; then
    # Poll for NetworkManager to auto-connect (up to 15s)
    for _ in $(seq 1 15); do
        if nmcli -t -f TYPE,STATE dev | grep -q "wifi:connected"; then
            exit 1
        fi
        sleep 1
    done
fi

# No network - start captive portal
echo "No network connection detected, starting captive portal" >&2
exit 0
