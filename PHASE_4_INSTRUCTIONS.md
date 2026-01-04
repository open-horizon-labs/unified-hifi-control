# Phase 4: LMS Adapter Implementation

**Base:** `38e21c9` - PR #14 merged (Bus Architecture verified present)
**Worktree:** `/Users/muness1/src/unified-hifi-control-lms`
**Branch:** `feature/lms-adapter`
**Parallel:** Phases 3, 4, 5 running simultaneously (equal priority)

---

## Pattern (Verified from Roon Reference)

**Roon adapter:** 78 lines wrapping existing `RoonClient` (370 lines)
**Structure:** Separate client file + thin adapter wrapper

**Follow this pattern:**
1. Create `/src/lms/client.js` - LMS JSON-RPC protocol implementation
2. Create `/src/bus/adapters/lms.js` - ~80 line wrapper (zone_id prefixing, interface mapping)

---

## Task 1: Create LMS Client (`/src/lms/client.js`)

LMS JSON-RPC over HTTP (port 9000). Reference: http://LMS_HOST:9000/html/docs/cli-api.html

**Key methods needed:**
- `getPlayers()` - List Squeezebox players
- `getPlayerStatus(playerId)` - Get track, state, volume
- `control(playerId, command)` - Play/pause/stop/skip
- URL for artwork: `http://HOST:PORT/music/current/cover.jpg?player=MAC`

**Install if needed:** `npm install node-fetch` (for HTTP requests)

---

## Task 2: Create Adapter (`/src/bus/adapters/lms.js`)

**Pattern:** Follow `/src/bus/adapters/roon.js` (78 lines):
- Constructor wraps LMS client
- `getZones()` - Map players to zones with `lms:` prefix
- `getNowPlaying(zone_id)` - Strip prefix, call client, re-add prefix
- `control()` - Map bus actions to LMS commands
- `getImage()` - Construct LMS artwork URL, fetch
- `getStatus()` - Return client status

**Zone ID format:** `lms:{playerid}` (MAC address)

---

## Task 3: Register (`/src/index.js`)

```javascript
const { LMSClient } = require('./lms/client');
const { LMSAdapter } = require('./bus/adapters/lms');

const lms = new LMSClient({
  host: process.env.LMS_HOST,
  port: process.env.LMS_PORT || 9000,
  logger: createLogger('LMS'),
});

if (process.env.LMS_HOST) {
  const lmsAdapter = new LMSAdapter(lms);
  bus.registerBackend('lms', lmsAdapter);
}
```

---

## Testing

```bash
export LMS_HOST=192.168.1.x
npm start
curl http://localhost:8088/zones  # Verify lms:* zones
```

---

## Create PR

```bash
git add src/lms/ src/bus/adapters/lms.js src/index.js
git commit -m "feat: Add LMS adapter (Phase 4)"
git push -u origin feature/lms-adapter
gh pr create --base master --title "Phase 4: LMS Adapter"
```

**Keep adapter ~80 lines like Roon. Client can be longer (protocol details).**
