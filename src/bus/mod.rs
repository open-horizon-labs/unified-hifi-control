//! Event bus for inter-component communication
//!
//! Uses tokio::sync::broadcast for pub/sub pattern.
//! Events are typed and can carry payloads.

use std::sync::Arc;
use tokio::sync::broadcast;

pub mod events;
pub use events::*;

/// Event bus handle for publishing and subscribing
#[derive(Clone)]
pub struct EventBus {
    sender: broadcast::Sender<BusEvent>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(256)
    }
}

impl EventBus {
    /// Create a new event bus with specified capacity
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Publish an event to all subscribers
    pub fn publish(&self, event: BusEvent) {
        // Ignore send errors (no subscribers)
        let _ = self.sender.send(event);
    }

    /// Subscribe to all events
    pub fn subscribe(&self) -> broadcast::Receiver<BusEvent> {
        self.sender.subscribe()
    }

    /// Get the number of current subscribers
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

/// Shared event bus wrapped in Arc for thread-safe sharing
pub type SharedBus = Arc<EventBus>;

/// Create a new shared event bus
pub fn create_bus() -> SharedBus {
    Arc::new(EventBus::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_pubsub() {
        let bus = create_bus();
        let mut rx = bus.subscribe();

        bus.publish(BusEvent::RoonConnected {
            core_name: "Test Core".to_string(),
            version: "1.0".to_string(),
        });

        let event = rx.recv().await.unwrap();
        match event {
            BusEvent::RoonConnected { core_name, .. } => {
                assert_eq!(core_name, "Test Core");
            }
            _ => panic!("Wrong event type"),
        }
    }

    #[tokio::test]
    async fn test_multiple_subscribers() {
        let bus = create_bus();
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        bus.publish(BusEvent::RoonDisconnected);

        assert!(matches!(
            rx1.recv().await.unwrap(),
            BusEvent::RoonDisconnected
        ));
        assert!(matches!(
            rx2.recv().await.unwrap(),
            BusEvent::RoonDisconnected
        ));
    }
}
