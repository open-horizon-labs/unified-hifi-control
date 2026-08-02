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
    assert_eq!(min, -64.0);
    assert_eq!(max, 0.0);
}

#[test]
fn critical_db_minus12_stays_minus12() {
    // THIS IS THE BUG THAT DAMAGED EQUIPMENT
    // -12 dB is a reasonable listening level, NOT maximum volume
    let output = db_output();
    let (min, max) = get_volume_range(Some(&output));
    assert_eq!(clamp(-12.0, min, max), -12.0);
}

#[test]
fn db_values_below_zone_min_clamp_to_min() {
    let output = db_output();
    let (min, max) = get_volume_range(Some(&output));
    assert_eq!(clamp(-100.0, min, max), -64.0);
}

#[test]
fn db_values_above_zone_max_clamp_to_max() {
    let output = db_output();
    let (min, max) = get_volume_range(Some(&output));
    assert_eq!(clamp(10.0, min, max), 0.0);
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
    assert_eq!(min, 0.0);
    assert_eq!(max, 100.0);
}

#[test]
fn pct_50_stays_50() {
    let output = pct_output();
    let (min, max) = get_volume_range(Some(&output));
    assert_eq!(clamp(50.0, min, max), 50.0);
}

#[test]
fn pct_values_below_0_clamp_to_0() {
    let output = pct_output();
    let (min, max) = get_volume_range(Some(&output));
    assert_eq!(clamp(-10.0, min, max), 0.0);
}

#[test]
fn pct_values_above_100_clamp_to_100() {
    let output = pct_output();
    let (min, max) = get_volume_range(Some(&output));
    assert_eq!(clamp(150.0, min, max), 100.0);
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
    assert_eq!(min, 0.0);
    assert_eq!(max, 100.0);
}

#[test]
fn none_output_uses_safe_defaults() {
    let (min, max) = get_volume_range(None);
    assert_eq!(min, 0.0);
    assert_eq!(max, 100.0);
}

// =============================================================================
// Clamp edge cases
// =============================================================================

#[test]
fn clamp_value_exactly_at_min_boundary() {
    assert_eq!(clamp(-64.0, -64.0, 0.0), -64.0);
    assert_eq!(clamp(0.0, 0.0, 100.0), 0.0);
}

#[test]
fn clamp_value_exactly_at_max_boundary() {
    assert_eq!(clamp(0.0, -64.0, 0.0), 0.0);
    assert_eq!(clamp(100.0, 0.0, 100.0), 100.0);
}

#[test]
fn clamp_value_in_middle_of_range() {
    assert_eq!(clamp(-32.0, -64.0, 0.0), -32.0);
    assert_eq!(clamp(50.0, 0.0, 100.0), 50.0);
}

// =============================================================================
// Relative volume safety
// =============================================================================

#[test]
fn relative_step_clamped_prevents_wild_jumps() {
    // Even if someone sends +50, it should be clamped to MAX_RELATIVE_STEP (10)
    let max_step = 10.0;
    assert_eq!(clamp(50.0, -max_step, max_step), max_step);
    assert_eq!(clamp(-50.0, -max_step, max_step), -max_step);
}

// =============================================================================
// Direct HQPlayer volume path (issue #328)
// =============================================================================
//
// `tests/architecture_lint.rs::volume_values_use_safe_defaults` is the existing guard, and it could
// not see the defect #328 found. Two reasons, both structural rather than accidental:
//
//   1. It walks `src/adapters` only. The routing surface that decides what level to write lives in
//      `src/knobs/routes.rs` (`dispatch_hqplayer_action`, shared with MCP's `hifi_control` since #401).
//   2. It matches the literal `.value.unwrap_or(50.0)`. The real line read
//      `value.and_then(|v| v.as_f64()).unwrap_or(50.0)` — same hazard, different spelling, no match.
//
// So this is deliberately not "the same lint with another directory added". It asserts a property of
// the HQPlayer control path — *no numeric literal is ever substituted for a missing level* — which
// holds regardless of how the substitution is spelled.
//
// The behavioural counterpart is
// `hqplayer_direct_zone::a_missing_or_unparseable_level_is_refused_never_defaulted`, which proves the
// daemon receives nothing. This lint exists because that test can only cover the paths it calls,
// while a future edit could add a fifth volume arm with a default and break no existing test.

