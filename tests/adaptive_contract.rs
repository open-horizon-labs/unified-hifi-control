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

// ============================================================================
// Canonical fixtures
//
// The fixtures under tests/fixtures/adaptive/ are part of the contract for
// sibling issues (#324 publication, #325 HQPlayer mapping, #326 composition).
// Their paths are additive-only: a fixture may gain fields, and new fixtures may
// be added, but renaming or deleting one breaks another issue's tests.
// ============================================================================

mod fixtures {
    use super::*;
    use unified_hifi_control::adaptive::change_set::{ConflictPolicy, EpochPolicy};
    use unified_hifi_control::adaptive::value::Authority;
    use unified_hifi_control::adaptive::{
        admit_document, ActorClass, ApplyEffect, ApplyLane, AvailabilityState, ChangeSetState,
        CommandOutcome, ConstraintEffect, ControlKind, Disruption, DivergenceKind, DraftMode,
        Grounding, LaneState, ProducerDocument, ReasonCode, ReasonScope, RecoveryState, RiskClass,
        SourceClass, Surface, TargetRole, TransportLane, ValueLane, WriteAttempt,
    };

    /// Fixtures stamped 1.0 that a v1.0 consumer must understand completely.
    pub const CANONICAL: &[&str] = &[
        "hqplayer_pipeline_v1",
        "hqplayer_degraded_lanes",
        "staged_intent_multi_surface",
        "command_outcomes",
    ];

    pub fn raw(name: &str) -> serde_json::Value {
        let path = format!("tests/fixtures/adaptive/{name}.json");
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!("fixture {path} must be readable: {error}");
        });
        serde_json::from_str(&text).unwrap_or_else(|error| {
            panic!("fixture {path} must be valid JSON: {error}");
        })
    }

    pub fn document(name: &str) -> ProducerDocument {
        admit_document(&raw(name))
            .unwrap_or_else(|refusal| panic!("fixture {name} must be admitted: {refusal}"))
    }

    #[test]
    fn every_canonical_fixture_is_admitted() {
        for name in CANONICAL {
            let _ = document(name);
        }
    }

    #[test]
    fn every_canonical_fixture_round_trips_exactly() {
        // Re-serializing a parsed fixture must reproduce the file's JSON value. This
        // is what makes the fixtures a wire-shape lock rather than just examples: a
        // field the model silently drops shows up here as an inequality.
        for name in CANONICAL {
            let original = raw(name);
            let parsed = document(name);
            let round = serde_json::to_value(&parsed).expect("serializes");
            assert_eq!(round, original, "fixture {name} did not round-trip exactly");
        }
    }

    #[test]
    fn canonical_fixtures_have_no_stray_fields() {
        // Unknown fields are ignorable by design, which means a typo in one of *our
        // own* fixtures would be captured as an extension and round-trip happily. A
        // fixture claiming to demonstrate held intent while actually demonstrating
        // None is worse than no fixture, so canonical examples must be extension-free.
        for name in CANONICAL {
            let parsed = document(name);
            let strays = parsed.extension_key_paths();
            assert!(
                strays.is_empty(),
                "canonical fixture {name} has unrecognized fields (likely typos): {strays:?}"
            );
        }
    }

    #[test]
    fn unsupported_major_fixture_is_refused_before_parsing() {
        let refusal = admit_document(&raw("unsupported_major"))
            .expect_err("a 2.0 document must be refused by a 1.x consumer");
        assert_eq!(
            refusal,
            Refusal::UnsupportedMajor {
                document: 2,
                consumer: 1
            }
        );
        // The body is deliberately shaped nothing like v1. Refusal must not depend on
        // the body being parseable, or a v2 document would surface as "malformed".
        assert!(
            !matches!(refusal, Refusal::Malformed { .. }),
            "version refusal must precede body validation"
        );
    }

    #[test]
    fn forward_compatible_fixture_is_admitted_and_degrades() {
        let parsed = document("forward_compatible_additions");
        assert_eq!(parsed.schema_version, SchemaVersion::new(1, 7));
        assert_eq!(
            parsed
                .schema_version
                .compatibility_for(CONSUMER_SCHEMA_VERSION),
            Compatibility::SupportedWithUnknownAdditions
        );

        // Unknown vocabulary members degrade to Unrecognized, keeping their spelling.
        assert_eq!(
            parsed.target.role,
            TargetRole::Unrecognized("spatial_processor".to_string())
        );
        let vector = parsed
            .control(&"future.spatial.head_tracking".into())
            .expect("control present");
        assert_eq!(
            vector.kind,
            ControlKind::Unrecognized("continuous_vector".to_string())
        );
        assert!(!vector.kind.is_recognized());

        // An unknown control kind must not hide the control or its observed value.
        assert!(vector.availability.permits_display());
        let observed = vector.lane(&ValueLane::Observed).expect("observed lane");
        assert_eq!(observed.grounding, Grounding::Grounded);
        assert!(!observed
            .value
            .as_ref()
            .expect("value present")
            .is_recognized());

        // Unknown availability states and reason codes survive too.
        let quarantined = parsed
            .control(&"future.spatial.room_profile".into())
            .expect("control present");
        assert_eq!(
            quarantined.availability.state,
            AvailabilityState::Unrecognized("quarantined".to_string())
        );
        assert!(
            !quarantined.availability.permits_mutation(),
            "an unrecognized availability state must not be assumed mutable"
        );
        assert!(quarantined.availability.permits_display());
    }

    #[test]
    fn forward_compatible_fixture_preserves_unknown_container_fields() {
        // Preservation is guaranteed at container level: document, producer identity,
        // target, lane health, control, lane value, change set, entry and operation.
        let parsed = document("forward_compatible_additions");
        let mut strays = parsed.extension_key_paths();
        strays.sort();
        assert_eq!(
            strays,
            [
                "controls[future.spatial.render_mode].telemetry_topic",
                "controls[future.spatial.render_mode].values[observed].confidence",
                "lanes[native].round_trip_ms",
                "policy_hints",
                "producer.trust_tier",
            ]
        );

        // A pass-through projection must not amputate the document.
        let round = serde_json::to_value(&parsed).expect("serializes");
        assert_eq!(round, raw("forward_compatible_additions"));
    }

    #[test]
    fn unknown_fields_below_container_level_are_ignored_not_fatal() {
        // Ignorable-but-not-preserved is the documented boundary. What must never
        // happen is a parse failure.
        let mut value = raw("hqplayer_pipeline_v1");
        value["controls"][0]["apply"]["verification"]["retry_hint"] =
            serde_json::json!("exponential");
        let parsed = admit_document(&value).expect("unknown nested field must be ignorable");
        assert!(parsed.extension_key_paths().is_empty());
    }

    // ------------------------------------------------------------------
    // Vocabulary coverage
    // ------------------------------------------------------------------

    fn string_values(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::String(text) => out.push(text.clone()),
            serde_json::Value::Array(items) => {
                for item in items {
                    string_values(item, out);
                }
            }
            serde_json::Value::Object(members) => {
                for member in members.values() {
                    string_values(member, out);
                }
            }
            _ => {}
        }
    }

    fn all_fixture_strings() -> Vec<String> {
        let mut out = Vec::new();
        for name in CANONICAL
            .iter()
            .copied()
            .chain(["forward_compatible_additions"])
        {
            string_values(&raw(name), &mut out);
        }
        out
    }

    #[test]
    fn every_vocabulary_member_has_a_worked_example() {
        // A vocabulary member no fixture exercises is speculative: nothing proves it
        // serializes, and #325 has nothing to map onto. This test is the forcing
        // function that keeps the model tied to worked examples.
        let present = all_fixture_strings();
        let mut missing: Vec<String> = Vec::new();

        let mut check = |vocabulary: &str, members: &[&str]| {
            for member in members {
                if !present.iter().any(|found| found == member) {
                    missing.push(format!("{vocabulary}::{member}"));
                }
            }
        };

        check("ValueLane", ValueLane::KNOWN_WIRE);
        check("Authority", Authority::KNOWN_WIRE);
        check("SourceClass", SourceClass::KNOWN_WIRE);
        check("Grounding", Grounding::KNOWN_WIRE);
        check("DivergenceKind", DivergenceKind::KNOWN_WIRE);
        check("ControlKind", ControlKind::KNOWN_WIRE);
        check("ApplyLane", ApplyLane::KNOWN_WIRE);
        check("ApplyEffect", ApplyEffect::KNOWN_WIRE);
        check("Disruption", Disruption::KNOWN_WIRE);
        check("RiskClass", RiskClass::KNOWN_WIRE);
        check("AvailabilityState", AvailabilityState::KNOWN_WIRE);
        check("ReasonCode", ReasonCode::KNOWN_WIRE);
        check("ReasonScope", ReasonScope::KNOWN_WIRE);
        check("ConstraintEffect", ConstraintEffect::KNOWN_WIRE);
        check("ActorClass", ActorClass::KNOWN_WIRE);
        check("Surface", Surface::KNOWN_WIRE);
        check("DraftMode", DraftMode::KNOWN_WIRE);
        check("ConflictPolicy", ConflictPolicy::KNOWN_WIRE);
        check("ChangeSetState", ChangeSetState::KNOWN_WIRE);
        check("EpochPolicy", EpochPolicy::KNOWN_WIRE);
        check("WriteAttempt", WriteAttempt::KNOWN_WIRE);
        check("CommandOutcome", CommandOutcome::KNOWN_WIRE);
        check("RecoveryState", RecoveryState::KNOWN_WIRE);
        check("TargetRole", TargetRole::KNOWN_WIRE);
        check("TransportLane", TransportLane::KNOWN_WIRE);
        check("LaneState", LaneState::KNOWN_WIRE);
        check(
            "Expr",
            &[
                "eq",
                "not_eq",
                "one_of",
                "in_range",
                "is_grounded",
                "all",
                "any",
                "not",
            ],
        );

        assert!(
            missing.is_empty(),
            "vocabulary members with no worked example in any canonical fixture: {missing:#?}"
        );
    }

    #[test]
    fn every_vocabulary_member_round_trips_through_json() {
        // Complements the fixture test: proves serialization for every member even
        // where a fixture only demonstrates one of them in context.
        macro_rules! assert_round_trips {
            ($type:ty) => {
                for member in <$type>::known() {
                    let json = serde_json::to_value(&member).expect("serializes");
                    let back: $type = serde_json::from_value(json.clone()).expect("deserializes");
                    assert_eq!(back, member, "{json} did not round-trip");
                    assert!(member.is_recognized());
                }
            };
        }
        assert_round_trips!(ValueLane);
        assert_round_trips!(Authority);
        assert_round_trips!(SourceClass);
        assert_round_trips!(Grounding);
        assert_round_trips!(DivergenceKind);
        assert_round_trips!(ControlKind);
        assert_round_trips!(ApplyLane);
        assert_round_trips!(ApplyEffect);
        assert_round_trips!(Disruption);
        assert_round_trips!(RiskClass);
        assert_round_trips!(AvailabilityState);
        assert_round_trips!(ReasonCode);
        assert_round_trips!(ReasonScope);
        assert_round_trips!(ConstraintEffect);
        assert_round_trips!(ActorClass);
        assert_round_trips!(Surface);
        assert_round_trips!(DraftMode);
        assert_round_trips!(ConflictPolicy);
        assert_round_trips!(ChangeSetState);
        assert_round_trips!(EpochPolicy);
        assert_round_trips!(WriteAttempt);
        assert_round_trips!(CommandOutcome);
        assert_round_trips!(RecoveryState);
        assert_round_trips!(TargetRole);
        assert_round_trips!(TransportLane);
        assert_round_trips!(LaneState);
    }

    #[test]
    fn unknown_vocabulary_members_keep_their_spelling() {
        let outcome: CommandOutcome =
            serde_json::from_value(serde_json::json!("quantum_superposition")).expect("accepts");
        assert_eq!(
            outcome,
            CommandOutcome::Unrecognized("quantum_superposition".to_string())
        );
        assert_eq!(
            serde_json::to_value(&outcome).expect("serializes"),
            serde_json::json!("quantum_superposition")
        );
    }
}

