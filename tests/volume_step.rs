//! Volume step regression tests
//!
//! Bug: Adapters hardcode volume_step to 1.0, ignoring backend-specific values.
//!
//! See: https://github.com/cloud-atlas-ai/unified-hifi-control/issues/152
//!
//! Test strategy:
//! 1. Source-scanning lint tests (catch obvious regressions)
//! 2. Unit tests calling pub(crate) conversion functions (verify behavior)

use std::fs;

// =============================================================================
// LINT TESTS: Source scanning to catch hardcoded step values
// These are a first line of defense, not a replacement for unit tests.
// =============================================================================

/// Roon adapter must not hardcode step: 1.0 - API provides the value.
#[test]
fn lint_roon_no_hardcoded_step() {
    let src =
        fs::read_to_string("src/adapters/roon.rs").expect("Failed to read src/adapters/roon.rs");

    // Bug: roon_zone_to_bus_zone hardcodes "step: 1.0"
    // Fix: use v.step.unwrap_or(1.0)
    let has_hardcoded = src.contains("step: 1.0,");

    assert!(
        !has_hardcoded,
        "REGRESSION: Roon adapter hardcodes 'step: 1.0'.\n\
         Fix: Use v.step.unwrap_or(1.0) in roon_zone_to_bus_zone()"
    );
}

/// LMS adapter must use step: 2.5 (from LMS server), not 1.0.
#[test]
fn lint_lms_uses_correct_step() {
    let src =
        fs::read_to_string("src/adapters/lms.rs").expect("Failed to read src/adapters/lms.rs");

    // LMS server hardcodes $increment = 2.5 in Slim/Player/Client.pm:755
    // Not queryable via API, so we must use the constant.
    let has_correct = src.contains("step: 2.5");

    assert!(
        has_correct,
        "REGRESSION: LMS adapter should use 'step: 2.5'.\n\
         LMS server hardcodes increment=2.5 in Slim/Player/Client.pm:755"
    );
}

/// OpenHome adapter should call Characteristics to get VolumeSteps.
#[test]
fn lint_openhome_queries_characteristics() {
    let src = fs::read_to_string("src/adapters/openhome.rs")
        .expect("Failed to read src/adapters/openhome.rs");

    // OpenHome Volume service exposes VolumeSteps via Characteristics action
    // Step = VolumeMax / VolumeSteps
    let calls_characteristics = src.contains("Characteristics");

    assert!(
        calls_characteristics,
        "OpenHome adapter should query 'Characteristics' action for VolumeSteps.\n\
         See: http://wiki.openhome.org/wiki/Av:Developer:VolumeService"
    );
}

/// HQPlayer correctly uses vol_range.step - ensure we don't regress.
#[test]
fn lint_hqplayer_uses_api_step() {
    let src = fs::read_to_string("src/adapters/hqplayer.rs")
        .expect("Failed to read src/adapters/hqplayer.rs");

    let uses_api = src.contains("vol_range.step");

    assert!(
        uses_api,
        "HQPlayer should use vol_range.step - don't break the working adapter!"
    );
}

// =============================================================================
// API ENDPOINT TESTS: Ensure /zones includes volume_step
// =============================================================================

/// ZoneInfo struct must include volume_control with step field
/// Bug: /zones returns volume_step: null because ZoneInfo doesn't map it from Zone
#[test]
fn lint_zones_endpoint_includes_volume_control() {
    let src =
        fs::read_to_string("src/knobs/routes.rs").expect("Failed to read src/knobs/routes.rs");

    // ZoneInfo must have volume_control field to expose step to clients
    let has_volume_control = src.contains("pub volume_control:") && src.contains("struct ZoneInfo");

    assert!(
        has_volume_control,
        "REGRESSION: ZoneInfo struct must include 'volume_control' field.\n\
         The /zones endpoint returns volume_step: null without this.\n\
         Fix: Add 'pub volume_control: Option<VolumeControl>' to ZoneInfo"
    );
}

/// get_all_zones_internal must map volume_control from Zone to ZoneInfo
#[test]
fn lint_zones_maps_volume_control() {
    let src =
        fs::read_to_string("src/knobs/routes.rs").expect("Failed to read src/knobs/routes.rs");

    // The mapping in get_all_zones_internal must include volume_control
    let maps_volume = src.contains("volume_control: z.volume_control");

    assert!(
        maps_volume,
        "REGRESSION: get_all_zones_internal must map volume_control from Zone.\n\
         Fix: Add 'volume_control: z.volume_control.clone()' to the ZoneInfo mapping"
    );
}

// =============================================================================
// UNIT TESTS: Call actual conversion functions
// Requires: make lms_player_to_zone and roon_zone_to_bus_zone pub(crate)
// =============================================================================

// TODO: Add unit tests once conversion functions are exposed as pub(crate):
//
// #[test]
// fn lms_player_to_zone_uses_step_2_5() {
//     let player = LmsPlayer { volume: 50, ... };
//     let zone = lms_player_to_zone(&player);
//     assert_eq!(zone.volume_control.unwrap().step, 2.5);
// }
//
// #[test]
// fn roon_zone_to_bus_zone_uses_api_step() {
//     let zone = Zone { outputs: vec![Output { volume: Some(VolumeInfo { step: Some(0.5), ... }) }] };
//     let bus_zone = roon_zone_to_bus_zone(&zone);
//     assert_eq!(bus_zone.volume_control.unwrap().step, 0.5);
// }
