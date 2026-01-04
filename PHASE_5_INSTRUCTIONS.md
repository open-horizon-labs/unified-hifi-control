# Phase 5: UPnP Adapter Implementation

**Base:** `38e21c9` - PR #14 merged (Bus Architecture verified)
**Worktree:** `/Users/muness1/src/unified-hifi-control-upnp`
**Branch:** `feature/upnp-adapter`
**Parallel:** Equal priority with Phases 3 & 4

---

## Pattern (from Roon Reference)

**Structure:** Client file + thin adapter (~80 lines)
- `/src/upnp/client.js` - UPnP SSDP discovery + SOAP control
- `/src/bus/adapters/upnp.js` - Wrapper with `upnp:` prefixing

---

## Task 1: Install & Create Client

```bash
npm install --save upnp-mediarenderer-client
```

Create `/src/upnp/client.js` wrapping the library for discovery and control.

---

## Task 2: Create Adapter (`/src/bus/adapters/upnp.js`)

**Pattern:** Follow `/src/bus/adapters/roon.js` (~80 lines)
- Wrap UPnP client
- Zone IDs: `upnp:{uuid}`
- Control point role (discover/control renderers)

---

## Task 3: Register (`/src/index.js`)

```javascript
const { UPnPClient } = require('./upnp/client');
const { UPnPAdapter } = require('./bus/adapters/upnp');

const upnp = new UPnPClient({ logger: createLogger('UPnP') });
const upnpAdapter = new UPnPAdapter(upnp);
bus.registerBackend('upnp', upnpAdapter);
```

---

## Create PR

```bash
git add src/upnp/ src/bus/adapters/upnp.js src/index.js package.json
git commit -m "feat: Add UPnP adapter (Phase 5)"
git push -u origin feature/upnp-adapter
gh pr create --base master --title "Phase 5: UPnP Adapter"
```
