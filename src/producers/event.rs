//! The internal adaptive bus.
//!
//! Deliberately **not** [`crate::bus::BusEvent`]. `src/api/mod.rs` serializes every
//! `BusEvent` verbatim into the `GET /events` SSE stream, which `docs/ARCHITECTURE.md`
//! documents as consumable by any HTTP client including ESP32 firmware. Adding a producer
//! document to that enum would be a response-schema change to a public endpoint *and*
//! publication of the v1 contract outside this repository — neither of which #324 is
//! scoped to do.
//!
//! [`AdaptiveEvent`] therefore derives no `Serialize`/`Deserialize`. The existing SSE
//! projection cannot carry it, and any future exposure has to be a new, deliberate code
//! path rather than an accident of adding a variant.

use std::sync::Arc;
use tokio::sync::broadcast;

use super::admission::{AdmissionRefusal, ProducerKey};
use crate::adaptive::{DocumentRevisions, ProducerDocument};

/// Producer lifecycle on the internal bus.
///
/// Ingress variants are written by adapters and read only by the aggregator. Egress
/// variants are written by the aggregator so an in-repository consumer can learn that
/// something changed without being handed an unadmitted document.
#[derive(Debug, Clone)]
pub enum AdaptiveEvent {
    /// An adapter published a producer document. Not yet admitted.
    ProducerPublished {
        /// The published document. Boxed: a document is large relative to the other
        /// variants and this enum travels through a broadcast channel.
        document: Box<ProducerDocument>,
    },
    /// A producer is gone. Distinct from a disconnected producer, which keeps publishing
    /// documents marked stale.
    ProducerRemoved {
        /// Stable producer id.
        ///
        /// **Known limit, recorded for #325.** This removes *every* target of that
        /// producer. A producer that publishes several documents — say an `instance`-scoped
        /// one and a `dsp_engine` one bound to a zone — cannot retire one target and keep
        /// another, because the event carries no [`ProducerKey`]. Not fixed here: no
        /// producer has more than one target yet, and adding a key to the event without a
        /// caller that needs it would be guessing at the shape. #325 will meet this first,
        /// and the fix is additive (a variant carrying a key).
        producer_id: String,
    },
    /// The aggregator admitted a document. Carries a pointer, never a payload: a consumer
    /// that wants the content must read the snapshot from the aggregator.
    SnapshotAdmitted {
        /// Which producer.
        key: ProducerKey,
        /// Where it now is.
        revisions: DocumentRevisions,
        /// How many change-set entries had to be demoted for coherence.
        repairs: usize,
    },
    /// The aggregator refused a document. The previous snapshot, if any, is retained.
    SnapshotRefused {
        /// Which producer.
        key: ProducerKey,
        /// Why.
        refusal: AdmissionRefusal,
    },
}

/// A broadcast channel for [`AdaptiveEvent`].
pub struct AdaptiveBus {
    sender: broadcast::Sender<AdaptiveEvent>,
}

impl AdaptiveBus {
    /// Create a bus with the given capacity.
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender }
    }

    /// Publish an event.
    ///
    /// The send result is inspected rather than discarded: `tests/ignored_send_lint.rs`
    /// allowlists `bus/mod.rs` only, and a silent drop here is precisely the failure a
    /// producer author would have no way to diagnose.
    pub fn publish(&self, event: AdaptiveEvent) {
        if self.sender.send(event).is_err() {
            tracing::trace!("adaptive bus has no subscribers; event dropped");
        }
    }

    /// Subscribe to producer lifecycle events.
    pub fn subscribe(&self) -> broadcast::Receiver<AdaptiveEvent> {
        self.sender.subscribe()
    }

    /// Current subscriber count.
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

impl Default for AdaptiveBus {
    fn default() -> Self {
        // Larger than the public bus's 256: a producer document is published on every poll
        // of every producer, and the aggregator is the only consumer that matters.
        Self::new(1024)
    }
}

/// Shared handle to the internal adaptive bus.
pub type SharedAdaptiveBus = Arc<AdaptiveBus>;

/// Create a shared internal adaptive bus.
pub fn create_adaptive_bus() -> SharedAdaptiveBus {
    Arc::new(AdaptiveBus::default())
}
