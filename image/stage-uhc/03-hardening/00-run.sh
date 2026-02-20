#!/bin/bash -e
#
# SD card wear mitigation and system hardening for appliance use.
#
# Goals:
# - Minimize writes to extend SD card lifespan
# - Set up tmpfs for volatile data
# - Configure journald for RAM-only logging
# - Set noatime on filesystems
#

echo "==> Applying SD card wear mitigation"

# --- noatime on root filesystem ---
# Add noatime to existing root partition entry (before we append tmpfs lines)
sed -i '/^[^#].*\/.*defaults/ s|defaults|defaults,noatime|' "${ROOTFS_DIR}/etc/fstab"

# --- tmpfs for volatile directories ---

cat >> "${ROOTFS_DIR}/etc/fstab" <<'FSTAB'
# SD card wear mitigation - volatile data in RAM
tmpfs /var/log     tmpfs defaults,noatime,nosuid,nodev,noexec,mode=0755,size=50M 0 0
tmpfs /var/tmp     tmpfs defaults,noatime,nosuid,nodev,noexec,mode=1777,size=50M 0 0
tmpfs /tmp         tmpfs defaults,noatime,nosuid,nodev,noexec,mode=1777,size=100M 0 0
FSTAB

# --- journald: log to RAM only, limit size ---

install -d -m 755 "${ROOTFS_DIR}/etc/systemd/journald.conf.d"
cat > "${ROOTFS_DIR}/etc/systemd/journald.conf.d/uhc-sdcard.conf" <<'JOURNALD'
[Journal]
# Store journal in RAM only (no persistent /var/log/journal)
Storage=volatile
# Limit RAM usage
RuntimeMaxUse=30M
RuntimeMaxFileSize=5M
# Compress to reduce RAM usage
Compress=yes
# Drop old entries rather than blocking
RateLimitIntervalSec=30s
RateLimitBurst=1000
JOURNALD

# --- Disable swap (saves SD writes) ---

on_chroot <<CHROOT
# Disable dphys-swapfile if present
systemctl disable dphys-swapfile.service 2>/dev/null || true
systemctl mask dphys-swapfile.service 2>/dev/null || true

# Remove swap file if it exists
rm -f /var/swap
CHROOT

# --- Sysctl tweaks for reduced writes ---

cat > "${ROOTFS_DIR}/etc/sysctl.d/99-uhc-sdcard.conf" <<'SYSCTL'
# Reduce write frequency
vm.dirty_writeback_centisecs = 6000
vm.dirty_expire_centisecs = 6000

# Reduce swappiness (prefer killing over swapping to SD)
vm.swappiness = 1
SYSCTL

# --- Disable man-db auto-update (saves writes + CPU on upgrades) ---

on_chroot <<CHROOT
if [ -f /etc/cron.daily/man-db ] || [ -f /etc/cron.weekly/man-db ]; then
    rm -f /etc/cron.daily/man-db /etc/cron.weekly/man-db
fi
CHROOT

echo "==> SD card hardening complete"
echo "    - tmpfs for /var/log, /var/tmp, /tmp"
echo "    - noatime on root filesystem"
echo "    - journald: volatile (RAM-only), 30MB max"
echo "    - swap disabled"
echo "    - reduced dirty writeback frequency"
