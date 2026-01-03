# Phase 1: Bus Foundation - Task Beads

**Phase Goal:** Create minimal viable bus abstraction. Zero user-visible changes.

**Status:** Ready for implementation
**Parallelization:** At backend adapter level, not bus internals level

---

## Parallelization Strategy

**Don't split the bus core** - adapter interface, Zone class, and Bus class are tightly coupled and need coherent design. Parallelize at the **backend adapter level** instead.

**Phase 1:** Single session on bus core (Bead A)
**Phase 2+:** Swarm on backend adapters (Roon/HQP/UPnP/LMS in parallel)

---

## Bead A: Bus Core Foundation

**Owner:** Single session (NOT parallelizable)
**Estimated:** 2-3 hours
**Depends on:** None

### Goal
Design and implement the bus abstraction layer with stable interfaces.

### Tasks
1. Create `/src/bus/` directory structure
2. Define adapter interface (what backends must implement)
3. Implement `Bus` class (zone registry, backend routing)
4. Implement `Zone` class (wrapper around backend adapter)
5. Write integration test harness (mock adapter for testing)

### Acceptance Criteria
- [ ] Bus core exists and is importable: `const { createBus } = require('./src/bus')`
- [ ] Adapter interface clearly defined (JSDoc or TypeScript types)
- [ ] Mock adapter can be registered: `bus.registerBackend('test', mockAdapter)`
- [ ] Bus methods work with mock: `getZones()`, `control()`, `getNowPlaying()`, `getArtwork()`
- [ ] Tests pass with mock adapter
- [ ] Zero integration with real backends yet (that's Phase 2)

### Deliverable
Working bus that:
- Accepts backend adapters conforming to interface
- Maintains zone registry (zone_id → Zone → backend adapter)
- Routes commands to correct backend based on zone_id
- Handles missing zones gracefully (error responses)

**Why NOT parallelizable?**
These components must be designed together:
- What methods does adapter interface expose?
- What does Zone class expect from adapter?
- How does Bus wire them together?

Splitting these creates integration thrash (incompatible interfaces, negotiation overhead).

---

## Bead B: Roon Adapter Wrapper

**Owner:** Can parallelize with Bead A *after* interface is defined
**Estimated:** 2 hours
**Depends on:** Adapter interface from Bead A (just the interface spec)

### Goal
Wrap existing RoonClient to conform to bus adapter interface.

### Tasks
1. Study existing `RoonClient` API (in `/src/roon/client.js`)
2. Create `/src/bus/adapters/roon.js`
3. Implement adapter interface methods:
   - `getZones()` - map Roon zones to unified format
   - `getNowPlaying(zone_id)` - fetch playback state
   - `control(zone_id, action, value)` - send transport commands
   - `getArtwork(zone_id, opts)` - fetch album artwork
4. Map Roon subscriptions to adapter events (if bus expects events)
5. Test adapter in isolation (unit tests with mocked Roon core)

### Acceptance Criteria
- [ ] `RoonAdapter` implements bus adapter interface
- [ ] All interface methods work
- [ ] Roon subscriptions integrated (zone changes propagate)
- [ ] Tests pass in isolation (no bus integration yet)

### Deliverable
`RoonAdapter` class that wraps existing `RoonClient` and exposes unified interface.

**Why parallelizable?**
Once adapter interface is defined (from Bead A), this work is independent. Can implement against interface spec while bus implementation continues.

**Integration happens in Phase 2** when we wire RoonAdapter into bus and update HTTP routes.

---

## Bead C: HQPlayer Adapter Wrapper

**Owner:** Can parallelize with Beads A and B
**Estimated:** 1.5 hours
**Depends on:** Adapter interface from Bead A

### Goal
Wrap existing HQPClient to conform to bus adapter interface.

### Tasks
1. Study existing `HQPClient` API (in `/src/hqplayer/client.js`)
2. Create `/src/bus/adapters/hqp.js`
3. Implement adapter interface methods
4. Handle HQPlayer specifics:
   - Single zone (fake zone_id: `hqp:pipeline`)
   - No upstream metadata (return placeholder or null)
   - DSP settings as backend-specific extensions
5. Test adapter in isolation

### Acceptance Criteria
- [ ] `HQPAdapter` implements bus adapter interface
- [ ] Single zone exposed (`hqp:pipeline`)
- [ ] Pipeline controls remain as backend-specific methods
- [ ] Tests pass in isolation

### Deliverable
`HQPAdapter` class that wraps existing `HQPClient`.

**Why parallelizable?**
Independent work once interface is defined. HQPlayer is simpler (single zone) so can be done in parallel with Roon adapter.

---

## Bead D: Architecture Documentation

**Owner:** Can parallelize with A, B, C
**Estimated:** 1 hour
**Depends on:** None (captures design decisions)

### Goal
Document bus architecture and adapter implementation patterns.

### Tasks
1. ✅ Already done: `/docs/architecture/bus-design.md`
2. Add sequence diagrams for control flow:
   - Frontend → HTTP → Bus → Adapter → Backend
   - Backend event → Adapter → Bus → MQTT bridge
3. Document zone overlap strategy:
   - How Roon zone + HQPlayer pipeline both represent same output
   - UI patterns for showing linked zones
4. Write adapter implementation guide:
   - How to implement a new backend adapter
   - Testing patterns
   - Integration checklist

### Acceptance Criteria
- [ ] Sequence diagrams added to `/docs/architecture/bus-design.md`
- [ ] Zone overlap strategy documented
- [ ] `/docs/bus/adapter-guide.md` created with implementation guide

### Deliverable
Complete documentation for bus architecture and how to add new backends.

**Why parallelizable?**
Pure documentation. Can be written while implementation proceeds. Captures design decisions but doesn't block code.

---

## Dependencies Graph

```
Bead A (Bus Core)
  └─> defines adapter interface
       ├─> Bead B (Roon Adapter) - waits for interface, then parallel
       ├─> Bead C (HQP Adapter) - waits for interface, then parallel
       └─> Bead D (Documentation) - fully parallel (independent)

Phase 2 Integration (after all beads complete):
  - Wire adapters into bus
  - Update HTTP routes to delegate to bus
  - Test knob backward compatibility
```

---

## Swarm Execution Strategy

### Sequential Start
1. **Session 1:** Start Bead A (bus core)
2. Wait for adapter interface to be defined (~30-45 min into Bead A)
3. Publish interface spec

### Parallel Work
Once interface is published:
4. **Session 1:** Continue Bead A (Bus and Zone implementation)
5. **Session 2:** Start Bead B (Roon adapter)
6. **Session 3:** Start Bead C (HQP adapter)
7. **Session 4:** Start Bead D (Documentation) - can start anytime

### Integration (Phase 2)
8. **Session 1:** Integrate adapters into bus, update HTTP routes

**Total time (serial):** ~7.5 hours
**Total time (parallel, 4 sessions):** ~3 hours

---

## Phase 1 Complete When

- ✅ Bus core exists (`/src/bus/index.js`)
- ✅ Adapter interface defined (`/src/bus/adapter.js`)
- ✅ Roon adapter exists (`/src/bus/adapters/roon.js`)
- ✅ Documentation complete
- ❌ HQP adapter (planned for Phase 3)
- ❌ Tests (planned for future phase)
- ✅ HTTP routes updated (Phase 2 complete)
- ✅ Bus integrated with main app (Phase 2 complete)

**Phase 1 output:** Reusable bus abstraction + two adapters, ready to integrate.

---

## Why This Granularity Works

**Bus internals are design-coherent:**
- Adapter interface, Zone class, Bus class must align
- Single session with full context makes better decisions
- No integration thrash from multiple sessions negotiating interfaces

**Backend adapters are truly independent:**
- Each adapter only depends on interface definition
- Can be implemented in parallel once interface is stable
- Clean integration (adapters conform to known interface)

**Future backends follow same pattern:**
- UPnP adapter (Phase 4)
- LMS adapter (Phase 5)
- Each is a separate bead, can be swarmed

**This enables swarm execution where it matters** - on backend adapters, not bus internals.
