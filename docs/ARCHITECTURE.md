# Architecture

## Vision

A source-agnostic hi-fi control platform where **complexity is absorbed by the bus and coordinator, not distributed across adapters or UI**.

## Target Architecture

```
┌─────────────────────────────────────────────────────────┐
│                   AdapterCoordinator                     │
│  (owns lifecycle: start/stop based on settings)         │
│  (publishes ShuttingDown on Ctrl+C)                     │
└─────────────────────────────────────────────────────────┘
           │
           ▼
┌─────────────────────────────────────────────────────────┐
│                    In-App Bus Runtime                    │
│  (bounded commands + ordered projection ingress)        │
└─────────────────────────────────────────────────────────┘
     ▲           ▲           ▲              │
     │           │           │              ▼
┌────────┐  ┌────────┐  ┌────────┐   ┌─────────────┐
│ LMS    │  │ Roon   │  │ UPnP   │   │   Zone      │
│Handle  │  │Handle  │  │Handle  │   │ Aggregator  │
│        │  │        │  │        │   │ (state)     │
│[Logic] │  │[Logic] │  │[Logic] │   └─────────────┘
└────────┘  └────────┘  └────────┘          │
                                             ▼
                                      ┌─────────────┐
                                      │   API/UI    │
                                      │   + SSE     │
                                      └─────────────┘
```

## Key Components

### AdapterCoordinator
- Single decision point for adapter lifecycle
- Starts only enabled adapters
- Publishes `ShuttingDown` on Ctrl+C
- Waits for adapter ACKs before exit

### AdapterHandle + AdapterLogic
- **AdapterLogic trait**: Adapter-specific discovery/protocol (what varies)
- **AdapterHandle**: Wraps logic with consistent lifecycle (what's common)
- Adapters can't forget shutdown handling - the handle does it
- ACK on stop is automatic

### EventBus
- Critical adapter communication uses private bounded request/reply lanes, never `broadcast`.
- Commands are correlated, deadline-aware, and routed to one provider or exact-zone endpoint.
- Native acceptance is not success: confirmation requires a causally linked projection commit.
- Canonical observations carry source epoch and sequence and commit atomically at one revision.
- The existing `BusEvent` broadcast is post-commit notification/lifecycle egress only. Receivers
  reread the aggregator; they never reconstruct canonical state from the notification stream.

### ZoneAggregator
- Single source of truth for zone state
- Admits ordered observations from the reliable bus and maintains the canonical projection
- Rejects stale/duplicate observations and marks sequence gaps reconciling until a snapshot commits
- Flushes zones on `AdapterStopping`
- API calls this, never adapters directly

### SSE (Server-Sent Events)
Real-time event streaming for clients via `/events` endpoint.

**Endpoint:** `GET /events`

**Event Types:**
| Event | Payload | Description |
|-------|---------|-------------|
| `RoonConnected` | — | Roon core discovered |
| `RoonDisconnected` | — | Roon core lost |
| `ZoneUpdated` | `{ zone_id }` | Zone state changed |
| `ZoneRemoved` | `{ zone_id }` | Zone no longer available |
| `NowPlayingChanged` | `{ zone_id }` | Track/playback changed |
| `VolumeChanged` | `{ zone_id }` | Volume level changed |
| `SeekPositionChanged` | `{ zone_id }` | Playback position changed |
| `HqpConnected` | — | HQPlayer connected |
| `HqpDisconnected` | — | HQPlayer disconnected |
| `HqpStateChanged` | — | HQPlayer state changed |
| `HqpPipelineChanged` | — | HQPlayer DSP pipeline changed |
| `LmsConnected` | — | LMS server connected |
| `LmsDisconnected` | — | LMS server disconnected |
| `LmsPlayerStateChanged` | `{ player_id }` | LMS player state changed |
| `OpenHomeDeviceFound` | — | OpenHome device discovered |
| `OpenHomeDeviceLost` | — | OpenHome device lost |
| `UpnpRendererFound` | — | UPnP renderer discovered |
| `UpnpRendererLost` | — | UPnP renderer lost |

**Message Format:**
```json
{"type":"NowPlayingChanged","payload":{"zone_id":"roon:1234567890"}}
```

**Usage:**
- Web UI uses EventSource API for reactive updates
- Any HTTP client can subscribe (curl, ESP32, etc.)
- Auto-reconnects on connection loss (EventSource spec)
- Closes gracefully on server shutdown

## Principles

1. **Disabled adapter = not started = nothing to show**
   - Coordinator checks settings before start
   - No "searching" for disabled backends

2. **Zone identity is the zone_id prefix**
   - `roon:`, `lms:`, `openhome:`, `upnp:`, `hqplayer:`
   - No separate `source` or `protocol` fields

3. **Adapters communicate only through the in-app bus**
   - Don't store zones (aggregator does)
   - Publish ordered observations and handle routed commands
   - Never call the aggregator or a client surface directly
   - Lifecycle managed by handle

4. **Clean shutdown path**
   - `ShuttingDown` → SSE handlers close
   - `AdapterStopping(prefix)` → Aggregator flushes
   - `stop()` with ACK → Coordinator waits
   - No hanging on Ctrl+C

## Adaptive-Control Producer Contract (dormant)

The repository retains an experimental versioned **producer document** contract for compatibility
and design history. No production adapter currently publishes it and no public surface consumes it.
See
[architecture/adaptive-producer-contract-v1.md](./architecture/adaptive-producer-contract-v1.md)
and [ADR 003](./adr/003-adaptive-producer-document-v1.md).

HQPlayer instead publishes its typed coherent observation through the reliable in-app projection
lane to `ZoneAggregator`, the same state owner used by every other zone. Web, knob, HTTP, and MCP surfaces read that aggregator and
share the managed command path. `src/adaptive/` remains description-only; it depends on nothing but
`serde`, `serde_json` and `std`, enforced by `tests/adaptive_dependency_lint.rs`.

## Implementation

See [ARCHITECTURE-RECOMMENDATION-A.md](./ARCHITECTURE-RECOMMENDATION-A.md) for detailed implementation plan.

See [ARCHITECTURE-GAP-ANALYSIS.md](./ARCHITECTURE-GAP-ANALYSIS.md) for analysis of current state vs this vision.
