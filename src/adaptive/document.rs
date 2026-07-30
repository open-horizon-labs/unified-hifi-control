//! The producer document envelope: identity, revisions, lane health and admission.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::change_set::{ChangeSet, DraftPolicy};
use super::command::OperationRecord;
use super::constraint::{Constraint, ValueLookup};
use super::control::{ControlGroup, ControlId, Reason};
use super::value::{is_false, Freshness, LaneValue, Timestamp, ValueLane};
use super::version::{Refusal, SchemaVersion, CONSUMER_SCHEMA_VERSION};
use super::vocab::semantic_enum;
use super::Control;

/// Unknown additive fields, preserved rather than merely ignored.
///
/// Ignoring an unknown field satisfies forward compatibility; preserving it also lets a
/// projection pass a v1.4 document through a v1.0 consumer without amputating it. The
/// cost is that `#[serde(deny_unknown_fields)]` is unavailable wherever this is
/// flattened, so strictness for *our own* canonical fixtures is enforced by
/// [`ProducerDocument::extension_key_paths`] instead.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Extensions(pub BTreeMap<String, serde_json::Value>);

impl Extensions {
    /// Whether any unknown fields were captured.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The captured field names.
    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.0.keys()
    }
}

/// A monotonic revision counter, scoped to a producer epoch.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Revision(pub u64);

impl Revision {
    /// The next revision.
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// A producer epoch.
///
/// Incremented whenever a producer re-establishes identity — a restart, a reconnect
/// that lost state, a configuration reload. Revisions are only comparable within one
/// epoch, which is why every [`RevisionRef`] carries the epoch it was taken in.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct ProducerEpoch(pub u64);

/// The two revisions a producer document carries.
///
/// Split because control identity and observed values change at very different rates.
/// A polling producer bumps `state` continuously; if that also bumped control identity,
/// every consumer would re-render its catalog on every poll and every open change set
/// would look stale.
///
/// The ordering rules, in full:
///
/// * Neither revision may regress within an epoch.
/// * A change set is validated against a [`RevisionRef`] holding *both* plus the epoch,
///   so a consumer never chooses which pair to compare.
/// * Comparison precedence is total: epoch change, then control plane, then state. See
///   [`super::Staleness::evaluate`].
/// * Anything other than an exact match requires revalidation before mutation. A
///   control-plane bump additionally means controls may have been added, removed or
///   redefined, so cached descriptors must be discarded, not just re-checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentRevisions {
    /// Bumped when control identity, choices, ranges, constraints or apply semantics
    /// change. Slow.
    pub control_plane: Revision,
    /// Bumped when observed values change. Fast.
    pub state: Revision,
}

impl DocumentRevisions {
    /// Construct from raw counters.
    pub fn new(control_plane: u64, state: u64) -> Self {
        Self {
            control_plane: Revision(control_plane),
            state: Revision(state),
        }
    }

    /// Whether either revision moved backwards relative to `previous`.
    ///
    /// A regressing document is an out-of-order delivery and must be dropped, not
    /// merged: applying it would resurrect removed controls.
    pub fn regresses_from(&self, previous: &DocumentRevisions) -> bool {
        self.control_plane < previous.control_plane || self.state < previous.state
    }

    /// Whether this document is strictly newer than `previous`.
    pub fn advances_over(&self, previous: &DocumentRevisions) -> bool {
        !self.regresses_from(previous) && self != previous
    }

    /// Pair these revisions with an epoch to produce a citable reference.
    pub fn at_epoch(&self, epoch: ProducerEpoch) -> RevisionRef {
        RevisionRef {
            epoch,
            control_plane: self.control_plane,
            state: self.state,
        }
    }
}

/// A citable producer position: epoch plus both revisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionRef {
    /// The epoch the revisions belong to.
    pub epoch: ProducerEpoch,
    /// Control-plane revision.
    pub control_plane: Revision,
    /// Observed-state revision.
    pub state: Revision,
}

semantic_enum! {
    /// What kind of target a producer document describes.
    pub enum TargetRole {
        /// A playback zone or renderer.
        Playback => "playback",
        /// A DSP or upsampling engine.
        DspEngine => "dsp_engine",
        /// Room correction, matrix or convolution.
        RoomCorrection => "room_correction",
        /// A whole-server or whole-instance scope with no single zone.
        Instance => "instance",
    }
}

