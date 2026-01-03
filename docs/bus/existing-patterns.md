# Existing Interface Patterns

**Purpose:** Document current backend interfaces to inform bus adapter design.

## Current Architecture (index.js)

```javascript
const roon = createRoonClient({ logger, base_url });
const hqp = new HQPClient({ logger });
const knobs = createKnobsStore({ logger });
const mqttService = createMqttService({ hqp, logger });  // HQP only!
const app = createApp({ roon, hqp, knobs, logger });

roon.start();
mqttService.connect();
```

**Pattern:** Factory functions + dependency injection

---

## RoonClient Interface (src/roon/client.js)

```javascript
{
  // Lifecycle
  start(): void

  // Zone Discovery
  getZones(opts = {}): Array<Zone>
  // Returns: [{ zone_id, zone_name, source: 'roon', state, output_name, ... }]

  // Playback State
  getNowPlaying(zone_id): PlaybackState | null
  // Returns: { line1, line2, line3, is_playing, volume, volume_min, volume_max,
  //           seek_position, length, zone_id, image_key }

  // Artwork
  getImage(image_key, opts): Promise<{ contentType, body }>
  // opts: { width, height, format }

  // Control
  control(zone_id, action, value): Promise<void>
  // Actions: 'play_pause', 'next', 'previous', 'vol_rel', 'vol_abs'

  // Status
  getStatus(): Object
  // Returns: { connected, core, zone_count, zones, now_playing }
}
```

### Key Features

**Event-Driven:**
- Uses Roon API subscriptions: `transport.subscribe_zones(callback)`
- Callbacks update internal state (`state.zones`, `state.nowPlayingByZone`)
- HTTP routes serve from cached state (no blocking calls to Roon Core)

**State Management:**
- `state.zones` - array of full zone objects
- `state.nowPlayingByZone` - Map of zone_id → playback summary
- Grace period for disconnects (serves stale data for 5s after transport loss)

**Volume Control:**
- Rate-limited (100ms intervals)
- Queued relative changes (max 25 steps per call)

---

## HQPClient Interface (src/hqplayer/client.js)

```javascript
{
  // Lifecycle
  constructor({ host, port, username, password, logger })

  // Configuration
  isConfigured(): boolean
  hasWebCredentials(): boolean
  configure({ host, port, username, password }): void

  // Profiles (Embedded only, requires web creds)
  fetchProfiles(): Promise<Array<{ value, title }>>
  loadProfile(profileValue): Promise<boolean>

  // Pipeline Control (via native protocol, port 4321)
  fetchPipeline(): Promise<PipelineState>
  setPipelineSetting(name, value): Promise<void>
  // Settings: 'mode', 'samplerate', 'filter1x', 'filterNx', 'shaper'
  setVolume(value): Promise<number>

  // Status
  getStatus(): Promise<Object>
  // Returns: { enabled, connected, host, port, product, version, isEmbedded,
  //           supportsProfiles, configName, profiles, pipeline }

  // Discovery (static)
  static discover(timeout): Promise<Array>
}
```

### Key Features

**Pull-Based:**
- No subscriptions/events
- Each call fetches fresh data from HQPlayer
- HTTP scraping (port 8088) + Native TCP protocol (port 4321)

**Single Zone:**
- HQPlayer is a pipeline, not a multi-zone player
- Would map to single zone: `hqp:pipeline`

**DSP Control:**
- Backend-specific: pipeline settings, profiles
- Not part of universal abstraction

**NO `getZones()` or `getNowPlaying()`:**
- Would need to be synthesized by adapter
- `getZones()` → return single `hqp:pipeline` zone
- `getNowPlaying()` → synthesize from `fetchPipeline()` state

---

## HTTP Routes Pattern (src/server/app.js)

### Roon Routes (Backend-Specific)

