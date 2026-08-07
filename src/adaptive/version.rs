//! Schema versioning and the v1 compatibility policy.
//!
//! Two independent version axes exist and must not be conflated:
//!
//! 1. **Document schema version** — stamped on every [`super::ProducerDocument`] as
//!    `major.minor`. Governs what a consumer may do with a document it receives.
//! 2. **Stored-artifact schema version** — stamped on persisted semantic artifacts
//!    (drafts, bindings, layouts). Deliberately independent of the application's
//!    release version, so upgrading the binary does not implicitly migrate data.
//!
//! The policy in one paragraph: same major is compatible in both directions; a newer
//! minor is accepted because v1 evolution is additive and unknown fields are
//! ignorable; any other major is refused outright rather than partially rendered.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

use super::constraint::ExprLimit;

/// The document schema version this build of the contract implements.
pub const CONSUMER_SCHEMA_VERSION: SchemaVersion = SchemaVersion { major: 1, minor: 0 };

/// The stored-artifact schema version this build reads and writes.
///
/// Separate constant from [`CONSUMER_SCHEMA_VERSION`] on purpose: the wire document
/// and the on-disk artifacts evolve on their own schedules.
pub const STORED_ARTIFACT_SCHEMA_VERSION: SchemaVersion = SchemaVersion { major: 1, minor: 0 };

/// A `major.minor` schema version.
///
/// Serialized as a dotted string (`"1.0"`), never as an object, so the value stays
/// readable in logs, fixtures and non-Rust consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SchemaVersion {
    /// Incompatible generation. A different major is refused.
    pub major: u16,
    /// Additive revision within a major. A newer minor is accepted.
    pub minor: u16,
}

impl SchemaVersion {
    /// Construct a version from its components.
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }

    /// Parse a `major.minor` string.
    ///
    /// Trailing dot-separated components are tolerated (`"1.4.2"` parses as major 1,
    /// minor 4) because a producer stamping a patch component is still declaring the
    /// same additive generation. Anything that is not two leading numeric components
    /// returns `None` so the caller can refuse it explicitly.
    pub fn parse(raw: &str) -> Option<Self> {
        let mut parts = raw.split('.');
        let major = parse_component(parts.next()?)?;
        let minor = parse_component(parts.next()?)?;
        // Remaining components are informational, but they must still be numeric so
        // that "1.0.beta" is refused rather than silently accepted.
        for extra in parts {
            parse_component(extra)?;
        }
        Some(Self { major, minor })
    }

    /// Decide what `consumer` may do with a document stamped `self`.
    pub fn compatibility_for(self, consumer: SchemaVersion) -> Compatibility {
        if self.major != consumer.major {
            return Compatibility::Refused(Refusal::UnsupportedMajor {
                document: self.major,
                consumer: consumer.major,
            });
        }
        if self.minor > consumer.minor {
            Compatibility::SupportedWithUnknownAdditions
        } else {
            Compatibility::Supported
        }
    }
}

fn parse_component(raw: &str) -> Option<u16> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

impl fmt::Display for SchemaVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

impl Serialize for SchemaVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for SchemaVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).ok_or_else(|| {
            serde::de::Error::custom(format!("not a major.minor schema version: {raw:?}"))
        })
    }
}

/// What a consumer may do with a document it has received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compatibility {
    /// Same major, same-or-older minor. Every field is understood.
    Supported,
    /// Same major, newer minor. Render it; unknown fields are ignorable and are
    /// preserved in [`super::Extensions`] rather than dropped.
    SupportedWithUnknownAdditions,
    /// Not usable. The consumer must surface the refusal, not a partial render.
    Refused(Refusal),
}

impl Compatibility {
    /// Whether the document may be projected into a surface at all.
    ///
    /// A refused document must never be partially rendered: a control plane built
    /// from half-understood data is worse than an explicit "unsupported" message,
    /// because the user cannot tell which controls are missing.
    pub fn is_usable(&self) -> bool {
        matches!(self, Self::Supported | Self::SupportedWithUnknownAdditions)
    }

