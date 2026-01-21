//! App-level SSE (Server-Sent Events) context.
//!
//! Provides a single EventSource connection shared across all components,
//! with typed event signals for reactive updates.

use dioxus::prelude::*;
use serde::Deserialize;

#[cfg(target_arch = "wasm32")]
use std::cell::RefCell;
#[cfg(target_arch = "wasm32")]
use std::rc::Rc;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

/// Payload for zone-related events
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct ZonePayload {
    pub zone_id: String,
}

/// Payload for LMS player events
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct LmsPlayerPayload {
    pub player_id: String,
}

/// Payload for volume changed events
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct VolumePayload {
    pub output_id: String,
    pub value: f32,
    pub is_muted: bool,
}

/// SSE event types from the server
/// Server sends: {"type":"EventName","payload":{...}}
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "type")]
pub enum SseEvent {
    // Roon events
    RoonConnected,
    RoonDisconnected,
    ZoneUpdated {
        payload: ZonePayload,
    },
    ZoneRemoved {
        payload: ZonePayload,
    },
    NowPlayingChanged {
        payload: ZonePayload,
    },
    VolumeChanged {
        payload: VolumePayload,
    },
    SeekPositionChanged {
        payload: ZonePayload,
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
        payload: LmsPlayerPayload,
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

impl SseEvent {
    /// Extract zone_id from zone-related events
    pub fn zone_id(&self) -> Option<&str> {
        match self {
            SseEvent::ZoneUpdated { payload } => Some(&payload.zone_id),
            SseEvent::ZoneRemoved { payload } => Some(&payload.zone_id),
            SseEvent::NowPlayingChanged { payload } => Some(&payload.zone_id),
            SseEvent::SeekPositionChanged { payload } => Some(&payload.zone_id),
            _ => None,
        }
    }
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
    /// Check if we should refresh zones/now_playing based on event types
    pub fn should_refresh_zones(&self) -> bool {
        matches!(
            self.last_event.read().as_ref(),
            Some(
                SseEvent::ZoneUpdated { .. }
                    | SseEvent::ZoneRemoved { .. }
                    | SseEvent::NowPlayingChanged { .. }
                    | SseEvent::SeekPositionChanged { .. }
                    | SseEvent::VolumeChanged { .. }
                    | SseEvent::RoonConnected
                    | SseEvent::RoonDisconnected
                    | SseEvent::LmsConnected
                    | SseEvent::LmsDisconnected
                    | SseEvent::LmsPlayerStateChanged { .. }
            )
        )
    }

    pub fn should_refresh_roon(&self) -> bool {
        matches!(
            self.last_event.read().as_ref(),
            Some(SseEvent::RoonConnected | SseEvent::RoonDisconnected)
        )
    }

    pub fn should_refresh_hqp(&self) -> bool {
        matches!(
            self.last_event.read().as_ref(),
            Some(
                SseEvent::HqpConnected
                    | SseEvent::HqpDisconnected
                    | SseEvent::HqpStateChanged
                    | SseEvent::HqpPipelineChanged
            )
        )
    }

    pub fn should_refresh_lms(&self) -> bool {
        matches!(
            self.last_event.read().as_ref(),
            Some(
                SseEvent::LmsConnected
                    | SseEvent::LmsDisconnected
                    | SseEvent::LmsPlayerStateChanged { .. }
            )
        )
    }

    pub fn should_refresh_discovery(&self) -> bool {
        matches!(
            self.last_event.read().as_ref(),
            Some(
                SseEvent::RoonConnected
                    | SseEvent::RoonDisconnected
                    | SseEvent::OpenHomeDeviceFound
                    | SseEvent::OpenHomeDeviceLost
                    | SseEvent::UpnpRendererFound
                    | SseEvent::UpnpRendererLost
            )
        )
    }

    pub fn should_refresh_knobs(&self) -> bool {
        matches!(
            self.last_event.read().as_ref(),
            Some(
                SseEvent::ZoneUpdated { .. }
                    | SseEvent::ZoneRemoved { .. }
                    | SseEvent::RoonConnected
                    | SseEvent::RoonDisconnected
                    | SseEvent::LmsConnected
                    | SseEvent::LmsDisconnected
            )
        )
    }
}

/// RAII guard to close EventSource on drop
#[cfg(target_arch = "wasm32")]
struct EventSourceGuard {
    es: web_sys::EventSource,
    // Store closures so they're dropped with the guard (prevents leaks)
    _onopen: Closure<dyn FnMut(web_sys::Event)>,
    _onmessage: Closure<dyn FnMut(web_sys::MessageEvent)>,
    _onerror: Closure<dyn FnMut(web_sys::Event)>,
}

#[cfg(target_arch = "wasm32")]
impl Drop for EventSourceGuard {
    fn drop(&mut self) {
        web_sys::console::log_1(&"SSE: Closing EventSource connection".into());
        self.es.close();
    }
}

/// Initialize SSE context provider - call once at app root
pub fn use_sse_provider() {
    let last_event = use_signal(|| None::<SseEvent>);
    let connected = use_signal(|| false);
    let event_count = use_signal(|| 0u64);

    let ctx = SseContext {
        last_event,
        connected,
        event_count,
    };

    use_context_provider(|| ctx);

    // Client-side only: establish actual EventSource connection
    #[cfg(target_arch = "wasm32")]
    {
        // Use a hook to store the EventSource guard - it persists across renders
        // and closes the connection when the component unmounts
        let _guard: Rc<RefCell<Option<EventSourceGuard>>> =
            use_hook(|| Rc::new(RefCell::new(None)));

        let guard_clone = _guard.clone();
        use_effect(move || {
            use web_sys::EventSource;

            // Only create if we don't have one already
            if guard_clone.borrow().is_some() {
                return;
            }

            // Create EventSource connection to /events
            let es = match EventSource::new("/events") {
                Ok(es) => es,
                Err(e) => {
                    web_sys::console::error_1(
                        &format!("Failed to create EventSource: {:?}", e).into(),
                    );
                    return;
                }
            };

            web_sys::console::log_1(&"SSE: Creating EventSource connection to /events".into());

            // onopen handler
            let mut connected_clone = connected;
            let onopen = Closure::wrap(Box::new(move |_: web_sys::Event| {
                web_sys::console::log_1(&"SSE: Connection opened".into());
                connected_clone.set(true);
            }) as Box<dyn FnMut(_)>);
            es.set_onopen(Some(onopen.as_ref().unchecked_ref()));

            // onmessage handler
            let mut last_event_clone = last_event;
            let mut event_count_clone = event_count;
            let onmessage = Closure::wrap(Box::new(move |e: web_sys::MessageEvent| {
                if let Some(data) = e.data().as_string() {
                    web_sys::console::log_1(
                        &format!("SSE: Received: {}", &data[..data.len().min(100)]).into(),
                    );
                    // Parse the SSE event
                    match serde_json::from_str::<SseEvent>(&data) {
                        Ok(event) => {
                            web_sys::console::log_1(
                                &format!("SSE: Parsed event: {:?}", event).into(),
                            );
                            last_event_clone.set(Some(event));
                            event_count_clone.set(event_count_clone() + 1);
                        }
                        Err(e) => {
                            web_sys::console::warn_1(&format!("SSE: Parse error: {}", e).into());
                        }
                    }
                }
            }) as Box<dyn FnMut(_)>);
            es.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));

            // onerror handler
            let mut connected_err = connected;
            let onerror = Closure::wrap(Box::new(move |_: web_sys::Event| {
                web_sys::console::warn_1(&"SSE: Connection error".into());
                connected_err.set(false);
            }) as Box<dyn FnMut(_)>);
            es.set_onerror(Some(onerror.as_ref().unchecked_ref()));

            // Store guard - closures are now owned by the guard and will be dropped properly
            *guard_clone.borrow_mut() = Some(EventSourceGuard {
                es,
                _onopen: onopen,
                _onmessage: onmessage,
                _onerror: onerror,
            });
        });
    }
}

/// Get SSE context - use in any component that needs SSE events
pub fn use_sse() -> SseContext {
    use_context::<SseContext>()
}