```javascript
app.get('/roon/status', (req, res) => res.json(roon.getStatus()));
app.get('/roon/zones', (req, res) => res.json(roon.getZones()));
app.get('/roon/now_playing', (req, res) => {
  const np = roon.getNowPlaying(req.query.zone_id);
  res.json({ ...np, zones: roon.getZones() });
});
app.get('/roon/image', async (req, res) => {
  const { contentType, body } = await roon.getImage(req.query.image_key, opts);
  res.set('Content-Type', contentType).send(body);
});
app.post('/roon/control', async (req, res) => {
  await roon.control(req.body.zone_id, req.body.action, req.body.value);
  res.json({ ok: true });
});
```

### HQPlayer Routes (Backend-Specific)

```javascript
app.get('/hqp/status', async (req, res) => res.json(await hqp.getStatus()));
app.get('/hqp/profiles', async (req, res) => res.json(await hqp.fetchProfiles()));
app.post('/hqp/profiles/load', async (req, res) => {
  await hqp.loadProfile(req.body.profile);
  res.json({ ok: true });
});
app.get('/hqp/pipeline', async (req, res) => res.json(await hqp.fetchPipeline()));
app.post('/hqp/pipeline', async (req, res) => {
  await hqp.setPipelineSetting(req.body.setting, req.body.value);
  res.json({ ok: true });
});
```

### Knob Routes (Roon-Only, Universal Interface)

```javascript
router.get('/zones', (req, res) => {
  const zones = roon.getZones();
  res.json({ zones });
});

router.get('/now_playing', (req, res) => {
  const data = roon.getNowPlaying(req.query.zone_id);
  res.json({ ...data, image_url, zones: roon.getZones(), config_sha });
});

router.get('/now_playing/image', async (req, res) => {
  const data = roon.getNowPlaying(req.query.zone_id);
  if (req.query.format === 'rgb565') {
    const { body } = await roon.getImage(data.image_key, opts);
    // Convert to RGB565 with sharp...
  }
});

router.post('/control', async (req, res) => {
  await roon.control(req.body.zone_id, req.body.action, req.body.value);
  res.json({ ok: true });
});
```

---

## MQTT Service Pattern (src/mqtt/index.js)

```javascript
createMqttService({ hqp, logger })
```

**Current Behavior:**
- Only receives `hqp` instance (HQPlayer only!)
- No Roon integration
- Polls `hqp.getStatus()` every 5s
- Publishes to `unified-hifi/hqplayer/*` topics
- Home Assistant discovery for HQP only

**After Bus:**
- Should receive `bus` instance instead
- Iterate `bus.getZones()` (all backends)
- Publish `unified-hifi/media_player/{zone_id}/*` topics
- Create HA media_player entities for ALL zones

---

## Adapter Interface Design (Based on Existing Patterns)

### Core Interface (What Knob Routes Expect)

```javascript
/**
 * Backend adapter interface - matches RoonClient pattern
 * that knob routes currently expect.
 */
interface BackendAdapter {
  // Lifecycle
  start(): Promise<void>

  // Zone Discovery
  getZones(opts?: {}): Zone[]
  // Returns: [{ zone_id, zone_name, source, state, ... }]

  // Playback State
  getNowPlaying(zone_id: string): PlaybackState | null
  // Returns: { line1, line2, line3, is_playing, volume, ... }

  // Control
  control(zone_id: string, action: string, value?: any): Promise<void>
  // Actions: 'play_pause', 'next', 'previous', 'vol_rel', 'vol_abs'

  // Artwork
  getArtwork(zone_id: string, opts?: {}): Promise<{ contentType: string, body: Buffer }>
  // Note: Takes zone_id, not image_key (bus routes by zone)

  // Status
  getStatus(): object

  // Events (optional - for push-based backends like Roon)
  on?(event: string, handler: Function): void
}
```

### Design Notes

1. **Matches RoonClient pattern:** Knob routes call these exact methods
2. **Zone-centric:** All methods take `zone_id` (bus routes to correct backend)
3. **Artwork by zone:** `getArtwork(zone_id)` instead of `getImage(image_key)`
   - Adapter internally maps zone → image_key (Roon) or fetches from backend
