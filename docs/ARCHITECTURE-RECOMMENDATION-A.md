# Recommendation A: Event Bus Architecture Refactor

**Status:** PROPOSED
**Date:** 2026-01-17
**Issue:** #83

## Summary

Refactor v3 to implement the event bus pattern with centralized coordination and aggregation.

## Target Architecture

```
┌─────────────────────────────────────────────────────────┐
│                   AdapterCoordinator                     │
│  (owns lifecycle: start/stop based on settings)         │
└─────────────────────────────────────────────────────────┘
           │
           ▼
┌─────────────────────────────────────────────────────────┐
│                       EventBus                           │
│  (tokio broadcast channel for events + commands)        │
└─────────────────────────────────────────────────────────┘
     ▲           ▲           ▲              │
     │           │           │              ▼
┌────────┐  ┌────────┐  ┌────────┐   ┌─────────────┐
│  LMS   │  │  Roon  │  │ UPnP   │   │   Zone      │
│Adapter │  │Adapter │  │Adapter │   │ Aggregator  │
│(events)│  │(events)│  │(events)│   │ (state)     │
└────────┘  └────────┘  └────────┘   └─────────────┘
                                             │
                                             ▼
                                      ┌─────────────┐
                                      │   API/UI    │
                                      └─────────────┘
```

## Key Components

### 1. AdapterCoordinator

Single decision point for adapter lifecycle.

```rust
pub struct AdapterCoordinator {
    adapters: HashMap<String, Arc<dyn Adapter>>,
    settings: Arc<RwLock<AdapterSettings>>,
    bus: SharedBus,
}

impl AdapterCoordinator {
    /// Start only adapters that are enabled in settings
    pub async fn start(&self) -> Result<()> {
        let settings = self.settings.read().await;
        for (name, adapter) in &self.adapters {
            if settings.is_enabled(name) {
                adapter.start(self.bus.clone()).await?;
            }
        }
        Ok(())
    }

    /// React to settings changes at runtime
    pub async fn on_settings_changed(&self, new_settings: AdapterSettings) {
        // Start newly enabled, stop newly disabled
    }
}
```

**Benefits:**
- Single place to add/remove adapters
- Settings changes handled uniformly
- No scattered `if settings.adapters.foo` checks

### 2. Adapter Trait + Handle Pattern

Split adapter logic from lifecycle management:

```rust
/// Adapter-specific logic (what each adapter implements)
#[async_trait]
pub trait AdapterLogic: Send + Sync {
    fn prefix(&self) -> &str;

    /// Run discovery/connection loop until cancellation
    async fn run(&self, ctx: AdapterContext) -> Result<()>;

    /// Handle a command (called by AdapterHandle)
    async fn handle_command(&self, zone_id: &str, cmd: Command) -> Result<CommandResponse>;
}

/// Context passed to adapter logic
pub struct AdapterContext {
    pub bus: SharedBus,              // For publishing events
    pub shutdown: CancellationToken, // For internal cancellation checks
}
```

**AdapterHandle** wraps any `AdapterLogic` and handles common lifecycle:

```rust
pub struct AdapterHandle<T: AdapterLogic> {
    logic: T,
    bus: SharedBus,
    shutdown: CancellationToken,
}

impl<T: AdapterLogic> AdapterHandle<T> {
    pub async fn run(self) {
        let prefix = self.logic.prefix();
        let mut rx = self.bus.subscribe();

        tokio::select! {
            // Run adapter-specific logic
            result = self.logic.run(AdapterContext {
                bus: self.bus.clone(),
                shutdown: self.shutdown.clone(),
            }) => {
                if let Err(e) = result {
                    tracing::error!("Adapter {} error: {}", prefix, e);
                }
            }

            // Watch for shutdown signal on bus
            _ = async {
                while let Ok(event) = rx.recv().await {
                    if matches!(event, BusEvent::ShuttingDown) {
                        break;
                    }
                }
            } => {
                tracing::info!("Adapter {} received shutdown signal", prefix);
            }

            // Direct cancellation (backup)
            _ = self.shutdown.cancelled() => {
                tracing::info!("Adapter {} cancelled", prefix);
            }
        }

        // Consistent cleanup - ACK is automatic
        self.bus.publish(BusEvent::AdapterStopped {
            prefix: prefix.to_string()
        });
    }
}
```

