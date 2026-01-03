# Bus Architecture Design

**Status:** Design approved, implementation pending
**Last Updated:** 2026-01-02
**Context:** Multi-backend audio control abstraction

---

## Problem Statement

Current architecture tightly couples backends (Roon, HQPlayer) to frontends (ESP32 knob, web UI, MQTT, MCP). Adding new backends (UPnP, LMS) requires changing every frontend. Knob firmware has Roon-specific protocol assumptions.

**Goal:** Decouple backends from frontends via unified bus. Any frontend controls any backend without knowing implementation details.

---

## Architecture: Hybrid Adapter Bus

### Core Components

```
┌─────────────────────────────────────────────────────────┐
│                    Frontends                            │
│  ESP32 Knob | Web UI | Home Assistant | MCP | Future    │
└─────────────────────────────────────────────────────────┘
                         │
                         ↓ (HTTP, MQTT, WebSocket)
┌─────────────────────────────────────────────────────────┐
│                   HTTP API Layer                        │
│  /zones, /now_playing, /control, /now_playing/image     │
└─────────────────────────────────────────────────────────┘
                         │
                         ↓
┌─────────────────────────────────────────────────────────┐
│                      Bus Core                           │
│  Zone Registry | Backend Routing | State Aggregation    │
└─────────────────────────────────────────────────────────┘
                         │
        ┌────────────────┼────────────────┬───────────────┐
        ↓                ↓                ↓               ↓
  ┌──────────┐    ┌──────────┐    ┌──────────┐   ┌──────────┐
  │  Roon    │    │ HQPlayer │    │  UPnP    │   │   LMS    │
  │ Adapter  │    │ Adapter  │    │ Adapter  │   │ Adapter  │
  └──────────┘    └──────────┘    └──────────┘   └──────────┘
        │                │                │               │
        ↓                ↓                ↓               ↓
   Roon API      HQP Native       UPnP SOAP        LMS JSON-RPC
```

### Bus Responsibilities

1. **Zone Registry:** Map zone_id → Zone instance → Backend adapter
2. **Command Routing:** Route control commands to correct backend
3. **State Aggregation:** Collect state from backend subscriptions/broadcasts
4. **Artwork Proxying:** Route image requests to backends, handle format conversion

### Backend Adapter Interface

All adapters implement:
```javascript
interface BackendAdapter {
  // Lifecycle
  connect(): Promise<void>
  disconnect(): Promise<void>
  isConnected(): boolean

  // Discovery
  getZones(): Zone[]

  // Playback
  getNowPlaying(zone_id): Promise<PlaybackState>
  control(zone_id, action, value): Promise<void>

  // Artwork
  getArtwork(zone_id, opts): Promise<Buffer>

  // Events (optional)
  on(event, handler): void
}
```

Backend-specific methods (HQP pipeline, Roon grouping) remain on adapters as extensions.

---

## Key Design Decisions

### 1. Overlapping Zones

Zones are NOT 1:1 with physical outputs. Multiple backends can expose the same physical device:

**Example:** Roon zone outputting to HQPlayer
- `roon:living_room` - Roon zone (playback control, metadata)
- `hqp:main` - HQPlayer zone (DSP control, pipeline state) - can have multiple instances

Both represent the same audio output, different control surfaces.

**Implementation:** Zone registry tracks linkages. UI can show combined view (Roon playback + HQP DSP on same card).

### 2. Backend-Native Event Handling

No forced polling layer. Backends use their native push/broadcast mechanisms:
- **Roon:** Subscription model (already push)
- **UPnP:** GENA event subscriptions
- **LMS:** Player event subscriptions
- **HQPlayer:** No push - poll on-demand

Bus aggregates events but doesn't force a specific pattern.

### 3. Backward Compatibility

**HTTP routes MUST NOT change:**
- `/zones` → list of all zones from all backends
- `/now_playing?zone_id=X` → unified playback state
- `/control` → routes to correct backend
- `/now_playing/image?zone_id=X&format=rgb565` → proxied artwork

Knob firmware unchanged. Web UI sees unified zone list.

### 4. MQTT Optional

Most backends don't need Home Assistant integration. MQTT bridge is separate service that:
- Polls `Bus.getZones()` (all backends)
- Publishes `media_player` discovery configs
- Can be disabled via env var

HQPlayer needed MQTT due to isolated ecosystem. Roon/UPnP/LMS may not.

---

## Implementation Phases