// ============================================================================
// Semantics the canonical HQPlayer example must express
// ============================================================================

mod canonical_semantics {
    use super::fixtures::document;
    use unified_hifi_control::adaptive::value::Authority;
    use unified_hifi_control::adaptive::{
        ApplyEffect, ApplyLane, AvailabilityState, ControlId, ControlValue, Disruption,
        DivergenceKind, Grounding, ReasonCode, RiskClass, ValueLane,
    };

    #[test]
    fn empty_identity_is_grounded_and_is_not_null() {
        // A default/unnamed matrix profile is a real selection. If it collapsed into
        // null, a consumer could not tell it from "the profile could not be read", and
        // a write-back would replace a deliberate default with a guess.
        let doc = document("hqplayer_pipeline_v1");
        let profile = doc
            .control(&ControlId::new("hqplayer.matrix.profile"))
            .expect("matrix profile control");
        let observed = profile.lane(&ValueLane::Observed).expect("observed lane");
        assert_eq!(observed.grounding, Grounding::Grounded);
        assert_eq!(observed.value, Some(ControlValue::Empty));
        assert!(observed.value.as_ref().expect("value").is_empty_identity());
        assert!(observed.is_consistent());

        // Contrast: a control the producer genuinely cannot read is ungrounded with a
        // reason, and carries no value at all.
        let dormant = doc
            .control(&ControlId::new("hqplayer.chain.sdm.filter"))
            .expect("sdm chain filter control");
        let unread = dormant.lane(&ValueLane::Observed).expect("observed lane");
        assert_eq!(unread.grounding, Grounding::Ungrounded);
        assert_eq!(unread.value, None);
        assert_eq!(unread.ungrounded_reason, Some(ReasonCode::ChainNotLoaded));
        assert!(unread.is_consistent());
    }

    #[test]
    fn held_intent_for_a_dormant_chain_is_visible_and_diverges() {
        let doc = document("hqplayer_pipeline_v1");
        let dormant = doc
            .control(&ControlId::new("hqplayer.chain.sdm.filter"))
            .expect("sdm chain filter control");

        let held = dormant.lane(&ValueLane::Held).expect("held lane");
        assert_eq!(held.grounding, Grounding::Grounded);
        assert_eq!(held.provenance.authority, Authority::HeldIntent);
        assert!(
            held.freshness.stale,
            "held values are explicitly not current"
        );

        assert!(dormant
            .divergences
            .iter()
            .any(|divergence| divergence.kind == DivergenceKind::HeldUnreachable));

        // Unavailable, with a machine-readable reason - and still displayable, so the
        // user can see the intent that is waiting.
        assert_eq!(dormant.availability.state, AvailabilityState::Unavailable);
        assert!(dormant.availability.is_well_formed());
        assert!(!dormant.availability.permits_mutation());
        assert!(dormant.availability.permits_display());
        assert_eq!(
            dormant.apply.as_ref().expect("apply semantics").effect,
            ApplyEffect::HeldUntilChainLoaded
        );
    }

