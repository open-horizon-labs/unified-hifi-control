//! Wire-shape contract between the Rust controller endpoints and UHCKit, the
//! Swift client used by the iOS companion and the watchOS controller (#619).
//!
//! `tests/api_contract.rs` already guards which *routes* exist. It says nothing
//! about the *shape* of what they return, so renaming `zone_name` to `name`
//! would sail past it and silently break every Swift surface at runtime — the
//! #611 failure mode, one layer down.
//!
//! This test closes that gap by serializing the real Rust response types and
//! comparing the resulting JSON key sets against
//! `tests/fixtures/uhckit_contract.json`. That same fixture file is decoded by
//! `UHCKitTests.ContractTests` on the Swift side, so one file is the single
//! source of truth for both languages:
//!
//! - Change a Rust field  -> this test fails.
//! - Update the fixture to match, but not the Swift models -> the Swift test fails.
//! - Update both -> the change is deliberate, reviewable, and gets the
//!   `api-change-approved` label like any other API change.
//!
//! Values in the fixture are illustrative; only the key sets and JSON types are
//! contractual. Optionality is contractual too, and is recorded per field in
//! the fixture's `optional` lists, because `#[serde(skip_serializing_if)]` on
//! `ZoneInfo::volume_control` is the difference between a Swift model that
//! decodes the live zone list and one that throws on it.

use serde_json::{Map, Value};
use std::collections::BTreeSet;
use unified_hifi_control::bus::events::{VolumeControl, VolumeScale};
use unified_hifi_control::knobs::routes::{DspInfo, NowPlayingResponse, ZoneInfo};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/uhckit_contract.json"))
        .expect("UHCKit contract fixture must be valid JSON")
}

/// Keys of a JSON object, ignoring `_`-prefixed documentation entries so the
/// fixture can explain itself without polluting the contract.
fn object_keys(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .expect("expected a JSON object")
        .keys()
        .filter(|key| !key.starts_with('_'))
        .cloned()
        .collect()
}

fn string_set(value: &Value) -> BTreeSet<String> {
    value
        .as_array()
        .expect("expected a JSON array of strings")
        .iter()
        .map(|item| item.as_str().expect("expected a string").to_string())
        .collect()
}

/// A `ZoneInfo` with every optional field populated, so serialization emits the
/// maximal key set.
///
/// Built as a struct literal rather than deserialized: `ZoneInfo` derives only
/// `Serialize`, and its `library_tabs: Vec<&'static str>` could not be
/// deserialized anyway. That is a feature here — adding a field to the Rust
/// type makes this file stop compiling, which is an even louder signal than a
/// failing assertion.
fn sample_zone_full() -> ZoneInfo {
    ZoneInfo {
        zone_id: "roon:1601deadbeef".to_string(),
        zone_name: "Front Family Room".to_string(),
        source: "roon".to_string(),
        state: "playing".to_string(),
        volume_control: Some(VolumeControl {
            value: 47.5,
            min: 0.0,
            max: 98.0,
            step: 0.5,
            is_muted: false,
            scale: VolumeScale::Percentage,
            output_id: Some("roon:1701deadbeef".to_string()),
        }),
        dsp: Some(DspInfo {
            r#type: "hqplayer".to_string(),
            instance: Some("embedded".to_string()),
            pipeline: Some("poly-sinc-gauss-long".to_string()),
            profiles: Some("default".to_string()),
        }),
        browse_supported: true,
        library_tabs: vec!["browse", "playlists"],
    }
}

/// The same type with its optional fields absent, which is what the live server
/// publishes for a zone with no volume control (e.g. "HQPlayer Embedded").
fn sample_zone_minimal() -> ZoneInfo {
    ZoneInfo {
        zone_id: "roon:16015f6dcafe".to_string(),
        zone_name: "HQPlayer Embedded".to_string(),
        source: "roon".to_string(),
        state: "stopped".to_string(),
        volume_control: None,
        dsp: None,
        browse_supported: true,
        library_tabs: vec!["browse", "playlists"],
    }
}

fn sample_now_playing() -> NowPlayingResponse {
    NowPlayingResponse {
        zone_id: "roon:1601deadbeef".to_string(),
        line1: "Summer (B-Sides)".to_string(),
        line2: "Moby".to_string(),
        line3: Some("Play & Play: The B Sides".to_string()),
        is_playing: true,
        volume: Some(47.5),
        volume_type: Some("number".to_string()),
        volume_min: Some(0.0),
        volume_max: Some(98.0),
        volume_step: Some(0.5),
        image_url: Some("/knob/now_playing/image?zone_id=roon%3A1601deadbeef".to_string()),
        image_key: Some("b7df27c8dd25dd65084d8f2d94f616d0".to_string()),
        seek_position: Some(104),
        length: Some(358),
        is_play_allowed: false,
        is_pause_allowed: true,
        is_next_allowed: true,
        is_previous_allowed: true,
        zones: vec![],
        config_sha: None,
        zones_sha: Some("4bb54fbe".to_string()),
    }
}

fn serialize(value: &impl serde::Serialize) -> Value {
    serde_json::to_value(value).expect("type must serialize")
}

