# Unified Hi-Fi Control

[![Build](https://github.com/open-horizon-labs/unified-hifi-control/actions/workflows/build.yml/badge.svg?branch=v3)](https://github.com/open-horizon-labs/unified-hifi-control/actions/workflows/build.yml)
[![GitHub Release](https://img.shields.io/github/v/release/open-horizon-labs/unified-hifi-control)](https://github.com/open-horizon-labs/unified-hifi-control/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/open-horizon-labs/unified-hifi-control/total)](https://github.com/open-horizon-labs/unified-hifi-control/releases)

Control your hi-fi system from anywhere — a hardware knob on your couch, your phone, or just ask Claude.

This bridge connects your music sources (Roon, LMS, UPnP) to any control surface you prefer. No vendor lock-in: mix and match sources, add HQPlayer DSP processing, and control it all from one place.

## Control Surfaces

Once the bridge is running, control your system from:

- **Web UI** — Built-in at `http://your-bridge:8088`
- **[roon-knob](https://github.com/muness/roon-knob)** — ESP32-S3 hardware knob with OLED display
- **iOS & Apple Watch** — In alpha testing. [Get in touch](https://github.com/open-horizon-labs/unified-hifi-control/issues) if you'd like to try it.
- **Claude & AI agents** — Via the built-in MCP server (see [MCP Server](#mcp-server-claude-integration) below)

## Installation

### Docker (Recommended)

```bash
docker pull muness/unified-hifi-control:latest
```

### Synology NAS (DSM 7)

Download the SPK package from [Releases](https://github.com/open-horizon-labs/unified-hifi-control/releases):
- `unified-hifi-control-x86_64-*-dsm7.spk` — Synology x86_64 platform family
- `unified-hifi-control-armv8-*-dsm7.spk` — Synology ARMv8 platform family

In Package Center, choose **Manual Install** and select the SPK matching the NAS's
Package Architecture. DSM 7 warns before installing third-party packages; this is
expected because DSM 7 no longer uses third-party package signing.

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
    image: muness/unified-hifi-control:latest
    # Required for Roon/UPnP discovery. Also publishes the bridge port (8088 by
    # default) on every interface the host has, unauthenticated — see
    # "Security and Network Exposure" below.
    network_mode: host
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

## Security and Network Exposure

**Unified Hi-Fi Control is unauthenticated by design. Treat it as a trusted-LAN service and do not port-forward it.**

The bridge listens on `0.0.0.0` — every interface the host has — and the web UI, the REST API, and the MCP endpoint all accept requests without credentials. That is deliberate: the ESP32 knobs, the iOS companion, browsers, and AI agents all need to reach it, and none of them have a way to hold a secret. It also matches the backends it sits in front of, which are themselves open on the LAN (Lyrion Music Server, for instance, requires no auth by default).

The port is `8088` unless you change it. `UHC_PORT` wins, then legacy `PORT`, then a `port` entry in the config file, then the default — so if you have set any of those, read "the bridge port" below rather than `8088`.

What that means in practice:

- **Anything on your network can control playback.** A guest's phone, an IoT gadget, or a compromised TV can change zones, volume, transport, and HQPlayer DSP settings. The blast radius is your music, not your data — there is no file access, no shell, and no credential store behind these endpoints.
- **Do not expose the bridge port to the internet.** `network_mode: host` in the Docker example publishes it on every interface, so a router port-forward or a permissive cloud firewall is all it takes. If you need remote access, use a VPN or an authenticating reverse proxy rather than forwarding the port.
- **Agents connected over MCP inherit that control.** If the agent you connect also reads untrusted text — web pages, track and playlist names, email — a prompt-injection attack could steer it into these tools. Connect agents you trust, and be aware that "control my stereo" is the ceiling of what they can be talked into here.

If your network is flat and you would rather not have guest devices reach the bridge, put it on a VLAN or restrict the bridge port at the firewall to the devices that need it.

## HQPlayer DSP Integration

If you route audio through HQPlayer for upsampling or filtering, this bridge lets you control HQPlayer's DSP settings (profiles, filters, shapers) alongside your zone controls.

**Note:** You need to set up audio routing to HQPlayer separately (via Roon, LMS/BubbleUPnP, or OpenHome). This bridge exposes the DSP controls, not the audio path.

### Setup

1. Open the web UI at `/hqplayer`
2. Enter your HQPlayer host IP and ports (native: 4321, web: 8088)
3. Link zones to HQPlayer instances — each zone can use a different HQPlayer
4. Zone now-playing info will include HQPlayer pipeline status

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                   Unified Hi-Fi Control Bridge                       │
│  ┌────────┐  ┌────────┐  ┌──────────┐  ┌────────┐  ┌──────────┐    │
│  │  Roon  │  │ Lyrion │  │ OpenHome │  │  UPnP  │  │ HQPlayer │    │
│  │        │  │  /LMS  │  │          │  │  /DLNA │  │   DSP    │    │
│  └────────┘  └────────┘  └──────────┘  └────────┘  └──────────┘    │
│                                                                      │
│  HTTP API + SSE                                                       │
└─────────────────────────────────────────────────────────────────────┘
                              │
              ┌───────────────┴───────────────┐
              ▼                               ▼
           ESP32                           Web UI
           Knob
```

## MCP Server (Claude Integration)

Control your hi-fi with natural language. The bridge includes an MCP server so Claude can search, play, queue music, adjust volume, and switch HQPlayer profiles.

### Setup

1. Start the bridge (`docker compose up -d`)
2. Add to your MCP config (Claude Code, ChatGPT, BoltAI, etc.):

```json
{
  "mcpServers": {
    "unified-hifi-control": {
      "type": "http",
      "url": "http://<your-bridge-host>:8088/mcp"
    }
  }
}
```

### Available Tools

| Tool | Description |
|------|-------------|
| `hifi_zones` | List available zones (Roon, LMS, OpenHome, UPnP) |
| `hifi_now_playing` | Get current track, artist, album, play state |
| `hifi_control` | Play, pause, next, previous, volume control |
| `hifi_search` | Search library, TIDAL, or Qobuz *(Roon, LMS)* |
| `hifi_play` | Search and play/queue in one command *(Roon, LMS)* |
| `hifi_status` | Overall bridge status |
| `hifi_hqplayer_status` | HQPlayer Embedded status and pipeline |
| `hifi_hqplayer_profiles` | List saved HQPlayer profiles |
| `hifi_hqplayer_load_profile` | Switch HQPlayer profile |
| `hifi_hqplayer_set_pipeline` | Change filter, shaper, dither settings |

*Search and play work with Roon and LMS. Transport controls work with all adapters.*

The MCP endpoint is unauthenticated like the rest of the bridge — see [Security and Network Exposure](#security-and-network-exposure).

### Example Usage

Ask Claude: "Play some jazz piano" or "Queue Hotel California" or "What's playing?" or "Turn the volume down"

<details>
<summary><strong>Firmware Updates (roon-knob)</strong></summary>

The bridge automatically downloads new [roon-knob](https://github.com/muness/roon-knob) firmware from GitHub every 6 hours. Knobs check `/firmware/version` on startup and OTA update from the bridge.

| Variable | Description | Default |
|----------|-------------|---------|
| `FIRMWARE_AUTO_UPDATE` | Enable/disable auto-download | `true` |
| `FIRMWARE_POLL_INTERVAL_MINUTES` | Check interval | `360` (6 hours) |

</details>

<details>
<summary><strong>Version History (for nerds)</strong></summary>

| Version | Stack | Notes |
|---------|-------|-------|
| **v1** | Node.js | Proof of concept — validated the idea of a unified control surface |
| **v2** | Node.js | Production release — in-memory event bus, multi-backend support |
| **v3** | Rust | Complete rewrite — native packages (Synology, QNAP, LMS plugin), 10x smaller memory footprint, single static binary |

The v3 rewrite was motivated by packaging requests (NAS users wanted native packages, not Docker) and the opportunity to dramatically reduce resource usage. The Rust binary uses ~15MB RAM vs ~150MB for Node.js.

</details>

<details>
<summary><strong>Development</strong></summary>

### Prerequisites

- Rust 1.84+ with `wasm32-unknown-unknown` target
- [Dioxus CLI](https://dioxuslabs.com/learn/0.7/getting_started)

```bash
rustup target add wasm32-unknown-unknown
cargo install dioxus-cli@0.7.10 --locked
cp scripts/pre-commit .git/hooks/
```

### Build & Run

```bash
make css                                              # Build Tailwind CSS
dx build --release --platform web --features web      # Build server + WASM

cd target/dx/unified-hifi-control/release/web
PORT=8088 ./server                                    # Run at http://localhost:8088
```

For hot reload during development:
```bash
PORT=8088 dx serve --release --platform web --features web --port 8088
```

### Test & Lint

```bash
cargo test --workspace
cargo fmt --check && cargo clippy -- -D warnings
```

**Note:** Use `dx build`, not `cargo build` — the web UI requires the WASM bundle that only `dx` produces.

</details>

## License

As of v2.5.0, this project is licensed under the [PolyForm Noncommercial 1.0.0](https://polyformproject.org/licenses/noncommercial/1.0.0/) license.

Versions up to and including v2.4.1-prior-license were released under a custom source-available license (see LICENSE-PRIOR).

For commercial licensing inquiries, see [COMMERCIAL-LICENSE.md](COMMERCIAL-LICENSE.md).