semantic_enum! {
    /// A transport a producer reads and writes through.
    ///
    /// Health is per-lane so a degraded persistence or telemetry path does not blank a
    /// producer whose native control lane is perfectly usable.
    pub enum TransportLane {
        /// The producer's native control protocol.
        Native => "native",
        /// The persistence path: config file, HTTP settings interface.
        Persistent => "persistent",
        /// Optional metrics or telemetry.
        Telemetry => "telemetry",
    }
}

semantic_enum! {
    /// The state of one transport lane.
    pub enum LaneState {
        /// Working.
        Connected => "connected",
        /// Reachable but unreliable or partially failing; last-good data is retained.
        Degraded => "degraded",
        /// Not reachable.
        Disconnected => "disconnected",
        /// Never set up. Different from disconnected: nothing is broken.
        Unconfigured => "unconfigured",
    }
}

/// Health of one transport lane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaneHealth {
    /// Which lane.
    pub lane: TransportLane,
    /// Its state.
    pub state: LaneState,
    /// When the lane last succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success: Option<Timestamp>,
    /// The last error, as reason data rather than a message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<Reason>,
    /// Freshness of whatever this lane last provided.
    #[serde(default, skip_serializing_if = "Freshness::is_default")]
    pub freshness: Freshness,
    /// Additive fields from a newer minor version, preserved for pass-through.
    #[serde(flatten, default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

impl LaneHealth {
    /// Whether values sourced from this lane may still be shown, marked stale.
    ///
    /// True even when disconnected: last-known data with an explicit staleness marker
    /// is more useful than a blank panel, provided it is never presented as current.
    pub fn permits_last_known_display(&self) -> bool {
        !matches!(self.state, LaneState::Unconfigured)
    }
}

/// Stable producer identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProducerIdentity {
    /// Stable semantic producer id, e.g. `hqplayer:living-room`. Survives restarts and
    /// address changes; never derived from an IP address or a socket handle.
    pub producer_id: String,
    /// Producer family, e.g. `hqplayer`.
    pub producer_type: String,
    /// Operator-visible instance label, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_label: Option<String>,
    /// Backend product version, informational only. Not the schema version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product_version: Option<String>,
    /// The current epoch.
    pub epoch: ProducerEpoch,
    /// Additive fields from a newer minor version, preserved for pass-through.
    #[serde(flatten, default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

/// What the document's controls act on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProducerTarget {
    /// The role this producer plays for the target.
    pub role: TargetRole,
    /// The prefixed zone id, when the producer is bound to a zone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone_id: Option<String>,
    /// Additive fields from a newer minor version, preserved for pass-through.
    #[serde(flatten, default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

/// An immutable, versioned snapshot of a producer's controllable state.
///
/// One document is one atomic revision. A consumer must never combine fields from two
/// documents: a mode read at revision 41 and a filter enumeration read at revision 43
/// can describe a combination that never existed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProducerDocument {
    /// The contract version this document is stamped with.
    pub schema_version: SchemaVersion,
    /// Who is producing it.
    pub producer: ProducerIdentity,
    /// What it acts on.
    pub target: ProducerTarget,
    /// Where the producer is.
    pub revisions: DocumentRevisions,
    /// Per-transport health.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lanes: Vec<LaneHealth>,
    /// Presentation containers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<ControlGroup>,
    /// The controls.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub controls: Vec<Control>,
    /// Draft-dependent constraints.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<Constraint>,
    /// How this producer handles drafts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_policy: Option<DraftPolicy>,
    /// Open and recently closed change sets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub change_sets: Vec<ChangeSet>,
    /// Operations whose outcome is still relevant, including unresolved ones.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operations: Vec<OperationRecord>,
    /// Whether the whole document is last-known rather than current.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stale: bool,
    /// Additive fields from a newer minor version, preserved for pass-through.
    #[serde(flatten, default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

impl ProducerDocument {
    /// A control by id.
    pub fn control(&self, id: &ControlId) -> Option<&Control> {
        self.controls.iter().find(|control| &control.id == id)
    }

    /// Health of one transport lane.
    pub fn lane_health(&self, lane: &TransportLane) -> Option<&LaneHealth> {
        self.lanes.iter().find(|health| &health.lane == lane)
    }

    /// The producer's current position.
    pub fn position(&self) -> RevisionRef {
        self.revisions.at_epoch(self.producer.epoch)
    }

