
---

## Docker Compose

```yaml
services:
  unified-hifi-control:
    image: muness/unified-hifi-control:{{VERSION}}
    network_mode: host  # Required for Roon mDNS discovery
    volumes:
      - ./data:/data  # Config + firmware stored here
    environment:
      - PORT=8088
      - CONFIG_DIR=/data
    restart: unless-stopped
```

**Update:** `docker compose pull && docker compose up -d`

**First time:** `docker compose up -d`

Then access http://localhost:8088/admin

**Note:** Port 8088 is also HQPlayer's default. If running both on the same host, change one.

---

## MCP Server (Claude Integration)

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

---

## Configuration

**HQPlayer:** Configure via `/admin` UI or environment variables:
- `HQP_HOST` - HQPlayer Embedded IP
- `HQP_PORT` - Web UI port (default: 8088)
- `HQP_USER` - Username (required for profile changes)
- `HQP_PASS` - Password

**MQTT (Home Assistant):**
- `MQTT_BROKER` - e.g., `mqtt://192.168.1.x:1883`