/// Assert that `actual` carries exactly the keys the fixture declares, and that
/// each shared key has the same JSON type.
fn assert_shape(actual: &Value, expected: &Value, what: &str) {
    let actual_keys = object_keys(actual);
    let expected_keys = object_keys(expected);

    let missing: Vec<_> = expected_keys.difference(&actual_keys).cloned().collect();
    let extra: Vec<_> = actual_keys.difference(&expected_keys).cloned().collect();

    assert!(
        missing.is_empty(),
        "{what}: the Rust type no longer emits {missing:?}, which UHCKit decodes. \
         If this is intentional, update tests/fixtures/uhckit_contract.json AND the \
         matching Swift model in companion/uhckit/Sources/UHCKit/Models.swift, then \
         label the PR api-change-approved."
    );
    assert!(
        extra.is_empty(),
        "{what}: the Rust type now emits {extra:?}, which UHCKit does not know about. \
         Add it to tests/fixtures/uhckit_contract.json and to the Swift model so the \
         Watch and iOS clients can see it, then label the PR api-change-approved."
    );

    let actual_object: &Map<String, Value> = actual.as_object().unwrap();
    let expected_object: &Map<String, Value> = expected.as_object().unwrap();
    for (key, expected_value) in expected_object {
        if key.starts_with('_') {
            continue;
        }
        let actual_value = &actual_object[key];
        // `null` in the fixture documents "nullable"; it constrains presence,
        // not the type carried when the value is present.
        if expected_value.is_null() || actual_value.is_null() {
            continue;
        }
        let expected_kind = json_kind(expected_value);
        let actual_kind = json_kind(actual_value);
        assert_eq!(
            actual_kind, expected_kind,
            "{what}: field `{key}` changed JSON type from {expected_kind} to {actual_kind}; \
             UHCKit decodes it as {expected_kind}."
        );
    }
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[test]
fn zone_shape_matches_uhckit() {
    let fixture = fixture();
    assert_shape(
        &serialize(&sample_zone_full()),
        &fixture["zone"]["full"],
        "GET /zones zone entry",
    );
}

#[test]
fn zone_volume_control_is_omitted_when_absent() {
    // Load-bearing: `volume_control` and `dsp` carry
    // `#[serde(skip_serializing_if = "Option::is_none")]`. If that ever becomes
    // a plain `Option`, the key starts appearing as `null` — harmless — but if
    // the `Option` itself is removed, UHCKit's optional stops matching reality
    // and the live zone list (which really does contain a zone with no volume)
    // fails to decode.
    let minimal = serialize(&sample_zone_minimal());
    let keys = object_keys(&minimal);

    for omitted in ["volume_control", "dsp"] {
        assert!(
            !keys.contains(omitted),
            "a zone without `{omitted}` must omit the key entirely, but got {minimal}"
        );
    }

    let required = string_set(&fixture()["zone"]["always_present"]);
    for key in &required {
        assert!(
            keys.contains(key),
            "zone entry must always carry `{key}`, but got {keys:?}"
        );
    }
}

#[test]
fn now_playing_shape_matches_uhckit() {
    let fixture = fixture();
    assert_shape(
        &serialize(&sample_now_playing()),
        &fixture["now_playing"],
        "GET /now_playing",
    );
}

#[test]
fn now_playing_always_emits_every_key() {
    // `NowPlayingResponse` carries no `skip_serializing_if`, so every optional
    // field is emitted as `null` rather than omitted. UHCKit relies on that
    // only for forward compatibility (it uses decodeIfPresent throughout), but
    // the knob firmware does not, so the property is worth pinning.
    let value = serialize(&sample_now_playing());
    let keys = object_keys(&value);
    let expected = object_keys(&fixture()["now_playing"]);
    assert_eq!(
        keys, expected,
        "every /now_playing key must be present even when null"
    );
}

#[test]
fn control_actions_are_all_accepted_by_the_server() {
    // The action strings UHCKit's `ControlAction` can emit. Each must appear in
    // the match arms of the knob control handler; otherwise the Swift enum can
    // produce a command the server rejects at runtime.
    let source = include_str!("../src/knobs/routes.rs");
    let actions = string_set(&fixture()["control"]["actions"]);

    for action in &actions {
        let needle = format!("\"{action}\"");
        assert!(
            source.contains(&needle),
            "UHCKit can send action `{action}`, but src/knobs/routes.rs never matches it. \
             Either the server dropped support or the Swift `ControlAction` enum drifted."
        );
    }
}

#[test]
fn control_request_shape_matches_uhckit() {
    // `KnobControlRequest` is what UHCKit's `ControlRequest` encodes into.
    // Deserializing the Swift-shaped body proves the field names line up.
    let body = &fixture()["control"]["request"];
    let parsed: unified_hifi_control::knobs::routes::KnobControlRequest =
        serde_json::from_value(body.clone())
            .expect("the body UHCKit encodes must deserialize into KnobControlRequest");

    assert_eq!(parsed.zone_id, "roon:1601deadbeef");
    assert_eq!(parsed.action, "vol_abs");
    assert_eq!(
        parsed.value.as_ref().and_then(Value::as_f64),
        Some(31.5),
        "`value` must survive as a number: it carries absolute volume"
    );
}
