//! Values, lanes, grounding, provenance and divergence.
//!
//! A control does not have "a value". It has up to five simultaneously legitimate
//! values, each with its own authority and freshness. Collapsing them into one scalar
//! is the failure this module exists to prevent: a level that is remembered while a
//! feature is disabled, a staged edit that has not been applied, and a value belonging
//! to a chain that is not loaded are three different truths, and a projection that
//! shows one as "the" value will eventually delete another.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use super::control::ReasonCode;
use super::document::{Extensions, Revision};
use super::vocab::semantic_enum;

semantic_enum! {
    /// Which truth a [`LaneValue`] carries.
    pub enum ValueLane {
        /// What the running engine reports right now. The only lane that is evidence
        /// of audible reality.
        Observed => "observed",
        /// What the saved configuration says the value will be at next start. May
        /// legitimately differ from `observed` without either being wrong.
        Persisted => "persisted",
        /// Staged intent that has been accepted into a change set but not applied.
        Desired => "desired",
        /// Intent for a chain, profile or feature that is not currently loaded or
        /// enabled. Held values are retained, not applied, and never silently dropped.
        Held => "held",
        /// The value an editor should display: a projection over the other lanes for a
        /// given change set. Never authoritative on its own.
        Effective => "effective",
    }
}

semantic_enum! {
    /// Who is asserting a value or an enumeration.
    ///
    /// Runtime observation outranks everything else. Static and catalog sources may
    /// enrich a value's presentation but must never override its identity, ordering or
    /// availability.
    pub enum Authority {
        /// The live engine, observed over its own protocol.
        Runtime => "runtime",
        /// The producer's persisted configuration.
        PersistedConfig => "persisted_config",
        /// A staged change set.
        StagedIntent => "staged_intent",
        /// Intent retained for an inactive chain or a disabled feature.
        HeldIntent => "held_intent",
        /// A consumer-side projection over other lanes.
        EditorProjection => "editor_projection",
        /// An explicitly scoped fallback used only when the runtime is silent.
        StaticFallback => "static_fallback",
    }
}

semantic_enum! {
    /// The mechanism a value or enumeration was obtained through.
    ///
    /// Recorded separately from [`Authority`] because the same authority can be
    /// reached by different mechanisms with different reliability.
    pub enum SourceClass {
        /// Read from the producer's native control protocol.
        LiveProtocol => "live_protocol",
        /// Read from a configuration file or persisted document.
        ConfigFile => "config_file",
        /// Read from the producer's own web/HTTP interface.
        WebInterface => "web_interface",
        /// Supplied by the semantic catalog (see issue #343 for provenance rules).
        Catalog => "catalog",
        /// Supplied by a consumer as intent, not as observation.
        Consumer => "consumer",
    }
}

semantic_enum! {
    /// Whether a lane has a value at all.
    ///
    /// This exists because `null` is ambiguous. "The producer has no reading for this"
    /// and "the producer reports the empty/default selection" are different facts, and
    /// a projection that conflates them will display a configured default as missing —
    /// or, worse, write a default over a value it failed to read.
    pub enum Grounding {
        /// A value is present. It may be [`ControlValue::Empty`], which is a real,
        /// grounded value meaning "the empty or default identity".
        Grounded => "grounded",
        /// No value is present, with a machine-readable reason.
        Ungrounded => "ungrounded",
    }
}

/// An RFC 3339 UTC timestamp.
///
/// Held as a string rather than a date type so this module needs no dependency beyond
/// `serde`, keeping it usable from WASM consumers and cheap to extract into its own
/// crate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(String);

impl Timestamp {
    /// Wrap an RFC 3339 string.
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// The underlying RFC 3339 string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// How current a value or lane is.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Freshness {
    /// When the value was observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<Timestamp>,
    /// Age in milliseconds at the time the document was produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_ms: Option<u64>,
    /// Whether the producer considers this value stale. Explicit rather than derived,
    /// because only the producer knows its own polling contract.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stale: bool,
}

impl Freshness {
    /// Whether every field is at its default, so fixtures may omit the object.
    pub fn is_default(&self) -> bool {
        self.observed_at.is_none() && self.age_ms.is_none() && !self.stale
    }
}

pub(crate) fn is_false(value: &bool) -> bool {
    !*value
}

/// Who said this, how, and as of which revision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    /// Whose truth this is.
    pub authority: Authority,
    /// How it was obtained.
    pub source: SourceClass,
    /// Catalog entry key, when a catalog contributed presentation data. The catalog's
    /// own provenance and licensing are governed by issue #343; no prose is stored here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_ref: Option<String>,
    /// The producer revision this assertion was made against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<Revision>,
}

impl Provenance {
    /// Provenance for a value read live from the engine.
    pub fn runtime() -> Self {
        Self {
            authority: Authority::Runtime,
            source: SourceClass::LiveProtocol,
            catalog_ref: None,
            revision: None,
        }
    }
}