    #[test]
    fn composite_fixed_volume_keeps_enabled_and_retained_level_separate() {
        // The audit case: on some daemon builds a disabled fixed-volume feature retains
        // its last level in a commented element. Feature-enabled, observed active value
        // and retained inactive value are three facts, and a verifier that only checks
        // the field it wrote will report success after destroying the retained one.
        let doc = document("hqplayer_pipeline_v1");
        let enabled = doc
            .control(&ControlId::new("hqplayer.volume.fixed.enabled"))
            .expect("fixed volume enable control");
        let level = doc
            .control(&ControlId::new("hqplayer.volume.fixed.level"))
            .expect("fixed volume level control");

        assert_eq!(enabled.observed(), Some(&ControlValue::Bool(false)));

        // Disabled, so there is no active level - but the retained one is still stated.
        assert_eq!(
            level
                .lane(&ValueLane::Observed)
                .expect("observed lane")
                .grounding,
            Grounding::Ungrounded
        );
        assert_eq!(
            level
                .lane(&ValueLane::Held)
                .expect("held lane")
                .grounded_value(),
            Some(&ControlValue::Decimal(-6.0))
        );

        // Editing either member is one server-side composite plan, and verification
        // must assert the sibling was preserved.
        let apply = enabled.apply.as_ref().expect("apply semantics");
        assert_eq!(apply.lane, ApplyLane::Composite);
        assert_eq!(
            apply.coupled_with,
            vec![ControlId::new("hqplayer.volume.fixed.level")]
        );
        assert_eq!(
            apply.verification.preserves,
            vec![ControlId::new("hqplayer.volume.fixed.level")],
            "enabling the feature must be verified to preserve the retained level"
        );
    }

    #[test]
    fn db_volume_is_a_verified_pending_range_with_a_unit() {
        let doc = document("hqplayer_pipeline_v1");
        let volume = doc
            .control(&ControlId::new("hqplayer.volume.level"))
            .expect("volume control");
        assert_eq!(volume.unit.as_deref(), Some("dB"));
        let range = volume.range.as_ref().expect("numeric range");
        assert_eq!(range.min, Some(-60.0));
        assert_eq!(range.max, Some(0.0));
        assert_eq!(range.step, Some(0.5));
        assert_eq!(range.unit.as_deref(), Some("dB"));
        assert_eq!(range.authority, Authority::Runtime);

        let apply = volume.apply.as_ref().expect("apply semantics");
        assert_eq!(apply.effect, ApplyEffect::VerifiedPending);
        assert!(
            apply.verification.provisional_ack,
            "a wire acknowledgement for volume is provisional until readback"
        );
        assert!(volume.is_mutable());

        // Runtime and configuration legitimately differ; the divergence is stated.
        assert!(volume
            .divergences
            .iter()
            .any(|divergence| divergence.kind == DivergenceKind::ObservedVsPersisted));
    }

    #[test]
    fn exact_rate_is_unavailable_in_source_mode_with_machine_readable_reasons() {
        let doc = document("hqplayer_pipeline_v1");
        let rate = doc
            .control(&ControlId::new("hqplayer.pipeline.rate.exact"))
            .expect("exact rate control");
        assert_eq!(rate.availability.state, AvailabilityState::Unavailable);
        let codes: Vec<&ReasonCode> = rate
            .availability
            .reasons
            .iter()
            .map(|reason| &reason.code)
            .collect();
        assert!(codes.contains(&&ReasonCode::NotApplicableInMode));
        assert!(codes.contains(&&ReasonCode::ConflictingSibling));
        // Reasons carry catalog keys, never prose. Display text and its licensing are
        // governed by #343.
        for reason in &rate.availability.reasons {
            assert!(
                reason.display_text_key.is_some(),
                "every availability reason needs a catalog key for display text"
            );
        }
        assert!(!rate.is_mutable());
    }

    #[test]
    fn restart_required_persistent_setting_states_its_cost_and_verification_lane() {
        let doc = document("hqplayer_pipeline_v1");
        let buffer = doc
            .control(&ControlId::new("hqplayer.settings.buffer_time"))
            .expect("buffer time control");
        let apply = buffer.apply.as_ref().expect("apply semantics");
        assert_eq!(apply.lane, ApplyLane::Persistent);
        assert_eq!(apply.effect, ApplyEffect::RestartRequired);
        assert_eq!(apply.disruption, Disruption::EngineRestart);
        assert_eq!(apply.risk, RiskClass::Disruptive);
        // Verification reads the persisted lane, not the running engine: the write is
        // stored now and audible later.
        assert_eq!(apply.verification.verify_lane, ValueLane::Persisted);
        assert!(buffer
            .divergences
            .iter()
            .any(|divergence| divergence.kind == DivergenceKind::ObservedVsPersisted));
    }

    #[test]
    fn adaptive_output_invalidates_the_capabilities_it_makes_unobservable() {
        let doc = document("hqplayer_pipeline_v1");
        let adaptive = doc
            .control(&ControlId::new("hqplayer.pipeline.adaptive_output"))
            .expect("adaptive output control");
        assert_eq!(adaptive.observed(), Some(&ControlValue::Bool(true)));
        assert_eq!(
            adaptive
                .apply
                .as_ref()
                .expect("apply semantics")
                .invalidates,
            vec![ControlId::new("hqplayer.pipeline.rate.exact")]
        );

        // Mode changes invalidate more, including the chain-scoped control.
        let mode = doc
            .control(&ControlId::new("hqplayer.pipeline.mode"))
            .expect("mode control");
        let invalidates = &mode.apply.as_ref().expect("apply semantics").invalidates;
        assert!(invalidates.contains(&ControlId::new("hqplayer.chain.sdm.filter")));
    }

    #[test]
    fn transport_actions_carry_no_value_and_name_the_field_that_verifies_them() {
        let doc = document("hqplayer_pipeline_v1");
        let play = doc
            .control(&ControlId::new("hqplayer.transport.play"))
            .expect("play control");
        assert!(play.values.is_empty(), "an action has no value");
        let verification = &play.apply.as_ref().expect("apply semantics").verification;
        assert_eq!(
            verification.verify_via,
            Some(ControlId::new("hqplayer.transport.state")),
            "readback is driven by data, not by setter-specific consumer logic"
        );
        assert_eq!(verification.verify_lane, ValueLane::Observed);
    }

    #[test]
    fn runtime_enumeration_may_contain_options_no_catalog_knows() {
        let doc = document("hqplayer_pipeline_v1");
        let filter = doc
            .control(&ControlId::new("hqplayer.pipeline.filter_1x"))
            .expect("filter control");
        let choices = filter.choices.as_ref().expect("choice set");
        assert_eq!(choices.authority, Authority::Runtime);
        let unknown = choices
            .choices
            .iter()
            .find(|choice| choice.id == "poly-sinc-hb-2s")
            .expect("engine-only option present");
        assert!(
            unknown.label_key.is_none(),
            "this option has no catalog entry"
        );
        assert!(
            unknown.engine_name.is_some(),
            "it must still be renderable by its engine name, and invokable"
        );
    }

    #[test]
    fn every_control_has_unique_lanes_and_well_formed_availability() {
        for name in super::fixtures::CANONICAL {
            let doc = document(name);
            for control in &doc.controls {
                assert!(
                    control.has_unique_lanes(),
                    "{name}: {} repeats a value lane",
                    control.id
                );
                assert!(
                    control.availability.is_well_formed(),
                    "{name}: {} has a non-available state with no reason",
                    control.id
                );
                for value in &control.values {
                    assert!(
                        value.is_consistent(),
                        "{name}: {} lane {} disagrees with its own grounding",
                        control.id,
                        value.lane
                    );
                }
            }
        }
    }