    /// The refusal, if this decision was a refusal.
    pub fn refusal(&self) -> Option<&Refusal> {
        match self {
            Self::Refused(reason) => Some(reason),
            _ => None,
        }
    }
}

/// Machine-readable reason a document or artifact was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum Refusal {
    /// The document's major generation is not the one this consumer implements.
    UnsupportedMajor {
        /// Major version found on the document or artifact.
        document: u16,
        /// Major version this consumer implements.
        consumer: u16,
    },
    /// A `schema_version` was present but is not a `major.minor` string.
    UnparsableVersion {
        /// The raw value, retained for diagnostics.
        raw: String,
    },
    /// No `schema_version` field was present. An unstamped *wire* document is
    /// refused; only *stored* artifacts have an adoption policy.
    MissingVersion,
    /// The version was acceptable but the body did not match the schema.
    Malformed {
        /// Deserialization detail, for logs rather than for users.
        detail: String,
    },
    /// A constraint expression exceeds the published depth or node bounds.
    ///
    /// Refused at admission rather than at evaluation. The bounds exist so evaluation
    /// cost is predictable on the smallest consumer, and an admitted document is
    /// evaluated by every surface that renders it — so a bound checked only where
    /// somebody remembers to call [`super::constraint::Expr::validate`] is not a bound.
    ConstraintTooComplex {
        /// The `id` of the constraint whose expression exceeded a bound, so a producer
        /// can find it without diffing the whole document.
        constraint: String,
        /// Which bound was exceeded, and by how much.
        limit: ExprLimit,
    },
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedMajor { document, consumer } => write!(
                f,
                "unsupported producer document major version {document} (this consumer implements {consumer})"
            ),
            Self::UnparsableVersion { raw } => {
                write!(f, "unparsable producer document schema version {raw:?}")
            }
            Self::MissingVersion => f.write_str("producer document has no schema_version"),
            Self::Malformed { detail } => write!(f, "malformed producer document: {detail}"),
            Self::ConstraintTooComplex { constraint, limit } => write!(
                f,
                "constraint {constraint:?} exceeds the published expression bounds: {limit:?}"
            ),
        }
    }
}

/// What a reader may do with a persisted semantic artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredCompatibility {
    /// Stamped, same major, same-or-older minor.
    Readable,
    /// Stamped, same major, newer minor. Readable; unknown additions are preserved.
    ReadableWithUnknownAdditions,
    /// Unstamped legacy artifact. Readable, and stamped with `adopt_as` the next time
    /// it is written — never rewritten merely because it was read.
    UnstampedAdoptOnWrite {
        /// Version to stamp on the next write.
        adopt_as: SchemaVersion,
    },
    /// Not readable. Refuse rather than guess, and never migrate silently.
    Refused(Refusal),
}

impl StoredCompatibility {
    /// Decide what a reader implementing `reader` may do with an artifact whose
    /// stamp is `stamp` (`None` for legacy data written before stamping existed).
    pub fn evaluate(stamp: Option<SchemaVersion>, reader: SchemaVersion) -> Self {
        match stamp {
            None => Self::UnstampedAdoptOnWrite { adopt_as: reader },
            Some(found) => match found.compatibility_for(reader) {
                Compatibility::Supported => Self::Readable,
                Compatibility::SupportedWithUnknownAdditions => Self::ReadableWithUnknownAdditions,
                Compatibility::Refused(reason) => Self::Refused(reason),
            },
        }
    }

    /// Whether the artifact's contents may be used.
    pub fn is_readable(&self) -> bool {
        !matches!(self, Self::Refused(_))
    }

    /// The version to stamp when this artifact is next written, if adoption applies.
    pub fn adoption_version(&self) -> Option<SchemaVersion> {
        match self {
            Self::UnstampedAdoptOnWrite { adopt_as } => Some(*adopt_as),
            _ => None,
        }
    }
}