/// A typed control value.
///
/// Wire form is `{"type": <kind>, "value": <payload>}`, with `value` omitted for
/// [`ControlValue::Empty`]. An unknown `type` degrades to
/// [`ControlValue::Unrecognized`] carrying the raw payload, so a v1.0 consumer can
/// still round-trip and display a value kind added in v1.4 rather than failing the
/// whole document.
#[derive(Debug, Clone, PartialEq)]
pub enum ControlValue {
    /// The empty or default identity — a real value, not an absence.
    ///
    /// HQPlayer's `[source]` mode and an unnamed default matrix profile are both
    /// grounded to `Empty`. Neither is `null`.
    Empty,
    /// A boolean.
    Bool(bool),
    /// An integer.
    Integer(i64),
    /// A real number, e.g. a dB level.
    Decimal(f64),
    /// Free text.
    Text(String),
    /// A stable semantic choice identifier from a [`super::ChoiceSet`]. Never a
    /// backend list index.
    Choice(String),
    /// An atomic composite value, e.g. a matrix cell set or a coupled
    /// enabled/level pair. Keys are ordered so the serialization is canonical and can
    /// be compared or hashed.
    Composite(BTreeMap<String, ControlValue>),
    /// An ordered list of values.
    List(Vec<ControlValue>),
    /// A value kind this build does not recognize.
    Unrecognized {
        /// The unrecognized `type` spelling.
        kind: String,
        /// The raw payload, retained so the value round-trips unchanged.
        raw: serde_json::Value,
    },
}

impl ControlValue {
    /// A choice value from a stable semantic id.
    pub fn choice(id: impl Into<String>) -> Self {
        Self::Choice(id.into())
    }

    /// Text value.
    pub fn text(value: impl Into<String>) -> Self {
        Self::Text(value.into())
    }

    /// The wire `type` discriminator.
    pub fn type_name(&self) -> &str {
        match self {
            Self::Empty => "empty",
            Self::Bool(_) => "bool",
            Self::Integer(_) => "integer",
            Self::Decimal(_) => "decimal",
            Self::Text(_) => "text",
            Self::Choice(_) => "choice",
            Self::Composite(_) => "composite",
            Self::List(_) => "list",
            Self::Unrecognized { kind, .. } => kind.as_str(),
        }
    }

    /// Whether this build understands this value kind.
    pub fn is_recognized(&self) -> bool {
        !matches!(self, Self::Unrecognized { .. })
    }

    /// Whether this is the grounded empty/default identity.
    ///
    /// Distinct from ungrounded: see [`Grounding`].
    pub fn is_empty_identity(&self) -> bool {
        matches!(self, Self::Empty)
    }

    /// Numeric projection, for range constraint evaluation.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Integer(v) => Some(*v as f64),
            Self::Decimal(v) => Some(*v),
            _ => None,
        }
    }
}

impl Serialize for ControlValue {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("type", self.type_name())?;
        match self {
            Self::Empty => {}
            Self::Bool(v) => map.serialize_entry("value", v)?,
            Self::Integer(v) => map.serialize_entry("value", v)?,
            Self::Decimal(v) => map.serialize_entry("value", v)?,
            Self::Text(v) | Self::Choice(v) => map.serialize_entry("value", v)?,
            Self::Composite(v) => map.serialize_entry("value", v)?,
            Self::List(v) => map.serialize_entry("value", v)?,
            Self::Unrecognized { raw, .. } => {
                if !raw.is_null() {
                    map.serialize_entry("value", raw)?;
                }
            }
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for ControlValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let raw = serde_json::Value::deserialize(deserializer)?;
        let object = raw
            .as_object()
            .ok_or_else(|| D::Error::custom("control value must be an object"))?;
        let kind = object
            .get("type")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| D::Error::custom("control value requires a string \"type\""))?;
        let payload = object.get("value");

        let require = |label: &str| -> Result<&serde_json::Value, D::Error> {
            payload.ok_or_else(|| {
                D::Error::custom(format!("{label} control value requires \"value\""))
            })
        };

