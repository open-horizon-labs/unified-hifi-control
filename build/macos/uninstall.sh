#!/bin/bash

# Uninstall script for Unified Hi-Fi Control

set -e

# Check for root privileges
if [[ $EUID -ne 0 ]]; then
    echo "This script requires administrator privileges."
    echo "Please run with: sudo $0"
    exit 1
fi

echo "Uninstalling Unified Hi-Fi Control..."

# Stop and unload the service
launchctl stop com.cloudatlas.unified-hifi-control 2>/dev/null || true
launchctl unload /Library/LaunchDaemons/com.cloudatlas.unified-hifi-control.plist 2>/dev/null || true

# Remove files
rm -f /usr/local/bin/unified-hifi-control
rm -f /Library/LaunchDaemons/com.cloudatlas.unified-hifi-control.plist

# Handle configuration data removal
# In non-interactive mode, preserve config by default
if [[ -t 0 ]]; then
    # Interactive mode - ask user
    read -p "Remove configuration data? [y/N] " -n 1 -r
    echo
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        rm -rf /usr/local/var/unified-hifi-control
        echo "Configuration data removed."
    else
        echo "Configuration data preserved at /usr/local/var/unified-hifi-control"
    fi
else
    # Non-interactive mode - preserve config
    echo "Non-interactive mode: configuration data preserved at /usr/local/var/unified-hifi-control"
    echo "To remove manually: sudo rm -rf /usr/local/var/unified-hifi-control"
fi

# Remove package receipt
pkgutil --forget com.cloudatlas.unified-hifi-control 2>/dev/null || true

echo "Unified Hi-Fi Control has been uninstalled."
