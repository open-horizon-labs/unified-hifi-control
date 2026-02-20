#!/bin/bash -e
#
# Install the UHC binary into the image.
#

echo "==> Installing UHC binary"

# Copy binary (placed here by build.sh)
install -m 755 "${STAGE_DIR}/01-uhc-binary/files/unified-hifi-control" \
    "${ROOTFS_DIR}/usr/bin/unified-hifi-control"

# Create data and config directories
install -d -m 755 "${ROOTFS_DIR}/var/lib/unified-hifi-control"
install -d -m 755 "${ROOTFS_DIR}/etc/unified-hifi-control"

echo "==> UHC binary installed to /usr/bin/unified-hifi-control"
echo "    $(ls -lh "${ROOTFS_DIR}/usr/bin/unified-hifi-control" | awk '{print $5}')"