        match kind {
            "empty" => Ok(Self::Empty),
            "bool" => require("bool")?
                .as_bool()
                .map(Self::Bool)
                .ok_or_else(|| D::Error::custom("bool control value requires a boolean")),
            "integer" => require("integer")?
                .as_i64()
                .map(Self::Integer)
                .ok_or_else(|| D::Error::custom("integer control value requires an integer")),
            "decimal" => require("decimal")?
                .as_f64()
                .map(Self::Decimal)
                .ok_or_else(|| D::Error::custom("decimal control value requires a number")),
            "text" => require("text")?
                .as_str()
                .map(|v| Self::Text(v.to_string()))
                .ok_or_else(|| D::Error::custom("text control value requires a string")),
            "choice" => require("choice")?
                .as_str()
                .map(|v| Self::Choice(v.to_string()))
                .ok_or_else(|| D::Error::custom("choice control value requires a string id")),
            "composite" => {
                let members = require("composite")?.as_object().ok_or_else(|| {
                    D::Error::custom("composite control value requires an object")
                })?;
                let mut out = BTreeMap::new();
                for (key, member) in members {
                    let parsed = serde_json::from_value(member.clone()).map_err(|error| {
                        D::Error::custom(format!("composite member {key:?}: {error}"))
                    })?;
                    out.insert(key.clone(), parsed);
                }
                Ok(Self::Composite(out))
            }
            "list" => {
                let members = require("list")?
                    .as_array()
                    .ok_or_else(|| D::Error::custom("list control value requires an array"))?;
                let mut out = Vec::with_capacity(members.len());
                for member in members {
                    out.push(
                        serde_json::from_value(member.clone())
                            .map_err(|error| D::Error::custom(format!("list member: {error}")))?,
                    );
                }
                Ok(Self::List(out))
            }
            other => Ok(Self::Unrecognized {
                kind: other.to_string(),
                raw: payload.cloned().unwrap_or(serde_json::Value::Null),
            }),
        }
    }
}

/// One lane's value for one control, with its authority and freshness.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaneValue {
    /// Which truth this is.
    pub lane: ValueLane,
    /// Whether a value is present at all.
    pub grounding: Grounding,
    /// The value. Present if and only if `grounding` is
    /// [`Grounding::Grounded`]; see [`LaneValue::is_consistent`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<ControlValue>,
    /// Why no value is present. Required when ungrounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ungrounded_reason: Option<ReasonCode>,
    /// Who asserted it and how.
    pub provenance: Provenance,
    /// How current it is.
    #[serde(default, skip_serializing_if = "Freshness::is_default")]
    pub freshness: Freshness,
    /// Additive fields from a newer minor version, preserved for pass-through.
    #[serde(flatten, default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

impl LaneValue {
    /// A grounded lane value.
    pub fn grounded(lane: ValueLane, value: ControlValue, provenance: Provenance) -> Self {
        Self {
            lane,
            grounding: Grounding::Grounded,
            value: Some(value),
            ungrounded_reason: None,
            provenance,
            freshness: Freshness::default(),
            extensions: Extensions::default(),
        }
    }

    /// An ungrounded lane value with a machine-readable reason.
    pub fn ungrounded(lane: ValueLane, reason: ReasonCode, provenance: Provenance) -> Self {
        Self {
            lane,
            grounding: Grounding::Ungrounded,
            value: None,
            ungrounded_reason: Some(reason),
            provenance,
            freshness: Freshness::default(),
            extensions: Extensions::default(),
        }
    }

    /// Whether grounding and payload agree.
    ///
    /// Grounded requires a value; ungrounded requires no value and a reason. A
    /// producer that violates this is asserting both "I have no reading" and a
    /// reading, which no consumer can resolve.
    pub fn is_consistent(&self) -> bool {
        match self.grounding {
            Grounding::Grounded => self.value.is_some(),
            Grounding::Ungrounded => self.value.is_none() && self.ungrounded_reason.is_some(),
            // An unrecognized grounding must not be treated as consistent, because a
            // consumer cannot know whether the payload is authoritative.
            Grounding::Unrecognized(_) => false,
        }
    }

    /// The value, if grounded.
    pub fn grounded_value(&self) -> Option<&ControlValue> {
        match self.grounding {
            Grounding::Grounded => self.value.as_ref(),
            _ => None,
        }
    }
}

semantic_enum! {
    /// Which pair of lanes disagree.
    pub enum DivergenceKind {
        /// The engine is running something the saved configuration does not describe.
        ObservedVsPersisted => "observed_vs_persisted",
        /// Staged intent has not been applied to the engine.
        ObservedVsDesired => "observed_vs_desired",
        /// Staged intent has not been persisted.
        PersistedVsDesired => "persisted_vs_desired",
        /// Held intent cannot be applied because its chain or feature is not loaded.
        HeldUnreachable => "held_unreachable",
    }
}

/// A recorded disagreement between two lanes.
///
/// Divergence is data, not an error. A consumer must be able to show that the engine
/// is running one thing and the configuration says another, rather than the producer
/// picking a winner and hiding the conflict.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Divergence {
    /// Which pair disagrees.
    pub kind: DivergenceKind,
    /// The lanes compared, in the order named by `kind`.
    pub lanes: [ValueLane; 2],
    /// When the divergence was detected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detected_at: Option<Timestamp>,
    /// Catalog key for an explanation, if one exists. Never prose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_text_key: Option<String>,
}
