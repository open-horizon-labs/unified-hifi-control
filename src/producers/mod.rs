//! Adaptive producer publication: the internal bus and the aggregator that owns state.
//!
//! #323 defined the v1 producer document. This module is the first thing that ever holds
//! one, and it is deliberately the *only* thing that does.
//!
//! ## Why this is not on the public bus
//!
//! `src/api/mod.rs` serializes every [`crate::bus::BusEvent`] verbatim into the
//! `GET /events` SSE stream, and `docs/ARCHITECTURE.md` documents that stream as
//! consumable by any HTTP client, ESP32 firmware included. Putting a producer document in
//! that enum would be a response-schema change to a public endpoint *and* publication of
//! the v1 contract outside this repository. #324 is scoped to do neither, so producer
//! lifecycle rides a separate internal [`AdaptiveBus`] whose event type derives no serde.
//!
//! ## Why the aggregator is a gate rather than a store
//!
//! [`admit`] is the only way a [`crate::adaptive::ProducerDocument`] enters
//! [`ProducerAggregator`], and the aggregator is the only holder of an admitted one. That
//! makes "an incoherent document never reaches a consumer" a property of the store rather
//! than of a code path: there is no other object of that type in the process to obtain.
//!
//! Two policies split the work, and the split is the load-bearing decision:
//!
//! * **Envelope problems refuse the whole document** — unsupported major, unparsable
//!   constraint bounds, an unprefixed zone id, a regressed revision or epoch, an
//!   inconsistent lane value, an illegal recorded outcome transition. Refusal is safe
//!   *because the previous snapshot is retained*: it fails to advance a producer rather
//!   than blanking one.
//! * **Published-intent incoherence demotes, never refuses** — because the only repair
//!   that preserves `valid` would be inventing a `desired` lane, and that fabricates
//!   intent the user never staged. Demotion lowers validity and touches nothing else.

pub mod admission;
pub mod aggregator;
pub mod event;

pub use admission::{
    admit, Admission, AdmissionKind, AdmissionRefusal, AdmittedDocument, IntentRepair, LaneDefect,
    ProducerKey,
};
pub use aggregator::{LaneWitness, ProducerAggregator, ProducerPresence, ProducerSnapshot};
pub use event::{create_adaptive_bus, AdaptiveBus, AdaptiveEvent, SharedAdaptiveBus};
