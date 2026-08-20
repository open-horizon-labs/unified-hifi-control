#!/usr/bin/env node
/**
 * MCP Server for Unified Hi-Fi Control by Open Horizon Labs
 *
 * Exposes hi-fi control capabilities to Claude and other MCP clients.
 * Connects to the running unified-hifi-control bridge via HTTP.
 */

const { Server } = require('@modelcontextprotocol/sdk/server/index.js');
const { StdioServerTransport } = require('@modelcontextprotocol/sdk/server/stdio.js');
const {
  ListToolsRequestSchema,
  CallToolRequestSchema,
} = require('@modelcontextprotocol/sdk/types.js');
const { version: VERSION } = require('../package.json');

const BRIDGE_URL = process.env.HIFI_BRIDGE_URL || 'http://localhost:3000';

const SERVER_INSTRUCTIONS = `
Unified Hi-Fi Control by Open Horizon Labs — MCP Server

This server connects you to a hi-fi control bridge that manages Roon music playback
and HQPlayer Embedded audio processing. Use these tools when the user wants to:
- Check what's playing or control playback
- Adjust volume or switch zones
- Configure HQPlayer audio pipeline settings

## Available Tools

### Playback Control (Roon)
- **hifi_zones**: List all available playback zones. Start here to get zone IDs.
- **hifi_now_playing**: Get current track, artist, album, play state, and volume for a zone.
- **hifi_control**: Control playback (play, pause, next, previous) or adjust volume (volume_set, volume_up, volume_down).

### Audio Pipeline (HQPlayer Embedded)
- **hifi_hqplayer_status**: Check if HQPlayer is configured and get current pipeline settings.
- **hifi_hqplayer_profiles**: List saved configuration profiles.
- **hifi_hqplayer_load_profile**: Switch to a different profile (restarts HQPlayer).
- **hifi_hqplayer_set_pipeline**: Change individual settings (filter, shaper, dither, etc).

### System Status
- **hifi_status**: Get overall bridge status (Roon connection state, HQPlayer config).

## Usage Patterns

1. **Starting a session**: Use \`hifi_zones\` to discover available zones, then \`hifi_now_playing\`
   to see what's playing. This gives context for subsequent commands.

2. **Playback control**: Always use the zone_id from \`hifi_zones\`. Actions include:
   play, pause, next, previous.

3. **Volume adjustment**: Use \`hifi_control\` with these actions:
   - \`volume_set\` with value (0-100): Set absolute volume level
   - \`volume_up\` with optional value: Increase volume (default +5)
   - \`volume_down\` with optional value: Decrease volume (default -5)

4. **HQPlayer tweaking**: Check \`hifi_hqplayer_profiles\` for presets, or use
   \`hifi_hqplayer_set_pipeline\` for fine-grained control of filters and shapers.

## Prerequisites

The unified-hifi-control bridge must be running (default: http://localhost:3000).
Set HIFI_BRIDGE_URL environment variable if running elsewhere.
`.trim();

async function apiFetch(path, options = {}) {
  const url = `${BRIDGE_URL}${path}`;
  const res = await fetch(url, {
    ...options,
    headers: {
      'Content-Type': 'application/json',
      ...options.headers,
    },
  });
  if (!res.ok) {
    const text = await res.text();
    throw new Error(`API error ${res.status}: ${text}`);
  }
  return res.json();
}

// Tool definitions
const TOOLS = [
  {
    name: 'hifi_zones',
    description: 'List all available Roon zones for playback control',
    inputSchema: { type: 'object', properties: {}, required: [] },
  },
  {
    name: 'hifi_now_playing',
    description: 'Get current playback state for a zone (track, artist, album, play state, volume)',
    inputSchema: {
      type: 'object',
      properties: {
        zone_id: { type: 'string', description: 'The zone ID to query (get from hifi_zones)' },
      },
      required: ['zone_id'],
    },
  },
  {
    name: 'hifi_control',
    description: 'Control playback: play, pause, next, previous, or adjust volume',
    inputSchema: {
      type: 'object',
      properties: {
        zone_id: { type: 'string', description: 'The zone ID to control' },
        action: {
          type: 'string',
          description: 'Action: play (toggle play/pause), pause (toggle play/pause), next, previous, volume_set (absolute), volume_up (relative increase), volume_down (relative decrease)',
        },
        value: { type: 'number', description: 'For volume actions: the level (0-100 for volume_set) or amount to change (for volume_up/volume_down)' },
      },
      required: ['zone_id', 'action'],
    },
  },
  {
    name: 'hifi_hqplayer_status',
    description: 'Get HQPlayer Embedded status and current pipeline settings',
    inputSchema: { type: 'object', properties: {}, required: [] },
  },
  {
    name: 'hifi_hqplayer_profiles',
    description: 'List available HQPlayer Embedded configurations',
    inputSchema: { type: 'object', properties: {}, required: [] },
  },
  {
    name: 'hifi_hqplayer_load_profile',
    description: 'Load an HQPlayer Embedded configuration (will restart HQPlayer)',
    inputSchema: {
      type: 'object',
      properties: {
        profile: { type: 'string', description: 'Configuration name to load (get from hifi_hqplayer_profiles)' },
      },
      required: ['profile'],
    },
  },
  {
    name: 'hifi_hqplayer_set_pipeline',
    description: 'Change an HQPlayer pipeline setting (filter, shaper, dither, etc)',
    inputSchema: {
      type: 'object',
      properties: {
        setting: { type: 'string', description: 'Setting to change: mode, samplerate, filter1x, filterNx, shaper, dither' },
        value: { type: 'string', description: 'New value for the setting' },
      },
      required: ['setting', 'value'],
    },
  },
  {
    name: 'hifi_status',
    description: 'Get overall bridge status (Roon connection, HQPlayer config)',
    inputSchema: { type: 'object', properties: {}, required: [] },
  },
];

