# Architecture Gap Analysis: v3 vs Event Bus Vision

**Date:** 2026-01-17
**Issue:** #83
**Reference:** [ARCHITECTURE.md](./ARCHITECTURE.md)

## Executive Summary

The v3 Rust implementation follows a **distributed state with notification bus** pattern rather than the **centralized aggregator with event bus** pattern described in ARCHITECTURE.md. This creates complexity in the UI layer and explains several open issues.

## Current State (v3 Rust)

```
┌─────────────────────────────────────────────────────────────────┐
│                       Axum HTTP Server                           │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │              AppState (shared reference)                  │   │
│  │                                                           │   │
│  │  RoonAdapter ──┬── Arc<RwLock<RoonState>>                │   │
│  │  LmsAdapter ───┼── Arc<RwLock<LmsState>>                 │   │
│  │  OpenHome ─────┼── Arc<RwLock<OpenHomeState>>            │   │
│  │  UPnP ─────────┴── Arc<RwLock<UPnPState>>                │   │
│  │                                                           │   │
│  │  EventBus ──── broadcast::channel (notifications only)   │   │
│  └──────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────┘
```

**Key characteristics:**
- Each adapter owns its own state (`Arc<RwLock<State>>`)
- Bus is notification-only (no commands, no aggregation)
- API handlers call adapters directly for zone data
- Adapter settings filter API responses, don't stop adapters

## Vision (ARCHITECTURE.md)

```
Coordinator → Bus → Adapters (publish events)
                  → Aggregator (owns state) → UI
```

**Key characteristics:**
- Adapters are stateless (publish events only)
- Aggregator owns all zone state
- UI talks to aggregator only
- Disabled adapter = not running = no events

## Gap Analysis

### 1. State Ownership

| Aspect | Vision | v3 Reality | Gap Severity |
|--------|--------|------------|--------------|
| Zone state location | Aggregator | Each adapter | **High** |
| State type | Event-sourced | Mutable cache | Medium |
| Single source of truth | Yes (aggregator) | No (N adapters) | **High** |

**Impact:** UI must coordinate across N adapters for unified view. Race conditions possible.

**File:** `src/knobs/routes.rs:83-177` - `get_all_zones_internal()` manually aggregates.

### 2. Bus Role

| Aspect | Vision | v3 Reality | Gap Severity |
|--------|--------|------------|--------------|
| Event types | All state changes | Notifications only | Medium |
| Command routing | Via bus | Direct adapter calls | Medium |
| Aggregation | Bus consumer | API handler | Medium |

**Impact:** Bus is underutilized. Commands bypass it entirely.

**File:** `src/bus/mod.rs` - Only defines notification events.

### 3. Adapter Lifecycle

| Aspect | Vision | v3 Reality | Gap Severity |
|--------|--------|------------|--------------|
| Disabled = not running | Yes | No (always runs) | **High** |
| UI shows disabled state | Never | Shows "searching" | **High** |

**Impact:** Issues #81, #80, #71 are symptoms of this gap.

**File:** `src/api/mod.rs:1428-1502` - `AdapterSettings` only filters API response, adapters still run.

### 4. UI Communication

| Aspect | Vision | v3 Reality | Gap Severity |
|--------|--------|------------|--------------|
| UI talks to | Aggregator only | Each adapter via AppState | Medium |
| Backend knowledge | None | Knows all adapter types | Medium |

**Impact:** UI layer is more complex than necessary.

**File:** `src/api/mod.rs` - Handlers reference specific adapter types.

## Root Cause: Incremental Migration

The v3 Rust port appears to be a faithful port of the v2 Node.js architecture. The Node.js version had the same distributed state pattern. The vision in ARCHITECTURE.md represents the *intended* architecture, not the *implemented* architecture in either version.

## Recommendations

### Option A: Full Refactor (High Effort, High Value)

1. Create `ZoneAggregator` struct that:
   - Subscribes to bus events
   - Maintains unified zone state
   - Exposes `get_zones()`, `get_zone(id)`, `get_now_playing(id)`

2. Change adapters to:
   - Publish zone events (discovered, updated, removed) to bus
   - Not store zones internally (stateless for zone data)
   - Still handle commands (receive via bus or direct call)

3. Change API handlers to:
   - Call aggregator for all zone queries
   - Route commands through aggregator or bus

4. Change adapter enable/disable to:
   - Actually stop adapter task when disabled
   - No events = no zones from that source

### Option B: Targeted Fixes (Low Effort, Addresses Symptoms)

1. **Fix #81, #80, #71:** Add adapter status to API that reflects actual state
2. **Fix disabled behavior:** Don't run SSDP discovery for disabled backends
3. **Keep distributed state:** Accept the complexity trade-off

### Option C: Document Intentional Deviation

If the distributed pattern is preferred for Rust (e.g., for performance, simplicity of async ownership), update ARCHITECTURE.md to reflect reality and add rationale.

## Affected Issues

| Issue | Symptom | Root Cause |
|-------|---------|------------|
| #81 | OpenHome "searching" when disabled | Adapter runs regardless of setting |
| #80 | LMS page vs Zones mismatch | Parallel state management |
| #71 | HQPlayer "not connected" on start | Backend state leaks to UI |
| #62 | LMS not auto-enabled | Config layer, not architecture |

## Superego Review Findings

### P1: The Vision May Be Wrong For Rust

The distributed state pattern using `Arc<RwLock<State>>` per adapter is idiomatic Rust. A centralized aggregator would require either message passing (adds latency) or a single `RwLock` over all state (creates contention). The vision may describe an ideal that doesn't fit Rust's ownership model.

### P2: The Real Problem Is Simpler

The issues (#81, #80, #71) have a single root cause: **adapters run regardless of the "enabled" setting**. In `main.rs`, adapters start unconditionally. `AdapterSettings` only filters the API response at query time.

This is a lifecycle bug, not an architectural crisis. Fix:
```rust
// In main.rs - check settings before starting
let settings = api::load_app_settings();
if settings.adapters.openhome {
    if let Err(e) = openhome.start().await { ... }
}
```

### P3: Severity Overstated

The gap analysis marks "Zone state location" as High severity, but:
- `get_all_zones_internal()` aggregates across adapters in 50 lines and works
- No evidence of actual race conditions causing bugs
- Users care about "searching" states, not architectural purity

### P4: Option A Risk Assessment

Full refactor touches every adapter, every API handler, the bus, and UI. This is a v4 rewrite disguised as a refactor. High risk for unproven benefit at RC stage.

## Decision

**Option B was initially recommended but rejected after review.**

- [ARCHITECTURE-RECOMMENDATION-B-REJECTED.md](./ARCHITECTURE-RECOMMENDATION-B-REJECTED.md) - Why targeted fixes are insufficient
- [ARCHITECTURE-RECOMMENDATION-A.md](./ARCHITECTURE-RECOMMENDATION-A.md) - Event bus refactor (approved approach)

**Key insight:** "High effort" is obsolete with agentic coding. The architectural debt of Option B isn't worth accepting.

## Next Steps

1. [x] Superego review of options
2. [x] Review superego recommendation - rejected Option B
3. [ ] Review Option A proposal
4. [ ] Implement event bus architecture