### Phase 1: Bus Foundation
Create minimal viable bus - no behavior changes yet.
- `Bus` class (zone registry, backend routing)
- Adapter interface definition
- `/src/bus/` directory structure

### Phase 2: Port Roon to Bus
Wrap existing `RoonClient` as `RoonAdapter`.
- HTTP routes delegate to bus
- Bus routes to single backend (Roon)
- Test knob backward compatibility

### Phase 3: Port HQPlayer to Bus
Add second backend, implement overlapping zones.
- `HQPAdapter` wraps `HQPClient`
- Detect Roon zone → HQP linkage
- Test web UI shows both backends

### Phase 4: Add UPnP Backend
First new backend proves extensibility.
- Implement `UPnPAdapter` (discovery, control)
- Register with bus
- Test knob controls UPnP renderers

### Phase 5: Add LMS Backend
Second new backend.
- Implement `LMSAdapter` (JSON-RPC)
- Register with bus
- Test playback control

### Phase 6: MQTT Extension (Optional)
Extend MQTT bridge to all backends.
- Iterate `Bus.getZones()`
- Publish discovery for all zones
- Test HA integration

---

## Zone Model

### Unified Zone Structure

```javascript
{
  zone_id: 'roon:living_room',  // Backend-prefixed unique ID
  zone_name: 'Living Room',     // Display name
  source: 'roon',               // Backend identifier
  state: 'playing',             // playing | paused | stopped | idle

  metadata: {
    line1: 'Blue in Green',     // Track title (primary)
    line2: 'Miles Davis',       // Artist (secondary)
    line3: 'Kind of Blue',      // Album (tertiary, optional)
    image_key: 'abc123',        // Backend-specific artwork reference
  },

  volume: {
    current: -20,               // Current level (backend-specific scale)
    min: -80,                   // Minimum value
    max: 0,                     // Maximum value
    type: 'db',                 // 'db' | 'percentage' | 'fixed'
    is_muted: false,
  },

  position: {
    seek_position: 125,         // Current position (seconds, nullable)
    length: 340,                // Track length (seconds, nullable)
  },

  capabilities: {
    has_transport: true,        // Supports play/pause/skip
    has_volume: true,           // Supports volume control
    has_seek: true,             // Supports position seeking
    has_dsp: false,             // Supports DSP settings (HQP only)
    supports_grouping: false,   // Supports zone grouping (Roon only)
  },

  // Backend-specific extensions (optional)
  backend_data: {
    // Roon: output_id, zone grouping info
    // HQPlayer: pipeline settings, profile
    // UPnP: device UUID, capabilities
    // LMS: player MAC, sync group
  }
}
```

### Zone ID Convention

Format: `{backend}:{identifier}`

Examples:
- `roon:1b8e2f4a-3c9d-4e5f-8a7b-6c9d8e7f6a5b` (Roon zone UUID)
- `hqp:main`, `hqp:office` (HQPlayer instances - supports multiple)
- `upnp:uuid:cd48a209-6fed-1034-8a0d-df599bc131a4` (UPnP device UUID)
- `lms:b8:27:eb:12:34:56` (LMS player MAC address)

Backend prefix enables routing without lookup.

---

## Transport Actions

### Universal Actions (All Backends)

```javascript
{
  action: 'play_pause',  // Toggle play/pause
  action: 'play',        // Start playback
  action: 'pause',       // Pause playback
  action: 'stop',        // Stop playback
  action: 'next',        // Skip to next track
  action: 'previous',    // Skip to previous track
  action: 'vol_rel',     // Relative volume change
  value: 5,              // (delta steps, positive or negative)
  action: 'vol_abs',     // Absolute volume set
  value: -25,            // (backend-specific scale)
  action: 'seek',        // Seek to position
  value: 180,            // (seconds)
}
```

### Backend-Specific Actions

Not routed through bus - use backend-specific routes:
- HQPlayer pipeline: `POST /hqp/pipeline` with `{setting, value}`
- Roon grouping: `POST /roon/grouping` (future)
- UPnP browsing: `GET /upnp/browse` (future)
- LMS playlists: `POST /lms/playlist` (future)

---

## Artwork Strategy

### Proxying Through Bus

**Endpoint:** `GET /now_playing/image?zone_id=X&format={rgb565|jpeg|png}`

**Flow:**
1. Frontend requests image for zone
2. Bus routes to correct backend adapter
3. Adapter fetches artwork (via image service, URL fetch, or local file)
4. Bus converts format if needed (JPEG/PNG → RGB565 for knob)
5. Returns image bytes with headers (`X-Image-Width`, `X-Image-Height`, `X-Image-Format`)