**Benefits:**
- Adapters only implement discovery/protocol logic
- Shutdown handling is consistent (can't forget)
- ACK on stop is automatic
- Fixes #73 (SSE shutdown) as a natural consequence

### 3. Extended Bus Events

```rust
pub enum BusEvent {
    // Zone lifecycle (adapters publish these)
    ZoneDiscovered { zone: Zone },
    ZoneUpdated { zone_id: String, update: ZoneUpdate },
    ZoneRemoved { zone_id: String },

    // Now playing (adapters publish these)
    NowPlayingChanged { zone_id: String, now_playing: NowPlaying },

    // Commands (API publishes, adapters consume)
    Command { zone_id: String, command: Command },
    CommandResponse { zone_id: String, result: Result<(), String> },

    // Adapter lifecycle (coordinator publishes)
    AdapterStopping { prefix: String },
    AdapterStopped { prefix: String },
    ZonesFlushed { prefix: String },

    // Global shutdown (coordinator publishes, SSE handlers watch)
    ShuttingDown,

    // Existing notification events (for backward compat during migration)
    RoonConnected { core_name: String, version: String },
    // etc.
}
```

### 4. ZoneAggregator

Single source of truth for zone state.

```rust
pub struct ZoneAggregator {
    zones: Arc<RwLock<HashMap<String, Zone>>>,
    now_playing: Arc<RwLock<HashMap<String, NowPlaying>>>,
}

impl ZoneAggregator {
    /// Subscribe to bus and maintain state
    pub async fn run(&self, mut rx: broadcast::Receiver<BusEvent>) {
        while let Ok(event) = rx.recv().await {
            match event {
                BusEvent::ZoneDiscovered { zone } => {
                    self.zones.write().await.insert(zone.zone_id.clone(), zone);
                }
                BusEvent::ZoneRemoved { zone_id } => {
                    self.zones.write().await.remove(&zone_id);
                }
                BusEvent::NowPlayingChanged { zone_id, now_playing } => {
                    self.now_playing.write().await.insert(zone_id, now_playing);
                }
                _ => {}
            }
        }
    }

    /// API calls this - no direct adapter access
    pub async fn get_zones(&self) -> Vec<Zone> {
        self.zones.read().await.values().cloned().collect()
    }

    pub async fn get_now_playing(&self, zone_id: &str) -> Option<NowPlaying> {
        self.now_playing.read().await.get(zone_id).cloned()
    }
}
```

### 5. Simplified AppState

```rust
pub struct AppState {
    pub coordinator: Arc<AdapterCoordinator>,
    pub aggregator: Arc<ZoneAggregator>,
    pub bus: SharedBus,
    // HQPlayer special handling (DSP service, not zone source)
    pub hqp_zone_links: Arc<HqpZoneLinkService>,
}
```

API handlers call `aggregator.get_zones()` instead of iterating adapters.

## Migration Path

### Phase 1: Add New Components (Non-Breaking)
1. Create `AdapterCoordinator` struct
2. Create `ZoneAggregator` struct
3. Add new `BusEvent` variants
4. Wire up in `main.rs` alongside existing code

### Phase 2: Migrate Adapters
For each adapter:
1. Add `Adapter` trait implementation
2. Change from storing zones to publishing events
3. Move zone state to aggregator

### Phase 3: Simplify API Layer
1. Change handlers to use `aggregator.get_zones()`
2. Remove direct adapter references from `AppState`
3. Route commands through bus

### Phase 4: Cleanup
1. Remove old zone storage from adapters
2. Remove scattered settings checks
3. Update tests

## Risks & Mitigations

| Risk | Mitigation |
|------|------------|
| Breaking existing behavior | Phase 1 is additive; can run old and new in parallel |
| Event ordering issues | Aggregator processes events sequentially |
| Performance regression | Tokio channels are cheap; benchmark before/after |

## Success Criteria

1. **Disabled adapter = nothing in UI** - No "searching" for disabled backends
2. **Single coordination point** - Adding an adapter means implementing the trait
3. **API layer simplified** - Handlers don't know about individual adapters
4. **Tests pass** - Existing functionality preserved

## Files To Modify

| File | Change |
|------|--------|
| `src/coordinator.rs` | New: AdapterCoordinator |
| `src/aggregator.rs` | New: ZoneAggregator |
| `src/bus/mod.rs` | Extended events (lifecycle, zone events) |
| `src/adapters/mod.rs` | New: Adapter trait definition |
| `src/adapters/roon.rs` | Implement Adapter trait, publish events |
| `src/adapters/lms.rs` | Implement Adapter trait, publish events |
| `src/adapters/openhome.rs` | Implement Adapter trait, publish events |
| `src/adapters/upnp.rs` | Implement Adapter trait, publish events |
| `src/adapters/hqplayer.rs` | Implement Adapter trait, publish events |
| `src/adapters/mqtt.rs` | **Remove** (re-add later as event consumer) |
| `src/api/mod.rs` | Use aggregator, simplified AppState |
| `src/main.rs` | Wire up coordinator and aggregator |
| `src/knobs/routes.rs` | Use aggregator instead of direct calls |

## Decisions on Open Questions

### 1. HQPlayer: Same Adapter Pattern

HQPlayer follows the same adapter/aggregator split. It's not a special case.

- Implements `Adapter` trait
- Publishes `ZoneDiscovered`/`ZoneUpdated` events for HQPlayer zones
- DSP linkage (applying HQPlayer to Roon/LMS zones) handled separately via `HqpZoneLinkService`

### 2. MQTT Bridge: Remove for Now

Rip out MQTT adapter to simplify the refactor. Can be re-added later following the new pattern.

- Reduces scope of initial refactor
- Home Assistant integration can return as a proper event consumer post-refactor

### 3. Shutdown Protocol: Yes, Immediate Stop with ACK

When an adapter is disabled at runtime:

```
Coordinator                    Bus                      Aggregator
    │                           │                            │
    │──AdapterStopping(prefix)──▶                            │
    │                           │──AdapterStopping(prefix)──▶│
    │                           │                            │
    │                           │   (aggregator flushes      │
    │                           │    zones with that prefix) │
    │                           │                            │
    │◀──────────────────────────│◀──ZonesFlushed(prefix)────│
    │                           │                            │
    │──stop()──▶ Adapter        │                            │
    │◀──ACK─────                │                            │
    │                           │                            │
    │──AdapterStopped(prefix)──▶│                            │
```

**Events:**
```rust
pub enum BusEvent {
    // ... existing events ...

    // Adapter lifecycle (coordinator publishes)
    AdapterStopping { prefix: String },
    AdapterStopped { prefix: String },

    // Aggregator responses
    ZonesFlushed { prefix: String },
}
```

**Aggregator behavior:**
- On `AdapterStopping`: Remove all zones where `zone_id.starts_with(prefix)`
- Publish `ZonesFlushed` to acknowledge

**Adapter behavior:**
- `stop()` is async, returns `Result<()>`
- Clean up connections, cancel tasks
- Coordinator waits for `stop()` to complete before publishing `AdapterStopped`
