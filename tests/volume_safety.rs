//! SAFETY CRITICAL: Volume control regression tests
//!
//! Bug: vol_abs used hardcoded 0-100 range, causing dB values like -12
//! to be clamped to 0 (maximum volume), risking equipment damage.
//!
//! Fix: Use zone's actual volume range (e.g., -64 to 0 dB).

use unified_hifi_control::adapters::roon::{clamp, get_volume_range, Output, VolumeInfo};

// =============================================================================
// dB scale zones (HQPlayer-like)
// =============================================================================

fn db_output() -> Output {
    Output {
        output_id: "hqp".to_string(),
        display_name: "HQPlayer".to_string(),
        volume: Some(VolumeInfo {
            value: Some(-20.0),
            min: Some(-64.0),
            max: Some(0.0),
            is_muted: Some(false),
            step: Some(1.0),
        }),
    }
}

#[test]
fn db_zone_respects_db_range() {
    let output = db_output();
    let (min, max) = get_volume_range(Some(&output));
    assert_eq!(min, -64);
    assert_eq!(max, 0);
}

#[test]
fn critical_db_minus12_stays_minus12() {
    // THIS IS THE BUG THAT DAMAGED EQUIPMENT
    // -12 dB is a reasonable listening level, NOT maximum volume
    let output = db_output();
    let (min, max) = get_volume_range(Some(&output));
    assert_eq!(clamp(-12, min, max), -12);
}

#[test]
fn db_values_below_zone_min_clamp_to_min() {
    let output = db_output();
    let (min, max) = get_volume_range(Some(&output));
    assert_eq!(clamp(-100, min, max), -64);
}

#[test]
fn db_values_above_zone_max_clamp_to_max() {
    let output = db_output();
    let (min, max) = get_volume_range(Some(&output));
    assert_eq!(clamp(10, min, max), 0);
}

// =============================================================================
// Percentage scale zones (typical devices)
// =============================================================================

fn pct_output() -> Output {
    Output {
        output_id: "sonos".to_string(),
        display_name: "Sonos".to_string(),
        volume: Some(VolumeInfo {
            value: Some(50.0),
            min: Some(0.0),
            max: Some(100.0),
            is_muted: Some(false),
            step: Some(1.0),
        }),
    }
}

#[test]
fn pct_zone_respects_0_100_range() {
    let output = pct_output();
    let (min, max) = get_volume_range(Some(&output));
    assert_eq!(min, 0);
    assert_eq!(max, 100);
}

#[test]
fn pct_50_stays_50() {
    let output = pct_output();
    let (min, max) = get_volume_range(Some(&output));
    assert_eq!(clamp(50, min, max), 50);
}

#[test]
fn pct_values_below_0_clamp_to_0() {
    let output = pct_output();
    let (min, max) = get_volume_range(Some(&output));
    assert_eq!(clamp(-10, min, max), 0);
}

#[test]
fn pct_values_above_100_clamp_to_100() {
    let output = pct_output();
    let (min, max) = get_volume_range(Some(&output));
    assert_eq!(clamp(150, min, max), 100);
}

// =============================================================================
// Missing volume info (fallback)
// =============================================================================

#[test]
fn missing_volume_uses_safe_defaults() {
    let output = Output {
        output_id: "no-vol".to_string(),
        display_name: "No Volume".to_string(),
        volume: None,
    };
    let (min, max) = get_volume_range(Some(&output));
    assert_eq!(min, 0);
    assert_eq!(max, 100);
}

#[test]
fn none_output_uses_safe_defaults() {
    let (min, max) = get_volume_range(None);
    assert_eq!(min, 0);
    assert_eq!(max, 100);
}

// =============================================================================
// Clamp edge cases
// =============================================================================

#[test]
fn clamp_value_exactly_at_min_boundary() {
    assert_eq!(clamp(-64, -64, 0), -64);
    assert_eq!(clamp(0, 0, 100), 0);
}

#[test]
fn clamp_value_exactly_at_max_boundary() {
    assert_eq!(clamp(0, -64, 0), 0);
    assert_eq!(clamp(100, 0, 100), 100);
}

#[test]
fn clamp_value_in_middle_of_range() {
    assert_eq!(clamp(-32, -64, 0), -32);
    assert_eq!(clamp(50, 0, 100), 50);
}

// =============================================================================
// Relative volume safety
// =============================================================================

#[test]
fn relative_step_clamped_prevents_wild_jumps() {
    // Even if someone sends +50, it should be clamped to MAX_RELATIVE_STEP (10)
    let max_step = 10;
    assert_eq!(clamp(50, -max_step, max_step), max_step);
    assert_eq!(clamp(-50, -max_step, max_step), -max_step);
}