    #[test]
    fn a_degraded_persistence_lane_does_not_blank_native_control() {
        let doc = document("hqplayer_degraded_lanes");
        let native = doc
            .lane_health(&unified_hifi_control::adaptive::TransportLane::Native)
            .expect("native lane health");
        assert_eq!(
            native.state,
            unified_hifi_control::adaptive::LaneState::Connected
        );

        let volume = doc
            .control(&ControlId::new("hqplayer.volume.level"))
            .expect("volume control");
        assert!(
            volume.is_mutable(),
            "native control stays usable while persistence is degraded"
        );
        assert_eq!(
            volume
                .lane(&ValueLane::Persisted)
                .expect("persisted lane")
                .ungrounded_reason,
            Some(ReasonCode::RequiresConnection)
        );

        // A setting that lives only on the degraded lane becomes read-only, with a
        // lane-scoped reason - not invisible.
        let buffer = doc
            .control(&ControlId::new("hqplayer.settings.buffer_time"))
            .expect("buffer time control");
        assert_eq!(buffer.availability.state, AvailabilityState::ReadOnly);
        assert!(!buffer.is_mutable());
        assert!(buffer.availability.permits_display());
        assert!(!doc.stale, "the document as a whole is not stale");
    }
}

// ============================================================================
// Revision ordering, staleness and out-of-order delivery
// ============================================================================

mod revisions {
    use unified_hifi_control::adaptive::{
        DocumentRevisions, ProducerEpoch, Revision, RevisionRef, Staleness,
    };

    #[test]
    fn neither_revision_may_regress() {
        let previous = DocumentRevisions::new(12, 4193);
        assert!(DocumentRevisions::new(12, 4192).regresses_from(&previous));
        assert!(DocumentRevisions::new(11, 4193).regresses_from(&previous));
        assert!(!DocumentRevisions::new(12, 4194).regresses_from(&previous));
        assert!(!DocumentRevisions::new(13, 4193).regresses_from(&previous));
    }

    #[test]
    fn out_of_order_delivery_is_droppable_not_mergeable() {
        // A late-arriving older snapshot must be recognizable as older so it cannot
        // resurrect removed controls.
        let current = DocumentRevisions::new(13, 4200);
        let late = DocumentRevisions::new(12, 4198);
        assert!(late.regresses_from(&current));
        assert!(!late.advances_over(&current));
        assert!(current.advances_over(&late));
        assert!(
            !current.advances_over(&current),
            "an identical snapshot is not an advance"
        );
    }

    #[test]
    fn a_fast_state_bump_does_not_touch_control_identity() {
        // The whole reason for splitting the revisions: polling must not churn the
        // control plane, or every consumer re-renders and every draft looks stale.
        let before = DocumentRevisions::new(12, 4193);
        let after = DocumentRevisions::new(12, 4194);
        assert_eq!(after.control_plane, before.control_plane);
        assert!(after.advances_over(&before));
    }

    fn base(epoch: u64, control_plane: u64, state: u64) -> RevisionRef {
        RevisionRef {
            epoch: ProducerEpoch(epoch),
            control_plane: Revision(control_plane),
            state: Revision(state),
        }
    }

    #[test]
    fn staleness_precedence_is_total_so_consumers_never_pick_the_wrong_pair() {
        let current = DocumentRevisions::new(13, 4300);

        // Epoch dominates everything, even when both revisions match.
        assert_eq!(
            Staleness::evaluate(&base(6, 13, 4300), ProducerEpoch(7), &current),
            Staleness::EpochChanged
        );
        // Control plane dominates state.
        assert_eq!(
            Staleness::evaluate(&base(7, 12, 4200), ProducerEpoch(7), &current),
            Staleness::ControlPlaneAdvanced
        );
        // State alone.
        assert_eq!(
            Staleness::evaluate(&base(7, 13, 4200), ProducerEpoch(7), &current),
            Staleness::StateAdvanced
        );
        // Exact match.
        assert_eq!(
            Staleness::evaluate(&base(7, 13, 4300), ProducerEpoch(7), &current),
            Staleness::Current
        );
    }

    #[test]
    fn anything_but_an_exact_match_requires_revalidation_before_mutation() {
        // Silent rebasing is never allowed: the user staged a value against what they
        // were shown, so a moved baseline is something they must see.
        assert!(!Staleness::Current.requires_revalidation());
        assert!(Staleness::StateAdvanced.requires_revalidation());
        assert!(Staleness::ControlPlaneAdvanced.requires_revalidation());
        assert!(Staleness::EpochChanged.requires_revalidation());

        assert_eq!(Staleness::Current.reason_code(), None);
        assert!(Staleness::StateAdvanced.reason_code().is_some());
        assert!(Staleness::EpochChanged.reason_code().is_some());
    }
}

// ============================================================================
// Change sets: generations, atomic detach and the lost-update guard
// ============================================================================

mod change_sets {
    use super::fixtures::document;
    use unified_hifi_control::adaptive::{
        ApplyLane, ChangeSet, ChangeSetEntry, ChangeSetId, ChangeSetState, ControlId, ControlValue,
        EntryValidity, RetireOutcome, Staleness, Timestamp,
    };

    fn draft() -> ChangeSet {
        let doc = document("staged_intent_multi_surface");
        doc.change_sets
            .iter()
            .find(|set| set.id == ChangeSetId::new("cs-web-3f9a"))
            .expect("web draft present")
            .clone()
    }

    fn entry(control: &str, value: f64) -> ChangeSetEntry {
        ChangeSetEntry {
            control: ControlId::new(control),
            lane: ApplyLane::Staged,
            desired: ControlValue::Decimal(value),
            base_observed: None,
            validity: EntryValidity::Valid,
            extensions: Default::default(),
        }
    }

    #[test]
    fn every_operation_targets_a_named_change_set() {
        // There is no "the pending changes" to address, which is what stops one surface
        // flushing another's draft.
        let doc = document("staged_intent_multi_surface");
        assert!(doc.change_sets.len() > 1, "multiple drafts coexist");
        for set in &doc.change_sets {
            assert!(!set.id.as_str().is_empty());
            assert!(!set.producer_id.is_empty());
        }
        // Distinct surfaces own distinct drafts.
        let surfaces: Vec<_> = doc
            .change_sets
            .iter()
            .map(|set| set.origin.surface.clone())
            .collect();
        assert!(surfaces.len() > 2);
    }

    #[test]
    fn staging_produces_a_successor_generation() {
        let mut set = draft();
        let before = set.generation;
        let after = set.stage(
            entry("hqplayer.volume.level", -9.0),
            Timestamp::new("2026-07-30T11:10:00Z"),
        );
        assert_eq!(after, before + 1);
        assert_eq!(set.generation, after);
    }

    #[test]
    fn a_completion_may_retire_only_the_generation_it_detached() {
        // This is the recorded lost-update race, encoded. An apply detaches generation
        // N; another surface stages during execution, making N+1; the completion for N
        // must not clear the draft.
        let mut set = draft();
        let detached = set.detach();
        assert_eq!(set.state, ChangeSetState::Detached);
        let staged_during_execution = set.stage(
            entry("hqplayer.pipeline.rate.exact", 384000.0),
            Timestamp::new("2026-07-30T11:10:05Z"),
        );
        assert_eq!(staged_during_execution, detached.generation + 1);
        assert_eq!(
            set.state,
            ChangeSetState::Draft,
            "staging during execution reopens the draft as a successor generation"
        );

        let outcome = set.retire(detached.generation, Timestamp::new("2026-07-30T11:10:06Z"));
        assert_eq!(
            outcome,
            RetireOutcome::GenerationSuperseded {
                attempted: detached.generation,
                current: staged_during_execution,
            }
        );
        assert!(
            !set.entries.is_empty(),
            "the stale completion must not clear later intent"
        );
        assert_ne!(set.state, ChangeSetState::Applied);

        // Retiring the current generation is allowed and clears exactly that.
        assert_eq!(
            set.retire(
                staged_during_execution,
                Timestamp::new("2026-07-30T11:10:07Z")
            ),
            RetireOutcome::Retired
        );
        assert!(set.entries.is_empty());
        assert_eq!(set.state, ChangeSetState::Applied);
    }