    /// Every unknown-field path captured anywhere in this document.
    ///
    /// Canonical examples must return an empty list: if one of our own fixtures has a
    /// misspelled field it lands in an [`Extensions`] map and would otherwise round-trip
    /// undetected, so the fixture would claim to demonstrate a behaviour it does not.
    /// Third-party documents may return entries; that is forward compatibility working.
    pub fn extension_key_paths(&self) -> Vec<String> {
        let mut paths = Vec::new();
        collect(&self.extensions, "", &mut paths);
        collect(&self.producer.extensions, "producer", &mut paths);
        collect(&self.target.extensions, "target", &mut paths);
        for control in &self.controls {
            let prefix = format!("controls[{}]", control.id);
            collect(&control.extensions, &prefix, &mut paths);
            for value in &control.values {
                collect(
                    &value.extensions,
                    &format!("{prefix}.values[{}]", value.lane),
                    &mut paths,
                );
            }
        }
        for health in &self.lanes {
            collect(
                &health.extensions,
                &format!("lanes[{}]", health.lane),
                &mut paths,
            );
        }
        for change_set in &self.change_sets {
            let prefix = format!("change_sets[{}]", change_set.id.as_str());
            collect(&change_set.extensions, &prefix, &mut paths);
            for entry in &change_set.entries {
                collect(
                    &entry.extensions,
                    &format!("{prefix}.entries[{}]", entry.control),
                    &mut paths,
                );
            }
        }
        for operation in &self.operations {
            collect(
                &operation.extensions,
                &format!("operations[{}]", operation.id.as_str()),
                &mut paths,
            );
        }
        paths
    }

    /// The editor-effective view of this document under an optional change set.
    ///
    /// Every surface evaluates constraints through this, so a web page, an MCP client
    /// and a device cannot disagree about the same draft.
    pub fn effective_view<'a>(&'a self, change_set: Option<&'a ChangeSet>) -> EffectiveView<'a> {
        EffectiveView {
            document: self,
            change_set,
        }
    }
}

fn collect(extensions: &Extensions, prefix: &str, out: &mut Vec<String>) {
    for key in extensions.keys() {
        if prefix.is_empty() {
            out.push(key.clone());
        } else {
            out.push(format!("{prefix}.{key}"));
        }
    }
}

/// A document plus an optional change set, resolving the effective lane.
pub struct EffectiveView<'a> {
    document: &'a ProducerDocument,
    change_set: Option<&'a ChangeSet>,
}

impl ValueLookup for EffectiveView<'_> {
    fn lookup(&self, control: &ControlId, lane: &ValueLane) -> Option<&LaneValue> {
        let descriptor = self.document.control(control)?;
        match lane {
            // The effective lane is a projection: a staged value if the draft has one,
            // otherwise the producer's own effective lane, otherwise observed. Resolving
            // it here — rather than in each surface — is what makes constraint
            // evaluation identical everywhere.
            ValueLane::Effective => {
                if let Some(staged) = self
                    .change_set
                    .and_then(|set| set.entries.iter().find(|entry| &entry.control == control))
                {
                    // Staged intent is exposed through the control's desired lane, which
                    // the producer publishes alongside the change set. Falling back to
                    // observed when the producer has not yet published a desired lane
                    // keeps evaluation total instead of silently unevaluatable.
                    let _ = staged;
                    return descriptor
                        .lane(&ValueLane::Desired)
                        .or_else(|| descriptor.lane(&ValueLane::Observed));
                }
                descriptor
                    .lane(&ValueLane::Effective)
                    .or_else(|| descriptor.lane(&ValueLane::Observed))
            }
            other => descriptor.lane(other),
        }
    }
}

/// Admit a producer document, applying the compatibility policy before parsing.
///
/// The version is inspected on the raw value first, deliberately. Deserializing and
/// *then* checking the version would mean a v2 document either fails with a confusing
/// schema error or, worse, partially succeeds — and a consumer cannot tell an
/// unsupported generation from corruption.
pub fn admit_document(raw: &serde_json::Value) -> Result<ProducerDocument, Refusal> {
    let stamped = raw.get("schema_version").ok_or(Refusal::MissingVersion)?;
    let text = stamped.as_str().ok_or_else(|| Refusal::UnparsableVersion {
        raw: stamped.to_string(),
    })?;
    let version = SchemaVersion::parse(text).ok_or_else(|| Refusal::UnparsableVersion {
        raw: text.to_string(),
    })?;

    if let Some(refusal) = version.compatibility_for(CONSUMER_SCHEMA_VERSION).refusal() {
        return Err(refusal.clone());
    }

    serde_json::from_value(raw.clone()).map_err(|error| Refusal::Malformed {
        detail: error.to_string(),
    })
}

/// Admit a producer document from JSON text.
pub fn admit_document_str(raw: &str) -> Result<ProducerDocument, Refusal> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|error| Refusal::Malformed {
            detail: error.to_string(),
        })?;
    admit_document(&value)
}
