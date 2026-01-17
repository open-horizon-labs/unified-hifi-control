# Architecture

## Vision

A source-agnostic hi-fi control platform where **complexity is absorbed by clear boundaries, not distributed across components**.

## Rust v3 Pattern: Distributed State with Notification Bus

The Rust implementation uses a pattern optimized for Rust's ownership model:

```
┌─────────────────────────────────────────────────────────┐
│                      main.rs                             │
│  (creates adapters based on config + settings)          │
└─────────────────────────────────────────────────────────┘
           │
           ▼
┌─────────────────────────────────────────────────────────┐
│                       AppState                           │
│  (shared reference to all adapters)                     │
└─────────────────────────────────────────────────────────┘
     │           │           │              │
     ▼           ▼           ▼              ▼
┌────────┐  ┌────────┐  ┌────────┐   ┌─────────────┐
│  LMS   │  │  Roon  │  │ UPnP   │   │  EventBus   │
│Adapter │  │Adapter │  │Adapter │   │ (broadcast) │
│ State  │  │ State  │  │ State  │   └─────────────┘
└────────┘  └────────┘  └────────┘          │
                                             ▼
                                      ┌─────────────┐
                                      │  SSE /events│
                                      │  (realtime) │
                                      └─────────────┘
```

### Why Distributed State for Rust

| Centralized Aggregator | Distributed State (v3) |
|------------------------|------------------------|
| Single `RwLock` contention | Independent locks per adapter |
| Channel hops add latency | Direct adapter access |
| Complex event routing | Simple `Arc<Adapter>` sharing |
| Debugging: "where did event go?" | Debugging: check adapter state |

The distributed pattern with `Arc<RwLock<State>>` per adapter is idiomatic Rust for async systems.

### How It Works

1. **Adapters own their state** - Each adapter has its own `Arc<RwLock<State>>`
2. **API aggregates on demand** - `get_all_zones_internal()` queries each enabled adapter
3. **Bus is for notifications** - Real-time updates via SSE, not state synchronization
4. **Zone ID prefix routes commands** - `roon:zone_123` → RoonAdapter.control()

### Key Principles

1. **Disabled backend = adapter not started = nothing to show**
   - Check `AdapterSettings` before calling `adapter.start()` in `main.rs`
   - UI never shows "searching" for a disabled backend

2. **Zone identity is the zone_id prefix**
   - `roon:`, `lms:`, `openhome:`, `upnp:`
   - No separate `source` or `protocol` fields needed

3. **Adapters are self-contained**
   - Handle their own discovery, state, and reconnection
   - Expose clean async interface to API layer

4. **API layer aggregates**
   - Query adapters, merge results, return to client
   - No persistent aggregated state (stateless aggregation)

## Anti-Patterns to Avoid

| Anti-Pattern | Why It's Wrong | Correct Approach |
|--------------|----------------|------------------|
| Adapter runs when disabled | Shows "searching" for nothing | Check settings before `start()` |
| UI polls for discovery status | UI knows too much about adapters | Expose unified status endpoint |
| Parallel state in UI | State sync bugs | UI is stateless, queries API |

## Implementation

See [adapter.md](./adapter.md) for the adapter implementation pattern (Node.js v2 reference).

See [ARCHITECTURE-GAP-ANALYSIS.md](./ARCHITECTURE-GAP-ANALYSIS.md) for the analysis of v3 vs original vision.
