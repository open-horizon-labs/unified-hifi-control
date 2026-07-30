//! The v1 adaptive-control producer contract (issue #323).
//!
//! A **producer** is anything that owns controllable state — an HQPlayer engine, a
//! room-correction matrix, a future Home Assistant domain. A **producer document** is
//! an immutable, versioned snapshot describing that producer's identity, controls,
//! current truths, constraints, staged intent and command outcomes, in terms no
//! consumer needs backend knowledge to interpret.
//!
//! ## What this module is, and is not
//!
//! This module is **only the contract**: types, vocabularies and the compatibility
//! policy. It deliberately contains no transport, no adapter access and no state
//! ownership.
//!
//! * The aggregator remains the single owner of authoritative state. Producers publish
//!   snapshots; the aggregator serves them (issue #324).
//! * Adapters map their native protocol onto these types (issue #325). Nothing here
//!   knows what XML, a socket or a list index is.
//! * Presentation, layout and matcher selection are consumer concerns (issue #326).
//!   A matcher may choose *which* controls a surface shows; it can never override an
//!   availability or constraint decision made here.
//! * Human-facing prose (labels, help text, filter descriptions) is **not** stored
//!   here. Controls carry catalog *keys*; the catalog and its provenance/licensing are
//!   governed by issue #343.
//!
//! To keep that boundary honest and to keep the eventual extraction into a standalone
//! crate mechanical, this module depends on nothing but `serde`, `serde_json` and
//! `std`. `tests/adaptive_dependency_lint.rs` enforces it.
//!
//! ## Five truths, not one value
//!
//! The central design decision is that a control does not have "a value". It has up to
//! five simultaneously legitimate values, each with its own provenance and freshness:
//!
//! | Lane | Meaning |
//! |------|---------|
//! | `observed` | what the running engine reports now |
//! | `persisted` | what the saved configuration says it will be next start |
//! | `desired` | staged intent not yet applied |
//! | `held` | intent for a chain/profile that is not currently loaded |
//! | `effective` | what an editor should display, projected from the others |
//!
//! Divergence between them is first-class data ([`value::Divergence`]), never silently
//! reconciled. See `docs/architecture/adaptive-producer-contract-v1.md`.

pub mod change_set;
pub mod command;
pub mod constraint;
pub mod control;
pub mod document;
pub mod value;
pub mod version;
pub mod vocab;

pub use change_set::{
    ActorClass, ChangeSet, ChangeSetEntry, ChangeSetId, ChangeSetState, ConflictPolicy,
    DetachedChangeSet, DraftMode, DraftPolicy, EntryValidity, EpochPolicy, Origin, Retention,
    RetireOutcome, Staleness, Surface,
};
pub use command::{
    CommandOutcome, OperationId, OperationRecord, OutcomeTransition, PlanStep, RecoveryState,
    TransitionRejected, WriteAttempt,
};
pub use constraint::{
    Constraint, ConstraintEffect, Evaluation, Expr, ExprLimit, Permission, ValueLookup, Visibility,
    MAX_EXPR_DEPTH, MAX_EXPR_NODES,
};
pub use control::{
    ApplyEffect, ApplyLane, ApplySemantics, Availability, AvailabilityState, Choice, ChoiceSet,
    Control, ControlGroup, ControlId, ControlKind, Disruption, NumericRange, Reason, ReasonCode,
    ReasonScope, RiskClass, Verification,
};
pub use document::{
    admit_document, admit_document_str, DocumentRevisions, EffectiveView, Extensions,
    IntentIncoherence, LaneHealth, LaneState, ProducerDocument, ProducerEpoch, ProducerIdentity,
    ProducerTarget, Revision, RevisionRef, TargetRole, TransportLane,
};
pub use value::{
    Authority, ControlValue, Divergence, DivergenceKind, Freshness, Grounding, LaneValue,
    Provenance, SourceClass, Timestamp, ValueLane,
};
pub use version::{
    Compatibility, Refusal, SchemaVersion, StoredCompatibility, CONSUMER_SCHEMA_VERSION,
    STORED_ARTIFACT_SCHEMA_VERSION,
};
