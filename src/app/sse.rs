//! App-level SSE (Server-Sent Events) context.
//!
//! Provides a single EventSource connection shared across all components,
//! with typed event signals for reactive updates.

use dioxus::prelude::*;
use serde::Deserialize;

/// SSE event types from the server
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type")]
pub enum SseEvent {
    // Roon events
    RoonConnected,
    RoonDisconnected,
    ZoneUpdated {
        zone_id: String,
    },
    ZoneRemoved {
        zone_id: String,
    },
    NowPlayingChanged {
        zone_id: String,
    },
    VolumeChanged {
        zone_id: String,
    },

    // HQPlayer events
    HqpConnected,
    HqpDisconnected,
    HqpStateChanged,
    HqpPipelineChanged,

    // LMS events
    LmsConnected,
    LmsDisconnected,
    LmsPlayerStateChanged {
        player_id: String,
    },

    // OpenHome events
    OpenHomeDeviceFound,
    OpenHomeDeviceLost,

    // UPnP events
    UpnpRendererFound,
    UpnpRendererLost,

    // Catch-all for unknown events
    #[serde(other)]
    Unknown,
}

/// Global SSE state shared via context
#[derive(Clone, Copy)]
pub struct SseContext {
    /// Last received event (triggers re-renders)
    pub last_event: Signal<Option<SseEvent>>,
    /// Connection status
    pub connected: Signal<bool>,
    /// Event counter (increments on each event, useful for triggering refreshes)
    pub event_count: Signal<u64>,
}

impl SseContext {
    /// Check if we should refresh based on event types
    pub fn should_refresh_zones(&self) -> bool {
        matches!(
            self.last_event.read().as_ref(),
            Some(SseEvent::ZoneUpdated { .. })
                | Some(SseEvent::ZoneRemoved { .. })
                | Some(SseEvent::NowPlayingChanged { .. })
                | Some(SseEvent::VolumeChanged { .. })
                | Some(SseEvent::RoonConnected)
                | Some(SseEvent::RoonDisconnected)
                | Some(SseEvent::LmsConnected)
                | Some(SseEvent::LmsDisconnected)
        )
    }

    pub fn should_refresh_roon(&self) -> bool {
        matches!(
            self.last_event.read().as_ref(),
            Some(SseEvent::RoonConnected) | Some(SseEvent::RoonDisconnected)
        )
    }

    pub fn should_refresh_hqp(&self) -> bool {
        matches!(
            self.last_event.read().as_ref(),
            Some(SseEvent::HqpConnected)
                | Some(SseEvent::HqpDisconnected)
                | Some(SseEvent::HqpStateChanged)
                | Some(SseEvent::HqpPipelineChanged)
        )
    }

    pub fn should_refresh_lms(&self) -> bool {
        matches!(
            self.last_event.read().as_ref(),
            Some(SseEvent::LmsConnected)
                | Some(SseEvent::LmsDisconnected)
                | Some(SseEvent::LmsPlayerStateChanged { .. })
        )
    }

    pub fn should_refresh_discovery(&self) -> bool {
        matches!(
            self.last_event.read().as_ref(),
            Some(SseEvent::RoonConnected)
                | Some(SseEvent::RoonDisconnected)
                | Some(SseEvent::OpenHomeDeviceFound)
                | Some(SseEvent::OpenHomeDeviceLost)
                | Some(SseEvent::UpnpRendererFound)
                | Some(SseEvent::UpnpRendererLost)
        )
    }

    pub fn should_refresh_knobs(&self) -> bool {
        matches!(
            self.last_event.read().as_ref(),
            Some(SseEvent::ZoneUpdated { .. })
                | Some(SseEvent::ZoneRemoved { .. })
                | Some(SseEvent::RoonConnected)
                | Some(SseEvent::RoonDisconnected)
                | Some(SseEvent::LmsConnected)
                | Some(SseEvent::LmsDisconnected)
        )
    }
}

/// Initialize SSE context provider - call once at app root
pub fn use_sse_provider() {
    let last_event = use_signal(|| None::<SseEvent>);
    let mut connected = use_signal(|| false);
    let event_count = use_signal(|| 0u64);

    let ctx = SseContext {
        last_event,
        connected,
        event_count,
    };

    use_context_provider(|| ctx);

    // Client-side only: use polling for updates
    // Note: True SSE with wasm-bindgen closures requires special handling
    // For now, we rely on use_resource polling in components
    #[cfg(target_arch = "wasm32")]
    {
        // Mark as connected immediately on client (SSR context will be false)
        connected.set(true);
    }
}

/// Get SSE context - use in any component that needs SSE events
pub fn use_sse() -> SseContext {
    use_context::<SseContext>()
}
