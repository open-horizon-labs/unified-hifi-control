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
//! lifecycle rides a bounded actor command channel. Its optional admitted/refused notification
//! channel derives no serde and never carries ingress state.
//!
//! ## Why the aggregator is a gate rather than a store
//!
//! [`admit`] is the only way a [`crate::adaptive::ProducerDocument`] enters
//! [`ProducerAggregator`], so every document the aggregator serves has passed the gate, and
//! the aggregator is the only thing in this repository that holds producer state.
//!
//! Stated precisely, because the loose version is the kind of claim that survives into the
//! next issue's assumptions: this does **not** mean an unadmitted document cannot exist.
//! `ProducerDocument` is a public type in a shared module and
//! [`crate::adaptive::admit_document_str`] hands one to any caller. What is true is that a
//! consumer reading state *from the aggregator* cannot receive one — and since no surface
//! may reach an adapter (`docs/ARCHITECTURE.md`, `tests/architecture_lint.rs`), the
//! aggregator is the only place to read it from.
//!
//! Two policies split the work, and the split is the load-bearing decision:
//!
//! * **Envelope problems refuse the whole document** — unsupported major, unparsable
//!   constraint bounds, an unprefixed zone id, a regressed revision or epoch, an
//!   inconsistent lane value, an illegal recorded outcome transition. Refusal is safe
//!   *because the previous snapshot is retained*: it fails to advance a producer rather
//!   than blanking one.
//!
//!   That argument holds from the **second** document onward. On a producer's first
//!   document there is nothing to retain, and every refusal reason is reachable there — an
//!   unprefixed zone id most obviously — so a refusal means the producer never appears at
//!   all. This is accepted rather than mitigated: a producer whose very first document is
//!   malformed has nothing correct to show, and the refusal is retained and queryable via
//!   [`ProducerAggregator::refusals`] precisely so that case is diagnosable.
//! * **Published-intent incoherence demotes, never refuses** — because the only repair
//!   that preserves `valid` would be inventing a `desired` lane, and that fabricates
//!   intent the user never staged. Demotion lowers validity and touches nothing else.

pub mod admission;
mod aggregator;
pub mod event;
pub mod hqplayer;
pub mod run;
pub mod runtime;

pub use admission::{
    admit, Admission, AdmissionKind, AdmissionRefusal, AdmittedDocument, IntentRepair, LaneDefect,
    ProducerKey, CONTROL_REMOVED_TEXT_KEY,
};
/// Debug-profile compatibility seam for the direct integration-test harness.
///
/// This is intentionally broader than `cfg(test)`: integration tests compile the library as a
/// dependency and therefore cannot see library `cfg(test)` items. Ordinary debug builds expose
/// the seam; release builds expose adaptive mutation only through the actor. Do not treat a
/// debug profile as an actor-boundary proof.
#[cfg(debug_assertions)]
#[doc(hidden)]
pub use aggregator::ProducerAggregator;
pub use aggregator::{LaneWitness, ProducerPresence, ProducerSnapshot};
pub use event::AdaptiveEvent;
pub use run::{AdapterRun, AdapterRunId, AdapterRuns, PublicationOrigin};
pub use runtime::{
    AdaptiveHandle, AdaptivePublishError, AdaptiveRuntime, AdaptiveRuntimeClosed, AdaptiveView,
    ProducerActor, RetirementOutcome,
};