4. **Events optional:** Push-based (Roon) vs pull-based (HQP) backends
5. **Backend-specific methods stay on adapter:**
   - `HQPAdapter.fetchPipeline()`, `setPipelineSetting()` - NOT in interface
   - Accessed via backend-specific routes (`/hqp/pipeline`)

---

## Refactoring Strategy

### Phase 1: Wrap Existing Clients

**RoonAdapter:**
```javascript
class RoonAdapter {
  constructor(roonClient) {
    this.roon = roonClient;  // Wrap existing client
  }

  start() { return this.roon.start(); }
  getZones(opts) { return this.roon.getZones(opts); }
  getNowPlaying(zone_id) { return this.roon.getNowPlaying(zone_id); }

  async getArtwork(zone_id, opts) {
    const np = this.roon.getNowPlaying(zone_id);
    if (!np?.image_key) throw new Error('No artwork');
    return this.roon.getImage(np.image_key, opts);
  }

  control(zone_id, action, value) {
    return this.roon.control(zone_id, action, value);
  }

  getStatus() { return this.roon.getStatus(); }
}
```

**HQPAdapter:**
```javascript
class HQPAdapter {
  constructor(hqpClient) {
    this.hqp = hqpClient;
  }

  async start() { /* no-op, HQP client doesn't need start */ }

  getZones() {
    if (!this.hqp.isConfigured()) return [];
    return [{
      zone_id: 'hqp:pipeline',
      zone_name: 'HQPlayer Pipeline',
      source: 'hqp',
      state: 'idle',  // Would need to fetch from status
    }];
  }

  async getNowPlaying(zone_id) {
    if (zone_id !== 'hqp:pipeline') return null;
    const status = await this.hqp.getStatus();
    // Synthesize playback state from pipeline status
    return {
      line1: status.pipeline?.activeFilter || 'Idle',
      line2: status.pipeline?.activeMode || '',
      line3: status.configName || '',
      is_playing: false,  // HQP doesn't control playback
      volume: status.pipeline?.volume?.value || null,
      // ...
    };
  }

  async getArtwork(zone_id) {
    throw new Error('HQPlayer has no artwork');
    // Or return placeholder
  }

  async control(zone_id, action, value) {
    // HQPlayer doesn't control transport (receives stream from upstream)
    throw new Error('HQPlayer does not support transport control');
  }

  getStatus() { return this.hqp.getStatus(); }

  // Backend-specific (not in interface)
  fetchPipeline() { return this.hqp.fetchPipeline(); }
  setPipelineSetting(name, value) { return this.hqp.setPipelineSetting(name, value); }
}
```

### Phase 2: Bus Routes to Adapters

```javascript
const bus = createBus({ logger });
bus.registerBackend('roon', new RoonAdapter(roon));
bus.registerBackend('hqp', new HQPAdapter(hqp));

// Knob routes now call bus instead of roon directly
router.get('/zones', (req, res) => {
  const zones = bus.getZones();  // All backends!
  res.json({ zones });
});

router.get('/now_playing', (req, res) => {
  const data = bus.getNowPlaying(req.query.zone_id);  // Bus routes to correct backend
  res.json({ ...data });
});
```

---

## Critical Insights

1. **RoonClient interface is the target:** Knob routes expect this exact interface
2. **HQPlayer is NOT a music source:** It's a pipeline/renderer
   - Can't provide transport control (no play/pause/next)
   - Can't provide metadata (receives stream from upstream)
   - Single "fake" zone representing the pipeline
3. **Artwork routing is tricky:**
   - Roon: `getImage(image_key)` where `image_key` comes from `getNowPlaying()`
   - Bus needs: `getArtwork(zone_id)` - adapter fetches image_key internally
4. **Events vs Polling:**
   - Roon: push-based (subscriptions)
   - HQP: pull-based (fetch on-demand)
   - Bus should support both patterns
5. **Backend-specific routes stay separate:**
   - `/hqp/pipeline` for DSP controls
   - `/roon/grouping` for zone grouping (future)
   - Don't force into universal abstraction
