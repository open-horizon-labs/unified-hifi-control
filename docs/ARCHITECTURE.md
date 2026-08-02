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
│                       EventBus                           │
│  (tokio broadcast: zone events, commands, lifecycle)    │
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
- Zone lifecycle: `ZoneDiscovered`, `ZoneUpdated`, `ZoneRemoved`
- Now playing: `NowPlayingChanged`
- Commands: `Command`, `CommandResponse`
- Lifecycle: `AdapterStopping`, `AdapterStopped`, `ZonesFlushed`, `ShuttingDown`

### ZoneAggregator
- Single source of truth for zone state
- Subscribes to bus, maintains `HashMap<zone_id, Zone>`
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
   - `roon:`, `lms:`, `openhome:`, `upnp:`, `hqp:`
   - No separate `source` or `protocol` fields

3. **Adapters are event publishers**
   - Don't store zones (aggregator does)
   - Publish events, handle commands
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

HQPlayer instead publishes its typed coherent observation directly to `ZoneAggregator`, the same
state owner used by every other zone. Web, knob, HTTP, and MCP surfaces read that aggregator and
share the managed command path. `src/adaptive/` remains description-only; it depends on nothing but
`serde`, `serde_json` and `std`, enforced by `tests/adaptive_dependency_lint.rs`.

## Implementation

See [ARCHITECTURE-RECOMMENDATION-A.md](./ARCHITECTURE-RECOMMENDATION-A.md) for detailed implementation plan.

See [ARCHITECTURE-GAP-ANALYSIS.md](./ARCHITECTURE-GAP-ANALYSIS.md) for analysis of current state vs this vision.
