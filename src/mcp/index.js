#!/usr/bin/env node
/**
 * MCP Server for Unified Hi-Fi Control
 *
 * Exposes hi-fi control capabilities to Claude and other MCP clients.
 * Connects to the running unified-hifi-control bridge via HTTP.
 */

const { McpServer } = require('@modelcontextprotocol/sdk/server/mcp.js');
const { StdioServerTransport } = require('@modelcontextprotocol/sdk/server/stdio.js');

const BRIDGE_URL = process.env.HIFI_BRIDGE_URL || 'http://localhost:3000';

const SERVER_INSTRUCTIONS = `
Unified Hi-Fi Control MCP Server - Control Your Music System

This server connects you to a hi-fi control bridge that manages Roon music playback
and HQPlayer Embedded audio processing. Use these tools when the user wants to:
- Check what's playing or control playback
- Adjust volume or switch zones
- Configure HQPlayer audio pipeline settings

## Available Tools

### Playback Control (Roon)
- **hifi_zones**: List all available playback zones. Start here to get zone IDs.
- **hifi_now_playing**: Get current track, artist, album, play state, and volume for a zone.
- **hifi_control**: Control playback (play, pause, next, previous, stop) or adjust volume.

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
   play, pause, playpause, next, previous, stop, volume.

3. **Volume adjustment**: Use \`hifi_control\` with action="volume". The value can be:
   - Absolute (0-100): "Set volume to 50"
   - Relative: "Turn it up" (+5), "Turn it down" (-5)

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

async function main() {
  const server = new McpServer({
    name: 'unified-hifi-control',
    version: '0.1.0',
    instructions: SERVER_INSTRUCTIONS,
  });

  // Tool: Get available zones
  server.tool(
    'hifi_zones',
    'List all available Roon zones for playback control',
    {},
    async () => {
      try {
        const data = await apiFetch('/zones');
        return {
          content: [{
            type: 'text',
            text: JSON.stringify(data.zones, null, 2),
          }],
        };
      } catch (err) {
        return {
          content: [{ type: 'text', text: `Error: ${err.message}` }],
          isError: true,
        };
      }
    }
  );

  // Tool: Get now playing info
  server.tool(
    'hifi_now_playing',
    'Get current playback state for a zone (track, artist, album, play state, volume)',
    {
      zone_id: {
        type: 'string',
        description: 'The zone ID to query (get from hifi_zones)',
        required: true,
      },
    },
    async ({ zone_id }) => {
      try {
        const data = await apiFetch(`/now_playing?zone_id=${encodeURIComponent(zone_id)}`);
        return {
          content: [{
            type: 'text',
            text: JSON.stringify(data, null, 2),
          }],
        };
      } catch (err) {
        return {
          content: [{ type: 'text', text: `Error: ${err.message}` }],
          isError: true,
        };
      }
    }
  );

  // Tool: Control playback
  server.tool(
    'hifi_control',
    'Control playback: play, pause, playpause, next, previous, stop, or adjust volume',
    {
      zone_id: {
        type: 'string',
        description: 'The zone ID to control',
        required: true,
      },
      action: {
        type: 'string',
        description: 'Action: play, pause, playpause, next, previous, stop, volume',
        required: true,
      },
      value: {
        type: 'number',
        description: 'For volume action: absolute level (0-100) or relative change (-10, +5, etc)',
        required: false,
      },
    },
    async ({ zone_id, action, value }) => {
      try {
        const body = { zone_id, action };
        if (value !== undefined) body.value = value;

        await apiFetch('/control', {
          method: 'POST',
          body: JSON.stringify(body),
        });

        // Return updated state
        const data = await apiFetch(`/now_playing?zone_id=${encodeURIComponent(zone_id)}`);
        return {
          content: [{
            type: 'text',
            text: `Action "${action}" executed.\n\nCurrent state:\n${JSON.stringify(data, null, 2)}`,
          }],
        };
      } catch (err) {
        return {
          content: [{ type: 'text', text: `Error: ${err.message}` }],
          isError: true,
        };
      }
    }
  );

  // Tool: Get HQPlayer status
  server.tool(
    'hifi_hqplayer_status',
    'Get HQPlayer Embedded status and current pipeline settings',
    {},
    async () => {
      try {
        const [status, pipeline] = await Promise.all([
          apiFetch('/hqp/status'),
          apiFetch('/hqp/pipeline').catch(() => ({ enabled: false })),
        ]);
        return {
          content: [{
            type: 'text',
            text: JSON.stringify({ status, pipeline }, null, 2),
          }],
        };
      } catch (err) {
        return {
          content: [{ type: 'text', text: `Error: ${err.message}` }],
          isError: true,
        };
      }
    }
  );

  // Tool: List HQPlayer profiles
  server.tool(
    'hifi_hqplayer_profiles',
    'List available HQPlayer Embedded configuration profiles',
    {},
    async () => {
      try {
        const data = await apiFetch('/hqp/profiles');
        return {
          content: [{
            type: 'text',
            text: JSON.stringify(data, null, 2),
          }],
        };
      } catch (err) {
        return {
          content: [{ type: 'text', text: `Error: ${err.message}` }],
          isError: true,
        };
      }
    }
  );

  // Tool: Load HQPlayer profile
  server.tool(
    'hifi_hqplayer_load_profile',
    'Load an HQPlayer Embedded configuration profile (will restart HQPlayer)',
    {
      profile: {
        type: 'string',
        description: 'Profile name to load (get from hifi_hqplayer_profiles)',
        required: true,
      },
    },
    async ({ profile }) => {
      try {
        await apiFetch('/hqp/profiles/load', {
          method: 'POST',
          body: JSON.stringify({ profile }),
        });
        return {
          content: [{
            type: 'text',
            text: `Profile "${profile}" loading. HQPlayer will restart.`,
          }],
        };
      } catch (err) {
        return {
          content: [{ type: 'text', text: `Error: ${err.message}` }],
          isError: true,
        };
      }
    }
  );

  // Tool: Set HQPlayer pipeline setting
  server.tool(
    'hifi_hqplayer_set_pipeline',
    'Change an HQPlayer pipeline setting (filter, shaper, dither, etc)',
    {
      setting: {
        type: 'string',
        description: 'Setting to change: mode, samplerate, filter1x, filterNx, shaper, dither',
        required: true,
      },
      value: {
        type: 'string',
        description: 'New value for the setting',
        required: true,
      },
    },
    async ({ setting, value }) => {
      try {
        await apiFetch('/hqp/pipeline', {
          method: 'POST',
          body: JSON.stringify({ setting, value }),
        });

        // Return updated pipeline
        const pipeline = await apiFetch('/hqp/pipeline');
        return {
          content: [{
            type: 'text',
            text: `Setting "${setting}" updated to "${value}".\n\nCurrent pipeline:\n${JSON.stringify(pipeline, null, 2)}`,
          }],
        };
      } catch (err) {
        return {
          content: [{ type: 'text', text: `Error: ${err.message}` }],
          isError: true,
        };
      }
    }
  );

  // Tool: Get bridge status
  server.tool(
    'hifi_status',
    'Get overall bridge status (Roon connection, HQPlayer config)',
    {},
    async () => {
      try {
        const data = await apiFetch('/api/status');
        return {
          content: [{
            type: 'text',
            text: JSON.stringify(data, null, 2),
          }],
        };
      } catch (err) {
        return {
          content: [{ type: 'text', text: `Error: ${err.message}` }],
          isError: true,
        };
      }
    }
  );

  const transport = new StdioServerTransport();
  await server.connect(transport);
}

main().catch(console.error);
