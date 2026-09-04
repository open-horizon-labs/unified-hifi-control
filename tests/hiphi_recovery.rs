use unified_hifi_control::app::api::HiphiPairingStatus;

#[test]
fn stopped_connector_does_not_promise_retries() {
    let status: HiphiPairingStatus = serde_json::from_value(serde_json::json!({
        "paired": true, "installation_id": "test", "connector_state": "offline"
    }))
    .unwrap();
    assert_eq!(status.display_state(), "Paired · offline");
}

#[test]
fn quarantined_connector_explains_pause() {
    let status: HiphiPairingStatus = serde_json::from_value(serde_json::json!({
        "paired": true, "installation_id": "test", "connector_state": "paused",
        "pause_reason": "cost_limit", "can_resume": true
    }))
    .unwrap();
    assert_eq!(status.display_state(), "Cloud paused · cost protection");
}