**Rationale:** Knob and simple controllers can't fetch arbitrary URLs or convert formats. Bridge abstracts complexity.

**Optimization:** Web UI and HA can fetch URLs directly if performance matters (optional enhancement).

---

## State Management

### No Forced Polling

Backends use native event mechanisms:

**Roon:**
```javascript
transport.subscribe_zones((cmd, data) => {
  if (cmd === 'zones_changed') {
    bus.updateZones(data.zones);
  }
});
```

**UPnP:**
```javascript
renderer.on('status', (status) => {
  bus.updateZone(zone_id, { state: status.state });
});
```

**LMS:**
```javascript
// Subscribe to player events via JSON-RPC
lms.subscribe('player', player_id, (event) => {
  bus.updateZone(zone_id, event);
});
```

**HQPlayer:**
```javascript
// No push events - fetch on-demand
async getNowPlaying(zone_id) {
  const status = await hqp.getStatus();
  return mapToPlaybackState(status);
}
```

Bus aggregates updates but doesn't impose polling layer.

---

## Critical Files

**Bus core (new):**
- `/src/bus/index.js` - Bus class, zone registry
- `/src/bus/adapter.js` - Adapter interface definition
- `/src/bus/zone.js` - Zone class

**Adapters (wrappers):**
- `/src/bus/adapters/roon.js` - Wrap `src/roon/client.js`
- `/src/bus/adapters/hqp.js` - Wrap `src/hqplayer/client.js`
- `/src/bus/adapters/upnp.js` - New UPnP implementation
- `/src/bus/adapters/lms.js` - New LMS implementation

**HTTP routes (refactor):**
- `/src/server/app.js` - Update routes to call `bus.*` methods
- `/src/knobs/routes.js` - Update knob routes to delegate to bus

**Entry point:**
- `/src/index.js` - Instantiate bus, register adapters

---

## Testing Strategy

### Phase 2 (Roon Port)
1. Start bridge with bus enabled
2. Connect knob - verify zones appear
3. Test play/pause/skip/volume - verify commands work
4. Compare knob behavior before/after - should be identical

### Phase 3 (HQPlayer Port)
1. Add HQPlayer adapter to bus
2. Verify web UI shows both Roon zones AND HQPlayer zone
3. Test overlapping zone detection (Roon → HQP linkage)
4. Test DSP controls on HQP-backed Roon zone

### Phase 4 (UPnP)
1. Discover UPnP renderers on network
2. Verify renderers appear in `/zones`
3. Test playback control (play/pause/volume)
4. Test knob controls UPnP device

### Phase 5 (LMS)
1. Connect to LMS instance
2. Verify players appear in `/zones`
3. Test playback control and metadata

---

## Success Criteria

After full implementation:

1. ✅ Knob firmware unchanged, controls all backends (Roon, HQP, UPnP, LMS)
2. ✅ Web UI unified zone list, controls work across backends
3. ✅ Backend-specific controls (HQP DSP) shown conditionally
4. ✅ Adding new backend = implement adapter + register with bus (no frontend changes)
5. ✅ MQTT optional (can be disabled, doesn't affect core functionality)
6. ✅ Overlapping zones work (Roon zone + HQP pipeline both visible)

---

## Future Enhancements

**WebSocket for Web UI:**
- Replace polling with real-time push
- Bus emits events, WebSocket service broadcasts
- Requires web UI refactor (separate effort)

**Zone Linking UI:**
- Show relationships between overlapping zones
- Combined cards (Roon playback + HQP DSP)
- Requires UI design work

**Additional Backends:**
- Tidal Connect (if API available)
- Qobuz Connect (if API available)
- Music Assistant (HA-native aggregator)
- Volumio (MPD-based)

---

## References

**Protocol Research:**
- UPnP MediaRenderer: https://www.npmjs.com/package/upnp-mediarenderer-client
- LMS JSON-RPC: https://gist.github.com/samtherussell/335bf9ba75363bd167d2470b8689d9f2
- HA Media Player: https://developers.home-assistant.io/docs/core/entity/media-player/

**Related Documentation:**
- `/docs/protocols/roon-api.md` (future)
- `/docs/protocols/hqplayer-native.md` (future)
- `/docs/protocols/upnp-control.md` (future)
- `/docs/protocols/lms-jsonrpc.md` (future)
