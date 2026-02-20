#!/bin/bash -e
#
# Install wifi-connect for captive portal WiFi provisioning.
# https://github.com/balena-os/wifi-connect
#

# Determine architecture inside the chroot
DPKG_ARCH=$(chroot "${ROOTFS_DIR}" dpkg --print-architecture)

WIFI_CONNECT_VERSION="4.4.6"
case "$DPKG_ARCH" in
    arm64|aarch64)
        WIFI_CONNECT_ARCH="aarch64"
        ;;
    armhf|armv7l)
        WIFI_CONNECT_ARCH="rpi"
        ;;
    *)
        echo "WARNING: wifi-connect does not support $DPKG_ARCH, skipping" >&2
        exit 0
        ;;
esac

WIFI_CONNECT_URL="https://github.com/balena-os/wifi-connect/releases/download/v${WIFI_CONNECT_VERSION}/wifi-connect-v${WIFI_CONNECT_VERSION}-linux-${WIFI_CONNECT_ARCH}.tar.gz"

echo "==> Installing wifi-connect ${WIFI_CONNECT_VERSION} (${WIFI_CONNECT_ARCH})"

# Download and extract wifi-connect
mkdir -p "${ROOTFS_DIR}/usr/local/share/wifi-connect"
wget -q -O /tmp/wifi-connect.tar.gz "$WIFI_CONNECT_URL"
tar xzf /tmp/wifi-connect.tar.gz -C "${ROOTFS_DIR}/usr/local/share/wifi-connect"
rm /tmp/wifi-connect.tar.gz

# Create symlink for the binary
ln -sf /usr/local/share/wifi-connect/wifi-connect "${ROOTFS_DIR}/usr/local/bin/wifi-connect"

# Disable dhcpcd (NetworkManager replaces it)
on_chroot <<CHROOT
systemctl disable dhcpcd.service 2>/dev/null || true
systemctl mask dhcpcd.service 2>/dev/null || true
systemctl enable NetworkManager.service
CHROOT

echo "==> wifi-connect installed"
