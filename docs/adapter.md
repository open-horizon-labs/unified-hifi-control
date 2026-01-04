# Bus Adapter Development Guide

Reference pattern for implementing backend adapters. Based on working OpenHome, Roon, and UPnP implementations.

## Core Principle

**One backend = One protocol = One prefix**

Each adapter implements a single protocol with a unique zone_id prefix:
- `roon:` → Roon API
- `openhome:` → OpenHome protocol
- `upnp:` → Basic UPnP/DLNA

The prefix is the ONLY identifier needed - no separate `source` or `protocol` fields.

## Zone Schema

```javascript
{
  zone_id: "prefix:unique-id",       // Backend prefix (roon:, openhome:, upnp:)
  zone_name: "Device Name",          // Human-readable name
  state: "playing",                  // playing | paused | stopped | buffering
  output_count: 1,
  output_name: "Output Device",
  device_name: "Manufacturer Model", // Optional
  volume_control: {                  // null if not supported
    type: "number",                  // number | db | incremental
    min: 0,
    max: 100,
    is_muted: false
  },

  // ONLY include if features are missing:
  unsupported: ["next", "previous", "track_metadata", "album_art"]
}
```

**Omit `unsupported` field entirely for full-featured backends (Roon, OpenHome).**

## Client Implementation Pattern

### Factory Function
```javascript
function create[Backend]Client(opts = {}) {
  const log = opts.logger || console;
  const onZonesChanged = opts.onZonesChanged || (() => {});

  // State management
  const state = { devices: new Map() };

  // Implementation...

  return {
    start,
    stop,
    getZones,
    getNowPlaying,
    control,
    getStatus,
    getImage,
  };
}
```

### Required Methods

**start()** - Initialize, return immediately (async discovery OK)
**stop()** - Clean up resources
**getZones()** - Return cached zones (sync)
**getNowPlaying(zone_id)** - Return cached metadata (sync)
**control(zone_id, action, value)** - Execute command (async)
**getStatus()** - Return diagnostic info
**getImage(image_key, opts)** - Fetch album art (async, can throw if unsupported)

## Adapter Wrapper

Thin wrapper adds prefix:

```javascript
class [Backend]Adapter {
  constructor(client) {
    this.client = client;
  }

  getZones() {
    return this.client.getZones().map(z => ({
      ...z,
      zone_id: `[prefix]:${z.zone_id}`,
    }));
  }

  getNowPlaying(zone_id) {
    const id = zone_id.replace(/^[prefix]:/, '');
    const np = this.client.getNowPlaying(id);
    return np ? { ...np, zone_id: `[prefix]:${np.zone_id}` } : null;
  }

  async control(zone_id, action, value) {
    const id = zone_id.replace(/^[prefix]:/, '');
    return this.client.control(id, action, value);
  }
}
```

## Order of Operations

### 1. Bus Creation
```javascript
const bus = createBus({ logger });
```

### 2. Client Creation with Callback
```javascript
const client = create[Backend]Client({
  logger,
  onZonesChanged: () => bus.refreshZones('[backend]'),
});
```

**Critical:** Pass bus.refreshZones callback so client can notify when zones change.

### 3. Adapter Registration
```javascript
const adapter = new [Backend]Adapter(client);
bus.registerBackend('[backend]', adapter);
```

### 4. Bus Start
```javascript
await bus.start();
// Calls adapter.start() for each backend
// Then calls refreshZones() once to populate initial cache
```

## Zone Change Notifications

Call `onZonesChanged()` when:
- ✅ Devices discovered
- ✅ Devices removed
- ✅ Device name/capabilities change
- ❌ Track changes (don't spam bus refresh)

```javascript
// On discovery
devices.set(id, device);
onZonesChanged();

// On device removed
devices.delete(id);
onZonesChanged();
```

## Unsupported Features

Only include `unsupported` array for limited backends:

```javascript
// Basic UPnP: Limited features
{
  zone_id: "upnp:abc123",
  unsupported: ["next", "previous", "track_metadata", "album_art"]
}

// OpenHome/Roon: Full features
{
  zone_id: "openhome:xyz789"
  // No unsupported field - omitted
}
```

UI checks: `zone.unsupported?.includes('next')` to hide buttons.

## Control Actions

Standard actions all adapters should handle:

**Transport:**
- `play` - Start playback
- `pause` - Pause playback
- `play_pause` - Toggle play/pause
- `stop` - Stop playback
- `next` - Skip to next track (throw Error if unsupported)
- `previous` / `prev` - Skip to previous track (throw Error if unsupported)

**Volume:**
- `vol_abs` - Set absolute volume (value = 0-100)
- `vol_rel` - Adjust volume relatively (value = delta)

## Testing Before Commit

1. **Start server with changes**
2. **Verify zones appear:** `curl .../status.json | jq '.zones'`
3. **Test controls:** Send control commands, verify they work
4. **Check UI:** Refresh browser, verify zones display correctly
5. **ONLY THEN** commit and push

## Reference Implementations

**OpenHome** (docs/reference-openhome.md):
- Full metadata via Info:Track polling
- All transport controls via Transport service
- Album art HTTP proxy
- No unsupported field

**Roon** (docs/reference-roon.md):
- WebSocket-based real-time updates
- Native image service
- Complex volume rate limiting
- No unsupported field

**Basic UPnP** (docs/reference-upnp.md):
- Minimal transport controls only
- Lazy client creation
- unsupported: ["next", "previous", "track_metadata", "album_art"]

## UPnP and OpenHome Architecture

**Separate clients for separate protocols** - each has its own discovery and control implementation.

**OpenHome Client** (`src/openhome/client.js`):
- SSDP discovery for OpenHome devices (`av-openhome-org` services)
- Full metadata via Info:Track polling
- All transport controls via Transport service
- No unsupported field (full features)

**UPnP Client** (`src/upnp/client.js`):
- SSDP discovery for MediaRenderer devices
- Basic transport via AVTransport/RenderingControl
- Limited features: unsupported: ['next', 'previous', 'track_metadata', 'album_art']

**OpenHome Adapter** (`src/bus/adapters/openhome.js`):
- Thin wrapper adding `openhome:` prefix

**UPnP Adapter** (`src/bus/adapters/upnp.js`):
- Thin wrapper adding `upnp:` prefix

**Registration** (from `src/index.js`):
```javascript
// Each protocol has its own client
const openhome = createOpenHomeClient({
  logger,
  onZonesChanged: () => bus.refreshZones('openhome'),
});
const openhomeAdapter = new OpenHomeAdapter(openhome, {
  onZonesChanged: () => bus.refreshZones('openhome'),
});
bus.registerBackend('openhome', openhomeAdapter);

const upnp = createUPnPClient({ logger });
const upnpAdapter = new UPnPAdapter(upnp, {
  onZonesChanged: () => bus.refreshZones('upnp'),
});
bus.registerBackend('upnp', upnpAdapter);
```

**Result**: 3 backends (roon, openhome, upnp), each with dedicated client and adapter.
