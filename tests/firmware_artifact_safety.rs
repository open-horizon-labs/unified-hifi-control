//! Firmware responses must never substitute another hardware image or erase a bootloader
//! with an OTA-only application. These cases exercise the real existing HTTP handlers.
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    routing::get,
    Router,
};
use tower::ServiceExt;
use unified_hifi_control::knobs;

async fn request(path: &str) -> (StatusCode, Vec<u8>) {
    let app = Router::new()
        .route("/firmware/version", get(knobs::firmware_version_handler))
        .route("/firmware/download", get(knobs::firmware_download_handler))
        .route("/manifest-s3.json", get(knobs::manifest_handler));
    let response = app
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    (
        response.status(),
        to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
}

// One test owns UHC_CONFIG_DIR throughout; there are no concurrent environment mutations
// in this integration-test process. All artifacts are disposable temporary fixtures.
#[tokio::test]
async fn legacy_ota_rejects_wrong_missing_and_escaping_images_before_serving_bytes() {
    let temp = tempfile::tempdir().unwrap();
    let previous = std::env::var_os("UHC_CONFIG_DIR");
    std::env::set_var("UHC_CONFIG_DIR", temp.path());
    let firmware = temp.path().join("firmware");
    std::fs::create_dir(&firmware).unwrap();
    std::fs::write(firmware.join("hiphi_frame.bin"), b"wrong hardware").unwrap();

    assert_eq!(
        request("/firmware/download").await.0,
        StatusCode::NOT_FOUND,
        "never fall back to a Frame image when the legacy Dial binary is absent"
    );

    std::fs::write(
        firmware.join("version.json"),
        r#"{"version":"2.5.2","file":"hiphi_frame.bin"}"#,
    )
    .unwrap();
    for path in ["/firmware/download", "/firmware/version"] {
        assert_eq!(
            request(path).await.0,
            StatusCode::NOT_FOUND,
            "wrong-device metadata at {path}"
        );
    }
    std::fs::write(temp.path().join("outside.bin"), b"private file").unwrap();
    std::fs::write(
        firmware.join("version.json"),
        r#"{"version":"2.5.2","file":"../outside.bin"}"#,
    )
    .unwrap();
    for path in ["/firmware/download", "/firmware/version"] {
        assert_eq!(
            request(path).await.0,
            StatusCode::NOT_FOUND,
            "escaping metadata at {path}"
        );
    }
    std::fs::write(firmware.join("roon_knob_merged.bin"), b"full install image").unwrap();
    std::fs::write(
        firmware.join("version.json"),
        r#"{"version":"2.5.2","file":"roon_knob_merged.bin"}"#,
    )
    .unwrap();
    assert_eq!(
        request("/firmware/download").await.0,
        StatusCode::NOT_FOUND,
        "OTA must not receive bootloader/partition merged bytes"
    );

    std::fs::write(
        firmware.join("version.json"),
        r#"{"version":"2.5.2","file":"roon_knob.bin"}"#,
    )
    .unwrap();
    assert_eq!(
        request("/firmware/version").await.0,
        StatusCode::NOT_FOUND,
        "do not advertise nonexistent application with zero size"
    );
    std::fs::write(firmware.join("roon_knob.bin"), b"correct legacy app").unwrap();
    assert_eq!(
        request("/firmware/download").await,
        (StatusCode::OK, b"correct legacy app".to_vec())
    );
    let (status, body) = request("/firmware/version").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"version":"2.5.2","size":18,"file":"roon_knob.bin"})
    );
    assert_eq!(
        request("/manifest-s3.json").await.0,
        StatusCode::NOT_FOUND,
        "clean-install manifest must not point OTA app at offset zero"
    );
    std::fs::write(firmware.join("roon_knob.bin"), b"").unwrap();
    assert_eq!(
        request("/firmware/download").await.0,
        StatusCode::NOT_FOUND,
        "empty firmware is unavailable"
    );
    std::fs::write(firmware.join("roon_knob.bin"), b"correct legacy app").unwrap();
    std::fs::write(firmware.join("version.json"), b"not valid metadata").unwrap();
    assert_eq!(
        request("/firmware/download").await.0,
        StatusCode::NOT_FOUND,
        "corrupt metadata must fail closed"
    );
    std::fs::write(
        firmware.join("roon_knob_v2.5.2.bin"),
        b"versioned legacy app",
    )
    .unwrap();
    std::fs::write(
        firmware.join("version.json"),
        r#"{"file":"roon_knob_v2.5.2.bin"}"#,
    )
    .unwrap();
    assert_eq!(
        request("/firmware/version").await.0,
        StatusCode::OK,
        "existing versioned legacy filenames remain supported"
    );
    #[cfg(unix)]
    {
        std::fs::remove_file(firmware.join("roon_knob.bin")).unwrap();
        std::os::unix::fs::symlink(
            temp.path().join("outside.bin"),
            firmware.join("roon_knob.bin"),
        )
        .unwrap();
        std::fs::write(
            firmware.join("version.json"),
            r#"{"version":"2.5.2","file":"roon_knob.bin"}"#,
        )
        .unwrap();
        assert_eq!(
            request("/firmware/download").await.0,
            StatusCode::NOT_FOUND,
            "named symlink cannot escape firmware storage"
        );
    }

    // The existing poller writes the root; a stale proposed per-target layout must not shadow it.
    #[cfg(unix)]
    std::fs::remove_file(firmware.join("roon_knob.bin")).unwrap();
    std::fs::write(firmware.join("roon_knob.bin"), b"fresh poll").unwrap();
    std::fs::write(
        firmware.join("version.json"),
        r#"{"version":"2.5.2","file":"roon_knob.bin"}"#,
    )
    .unwrap();
    std::fs::create_dir(firmware.join("knob")).unwrap();
    std::fs::write(firmware.join("knob/roon_knob.bin"), b"stale proposed cache").unwrap();
    std::fs::write(
        firmware.join("knob/version.json"),
        r#"{"version":"1.0.0","file":"roon_knob.bin"}"#,
    )
    .unwrap();
    assert_eq!(request("/firmware/download").await.1, b"fresh poll");
    std::fs::remove_dir_all(firmware.join("knob")).unwrap();
    std::fs::write(
        firmware.join("version.json"),
        r#"{"version":"2.5.2","file":"roon_knob_v2.7.0.bin"}"#,
    )
    .unwrap();
    std::fs::write(firmware.join("roon_knob_v2.7.0.bin"), b"new version").unwrap();
    assert_eq!(
        request("/firmware/version").await.0,
        StatusCode::NOT_FOUND,
        "metadata cannot advertise an unrelated version"
    );
    #[cfg(unix)]
    {
        std::fs::write(
            temp.path().join("outside.json"),
            r#"{"version":"2.5.2","file":"roon_knob.bin"}"#,
        )
        .unwrap();
        std::fs::remove_file(firmware.join("version.json")).unwrap();
        std::os::unix::fs::symlink(
            temp.path().join("outside.json"),
            firmware.join("version.json"),
        )
        .unwrap();
        assert_eq!(
            request("/firmware/download").await.0,
            StatusCode::NOT_FOUND,
            "metadata symlink is not trusted"
        );
        let outside = temp.path().join("outside-firmware");
        std::fs::rename(&firmware, &outside).unwrap();
        std::fs::remove_file(outside.join("version.json")).unwrap();
        std::fs::write(
            outside.join("version.json"),
            r#"{"version":"2.5.2","file":"roon_knob.bin"}"#,
        )
        .unwrap();
        std::os::unix::fs::symlink(&outside, &firmware).unwrap();
        assert_eq!(
            request("/firmware/download").await.0,
            StatusCode::NOT_FOUND,
            "parent firmware directory cannot redirect artifacts"
        );
    }
    if let Some(value) = previous {
        std::env::set_var("UHC_CONFIG_DIR", value);
    } else {
        std::env::remove_var("UHC_CONFIG_DIR");
    }
}
