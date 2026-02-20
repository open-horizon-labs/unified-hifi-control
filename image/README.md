# UHC Raspberry Pi Image

Flashable SD card image that turns any Raspberry Pi into a UHC bridge appliance.
No SSH, no Docker, no Linux knowledge required.

## Supported Hardware

| Model | Architecture | Status |
|-------|-------------|--------|
| Pi Zero 2 W | armv7 | Primary target |
| Pi 2 Model B | armv7 | Supported |
| Pi 3 Model B/B+ | arm64 | Supported |
| Pi 4 Model B | arm64 | Supported |
| Pi 5 | arm64 | Supported |

## Quick Start

### 1. Build the image

```bash
# arm64 (Pi 3/4/5) - default
./image/build.sh

# armv7 (Pi Zero 2 W, Pi 2/3)
./image/build.sh --arch armv7

# Use a pre-built binary from CI
./image/build.sh --arch arm64 --binary ./path/to/unified-hifi-control
```

### 2. Flash the SD card

Use [Raspberry Pi Imager](https://www.raspberrypi.com/software/) (recommended):
1. Open Pi Imager
2. Choose OS -> "Use custom" -> select `image/deploy/uhc-pi-arm64.img.gz`
3. Choose storage -> select your SD card
4. Write

Or use `dd`:
```bash
gunzip -c image/deploy/uhc-pi-arm64.img.gz | sudo dd of=/dev/sdX bs=4M status=progress
sync
```

### 3. Boot and connect

Insert SD card, power on the Pi. Three scenarios:

**Ethernet connected:** UHC starts automatically.
Browse to http://uhc.local:8088 within 60 seconds.

**WiFi pre-configured (Pi Imager):** If you set WiFi credentials in Pi Imager's
settings before flashing, the Pi connects automatically. Browse to http://uhc.local:8088.

**No network configured:** The Pi creates a WiFi hotspot named **UHC-Setup**.
1. Connect your phone/laptop to "UHC-Setup"
2. A captive portal opens automatically
3. Select your WiFi network and enter the password
4. The Pi connects to your WiFi and starts UHC
5. Browse to http://uhc.local:8088

## Build Prerequisites

- Docker (pi-gen runs in containers)
- One of:
  - `cross` (`cargo install cross`) for automatic cross-compilation
  - A pre-built UHC binary for the target architecture

## What's in the Image

- **Base:** Raspberry Pi OS Lite (Bookworm) - minimal, headless
- **UHC binary:** `/usr/bin/unified-hifi-control`
- **Services:**
  - `unified-hifi-control.service` - main UHC bridge (auto-starts, auto-restarts)
  - `wifi-connect.service` - captive portal (runs only when no network configured)
  - `avahi-daemon.service` - mDNS (advertises `uhc.local` and `_uhc._tcp`)
- **Network:** NetworkManager (replaces dhcpcd for wifi-connect compatibility)
- **SSH:** Enabled (user: `uhc`, password: `uhc`)
- **SD card protection:**
  - `/var/log`, `/var/tmp`, `/tmp` on tmpfs (RAM)
  - Journal to RAM only (30MB cap)
  - noatime on root filesystem
  - Swap disabled
  - Reduced dirty writeback frequency

## Default Credentials

| | |
|---|---|
| **Hostname** | uhc |
| **SSH user** | uhc |
| **SSH password** | uhc |
| **Web UI** | http://uhc.local:8088 |
| **WiFi AP** | UHC-Setup (when no network configured) |

> **Change the SSH password** after first login: `passwd`

## Architecture

```text
Power on
  -> Ethernet? -> UHC starts -> uhc.local:8088 ready
  -> WiFi pre-configured? -> Connect -> UHC starts -> uhc.local:8088 ready
  -> No network? -> "UHC-Setup" AP
      -> Phone connects -> Captive portal
      -> User picks WiFi -> Pi connects
      -> UHC starts -> uhc.local:8088 ready
```

## Troubleshooting

### Can't find uhc.local

mDNS (`.local` addresses) can be flaky on some routers that block multicast.
Find the Pi's IP address instead:
- Check your router's DHCP lease table
- Or from another machine: `avahi-browse -art | grep uhc`
- Ethernet is always more reliable than WiFi for mDNS

### UHC not starting

SSH in and check:
```bash
ssh uhc@uhc.local
systemctl status unified-hifi-control
journalctl -u unified-hifi-control -f
```

### WiFi portal not appearing

The captive portal only starts when no network is configured.
If the Pi previously connected to WiFi, it remembers the credentials.

To reset WiFi and trigger the portal again:
```bash
ssh uhc@uhc.local
sudo nmcli con delete <connection-name>
sudo rm -f /var/lib/uhc-wifi-configured
sudo systemctl restart wifi-connect
```

## Development

### Clean rebuild
```bash
./image/build.sh --clean
```

### Image structure (pi-gen stages)

| Stage | Contents |
|-------|----------|
| stage0-2 | Standard Pi OS Lite (Bookworm) |
| stage-uhc/00-install-deps | NetworkManager, Avahi, wifi-connect |
| stage-uhc/01-uhc-binary | UHC binary to /usr/bin |
| stage-uhc/02-services | systemd services, Avahi mDNS config |
| stage-uhc/03-hardening | SD card wear mitigation, tmpfs, sysctl |

### Customizing wifi-connect portal

wifi-connect supports a custom React frontend. To brand the captive portal,
add UI files to `stage-uhc/00-install-deps/files/ui/` and modify `00-run.sh`
to copy them to `/usr/local/share/wifi-connect/ui/`.