    #[test]
    fn detach_snapshots_the_entries_it_will_execute() {
        // Preflight and execution operate on the snapshot, so "the operation applied
        // what it displayed" is true even under concurrent staging.
        let mut set = draft();
        let detached = set.detach();
        let count = detached.entries.len();
        set.stage(
            entry("hqplayer.volume.level", -3.0),
            Timestamp::new("2026-07-30T11:11:00Z"),
        );
        assert_eq!(
            detached.entries.len(),
            count,
            "the detached snapshot is immutable"
        );
        assert_eq!(detached.base, set.base);
    }

    #[test]
    fn restaging_the_same_control_and_lane_replaces_rather_than_duplicates() {
        let mut set = draft();
        let control = ControlId::new("hqplayer.volume.level");
        set.stage(
            entry("hqplayer.volume.level", -9.0),
            Timestamp::new("2026-07-30T11:12:00Z"),
        );
        set.stage(
            entry("hqplayer.volume.level", -8.0),
            Timestamp::new("2026-07-30T11:12:01Z"),
        );
        let matching = set
            .entries
            .iter()
            .filter(|found| found.control == control)
            .count();
        assert_eq!(matching, 1);
        assert_eq!(
            set.entry(&control, &ApplyLane::Staged)
                .expect("entry present")
                .desired,
            ControlValue::Decimal(-8.0)
        );
    }

    #[test]
    fn a_stale_or_invalid_entry_blocks_apply_and_says_why() {
        let set = draft();
        assert!(
            !set.may_apply(),
            "a draft with a stale base or an invalid combination must not apply as-is"
        );

        let stale = set
            .entry(&ControlId::new("hqplayer.volume.level"), &ApplyLane::Staged)
            .expect("volume entry");
        match &stale.validity {
            EntryValidity::StaleBase { observed_now } => {
                assert_eq!(
                    observed_now.as_ref(),
                    Some(&ControlValue::Decimal(-14.0)),
                    "the entry reports what is observed now, so the user sees the conflict"
                );
            }
            other => panic!("expected a stale base, got {other:?}"),
        }
        assert!(!stale.validity.may_apply());

        let invalid = set
            .entry(
                &ControlId::new("hqplayer.pipeline.rate.exact"),
                &ApplyLane::Staged,
            )
            .expect("rate entry");
        assert!(matches!(
            invalid.validity,
            EntryValidity::DraftInvalid { .. }
        ));
    }

    #[test]
    fn a_shared_draft_names_every_contributor() {
        let set = draft();
        assert!(
            set.contributors.len() > 1,
            "a draft several surfaces contributed to must name them all"
        );
    }

    #[test]
    fn a_draft_based_on_an_earlier_epoch_is_expired_not_rebased() {
        let doc = document("staged_intent_multi_surface");
        let orphan = doc
            .change_sets
            .iter()
            .find(|set| set.id == ChangeSetId::new("cs-auto-1104"))
            .expect("automation draft present");
        assert_eq!(orphan.state, ChangeSetState::Expired);
        assert_eq!(
            Staleness::evaluate(&orphan.base, doc.producer.epoch, &doc.revisions),
            Staleness::EpochChanged
        );
        assert!(orphan.retention.expires_at.is_some());
    }
}

// ============================================================================
// Command outcomes, correlation and the audit trail
// ============================================================================

mod outcomes {
    use super::fixtures::document;
    use unified_hifi_control::adaptive::{
        CommandOutcome, OperationId, OperationRecord, Timestamp, WriteAttempt,
    };

    fn operation(id: &str) -> OperationRecord {
        document("command_outcomes")
            .operations
            .iter()
            .find(|record| record.id == OperationId::new(id))
            .unwrap_or_else(|| panic!("operation {id} present"))
            .clone()
    }

    #[test]
    fn success_and_failure_are_not_a_sufficient_vocabulary() {
        // Eight distinct outcomes appear on the same document. None of them is
        // reducible to "ok" or "error" without losing what a consumer must do next.
        let doc = document("command_outcomes");
        let distinct: std::collections::BTreeSet<&str> = doc
            .operations
            .iter()
            .map(|record| record.outcome.as_str())
            .collect();
        for expected in [
            "applied",
            "ignored",
            "rejected",
            "superseded",
            "disconnected",
            "timed_out",
            "held",
            "restart_required",
            "indeterminate",
        ] {
            assert!(
                distinct.contains(expected),
                "canonical outcomes must include {expected}"
            );
        }
    }

    #[test]
    fn a_dropped_transport_after_a_write_is_indeterminate_not_failed() {
        let record = operation("op-provisional-9001");
        assert_eq!(record.write_attempt, WriteAttempt::AcknowledgedProvisional);
        assert_eq!(record.outcome, CommandOutcome::Indeterminate);
        assert!(
            !record.is_state_evidence(),
            "possibly-applied is not evidence about the producer's state"
        );
        assert!(record.outcome.awaits_convergence());
        assert!(!record.outcome.is_terminal());
        assert!(
            record.reason.is_some(),
            "the reason must be machine-readable"
        );
    }

    #[test]
    fn consumers_never_infer_state_from_whether_a_call_returned() {
        // The two facts are tracked separately, and only some combinations are
        // evidence of state.
        for (attempt, outcome, evidence) in [
            (WriteAttempt::Confirmed, CommandOutcome::Applied, true),
            (
                WriteAttempt::AcknowledgedProvisional,
                CommandOutcome::Indeterminate,
                false,
            ),
            (WriteAttempt::Attempted, CommandOutcome::TimedOut, false),
            (
                WriteAttempt::NotAttempted,
                CommandOutcome::Disconnected,
                true,
            ),
        ] {
            assert_eq!(
                outcome.is_state_evidence(),
                evidence,
                "{attempt} / {outcome} state-evidence classification"
            );
        }
    }

    #[test]
    fn readback_may_resolve_indeterminate_but_nothing_may_regress_into_it() {
        let mut record = operation("op-provisional-9001");
        let history_before = record.history.len();

        for resolution in [
            CommandOutcome::Applied,
            CommandOutcome::Superseded,
            CommandOutcome::Rejected,
            CommandOutcome::Divergent,
        ] {
            let mut candidate = record.clone();
            assert!(
                candidate
                    .transition(
                        resolution.clone(),
                        Timestamp::new("2026-07-30T11:20:00Z"),
                        None,
                        None
                    )
                    .is_ok(),
                "indeterminate must be resolvable to {resolution}"
            );
            assert_eq!(candidate.history.len(), history_before + 1);
        }

        // Resolve, then attempt to decay back into a maybe.
        record
            .transition(
                CommandOutcome::Applied,
                Timestamp::new("2026-07-30T11:20:00Z"),
                None,
                None,
            )
            .expect("resolves");
        let rejected = record
            .transition(
                CommandOutcome::Indeterminate,
                Timestamp::new("2026-07-30T11:21:00Z"),
                None,
                None,
            )
            .expect_err("an authoritative outcome must not decay");
        assert_eq!(rejected.from, CommandOutcome::Applied);
        assert_eq!(rejected.to, CommandOutcome::Indeterminate);
    }

    #[test]
    fn a_new_operation_cannot_erase_the_audit_trail() {
        let mut record = operation("op-provisional-9001");
        let original = record.history.clone();
        assert!(!original.is_empty());
        record
            .transition(
                CommandOutcome::Applied,
                Timestamp::new("2026-07-30T11:22:00Z"),
                None,
                None,
            )
            .expect("resolves");
        assert_eq!(
            &record.history[..original.len()],
            &original[..],
            "history is append-only"
        );
        assert_eq!(record.history.len(), original.len() + 1);
        // The trail is what distinguishes "never applied" from "applied, then replaced".
        assert!(record
            .history
            .iter()
            .any(|transition| transition.to == CommandOutcome::Indeterminate));
    }

