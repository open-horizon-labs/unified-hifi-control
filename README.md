# Unified Hi-Fi Control

A source-agnostic hi-fi control bridge that connects music sources and audio pipeline control to any surface — hardware knobs, web UIs, or Home Assistant.

## Vision

Hi-fi software assumes you're at a computer or using vendor-specific apps. This bridge fills the gap:

- **Music Sources:** Roon, Lyrion (formerly LMS/Squeezebox); OpenHome and UPnP/DLNA planned
- **Audio Pipeline:** HQPlayer, receiver control (future)
- **Surfaces:** Anything that speaks HTTP or MQTT — ESP32 hardware, web UIs, Home Assistant, Claude (via MCP), etc.

## Status

🚧 **In Development** — Consolidating [roon-knob](https://github.com/muness/roon-knob) bridge and [hqp-profile-switcher](https://github.com/muness/roon-extension-hqp-profile-switcher) into a unified platform.

## Quick Start (Docker)

```yaml
# docker-compose.yml
services:
  unified-hifi-control:
    image: muness/unified-hifi-control:latest
    network_mode: host  # Required for Roon mDNS discovery
    volumes:
      - ./data:/data
      - ./firmware:/app/firmware
    environment:
      - PORT=8088
      - CONFIG_DIR=/data
      # Optional: Lyrion configuration (or configure via web UI at /settings)
      # - LMS_HOST=192.168.1.x
      # - LMS_PORT=9000
      # - LMS_USERNAME=admin
      # - LMS_PASSWORD=secret
    restart: unless-stopped
```

```bash
docker compose up -d
# Access http://localhost:8088/admin
```

**Note:** Port 8088 is also HQPlayer's default. If running both on the same host, change one.

## Architecture

```
┌───────────────────────────────────────────────────────────┐
│              Unified Hi-Fi Control Bridge                  │
│  ┌──────────┐  ┌──────────┐  ┌──────────────┐             │
│  │   Roon   │  │  Lyrion  │  │  HQPlayer    │             │
│  │          │  │          │  │              │             │
│  └──────────┘  └──────────┘  └──────────────┘             │
│                                                            │
│  HTTP API + optional MQTT                                  │
└───────────────────────────────────────────────────────────┘
              │
    ┌─────────┼─────────┐
    ▼         ▼         ▼
  ESP32     Web UI    Home Assistant
  Knob
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
| `hifi_zones` | List available zones (Roon, Lyrion) |
| `hifi_now_playing` | Get current track, artist, album, play state |
| `hifi_control` | Play, pause, next, previous, volume control |
| `hifi_hqplayer_status` | HQPlayer Embedded status and pipeline |
| `hifi_hqplayer_profiles` | List saved HQPlayer profiles |
| `hifi_hqplayer_load_profile` | Switch HQPlayer profile |
| `hifi_hqplayer_set_pipeline` | Change filter, shaper, dither settings |
| `hifi_status` | Overall bridge status |

### Example Usage

Ask Claude: "What's playing right now?" or "Turn the volume down a bit" or "Switch to my DSD profile in HQPlayer"

## Related

- [roon-knob](https://github.com/muness/roon-knob) — ESP32-S3 hardware controller (firmware)

## License

ISC