// Tool handlers
async function handleTool(name, args) {
  try {
    switch (name) {
      case 'hifi_zones': {
        const data = await apiFetch('/zones');
        return { content: [{ type: 'text', text: JSON.stringify(data.zones, null, 2) }] };
      }

      case 'hifi_now_playing': {
        const { zone_id } = args;
        const data = await apiFetch(`/now_playing?zone_id=${encodeURIComponent(zone_id)}`);
        return { content: [{ type: 'text', text: JSON.stringify(data, null, 2) }] };
      }

      case 'hifi_control': {
        const { zone_id, action, value } = args;
        // Translate MCP actions to backend actions
        let backendAction = action;
        let backendValue = value;
        switch (action) {
          case 'play':
          case 'pause':
          case 'playpause':
            backendAction = 'play_pause';
            break;
          case 'next':
            backendAction = 'next';
            break;
          case 'previous':
          case 'prev':
            backendAction = 'previous';
            break;
          case 'volume_set':
            backendAction = 'vol_abs';
            backendValue = Number(value);
            break;
          case 'volume_up':
            backendAction = 'vol_rel';
            backendValue = Math.abs(Number(value) || 5); // default +5 if no value
            break;
          case 'volume_down':
            backendAction = 'vol_rel';
            backendValue = -Math.abs(Number(value) || 5); // default -5 if no value
            break;
          default:
            // Pass through for any other actions
            break;
        }
        const body = { zone_id, action: backendAction };
        if (backendValue !== undefined) body.value = backendValue;
        await apiFetch('/control', { method: 'POST', body: JSON.stringify(body) });
        const data = await apiFetch(`/now_playing?zone_id=${encodeURIComponent(zone_id)}`);
        return { content: [{ type: 'text', text: `Action "${action}" executed.\n\nCurrent state:\n${JSON.stringify(data, null, 2)}` }] };
      }

      case 'hifi_hqplayer_status': {
        const [status, pipeline] = await Promise.all([
          apiFetch('/hqp/status'),
          apiFetch('/hqp/pipeline').catch(() => ({ enabled: false })),
        ]);
        return { content: [{ type: 'text', text: JSON.stringify({ status, pipeline }, null, 2) }] };
      }

      case 'hifi_hqplayer_profiles': {
        const data = await apiFetch('/hqp/profiles');
        return { content: [{ type: 'text', text: JSON.stringify(data, null, 2) }] };
      }

      case 'hifi_hqplayer_load_profile': {
        const { profile } = args;
        await apiFetch('/hqp/profiles/load', { method: 'POST', body: JSON.stringify({ profile }) });
        return { content: [{ type: 'text', text: `Configuration "${profile}" loading. HQPlayer will restart.` }] };
      }

      case 'hifi_hqplayer_set_pipeline': {
        const { setting, value } = args;
        await apiFetch('/hqp/pipeline', { method: 'POST', body: JSON.stringify({ setting, value }) });
        const pipeline = await apiFetch('/hqp/pipeline');
        return { content: [{ type: 'text', text: `Setting "${setting}" updated to "${value}".\n\nCurrent pipeline:\n${JSON.stringify(pipeline, null, 2)}` }] };
      }

      case 'hifi_status': {
        const data = await apiFetch('/api/status');
        return { content: [{ type: 'text', text: JSON.stringify(data, null, 2) }] };
      }

      default:
        return { content: [{ type: 'text', text: `Unknown tool: ${name}` }], isError: true };
    }
  } catch (err) {
    return { content: [{ type: 'text', text: `Error: ${err.message}` }], isError: true };
  }
}

async function main() {
  const server = new Server(
    { name: 'unified-hifi-control', version: VERSION },
    { capabilities: { tools: {} }, instructions: SERVER_INSTRUCTIONS }
  );

  // List available tools
  server.setRequestHandler(ListToolsRequestSchema, async () => {
    return { tools: TOOLS };
  });

  // Handle tool calls
  server.setRequestHandler(CallToolRequestSchema, async (request) => {
    const { name, arguments: args } = request.params;
    return handleTool(name, args || {});
  });

  const transport = new StdioServerTransport();
  await server.connect(transport);
}

main().catch(console.error);
