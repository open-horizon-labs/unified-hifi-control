# Setup Hi-Fi Control MCP

Set up the Unified Hi-Fi Control MCP server to control your music system from Claude.

**Prerequisites:** You need the unified-hifi-control bridge running somewhere on your network. This MCP server connects to the bridge via HTTP.

**Execute these steps in order:**

## Step 1: Determine bridge URL

Ask the user where their unified-hifi-control bridge is running:
- **Local (default):** `http://localhost:3000`
- **Docker:** Check their Docker host IP
- **Remote:** Ask for the URL

Store this as `HIFI_BRIDGE_URL`.

## Step 2: Test the bridge connection

Verify the bridge is reachable:
```bash
curl -s ${HIFI_BRIDGE_URL}/status
```

If this fails, help the user:
- Start the bridge: `cd /path/to/unified-hifi-control && npm start`
- Or via Docker: `docker run -p 3000:3000 cloud-atlas-ai/unified-hifi-control`

## Step 3: Add MCP server to Claude Code

Use the `claude mcp add` command:
```bash
claude mcp add hifi-control --scope user -e HIFI_BRIDGE_URL=${HIFI_BRIDGE_URL} -- npx -y @cloud-atlas-ai/unified-hifi-control-mcp
```

Or for a local install:
```bash
claude mcp add hifi-control --scope user -e HIFI_BRIDGE_URL=${HIFI_BRIDGE_URL} -- node /path/to/unified-hifi-control/src/mcp/index.js
```

## Step 4: Inform the user

Tell the user:
1. Setup is complete
2. They need to **restart Claude Code** for the MCP to load
3. After restart, hi-fi control tools will be available:
   - `hifi_zones` - List Roon zones
   - `hifi_now_playing` - What's playing
   - `hifi_control` - Play, pause, volume
   - `hifi_hqplayer_*` - HQPlayer control
4. Try asking: "What's playing right now?" or "Turn it up a bit"
