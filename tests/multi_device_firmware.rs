//! Real HTTP selection must never flash another target or silently opt into prereleases.
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    routing::get,
    Router,
};
use tower::ServiceExt;
use unified_hifi_control::knobs;

async fn request(path: &str, identity: Option<&str>) -> (StatusCode, Vec<u8>) {
    let app = Router::new()
        .route("/firmware/version", get(knobs::firmware_version_handler))
        .route("/firmware/download", get(knobs::firmware_download_handler));
    let mut request = Request::builder().uri(path);
    if let Some(identity) = identity {
        request = request.header("X-Device-Type", identity);
    }
    let response = app
        .oneshot(request.body(Body::empty()).unwrap())
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

fn cache(root: &std::path::Path, directory: &str, file: &str, version: &str, bytes: &[u8]) {
    let directory = root.join("firmware").join(directory);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join(file), bytes).unwrap();
    std::fs::write(
        directory.join("version.json"),
        serde_json::json!({"version":version,"file":file}).to_string(),
    )
    .unwrap();
}

#[tokio::test]
async fn target_and_channel_selection_isolates_bytes_and_honors_actual_firmware_header() {
    let directory = tempfile::tempdir().unwrap();
    std::env::set_var("UHC_CONFIG_DIR", directory.path());
    cache(directory.path(), "", "roon_knob.bin", "2.5.2", b"legacy");
    assert_eq!(
        request("/firmware/download?device_type=frame", None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    cache(
        directory.path(),
        "stable/frame",
        "hiphi_frame_v3.0.0.bin",
        "3.0.0",
        b"frame",
    );
    cache(
        directory.path(),
        "alpha/frame",
        "hiphi_frame_v3.1.0-alpha.5.bin",
        "3.1.0-alpha.5",
        b"alpha frame",
    );
    cache(
        directory.path(),
        "stable/tough",
        "hiphi_tough_v3.0.0.bin",
        "3.0.0",
        b"tough",
    );
    assert_eq!(
        request("/firmware/download", None).await,
        (StatusCode::OK, b"legacy".to_vec())
    );
    assert_eq!(
        request("/firmware/download?device_type=frame", None).await,
        (StatusCode::OK, b"frame".to_vec())
    );
    assert_eq!(
        request("/firmware/download", Some("hiphi-frame")).await,
        (StatusCode::OK, b"frame".to_vec())
    );
    assert_eq!(
        request("/firmware/download?device_type=tough", Some("hiphi-tough"))
            .await
            .1,
        b"tough"
    );
    assert_eq!(
        request("/firmware/download?device_type=frame&channel=alpha", None)
            .await
            .1,
        b"alpha frame"
    );
    assert_eq!(
        request("/firmware/download?device_type=frame&channel=beta", None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    for path in [
        "/firmware/version?device_type=oops",
        "/firmware/download?device_type=../../frame",
        "/firmware/version?channel=nightly",
    ] {
        assert_eq!(request(path, None).await.0, StatusCode::BAD_REQUEST);
    }
    assert_eq!(
        request("/firmware/download?device_type=frame", Some("hiphi-tough"))
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request("/firmware/download", Some("hiphi-dial")).await.0,
        StatusCode::NOT_FOUND,
        "modern Dial stable must not borrow legacy bytes"
    );
    let (status, bytes) = request("/firmware/version?device_type=frame", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
        serde_json::json!({"version":"3.0.0","file":"hiphi_frame_v3.0.0.bin","size":5})
    );
    cache(
        directory.path(),
        "stable/frame",
        "hiphi_tough.bin",
        "3.0.0",
        b"wrong device",
    );
    assert_eq!(
        request("/firmware/download?device_type=frame", None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    cache(
        directory.path(),
        "stable/frame",
        "hiphi_frame_v3.1.0-alpha.5.bin",
        "3.1.0-alpha.5",
        b"wrong channel",
    );
    assert_eq!(
        request("/firmware/download?device_type=frame", None)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    std::fs::write(
        directory.path().join("firmware/stable/frame/version.json"),
        r#"{"file":"hiphi_frame_v3.1.0-alpha.5.bin"}"#,
    )
    .unwrap();
    assert_eq!(
        request("/firmware/download?device_type=frame", None)
            .await
            .0,
        StatusCode::NOT_FOUND,
        "missing version cannot hide alpha payload in stable cache"
    );
    #[cfg(unix)]
    {
        std::fs::remove_dir_all(directory.path().join("firmware/stable")).unwrap();
        std::os::unix::fs::symlink(
            directory.path().join("firmware/alpha"),
            directory.path().join("firmware/stable"),
        )
        .unwrap();
        assert_eq!(
            request("/firmware/download?device_type=frame", None)
                .await
                .0,
            StatusCode::NOT_FOUND,
            "channel ancestor symlink must not redirect cache"
        );
    }
    std::env::remove_var("UHC_CONFIG_DIR");
}
