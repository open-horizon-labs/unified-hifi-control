# Unified Hi-Fi Control

A source-agnostic hi-fi control bridge that connects music sources and audio pipeline control to any surface — hardware knobs, web UIs, or Home Assistant.

## Vision

Hi-fi software assumes you're at a computer or using vendor-specific apps. This bridge fills the gap:

- **Music Sources:** Roon, Lyrion/LMS, OpenHome, UPnP/DLNA — all optional, any can contribute zones
- **Audio Pipeline:** HQPlayer DSP enrichment (link any zone to HQPlayer for upsampling/filtering)
- **Surfaces:** Anything that speaks HTTP or MQTT — ESP32 hardware, web UIs, Home Assistant, Claude (via MCP), etc.

## Status

**Stable: [v2.7.0](https://github.com/open-horizon-labs/unified-hifi-control/releases/tag/v2.7.0)** (Node.js) — Production ready

**Preview: v3.0.0-rc.1** (Rust) — Native packages for Synology, QNAP, LMS; 10x smaller memory footprint

Works with [roon-knob](https://github.com/muness/roon-knob) for hardware control of Roon, OpenHome, UPnP, or LMS/Lyrion zones.

## Installation

### Docker (Recommended)

```bash
docker pull muness/unified-hifi-control:v3.0.0-rc.1
```

### Synology NAS (DSM 7)

Download the SPK package from [Releases](https://github.com/open-horizon-labs/unified-hifi-control/releases):
- `unified-hifi-control_*_apollolake.spk` — Intel x86_64 (DS918+, DS920+, etc.)
- `unified-hifi-control_*_rtd1296.spk` — ARM64 (DS220+, DS420+, etc.)

### QNAP NAS

Download the QPKG package from [Releases](https://github.com/open-horizon-labs/unified-hifi-control/releases):
- `unified-hifi-control_*_x86_64.qpkg` — Intel/AMD x86_64
- `unified-hifi-control_*_arm_64.qpkg` — ARM64

### LMS Plugin

Add this repository URL in LMS Settings → Plugins → Additional Repositories:
```
https://raw.githubusercontent.com/open-horizon-labs/unified-hifi-control/v3/lms-plugin/repo.xml
```
Then install "Unified Hi-Fi Control" from the plugin list. The plugin automatically downloads and manages the bridge binary.

### Binary Downloads

Pre-built binaries available for Linux (x64, arm64, armv7), macOS (x64, arm64), and Windows from [Releases](https://github.com/open-horizon-labs/unified-hifi-control/releases).

## Quick Start (Docker)

```yaml
# docker-compose.yml
services:
  unified-hifi-control:
    image: muness/unified-hifi-control:v3.0.0-rc.1
    network_mode: host  # Required for Roon/UPnP discovery
    volumes:
      - ./data:/data
    environment:
      - CONFIG_DIR=/data
      # - UHC_PORT=8088       # Bridge port (default: 8088)
      # - RUST_LOG=info       # Log level: trace, debug, info, warn, error
    restart: unless-stopped
```

```bash
docker compose up -d
# Access http://localhost:8088
```

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `UHC_PORT` | Bridge HTTP port | `8088` |
| `CONFIG_DIR` | Directory for config/state files | `/data` |
| `RUST_LOG` | Log filter (e.g., `info`, `debug`, `unified_hifi_control=debug`) | `debug` |
| `LMS_HOST` | Auto-configure LMS backend (used by LMS plugin) | — |
| `LMS_PORT` | LMS server port | `9000` |

Legacy aliases: `PORT` (→ `UHC_PORT`), `LOG_LEVEL` (→ `RUST_LOG`)

**Note:** Port 8088 is also HQPlayer's default. If running both on the same host, change one.

## HQPlayer DSP Integration

**For users who already route audio through HQPlayer:** Expose HQPlayer's DSP controls (profile switching, filter selection) directly from your zones, without managing HQPlayer as a separate playback zone.

### Prerequisites

Zone linking assumes you've already configured audio routing through HQPlayer using one of these methods:

- **Roon:** Native HQPlayer integration (Settings → Audio → select HQPlayer as output)
- **LMS/BubbleUPnP:** Route to HQPlayer's UPnP renderer (HQPlayer Embedded exposes this)
- **OpenHome:** Use BubbleUPnP Server to expose HQPlayer's UPnP renderer as OpenHome

This bridge doesn't handle audio routing - it exposes DSP controls for HQPlayer instances you've already integrated.

### Setup

Configure HQPlayer via the web UI at `/hqplayer`:
1. Enter your HQPlayer host IP address
2. Set the native port (default: 4321) and web port (default: 8088)
3. Optionally add web credentials for profile switching (HQPlayer Embedded)
4. Click Save — connection status updates automatically

### Zone Linking

Use web UI at `/hqplayer` to link your zones:
1. Select a zone (e.g., "Living Room" Roon zone that outputs to HQPlayer)
2. Choose which HQPlayer instance handles its DSP
3. Zone's now-playing data now includes HQPlayer pipeline info

### Features

- **Multi-instance support:** Run multiple HQPlayer instances, link different zones to each
- **Zone enrichment:** Primary zones show HQPlayer DSP status in `backend_data.hqp`
- **Profile switching:** Load HQPlayer Embedded profiles via web UI or MCP tools
- **Pipeline control:** Adjust filter, shaper, and dither settings

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                   Unified Hi-Fi Control Bridge                       │
│  ┌────────┐  ┌────────┐  ┌──────────┐  ┌────────┐  ┌──────────┐    │
│  │  Roon  │  │ Lyrion │  │ OpenHome │  │  UPnP  │  │ HQPlayer │    │
│  │        │  │  /LMS  │  │          │  │  /DLNA │  │   DSP    │    │
│  └────────┘  └────────┘  └──────────┘  └────────┘  └──────────┘    │
│                                                                      │
│  HTTP API + SSE + optional MQTT                                      │
└─────────────────────────────────────────────────────────────────────┘
                              │
              ┌───────────────┼───────────────┐
              ▼               ▼               ▼
           ESP32           Web UI        Home Assistant
           Knob                          (via MQTT)
```

## MCP Server (Claude Integration)

The bridge includes an MCP server that lets Claude control your hi-fi system directly.

### Setup

1. Start the bridge: `docker compose up -d` or `npm start`
2. Add to your Claude Code MCP config:

```json
{
  "mcpServers": {
    "hifi": {
      "command": "npx",
      "args": ["unified-hifi-control-mcp"],
      "env": {
        "HIFI_BRIDGE_URL": "http://localhost:8088"
      }
    }
  }
}
```

### Available Tools

| Tool | Description |
|------|-------------|
| `hifi_zones` | List available zones (Roon, Lyrion, OpenHome, UPnP) |
| `hifi_now_playing` | Get current track, artist, album, play state |
| `hifi_control` | Play, pause, next, previous, volume control |
| `hifi_hqplayer_status` | HQPlayer Embedded status and pipeline |
| `hifi_hqplayer_profiles` | List saved HQPlayer profiles |
| `hifi_hqplayer_load_profile` | Switch HQPlayer profile |
| `hifi_hqplayer_set_pipeline` | Change filter, shaper, dither settings |
| `hifi_status` | Overall bridge status |

### Example Usage

Ask Claude: "What's playing right now?" or "Turn the volume down a bit" or "Switch to my DSD profile in HQPlayer"

## Firmware Updates

The bridge automatically polls GitHub for new [roon-knob](https://github.com/muness/roon-knob) firmware releases every 6 hours (default, configurable) and downloads updates when available. Knobs check `/firmware/version` on startup and can OTA update from the bridge.

**Opt-out:** Set `FIRMWARE_AUTO_UPDATE=false` to disable automatic polling and downloading. The bridge will not check GitHub for updates, but the `/firmware/version` and `/firmware/download` endpoints remain available for manual firmware management.

**Configuration:**
- `FIRMWARE_AUTO_UPDATE` - Enable/disable automatic updates (default: `true`)
- `FIRMWARE_POLL_INTERVAL_MINUTES` - Poll interval in minutes when auto-update enabled (default: `360` minutes / 6 hours)
- `FIRMWARE_POLL_INTERVAL_MS` - Legacy: Poll interval in milliseconds (prefer `_MINUTES` above)

If MQTT is enabled, firmware version is published to `unified-hifi/firmware/version` for Home Assistant monitoring.

## Related

- [roon-knob](https://github.com/muness/roon-knob) — ESP32-S3 hardware controller (firmware)

<details>
<summary><strong>Version History (for nerds)</strong></summary>

| Version | Stack | Notes |
|---------|-------|-------|
| **v1** | Node.js | Proof of concept — validated the idea of a unified control surface |
| **v2** | Node.js | Production release — in-memory event bus, multi-backend support |
| **v3** | Rust | Complete rewrite — native packages (Synology, QNAP, LMS plugin), 10x smaller memory footprint, single static binary |

The v3 rewrite was motivated by packaging requests (NAS users wanted native packages, not Docker) and the opportunity to dramatically reduce resource usage. The Rust binary uses ~15MB RAM vs ~150MB for Node.js.

</details>

## Development

### Prerequisites

- Rust 1.84+ with `wasm32-unknown-unknown` target
- [Dioxus CLI](https://dioxuslabs.com/learn/0.6/getting_started)
- [sccache](https://github.com/mozilla/sccache) (shared compilation cache - speeds up rebuilds significantly)
- `curl` (for Tailwind CLI download)

```bash
rustup target add wasm32-unknown-unknown
cargo install dioxus-cli --locked

# Optional: shared compilation cache (speeds up rebuilds significantly)
brew install sccache  # macOS, or: cargo install sccache
echo 'export RUSTC_WRAPPER=sccache' >> ~/.zshrc  # or ~/.bashrc

# Install pre-commit hook (runs fmt + clippy)
cp scripts/pre-commit .git/hooks/
```

### Build

```bash
# Build Tailwind CSS (auto-downloads standalone CLI, no Node.js)
make css

# Full build with web UI (WASM + server) - REQUIRED for web UI
dx build --release --platform web
```

**Note:** `cargo build` only builds the server without the WASM client. The web UI requires `dx build` which produces both the server binary and the WASM bundle needed for hydration (interactive components).

### Run

```bash
# Run from dx output directory (contains required wasm assets)
./target/dx/unified-hifi-control/release/web/unified-hifi-control

# Access at http://127.0.0.1:8088
```

**Important:** The server must be run from the `dx build` output directory where the `public/wasm/` folder exists. Running the binary from elsewhere will cause a panic or non-functional UI.

### CSS Development

```bash
# Watch mode - rebuilds CSS on changes to src/input.css or .rs files
make css-watch

# In another terminal, run dx serve for hot reload
dx serve
```

### Test

```bash
cargo test --workspace
```

### Lint

```bash
cargo fmt --check
cargo clippy -- -D warnings
```

## License

As of v2.5.0, this project is licensed under the [PolyForm Noncommercial 1.0.0](https://polyformproject.org/licenses/noncommercial/1.0.0/) license.

Versions up to and including v2.4.1-prior-license were released under a custom source-available license (see LICENSE-PRIOR).

For commercial licensing inquiries, see [COMMERCIAL-LICENSE.md](COMMERCIAL-LICENSE.md).
