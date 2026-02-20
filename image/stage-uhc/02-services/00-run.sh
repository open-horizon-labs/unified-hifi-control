#!/bin/bash -e
#
# Install and enable systemd services, Avahi mDNS, and wifi-connect.
#

echo "==> Installing UHC services"

# Install systemd services
install -m 644 "${STAGE_DIR}/02-services/files/unified-hifi-control.service" \
    "${ROOTFS_DIR}/etc/systemd/system/unified-hifi-control.service"

install -m 644 "${STAGE_DIR}/02-services/files/wifi-connect.service" \
    "${ROOTFS_DIR}/etc/systemd/system/wifi-connect.service"

# Install WiFi check script
install -m 755 "${STAGE_DIR}/02-services/files/wifi-connect-check.sh" \
    "${ROOTFS_DIR}/usr/local/bin/wifi-connect-check"

# Install Avahi service file for mDNS discovery
install -d -m 755 "${ROOTFS_DIR}/etc/avahi/services"
install -m 644 "${STAGE_DIR}/02-services/files/uhc.service" \
    "${ROOTFS_DIR}/etc/avahi/services/uhc.service"

# Configure Avahi to advertise uhc.local
if [ -f "${ROOTFS_DIR}/etc/avahi/avahi-daemon.conf" ]; then
    # Enable hostname publishing
    sed -i 's/^#*host-name=.*/host-name=uhc/' \
        "${ROOTFS_DIR}/etc/avahi/avahi-daemon.conf"
    sed -i 's/^#*publish-hinfo=.*/publish-hinfo=yes/' \
        "${ROOTFS_DIR}/etc/avahi/avahi-daemon.conf"
    sed -i 's/^#*publish-workstation=.*/publish-workstation=no/' \
        "${ROOTFS_DIR}/etc/avahi/avahi-daemon.conf"
fi

# Enable services
on_chroot <<CHROOT
systemctl enable unified-hifi-control.service
systemctl enable wifi-connect.service
systemctl enable avahi-daemon.service
CHROOT

# Force password change on first SSH login
on_chroot <<CHROOT
chage -d 0 uhc
CHROOT

echo "==> Services installed and enabled"
echo "    - unified-hifi-control.service (UHC bridge)"
echo "    - wifi-connect.service (captive portal)"
echo "    - avahi-daemon.service (mDNS/uhc.local)"
