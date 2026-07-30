//! Contract tests for the v1 adaptive-control producer document (issue #323).
//!
//! These tests are the executable half of
//! `docs/architecture/adaptive-producer-contract-v1.md`. The prose is normative for
//! humans; this file is normative for consumers.
//!
//! Test-first: every section here was written against the specification before the
//! corresponding module in `src/adaptive/` existed.

use unified_hifi_control::adaptive::version::{
    Compatibility, Refusal, SchemaVersion, StoredCompatibility, CONSUMER_SCHEMA_VERSION,
};

// ============================================================================
// Schema version parsing and ordering
// ============================================================================

mod schema_version {
    use super::*;

    #[test]
    fn v1_is_the_shipped_consumer_version() {
        assert_eq!(CONSUMER_SCHEMA_VERSION.major, 1);
        assert_eq!(CONSUMER_SCHEMA_VERSION.minor, 0);
    }

    #[test]
    fn parses_major_minor() {
        let v = SchemaVersion::parse("1.4").expect("1.4 is valid");
        assert_eq!((v.major, v.minor), (1, 4));
    }

    #[test]
    fn tolerates_extra_trailing_components() {
        // A producer that stamps "1.4.2" is still declaring major 1, minor 4.
        // Refusing it would be a compatibility break dressed up as strictness.
        let v = SchemaVersion::parse("1.4.2").expect("trailing components tolerated");
        assert_eq!((v.major, v.minor), (1, 4));
    }

    #[test]
    fn rejects_unparsable_versions() {
        for bad in ["", "1", "x.y", "1.x", ".1", "-1.0", "1.-1", "1..2"] {
            assert!(
                SchemaVersion::parse(bad).is_none(),
                "{bad:?} must not parse as a schema version"
            );
        }
    }

    #[test]
    fn serializes_as_a_dotted_string_not_an_object() {
        let v = SchemaVersion::new(1, 0);
        let json = serde_json::to_value(v).expect("serializes");
        assert_eq!(json, serde_json::json!("1.0"));
    }

    #[test]
    fn round_trips_through_json() {
        let v = SchemaVersion::new(1, 7);
        let round: SchemaVersion =
            serde_json::from_value(serde_json::to_value(v).expect("ser")).expect("de");
        assert_eq!(round, v);
    }
}

// ============================================================================
// Compatibility policy: additive minors accepted, unknown majors refused
// ============================================================================

mod compatibility_policy {
    use super::*;

    #[test]
    fn same_version_is_supported() {
        assert_eq!(
            SchemaVersion::new(1, 0).compatibility_for(CONSUMER_SCHEMA_VERSION),
            Compatibility::Supported
        );
    }

    #[test]
    fn older_minor_is_supported() {
        // A 1.0 consumer reading a document that predates its own minor.
        assert_eq!(
            SchemaVersion::new(1, 0).compatibility_for(SchemaVersion::new(1, 3)),
            Compatibility::Supported
        );
    }

    #[test]
    fn newer_minor_is_supported_with_unknown_additions() {
        // Additive evolution: a 1.0 consumer must render a 1.9 document.
        assert_eq!(
            SchemaVersion::new(1, 9).compatibility_for(SchemaVersion::new(1, 0)),
            Compatibility::SupportedWithUnknownAdditions
        );
    }

    #[test]
    fn newer_major_is_refused() {
        assert_eq!(
            SchemaVersion::new(2, 0).compatibility_for(CONSUMER_SCHEMA_VERSION),
            Compatibility::Refused(Refusal::UnsupportedMajor {
                document: 2,
                consumer: 1,
            })
        );
    }

    #[test]
    fn older_major_is_refused() {
        // v0 pre-release documents are not v1 documents. Fail safely rather than
        // guessing which fields moved.
        assert_eq!(
            SchemaVersion::new(0, 9).compatibility_for(CONSUMER_SCHEMA_VERSION),
            Compatibility::Refused(Refusal::UnsupportedMajor {
                document: 0,
                consumer: 1,
            })
        );
    }

    #[test]
    fn refusal_is_not_partial_rendering() {
        let refused = SchemaVersion::new(2, 0).compatibility_for(CONSUMER_SCHEMA_VERSION);
        assert!(
            !refused.is_usable(),
            "a refused document must not be rendered"
        );
        assert!(
            SchemaVersion::new(1, 9)
                .compatibility_for(SchemaVersion::new(1, 0))
                .is_usable(),
            "a newer minor must remain usable"
        );
    }
}

// ============================================================================
// Stored-artifact versioning: independent of the application version
// ============================================================================

mod stored_artifacts {
    use super::*;

    #[test]
    fn stamped_same_major_is_readable() {
        assert_eq!(
            StoredCompatibility::evaluate(Some(SchemaVersion::new(1, 2)), SchemaVersion::new(1, 4)),
            StoredCompatibility::Readable
        );
    }

    #[test]
    fn stamped_newer_minor_is_readable_because_additions_are_ignorable() {
        assert_eq!(
            StoredCompatibility::evaluate(Some(SchemaVersion::new(1, 9)), SchemaVersion::new(1, 0)),
            StoredCompatibility::ReadableWithUnknownAdditions
        );
    }

    #[test]
    fn stamped_newer_major_is_refused_never_migrated_silently() {
        assert_eq!(
            StoredCompatibility::evaluate(Some(SchemaVersion::new(2, 0)), SchemaVersion::new(1, 0)),
            StoredCompatibility::Refused(Refusal::UnsupportedMajor {
                document: 2,
                consumer: 1,
            })
        );
    }

    #[test]
    fn unstamped_legacy_data_is_adopted_on_write_not_on_read() {
        let decision = StoredCompatibility::evaluate(None, SchemaVersion::new(1, 0));
        assert_eq!(
            decision,
            StoredCompatibility::UnstampedAdoptOnWrite {
                adopt_as: SchemaVersion::new(1, 0)
            },
            "unstamped legacy artifacts are readable and get stamped the next time \
             they are written, never rewritten just because they were read"
        );
        assert!(decision.is_readable());
    }

    #[test]
    fn stored_version_is_independent_of_the_application_version() {
        // The reader version passed here is the *stored artifact* schema version the
        // application understands, which is deliberately not the app's release
        // version. This test exists to pin that the API takes a SchemaVersion and
        // has no coupling to CARGO_PKG_VERSION.
        let reader = SchemaVersion::new(1, 0);
        assert!(
            StoredCompatibility::evaluate(Some(SchemaVersion::new(1, 0)), reader).is_readable()
        );
    }
}