    #[test]
    fn a_multi_revision_plan_declares_its_boundaries_and_per_step_revisions() {
        let record = operation("op-multirev-9011");
        assert!(
            record.is_multi_revision(),
            "a plan whose later steps need capabilities the earlier steps create must \
             declare that it is not a single-revision atomic preflight"
        );
        assert_eq!(record.outcome, CommandOutcome::PartiallyApplied);
        assert_eq!(record.revision_boundaries.len(), 2);
        assert_eq!(record.steps.len(), 2);
        for step in &record.steps {
            assert!(
                step.observed.is_some(),
                "every step carries the revision it observed"
            );
        }
        assert_eq!(record.steps[0].outcome, CommandOutcome::Applied);
        assert_eq!(record.steps[1].outcome, CommandOutcome::Rejected);
        // Retry is a replan, not a replay.
        assert_eq!(
            record.recovery.as_ref().map(|state| state.as_str()),
            Some("replan_required")
        );
    }

    #[test]
    fn held_intent_that_proves_invalid_terminates_visibly() {
        let record = operation("op-expired-9010");
        assert_eq!(record.outcome, CommandOutcome::Expired);
        assert!(record.outcome.is_terminal());
        assert!(record.reason.is_some(), "expiry carries a reason");
        assert!(!record.correlation_id.is_empty(), "and a correlation");
        assert!(record
            .history
            .iter()
            .any(|transition| transition.from == CommandOutcome::Held));
    }

    #[test]
    fn completion_records_the_generation_it_executed() {
        let record = operation("op-provisional-9001");
        assert!(record.change_set.is_some());
        assert_eq!(
            record.generation,
            Some(3),
            "an operation names the exact generation it may retire"
        );
    }

    #[test]
    fn every_operation_carries_a_correlation_id() {
        for record in &document("command_outcomes").operations {
            assert!(
                !record.correlation_id.is_empty(),
                "{} has no correlation id",
                record.id.as_str()
            );
        }
    }
}

// ============================================================================
// Draft-dependent constraints
// ============================================================================

mod constraints {
    use super::fixtures::document;
    use unified_hifi_control::adaptive::constraint::{ValueLookup, MAX_EXPR_DEPTH, MAX_EXPR_NODES};
    use unified_hifi_control::adaptive::control::{Reason, ReasonScope};
    use unified_hifi_control::adaptive::value::ValueLane;
    use unified_hifi_control::adaptive::{
        Constraint, ControlId, ControlValue, Evaluation, Expr, LaneValue, Permission, Provenance,
        ReasonCode, Visibility,
    };

    /// A minimal lookup so evaluation can be tested without a whole document.
    struct Fixed(Vec<(ControlId, ValueLane, LaneValue)>);

    impl ValueLookup for Fixed {
        fn lookup(&self, control: &ControlId, lane: &ValueLane) -> Option<&LaneValue> {
            self.0
                .iter()
                .find(|(id, found, _)| id == control && found == lane)
                .map(|(_, _, value)| value)
        }
    }

    fn view(entries: Vec<(&str, ValueLane, ControlValue)>) -> Fixed {
        Fixed(
            entries
                .into_iter()
                .map(|(id, lane, value)| {
                    (
                        ControlId::new(id),
                        lane.clone(),
                        LaneValue::grounded(lane, value, Provenance::runtime()),
                    )
                })
                .collect(),
        )
    }

    fn constraint(id: &str) -> Constraint {
        document("hqplayer_pipeline_v1")
            .constraints
            .iter()
            .find(|found| found.id == id)
            .unwrap_or_else(|| panic!("constraint {id} present"))
            .clone()
    }

    #[test]
    fn a_control_available_on_the_engine_can_still_be_draft_invalid() {
        // The distinction the whole module exists for: observed availability and
        // draft-dependent validity are different questions.
        let doc = document("staged_intent_multi_surface");
        let rate = doc
            .control(&ControlId::new("hqplayer.pipeline.rate.exact"))
            .expect("rate control");
        assert!(
            rate.is_mutable(),
            "the running engine accepts this control right now"
        );

        let constraint = doc
            .constraints
            .iter()
            .find(|found| found.id == "hqplayer.constraint.pcm_rate_invalid_in_sdm")
            .expect("constraint present");
        let effective = doc.effective_view(doc.change_sets.first());
        let evaluation = constraint.when.evaluate(&effective);
        assert_eq!(
            evaluation,
            Evaluation::True,
            "the staged mode makes the proposed combination invalid"
        );
        assert!(matches!(
            constraint.permission(&evaluation),
            Permission::Denied(_)
        ));
    }

    #[test]
    fn evaluation_uses_the_same_editor_effective_values_on_every_surface() {
        // Two consumers looking at the same document and change set must reach the same
        // verdict. Both go through effective_view, so there is no second code path to
        // diverge.
        let doc = document("staged_intent_multi_surface");
        let constraint = doc.constraints.first().expect("constraint present").clone();
        let first = doc.effective_view(doc.change_sets.first());
        let second = doc.effective_view(doc.change_sets.first());
        assert_eq!(
            constraint.when.evaluate(&first),
            constraint.when.evaluate(&second)
        );

        // And the effective lane really does follow staged intent, not observed state.
        let mode = ControlId::new("hqplayer.pipeline.mode");
        let staged = first
            .lookup(&mode, &ValueLane::Effective)
            .and_then(LaneValue::grounded_value)
            .cloned();
        assert_eq!(staged, Some(ControlValue::choice("sdm")));
        let observed = first
            .lookup(&mode, &ValueLane::Observed)
            .and_then(LaneValue::grounded_value)
            .cloned();
        assert_eq!(observed, Some(ControlValue::choice("pcm")));
    }

    #[test]
    fn an_unknown_operator_never_hides_the_control_or_its_value() {
        // Visibility fails open. Hiding the control the user needs in order to leave
        // the state is worse than showing one whose constraint we cannot check.
        let unknown = Expr::Unrecognized {
            operator: "matches_regex".to_string(),
            raw: serde_json::json!({"op": "matches_regex"}),
        };
        let evaluation = unknown.evaluate(&view(vec![]));
        assert_eq!(
            evaluation,
            Evaluation::Unevaluatable {
                reason: ReasonCode::UnknownConstraint
            }
        );

        let subject = constraint("hqplayer.constraint.exact_rate_requires_explicit_mode");
        match subject.visibility(&evaluation) {
            Visibility::ShowWithReason(reason) => {
                assert_eq!(reason.code, ReasonCode::UnknownConstraint);
                assert!(reason.display_text_key.is_some());
            }
            other => panic!("expected an annotated show, got {other:?}"),
        }
        assert!(subject.visibility(&evaluation).renders_control());
    }

    #[test]
    fn an_unknown_operator_is_never_reported_as_satisfied() {
        // Permission fails closed. Fail-open is right for what the user can see and
        // wrong for what the system will accept.
        let subject = constraint("hqplayer.constraint.exact_rate_requires_explicit_mode");
        let undecided = Evaluation::Unevaluatable {
            reason: ReasonCode::UnknownConstraint,
        };
        let permission = subject.permission(&undecided);
        assert!(
            !permission.asserts_satisfied(),
            "an unevaluatable constraint must not claim the combination is valid"
        );
        assert!(matches!(
            permission,
            Permission::RequiresProducerValidation(_)
        ));

        // A definite negative is a plain allow; a definite positive on an invalidating
        // constraint is a denial.
        assert!(subject.permission(&Evaluation::False).asserts_satisfied());
        assert!(matches!(
            subject.permission(&Evaluation::True),
            Permission::Denied(_)
        ));
    }

