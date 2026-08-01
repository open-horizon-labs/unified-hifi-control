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
use crate::adaptive::{DocumentRevisions, ProducerEpoch};

/// Aggregator egress notifications on the internal bus.
///
/// This enum deliberately has no producer ingress variants. Adapters can only publish through
/// [`super::AdaptiveHandle`], whose bounded command channel cannot silently lag or drop state.
#[derive(Debug, Clone)]
pub enum AdaptiveEvent {
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
    /// A committed producer retirement changed the read-only view.
    ProducerRetired {
        /// Stable producer id whose targets are no longer visible.
        producer_id: String,
        /// Producer epoch retired through.
        retired_through: ProducerEpoch,
        /// Number of visible target snapshots removed.
        removed: usize,
    },
}

/// Broadcast egress for admitted/refused notifications.
///
/// Lag here can only make a consumer re-read the current [`super::AdaptiveView`]; it cannot
/// lose producer state because this channel is never used for ingress.
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
    pub(super) fn publish(&self, event: AdaptiveEvent) {
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
        // Egress is only a re-read hint, but a generous buffer avoids needless lag recovery
        // when several consumers briefly fall behind a burst of producer polls.
        Self::new(1024)
    }
}

/// Shared handle to the internal adaptive bus.
pub type SharedAdaptiveBus = Arc<AdaptiveBus>;

/// Create a shared internal adaptive bus.
pub(super) fn create_adaptive_bus() -> SharedAdaptiveBus {
    Arc::new(AdaptiveBus::default())
}
