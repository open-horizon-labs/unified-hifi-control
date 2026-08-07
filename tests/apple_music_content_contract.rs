//! Executable contract fixtures for the proposed Apple Music content bridge (#463).
//!
//! These tests intentionally validate only the proposed, redacted envelopes.
//! They do not enable routes or claim that a companion can execute them before
//! the API-contract gate is approved.

use serde_json::Value;
use std::collections::BTreeSet;

const OPERATIONS: &[&str] = &[
    "catalog_search",
    "library",
    "playlists",
    "playlist_tracks",
    "recent",
    "recommendations",
    "play_ref",
    "queue_plan",
    "playlist_create",
    "playlist_add",
    "playlist_update",
    "favorite_add",
    "rating_set",
    "context",
];
const OUTCOMES: &[&str] = &[
    "success",
    "unsupported",
    "unauthorized",
    "subscription_required",
    "restricted",
    "not_found",
    "offline",
    "rate_limited",
    "stale_owner",
    "conflict",
    "invalid",
    "failed",
];
const FORBIDDEN_KEYS: &[&str] = &[
    "access_token",
    "developer_token",
    "authorization",
    "authorization_material",
    "apple_id",
    "raw_apple_id",
    "audio",
    "email",
];

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/apple_music_content_bridge.json"))
        .expect("Apple content bridge fixture must be valid JSON")
}

fn keys(value: &Value) -> BTreeSet<String> {
    value
        .as_array()
        .expect("fixture vocabulary must be an array")
        .iter()
        .map(|item| {
            item.as_str()
                .expect("vocabulary entry must be a string")
                .to_string()
        })
        .collect()
}

fn assert_no_forbidden_keys(value: &Value) {
    match value {
        Value::Object(object) => {
            for key in object.keys() {
                assert!(
                    !FORBIDDEN_KEYS
                        .iter()
                        .any(|forbidden| key.eq_ignore_ascii_case(forbidden)),
                    "forbidden provider/credential key crossed the proposed bridge: {key}"
                );
            }
            for child in object.values() {
                assert_no_forbidden_keys(child);
            }
        }
        Value::Array(items) => items.iter().for_each(assert_no_forbidden_keys),
        _ => {}
    }
}

#[test]
fn fixture_pins_the_proposed_operation_and_outcome_vocabularies() {
    let value = fixture();
    assert_eq!(
        keys(&value["operations"]),
        OPERATIONS.iter().map(|v| v.to_string()).collect()
    );
    assert_eq!(
        keys(&value["outcomes"]),
        OUTCOMES.iter().map(|v| v.to_string()).collect()
    );
}

#[test]
fn requests_are_bounded_redacted_and_mutations_are_safeguarded() {
    let value = fixture();
    assert_no_forbidden_keys(&value);
    let refused = value["refused_operations"]
        .as_array()
        .expect("refused operations");
    assert!(refused
        .iter()
        .any(|operation| operation == "playlist_remove"));
    assert!(refused
        .iter()
        .any(|operation| operation == "favorite_remove"));
    assert!(refused
        .iter()
        .any(|operation| operation == "library_remove"));

    let params = &value["success"]["request"]["params"];
    assert!(params["limit"].as_u64().is_some_and(|limit| limit <= 50));
    assert_eq!(value["success"]["request"]["confirm"], false);

    let mutation = &value["mutation"]["request"];
    assert_eq!(mutation["confirm"], true);
    assert!(mutation["idempotency_key"].as_str().is_some());
    assert!(mutation["precondition"].is_object());
    assert!(mutation["params"]["item_refs"]
        .as_array()
        .is_some_and(|items| items.len() <= 200));
}

#[test]
fn successful_items_use_owner_scoped_opaque_refs_and_bounded_errors() {
    let value = fixture();
    let item = &value["success"]["response"]["data"]["items"][0];
    assert!(item["ref"]
        .as_str()
        .is_some_and(|reference| reference.starts_with("ref_")));
    assert_eq!(item["provider"], "applemusic");
    assert!(matches!(
        item["source_kind"].as_str(),
        Some("catalog" | "library" | "playlist" | "recent" | "recommendation")
    ));

    for example in value["outcome_examples"]
        .as_array()
        .expect("outcome examples")
    {
        assert!(OUTCOMES.contains(&example["outcome"].as_str().expect("outcome")));
        assert!(example["error"]["message"]
            .as_str()
            .is_some_and(|message| message.len() <= 512));
        assert!(example["error"]["code"]
            .as_str()
            .is_some_and(|code| code.len() <= 128));
    }
}

#[test]
fn proposal_document_mentions_every_fixture_vocabulary_value() {
    let document = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docs/apple-music-content-bridge.md"
    ))
    .expect("content bridge proposal must be present");
    for value in OPERATIONS.iter().chain(OUTCOMES.iter()) {
        assert!(
            document.contains(value),
            "proposal is missing vocabulary value {value}"
        );
    }
    for value in ["playlist_remove", "favorite_remove", "library removal"] {
        assert!(
            document.contains(value),
            "proposal is missing refusal guardrail {value}"
        );
    }
}