    #[test]
    fn an_inert_or_restart_requiring_combination_is_allowed_deliberately() {
        // Only `invalidates` denies. Choosing a setting that will not be audible until
        // restart, or that is inert in the current chain, is a legitimate user choice.
        for id in [
            "hqplayer.constraint.exact_rate_inert_when_adaptive",
            "hqplayer.constraint.buffer_time_needs_restart",
            "hqplayer.constraint.sdm_filters_hidden_outside_sdm",
        ] {
            let subject = constraint(id);
            assert!(
                subject.permission(&Evaluation::True).asserts_satisfied(),
                "{id} must not deny; its effect is not invalidation"
            );
        }
    }

    #[test]
    fn hides_choice_hides_a_choice_and_never_the_control() {
        let subject = constraint("hqplayer.constraint.sdm_filters_hidden_outside_sdm");
        assert!(subject.visibility(&Evaluation::True).renders_control());
    }

    #[test]
    fn kleene_logic_prefers_a_definite_answer_over_an_undecidable_sibling() {
        let undecided = Expr::Unrecognized {
            operator: "future_op".to_string(),
            raw: serde_json::json!({"op": "future_op"}),
        };
        let definitely_true = Expr::IsGrounded {
            control: ControlId::new("a"),
            lane: ValueLane::Observed,
        };
        let definitely_false = Expr::IsGrounded {
            control: ControlId::new("missing"),
            lane: ValueLane::Observed,
        };
        let context = view(vec![("a", ValueLane::Observed, ControlValue::Bool(true))]);

        // `all`: one definite false settles it.
        assert_eq!(
            Expr::All(vec![definitely_false.clone(), undecided.clone()]).evaluate(&context),
            Evaluation::False
        );
        // `all`: without a false, an undecidable child makes the whole thing undecidable.
        assert!(matches!(
            Expr::All(vec![definitely_true.clone(), undecided.clone()]).evaluate(&context),
            Evaluation::Unevaluatable { .. }
        ));
        // `any`: one definite true settles it.
        assert_eq!(
            Expr::Any(vec![definitely_true.clone(), undecided.clone()]).evaluate(&context),
            Evaluation::True
        );
        // `any`: without a true, an undecidable child makes the whole thing undecidable.
        assert!(matches!(
            Expr::Any(vec![definitely_false.clone(), undecided.clone()]).evaluate(&context),
            Evaluation::Unevaluatable { .. }
        ));
        // `not` propagates undecidability rather than inverting it into a definite answer.
        assert!(matches!(
            Expr::Not(Box::new(undecided)).evaluate(&context),
            Evaluation::Unevaluatable { .. }
        ));
        assert_eq!(
            Expr::Not(Box::new(definitely_true)).evaluate(&context),
            Evaluation::False
        );
    }

    #[test]
    fn an_ungrounded_operand_is_undecidable_not_false() {
        // "I cannot read this" must not silently satisfy a constraint.
        let context = Fixed(vec![(
            ControlId::new("a"),
            ValueLane::Observed,
            LaneValue::ungrounded(
                ValueLane::Observed,
                ReasonCode::Unobservable,
                Provenance::runtime(),
            ),
        )]);
        let expression = Expr::Eq {
            control: ControlId::new("a"),
            lane: ValueLane::Observed,
            value: ControlValue::Bool(true),
        };
        assert!(matches!(
            expression.evaluate(&context),
            Evaluation::Unevaluatable { .. }
        ));
    }

    #[test]
    fn every_operator_evaluates_over_the_canonical_document() {
        let doc = document("hqplayer_pipeline_v1");
        let effective = doc.effective_view(None);
        for subject in &doc.constraints {
            let evaluation = subject.when.evaluate(&effective);
            // Whatever the verdict, visibility never removes the control and permission
            // never over-claims.
            assert!(subject.visibility(&evaluation).renders_control());
            if matches!(evaluation, Evaluation::Unevaluatable { .. }) {
                assert!(!subject.permission(&evaluation).asserts_satisfied());
            }
        }

        // The canonical example is in the source-following mode, so the exact-rate
        // constraint really does fire.
        let exact_rate = doc
            .constraints
            .iter()
            .find(|found| found.id == "hqplayer.constraint.exact_rate_requires_explicit_mode")
            .expect("constraint present");
        assert_eq!(exact_rate.when.evaluate(&effective), Evaluation::True);
    }

    #[test]
    fn expressions_are_fully_recognized_in_canonical_fixtures_and_bounded() {
        let doc = document("hqplayer_pipeline_v1");
        for subject in &doc.constraints {
            assert!(
                subject.when.is_fully_recognized(),
                "{} uses an operator this build does not know",
                subject.id
            );
            subject.when.validate().unwrap_or_else(|limit| {
                panic!("{} exceeds the published bounds: {limit:?}", subject.id)
            });
            assert!(subject.when.depth() <= MAX_EXPR_DEPTH);
            assert!(subject.when.node_count() <= MAX_EXPR_NODES);
        }

        // A newer minor's operator is recognized as unknown, at any nesting depth.
        let future = document("forward_compatible_additions");
        let nested = future
            .constraints
            .iter()
            .find(|found| found.id == "future.constraint.nested_unknown_operand")
            .expect("nested constraint present");
        assert!(
            !nested.when.is_fully_recognized(),
            "an unknown operator nested inside `all` must still be detected"
        );
    }

    #[test]
    fn expression_bounds_are_enforced() {
        let mut deep = Expr::IsGrounded {
            control: ControlId::new("a"),
            lane: ValueLane::Observed,
        };
        for _ in 0..MAX_EXPR_DEPTH {
            deep = Expr::Not(Box::new(deep));
        }
        assert!(
            deep.validate().is_err(),
            "an expression deeper than the published bound must be refused"
        );

        let wide = Expr::All(
            (0..=MAX_EXPR_NODES)
                .map(|_| Expr::IsGrounded {
                    control: ControlId::new("a"),
                    lane: ValueLane::Observed,
                })
                .collect(),
        );
        assert!(
            wide.validate().is_err(),
            "an expression with more nodes than the published bound must be refused"
        );
    }

    #[test]
    fn a_matcher_cannot_reinterpret_a_producer_constraint() {
        // Presentation hints and producer safety live in different places on purpose.
        // A widget hint is advisory; permission is not derived from it.
        let doc = document("hqplayer_pipeline_v1");
        let rate = doc
            .control(&ControlId::new("hqplayer.pipeline.rate.exact"))
            .expect("rate control");
        assert_eq!(rate.widget_hint.as_deref(), Some("select"));
        assert!(
            !rate.is_mutable(),
            "no presentation choice can make an unavailable control mutable"
        );

        let subject = constraint("hqplayer.constraint.exact_rate_requires_explicit_mode");
        let denial = subject.permission(&Evaluation::True);
        assert!(matches!(denial, Permission::Denied(_)));
        // Reasons carry codes plus catalog keys; display text is governed separately.
        if let Permission::Denied(reason) = denial {
            assert_eq!(reason.scope, ReasonScope::Draft);
            assert!(reason.display_text_key.is_some());
        }
        let _ = Reason::draft(ReasonCode::DraftInvalid);
    }
}

// ============================================================================
// Choice-set authority
// ============================================================================

mod choice_authority {
    use unified_hifi_control::adaptive::value::{Authority, SourceClass};
    use unified_hifi_control::adaptive::{Choice, ChoiceSet};

    fn runtime() -> ChoiceSet {
        ChoiceSet::runtime(vec![
            Choice::new("poly-sinc-gauss-long", "poly-sinc-gauss-long"),
            Choice::new("brand-new-filter", "brand-new-filter"),
        ])
    }

