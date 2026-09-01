## Installation

### Docker

```yaml
services:
  unified-hifi-control:
    image: muness/unified-hifi-control:{{VERSION}}
    network_mode: host
    volumes:
      - ./data:/data
    environment:
      - CONFIG_DIR=/data
    restart: unless-stopped
```

```bash
docker compose up -d
# Access http://localhost:8088
```

### QNAP NAS

Download the QPKG package from the assets below:
- `unified-hifi-control_*_x86_64.qpkg` — Intel/AMD x86_64
- `unified-hifi-control_*_arm_64.qpkg` — ARM64

### Roon Extension Manager

Search for "Unified Hi-Fi Control" in Roon Extension Manager and install.

### LMS Plugin

Add this repository URL in LMS Settings → Plugins → Additional Repositories:
```
https://raw.githubusercontent.com/open-horizon-labs/unified-hifi-control/v3/lms-plugin/repo.xml
```
Then install "Unified Hi-Fi Control" from the plugin list.

---

## MCP Server (Claude Integration)

The bridge includes a built-in MCP server. Add to your MCP config (Claude Code, Claude Desktop, etc.):

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

Replace `<your-bridge-host>` with your bridge IP or hostname (e.g., `localhost`, `192.168.1.100`, `nas.local`).

---

## Configuration

Configure all backends (Roon, LMS, HQPlayer, UPnP/OpenHome) via the web UI at `http://<your-bridge-host>:8088`.
