# HQPlayer Multi-Instance Support

## Overview

unified-hifi-control now supports running multiple HQPlayer instances simultaneously (e.g., HQPlayer Embedded + HQPlayer Desktop). Each instance appears as a separate controllable zone in the bus.

## Configuration

### Config File Format

Edit `data/hqp-config.json` to define multiple instances:

```json
[
  {
    "name": "embedded",
    "host": "192.168.1.61",
    "port": 8088,
    "username": "audiolinux",
    "password": "audiolinux"
  },
  {
    "name": "desktop",
    "host": "192.168.1.62",
    "port": 8088,
    "username": "",
    "password": ""
  }
]
```

**Fields:**
- `name`: Unique identifier for this instance (used in zone IDs)
- `host`: IP address or hostname of HQPlayer
- `port`: Web UI port (default: 8088)
- `username`: Web UI username (optional, required for profile loading on Embedded)
- `password`: Web UI password (optional, required for profile loading on Embedded)

### Backward Compatibility

The old single-instance format is still supported:

```json
{
  "host": "192.168.1.61",
  "port": 8088,
  "username": "audiolinux",
  "password": "audiolinux"
}
```

This will create a single instance with name "default".

### Environment Variables

You can also configure a single instance via environment variables:

```bash
HQP_NAME=embedded
HQP_HOST=192.168.1.61
HQP_PORT=8088
HQP_USER=audiolinux
HQP_PASS=audiolinux
```

## Zone IDs

Each HQPlayer instance creates a zone with ID pattern: `hqp:{instance_name}`

Examples:
- `hqp:embedded` - HQPlayer Embedded instance
- `hqp:desktop` - HQPlayer Desktop instance
- `hqp:default` - Default instance (single-instance or legacy config)

## API Endpoints

### List Instances

```http
GET /hqp/instances
```

Returns:
```json
{
  "instances": [
    {
      "name": "embedded",
      "host": "192.168.1.61",
      "port": 8088,
      "configured": true
    },
    {
      "name": "desktop",
      "host": "192.168.1.62",
      "port": 8088,
      "configured": true
    }
  ]
}
```

### Instance-Specific Operations

All HQP endpoints now support an optional `instance` parameter:

```http
GET /hqp/status?instance=embedded
GET /hqp/profiles?instance=desktop
GET /hqp/pipeline?instance=embedded
POST /hqp/pipeline
  { "instance": "embedded", "setting": "mode", "value": "0" }
POST /hqp/profiles/load
  { "instance": "embedded", "profile": "DSD512" }
POST /hqp/configure
  { "instance": "desktop", "host": "192.168.1.62", "port": 8088 }
```

If `instance` is not specified, the first configured instance is used (backward compatibility).

## Bus Integration

Each instance registers with the bus as a separate backend:

```javascript
bus.registerBackend('hqp:embedded', embeddedAdapter);
bus.registerBackend('hqp:desktop', desktopAdapter);
```

Control via bus:

```javascript
// Get all zones (includes both HQP instances)
const zones = bus.getZones();
// Returns: [
//   { zone_id: 'roon:...', ... },
//   { zone_id: 'hqp:embedded', display_name: 'HQPlayer embedded', ... },
//   { zone_id: 'hqp:desktop', display_name: 'HQPlayer desktop', ... }
// ]

// Control specific instance
await bus.control('hqp:embedded', 'play');
await bus.control('hqp:desktop', 'pause');

// Get now playing
const np = bus.getNowPlaying('hqp:embedded');
```

## Architecture

### Components

1. **HQPClient** (`src/hqplayer/client.js`)
   - Wraps single HQPlayer instance
   - Handles web UI and native protocol communication
   - Unchanged from single-instance design

2. **HQPAdapter** (`src/bus/adapters/hqp.js`)
   - Implements bus adapter interface
   - Wraps one HQPClient with an instance name
   - Returns zone with ID `hqp:{instanceName}`

3. **Instance Loader** (`src/index.js`)
   - Reads config file (supports both old and new format)
   - Creates HQPClient + HQPAdapter pair for each instance
   - Registers each adapter with bus

4. **Server Routes** (`src/server/app.js`)
   - Updated to support optional `instance` parameter
   - Routes requests to specific instance by name
   - Maintains backward compatibility (uses first instance if not specified)

### Data Flow

```text
Config File (hqp-config.json)
  ↓
loadHQPInstances() reads config
  ↓
For each instance:
  Create HQPClient(host, port, ...)
  Create HQPAdapter(client, { instanceName })
  bus.registerBackend(`hqp:${instanceName}`, adapter)
  ↓
Bus routes commands by zone_id prefix
  zone_id='hqp:embedded' → routes to hqp:embedded backend
  zone_id='hqp:desktop' → routes to hqp:desktop backend
```

## Testing

1. Create multi-instance config:
   ```bash
   cp data/hqp-config.example-multi.json data/hqp-config.json
   # Edit with your actual hosts
   ```

2. Start server:
   ```bash
   npm start
   ```

3. Check instances:
   ```bash
   curl http://localhost:8088/hqp/instances
   ```

4. Check bus zones:
   ```bash
   # (Requires bus debug endpoint or UI)
   ```

5. Control instances:
   ```bash
   # Via bus (if bus HTTP endpoints exist)
   curl -X POST http://localhost:8088/api/control \
     -H 'Content-Type: application/json' \
     -d '{"zone_id":"hqp:embedded","action":"play"}'
   ```

## Migration Guide

### From Single to Multi-Instance

**Before (single instance):**
```json
{
  "host": "192.168.1.61",
  "port": 8088,
  "username": "audiolinux",
  "password": "audiolinux"
}
```

**After (multi-instance):**
```json
[
  {
    "name": "embedded",
    "host": "192.168.1.61",
    "port": 8088,
    "username": "audiolinux",
    "password": "audiolinux"
  }
]
```

**Or keep old format** - it still works! The system will automatically convert it to:
- Instance name: "default"
- Zone ID: `hqp:default`

### API Calls

**Before:**
```javascript
GET /hqp/status
GET /hqp/pipeline
POST /hqp/pipeline { "setting": "mode", "value": "0" }
```

**After (same calls work, routes to first instance):**
```javascript
GET /hqp/status
GET /hqp/pipeline
POST /hqp/pipeline { "setting": "mode", "value": "0" }
```

**After (specify instance):**
```javascript
GET /hqp/status?instance=embedded
GET /hqp/pipeline?instance=desktop
POST /hqp/pipeline { "instance": "embedded", "setting": "mode", "value": "0" }
```

## Limitations

- Instance names must be unique
- Zone IDs cannot be changed at runtime (require server restart)
- Each instance requires separate network connectivity
- Profile loading only works on HQPlayer Embedded with web credentials

## Future Enhancements

- Dynamic instance add/remove via API (no restart required)
- Per-instance status monitoring and reconnect logic
- Aggregate view of all instances in UI
- Instance health checks and automatic failover