    fn catalog() -> ChoiceSet {
        ChoiceSet {
            authority: Authority::StaticFallback,
            source: SourceClass::Catalog,
            revision: None,
            choices: vec![
                Choice {
                    id: "poly-sinc-gauss-long".to_string(),
                    label_key: Some("choice.filter.poly_sinc_gauss_long".to_string()),
                    engine_name: None,
                    availability: None,
                },
                Choice {
                    id: "retired-filter".to_string(),
                    label_key: Some("choice.filter.retired".to_string()),
                    engine_name: None,
                    availability: None,
                },
            ],
        }
    }

    #[test]
    fn a_catalog_may_add_labels_but_never_change_the_option_set() {
        let enriched = runtime().enrich_from_catalog(&catalog());

        // Same options, same order.
        assert_eq!(
            enriched
                .choices
                .iter()
                .map(|choice| choice.id.as_str())
                .collect::<Vec<_>>(),
            vec!["poly-sinc-gauss-long", "brand-new-filter"]
        );
        // The catalog cannot introduce an option the engine did not report.
        assert!(!enriched.contains("retired-filter"));
        // Runtime remains the authority.
        assert_eq!(enriched.authority, Authority::Runtime);
        assert_eq!(enriched.source, SourceClass::LiveProtocol);
        // Labels are added where the engine had none.
        assert_eq!(
            enriched.choices[0].label_key.as_deref(),
            Some("choice.filter.poly_sinc_gauss_long")
        );
    }

    #[test]
    fn an_option_the_catalog_has_never_heard_of_stays_renderable() {
        let enriched = runtime().enrich_from_catalog(&catalog());
        let novel = enriched
            .choices
            .iter()
            .find(|choice| choice.id == "brand-new-filter")
            .expect("engine-only option retained");
        assert!(novel.label_key.is_none());
        assert_eq!(novel.engine_name.as_deref(), Some("brand-new-filter"));
    }

    #[test]
    fn enrichment_never_overrides_an_existing_label() {
        let mut set = runtime();
        set.choices[0].label_key = Some("project.authored.label".to_string());
        let enriched = set.enrich_from_catalog(&catalog());
        assert_eq!(
            enriched.choices[0].label_key.as_deref(),
            Some("project.authored.label")
        );
    }
}

// ============================================================================
// Coherence of published intent (specification rules C1 and C2)
// ============================================================================

mod intent_coherence {
    use super::fixtures::{document, CANONICAL};
    use unified_hifi_control::adaptive::{
        ApplyLane, ChangeSetEntry, ControlId, ControlValue, EntryValidity, IntentIncoherence,
        Timestamp, ValueLane,
    };

    #[test]
    fn every_canonical_fixture_publishes_coherent_intent() {
        // C1 + C2 over the shipped examples. If a fixture ever publishes a valid entry
        // without a matching desired lane, constraints over that control would silently
        // evaluate against the running engine instead of the draft.
        for name in CANONICAL {
            let doc = document(name);
            let violations = doc.intent_coherence_violations();
            assert!(
                violations.is_empty(),
                "fixture {name} publishes incoherent intent: {violations:#?}"
            );
        }
    }

    #[test]
    fn a_valid_entry_agrees_with_the_controls_desired_lane() {
        // Spot-check the positive case explicitly rather than only through the aggregate.
        let doc = document("staged_intent_multi_surface");
        let mode = ControlId::new("hqplayer.pipeline.mode");
        let entry = doc
            .change_sets
            .iter()
            .flat_map(|set| &set.entries)
            .find(|entry| entry.control == mode && entry.validity.may_apply())
            .expect("a valid mode entry is staged");
        let published = doc
            .control(&mode)
            .and_then(|control| control.lane(&ValueLane::Desired))
            .and_then(|lane| lane.value.as_ref())
            .expect("the control publishes a desired lane");
        assert_eq!(published, &entry.desired);
    }

    #[test]
    fn c1_a_valid_entry_without_a_desired_lane_is_a_violation() {
        let mut doc = document("staged_intent_multi_surface");
        let orphan = ControlId::new("hqplayer.pipeline.rate.exact");
        // rate.exact is published with an observed lane only. Staging a *valid* entry for
        // it without adding a desired lane is exactly the silent failure C1 forbids.
        doc.change_sets[0].stage(
            ChangeSetEntry {
                control: orphan.clone(),
                lane: ApplyLane::Staged,
                desired: ControlValue::choice("384000"),
                base_observed: None,
                validity: EntryValidity::Valid,
                extensions: Default::default(),
            },
            Timestamp::new("2026-07-30T11:30:00Z"),
        );
        assert_eq!(
            doc.intent_coherence_violations(),
            vec![IntentIncoherence::MissingDesiredLane {
                control: orphan,
                change_set: doc.change_sets[0].id.clone(),
            }]
        );
    }

    #[test]
    fn c1_a_valid_entry_that_disagrees_with_the_lane_is_a_violation() {
        let mut doc = document("staged_intent_multi_surface");
        let mode = ControlId::new("hqplayer.pipeline.mode");
        let entry = doc.change_sets[0]
            .entries
            .iter_mut()
            .find(|entry| entry.control == mode)
            .expect("mode entry present");
        entry.desired = ControlValue::choice("pcm"); // the lane still publishes "sdm"
        assert_eq!(
            doc.intent_coherence_violations(),
            vec![IntentIncoherence::DesiredLaneDisagrees {
                control: mode,
                change_set: doc.change_sets[0].id.clone(),
            }]
        );
    }

    #[test]
    fn c2_two_drafts_cannot_both_hold_a_valid_entry_for_one_control() {
        // One desired lane cannot represent two concurrent drafts, so the second draft
        // must be marked Conflicts rather than Valid.
        let mut doc = document("staged_intent_multi_surface");
        let mode = ControlId::new("hqplayer.pipeline.mode");
        let first = doc.change_sets[0].id.clone();
        let second = doc.change_sets[1].id.clone();
        doc.change_sets[1].stage(
            ChangeSetEntry {
                control: mode.clone(),
                lane: ApplyLane::Staged,
                desired: ControlValue::choice("sdm"),
                base_observed: None,
                validity: EntryValidity::Valid,
                extensions: Default::default(),
            },
            Timestamp::new("2026-07-30T11:31:00Z"),
        );
        assert_eq!(
            doc.intent_coherence_violations(),
            vec![IntentIncoherence::MultipleValidDrafts {
                control: mode,
                change_set: second,
                also_claimed_by: first,
            }]
        );
    }

    #[test]
    fn entries_that_cannot_apply_are_exempt() {
        // The multi-surface fixture deliberately holds stale, conflicting, draft-invalid
        // and needs-validation entries whose values differ from the published desired
        // lane. Apply is already blocked for those, so the effective projection over them
        // is advisory and no lane is required.
        let doc = document("staged_intent_multi_surface");
        let non_applying = doc
            .change_sets
            .iter()
            .flat_map(|set| &set.entries)
            .filter(|entry| !entry.validity.may_apply())
            .count();
        assert!(
            non_applying >= 4,
            "the fixture should exercise several non-applying validities, found {non_applying}"
        );
        assert!(doc.intent_coherence_violations().is_empty());
    }

    #[test]
    fn c1_a_valid_entry_for_a_removed_control_is_a_violation() {
        // Reachable after a control-plane advance removed a control an open draft still
        // targets. Reported distinctly so a consumer can say "this control is gone"
        // rather than the generic "needs producer validation".
        let mut doc = document("staged_intent_multi_surface");
        let mode = ControlId::new("hqplayer.pipeline.mode");
        doc.controls.retain(|control| control.id != mode);
        assert_eq!(
            doc.intent_coherence_violations(),
            vec![IntentIncoherence::UnknownControl {
                control: mode,
                change_set: doc.change_sets[0].id.clone(),
            }]
        );
    }
}