/// The body of `hqp_command_from_published_zone`, from its signature to the sibling that follows
/// it. The reliable command runtime moved native I/O out of the surface dispatcher; request-value
/// validation and conversion now live in this pure semantic-command builder.
///
/// The safety-critical capability checks and volume clamp/no-default logic used to live inline in
/// `control_hqplayer`; #401 extracted them into this transport-neutral function (shared by the HTTP
/// knob handler and MCP's `hifi_control` tool) so the same command core backs both surfaces. The
/// lint follows the logic to its new home rather than continuing to describe `control_hqplayer`,
/// which is now a thin wrapper with none of these arms in its own body.
fn hqp_command_from_published_zone_body() -> String {
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/knobs/routes.rs"),
    )
    .expect("read the routing surface");

    let start = source.find("fn hqp_command_from_published_zone(").expect(
        "hqp_command_from_published_zone must exist: HQPlayer values are validated before bus dispatch",
    );
    let rest = &source[start..];
    let end = rest
        .find("\nasync fn control_hqplayer(")
        .expect("the function that follows hqp_command_from_published_zone must still follow it");
    rest[..end].to_string()
}

#[test]
fn the_hqplayer_volume_path_never_substitutes_a_numeric_level() {
    let body = hqp_command_from_published_zone_body();

    // Non-vacuity: the arms this is a claim about have to be present, or the lint passes by
    // describing a function that no longer does the thing.
    for marker in ["Command::VolumeAbsolute", "vol_abs", "vol_up"] {
        assert!(
            body.contains(marker),
            "`{marker}` is gone from hqp_command_from_published_zone; this lint is now vacuous and must be \
             updated alongside whatever replaced it"
        );
    }

    // A numeric default anywhere in this function is a level nobody asked for. For a dB zone the
    // familiar constants are the dangerous ones — `0.0` is full scale and `50.0` is above every
    // possible maximum — so no `unwrap_or` with a numeric argument is permitted at all. The safe
    // shapes are `filter(...)` + an early return, or `clamp` between two *observed* bounds.
    let mut offenders = Vec::new();
    for (index, line) in body.lines().enumerate() {
        // Comments discuss the hazard by name — including the two constants above — and a lint that
        // cannot tell an explanation from an occurrence trains people to delete the explanation.
        if line.trim_start().starts_with("//") {
            continue;
        }
        let Some(after) = line.split("unwrap_or(").nth(1) else {
            continue;
        };
        let argument = after.trim_start();
        let starts_numeric = argument
            .chars()
            .next()
            .map(|c| c.is_ascii_digit() || c == '-' || c == '+')
            .unwrap_or(false);
        if starts_numeric {
            offenders.push(format!("  line +{}: {}", index + 1, line.trim()));
        }
    }

    assert!(
        offenders.is_empty(),
        "\n\nSAFETY: dispatch_hqplayer_action substitutes a numeric volume level.\n\n\
         A level the client did not supply must be REFUSED, not invented. On a dB zone 0.0 is full \
         scale and 50.0 is above every possible maximum, and this path writes straight to a DAC.\n\n\
         Offending lines:\n{}\n\n\
         Safe shapes: `.filter(|v| v.is_finite())` with an early return for `None`, or `clamp` \
         between the zone's own observed `min`/`max`.\n",
        offenders.join("\n")
    );
}

#[test]
fn the_hqplayer_volume_path_clamps_to_the_zones_observed_bounds() {
    let surface = hqp_command_from_published_zone_body();

    // Absolute requests can be clamped immediately to the aggregator's observed range.
    let surface_clamps: Vec<&str> = surface
        .lines()
        .filter(|line| line.contains(".clamp("))
        .collect();
    assert_eq!(
        surface_clamps.len(),
        1,
        "the absolute HQPlayer volume arm must have exactly one surface clamp"
    );
    assert!(
        surface_clamps[0].contains("vc.min") && surface_clamps[0].contains("vc.max"),
        "the absolute volume clamp must use the zone's observed bounds: {}",
        surface_clamps[0].trim()
    );

    // Relative requests cannot safely resolve against the published value: simultaneous knob
    // turns may all have observed the same revision. They must remain relative until the adapter's
    // serialized native endpoint reads the current State and VolumeRange, then clamps there.
    assert!(
        surface.contains("Ok(Command::VolumeRelative"),
        "relative HQPlayer volume must remain semantic until serialized native execution"
    );
    let adapter = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/adapters/hqplayer.rs"),
    )
    .expect("read the HQPlayer adapter");
    let start = adapter
        .find("async fn adjust_volume_db_verified(")
        .expect("the serialized relative-volume endpoint must exist");
    let rest = &adapter[start..];
    let end = rest
        .find("\n    /// Resolve Play/Pause")
        .expect("the function following relative-volume execution must still follow it");
    let relative = &rest[..end];
    for marker in [
        "operation_lock.lock().await",
        "get_volume_range_with_generation().await",
        "get_state_on_transport(generation).await",
        ".clamp(range.min_db, range.max_db)",
    ] {
        assert!(
            relative.contains(marker),
            "serialized relative-volume safety lost `{marker}`"
        );
    }
}
