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

Configuration files are stored in `/etc/unified-hifi-control/`.

### Environment Variables

The systemd service supports these environment variables (edit the service file or use a drop-in):

| Variable | Default | Description |
|----------|---------|-------------|
| `PORT` | `8088` | HTTP server port |
| `CONFIG_DIR` | `/etc/unified-hifi-control` | Configuration directory |
| `DATA_DIR` | `/var/lib/unified-hifi-control` | State/data directory |
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

## File Locations

| Path | Description |
|------|-------------|
| `/usr/bin/unified-hifi-control` | Binary |
| `/usr/share/unified-hifi-control/public/` | Web assets |
| `/etc/unified-hifi-control/` | Configuration |
| `/var/lib/unified-hifi-control/` | Runtime state, Roon tokens |
| `/usr/lib/systemd/system/unified-hifi-control.service` | Systemd service |

## Uninstallation

```bash
# Using yay
yay -Rns unified-hifi-control-bin

# Manual
sudo pacman -Rns unified-hifi-control-bin
```

Configuration and state directories are preserved. Remove manually if no longer needed:

```bash
sudo rm -rf /etc/unified-hifi-control
sudo rm -rf /var/lib/unified-hifi-control
```

## Building from Source

If you prefer to build from source instead of using the binary package:

### Prerequisites

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup target add wasm32-unknown-unknown

# Install Dioxus CLI
cargo install dioxus-cli

# Install Node.js (for Tailwind CSS)
sudo pacman -S nodejs npm
```

### Build

```bash
git clone https://github.com/open-horizon-labs/unified-hifi-control.git
cd unified-hifi-control
git checkout v3

# Build CSS
make css

# Build web assets
dx build --release --platform web --features web

# Build server binary
cargo build --release
```

The binary will be at `target/release/unified-hifi-control`.

## RoPieee / AudioLinux Integration

For RoPieee and AudioLinux developers: this package follows standard Arch packaging conventions. The PKGBUILD can be adapted for inclusion in your distribution's package repository.

Key considerations:
- Binary is statically linked (musl) with no runtime dependencies
- Web assets are required at `/usr/share/unified-hifi-control/public/` (symlinked to state dir)
- Systemd service uses `DynamicUser=yes` for security
- Configuration persists in `/etc/unified-hifi-control/`
