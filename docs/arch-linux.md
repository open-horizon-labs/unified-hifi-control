# Arch Linux Installation

Unified Hi-Fi Control is available for Arch Linux and Arch-based distributions (RoPieee, AudioLinux, etc.).

## Installation from AUR

### Using an AUR helper (recommended)

```bash
# Using yay
yay -S unified-hifi-control-bin

# Using paru
paru -S unified-hifi-control-bin
```

### Manual installation

```bash
git clone https://aur.archlinux.org/unified-hifi-control-bin.git
cd unified-hifi-control-bin
makepkg -si
```

## Post-Installation

### Start the service

```bash
# Enable and start the service
sudo systemctl enable --now unified-hifi-control

# Check status
sudo systemctl status unified-hifi-control

# View logs
journalctl -u unified-hifi-control -f
```

### Access the Web UI

Open your browser to: **http://localhost:8088**

## Configuration

Settings, adapter configuration, and pairing state are stored in
`/var/lib/unified-hifi-control/`. systemd creates this directory and grants the
service's dynamic user access to it.

### Environment Variables

The systemd service supports these environment variables (edit the service file or use a drop-in):

| Variable | Default | Description |
|----------|---------|-------------|
| `PORT` | `8088` | HTTP server port |
| `CONFIG_DIR` | `/var/lib/unified-hifi-control` | Settings, adapter configuration, and persistent state directory |
| `RUST_LOG` | `info` | Log level (trace, debug, info, warn, error) |

To customize, create a drop-in:

```bash
sudo systemctl edit unified-hifi-control
```

Add your overrides:

```ini
[Service]
Environment=PORT=9000
Environment=RUST_LOG=debug
```

### Upgrading from packages that used `/etc`

Older package versions pointed `CONFIG_DIR` at `/etc/unified-hifi-control/`.
The service could read administrator-created files there, but its dynamic user
could not update them. If that directory contains configuration you still use,
move it into the systemd-managed state directory before restarting:

```bash
# Start once so systemd provisions the state directory with the dynamic user's ownership.
sudo systemctl start unified-hifi-control
sudo systemctl stop unified-hifi-control
sudo cp -a /etc/unified-hifi-control/. /var/lib/unified-hifi-control/
sudo chown -R -H --reference=/var/lib/unified-hifi-control /var/lib/unified-hifi-control
sudo systemctl start unified-hifi-control
```

After confirming the migrated settings work, the legacy `/etc` directory can
be removed. Existing drop-ins that override `CONFIG_DIR` remain compatible.

## File Locations

| Path | Description |
|------|-------------|
| `/usr/bin/unified-hifi-control` | Binary |
| `/var/lib/unified-hifi-control/` | Settings, adapter configuration, and persistent state |
| `/usr/lib/systemd/system/unified-hifi-control.service` | Systemd service |

Web assets are embedded in the binary; no separate web asset directory is installed.

## Uninstallation

```bash
# Using yay
yay -Rns unified-hifi-control-bin

# Manual
sudo pacman -Rns unified-hifi-control-bin
```

Configuration and state are preserved. Remove them manually if no longer needed:

```bash
sudo rm -rf /var/lib/unified-hifi-control
```

## Building from Source

If you prefer to build from source instead of using the binary package:

### Prerequisites

```bash
# Install build tools and Rust
sudo pacman -S --needed base-devel rustup
rustup default stable
rustup target add wasm32-unknown-unknown

# Install Dioxus CLI
cargo install dioxus-cli@0.7.10 --locked
```

### Build

```bash
git clone https://github.com/open-horizon-labs/unified-hifi-control.git
cd unified-hifi-control
git checkout v3

# Build matching CSS, server, and WASM hydration bundle, then run it.
UHC_PORT=8088 make web-run
```

## RoPieee / AudioLinux Integration

For RoPieee and AudioLinux developers: this package follows standard Arch packaging conventions. The PKGBUILD can be adapted for inclusion in your distribution's package repository.

Key considerations:
- Binary is statically linked (musl) with no runtime dependencies
- Web assets are embedded in the binary
- Systemd service uses `DynamicUser=yes` for security
- Configuration and state persist in `/var/lib/unified-hifi-control/`
