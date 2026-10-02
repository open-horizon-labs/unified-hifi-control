//! Device identity survives legacy requests and restarts without guessing from firmware versions.
use unified_hifi_control::knobs::store::{Knob, KnobStore};

#[tokio::test]
async fn controller_identity_is_canonical_persistent_and_never_erased_by_legacy_requests() {
    let temporary = tempfile::tempdir().unwrap();
    let previous = std::env::var_os("UHC_CONFIG_DIR");
    std::env::set_var("UHC_CONFIG_DIR", temporary.path());
    let store = KnobStore::new();
    let legacy = store.get_or_create("legacy", Some("2.5.2")).await;
    assert_eq!(
        serde_json::to_value(&legacy).unwrap()["device_type"],
        "knob"
    );
    let frame = store
        .get_or_create_with_device_type("frame", Some("2.7.0-alpha.5"), Some("hiphi-frame"))
        .await;
    let sha = frame.config_sha.clone();
    assert_eq!(
        serde_json::to_value(&frame).unwrap()["device_type"],
        "frame"
    );
    for identity in [None, Some("frmae"), Some("../dial"), Some("")] {
        let preserved = store
            .get_or_create_with_device_type("frame", None, identity)
            .await;
        assert_eq!(
            serde_json::to_value(&preserved).unwrap()["device_type"],
            "frame"
        );
        assert_eq!(
            preserved.config_sha, sha,
            "identity must not reset power settings or config hash"
        );
    }
    let m5 = store
        .get_or_create_with_device_type("m5", None, Some("hiphi-dial-beta"))
        .await;
    assert_eq!(serde_json::to_value(&m5).unwrap()["device_type"], "m5dial");
    let restored = KnobStore::new();
    let summaries = serde_json::to_value(restored.list().await).unwrap();
    assert_eq!(
        summaries
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["knob_id"] == "frame")
            .unwrap()["device_type"],
        "frame"
    );
    let mut old_record = serde_json::to_value(&legacy).unwrap();
    old_record.as_object_mut().unwrap().remove("device_type");
    let migrated: Knob = serde_json::from_value(old_record).unwrap();
    assert_eq!(
        serde_json::to_value(migrated).unwrap()["device_type"],
        "knob"
    );
    if let Some(value) = previous {
        std::env::set_var("UHC_CONFIG_DIR", value);
    } else {
        std::env::remove_var("UHC_CONFIG_DIR");
    }
}
