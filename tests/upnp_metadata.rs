//! UPnP track metadata, driven against a mock renderer (#420).
//!
//! The UPnP adapter previously reported no track metadata at all, behind a
//! comment claiming "pure UPnP doesn't provide track metadata". It does:
//! `AVTransport::GetPositionInfo` returns `TrackMetaData` carrying DIDL-Lite.
//! These tests drive the real poll path, so they fail against that behavior.

mod mock_servers;

use mock_servers::upnp::MockUpnpRenderer;
use unified_hifi_control::adapters::upnp::UPnPAdapter;
use unified_hifi_control::bus::create_bus;

const ART: &str = "http://10.0.0.5/art/42.jpg";

/// Register the mock with the adapter and poll it once.
async fn probe(adapter: &UPnPAdapter, mock: &MockUpnpRenderer) {
    let addr = mock.addr();
    adapter
        .probe_renderer_for_test(
            "mock-upnp-uuid-12345",
            "Mock UPnP Renderer",
            &format!("http://{}/AVTransport/control", addr),
            &format!("http://{}/RenderingControl/control", addr),
        )
        .await
        .expect("probe succeeds");
}

#[tokio::test]
async fn upnp_reports_track_metadata_from_get_position_info() {
    let mock = MockUpnpRenderer::start().await;
    mock.set_state("PLAYING").await;
    mock.set_track(
        "http://10.0.0.5/stream/42.flac",
        "Hoppipolla",
        "Sigur Ros",
        "Takk...",
        ART,
    )
    .await;

    let bus = create_bus();
    let adapter = UPnPAdapter::new(bus);
    probe(&adapter, &mock).await;

    let np = adapter
        .get_now_playing("mock-upnp-uuid-12345")
        .await
        .expect("renderer is known");

    assert_eq!(np.line1, "Hoppipolla", "line1 should be the track title");
    assert_eq!(np.line2, "Sigur Ros", "line2 should be the artist");
    assert_eq!(np.line3, "Takk...", "line3 should be the album");
    assert_eq!(
        np.image_key.as_deref(),
        Some(ART),
        "image_key should come from upnp:albumArtURI, which carries a \
         dlna:profileID attribute in real DIDL-Lite"
    );

    mock.stop().await;
}

#[tokio::test]
async fn upnp_zone_carries_now_playing() {
    let mock = MockUpnpRenderer::start().await;
    mock.set_state("PLAYING").await;
    mock.set_track("uri://1", "Svefn-g-englar", "Sigur Ros", "Agaetis", ART)
        .await;

    let bus = create_bus();
    let adapter = UPnPAdapter::new(bus);
    probe(&adapter, &mock).await;

    let zones = adapter.get_zones().await;
    let zone = zones.first().expect("one zone");
    // UPnPZone carries the bare uuid; the `upnp:` prefix is applied at the
    // aggregator/API boundary, not inside the adapter's own DTO.
    assert_eq!(zone.zone_id, "mock-upnp-uuid-12345");

    let renderer = adapter
        .get_renderer("mock-upnp-uuid-12345")
        .await
        .expect("renderer is known");
    let track = renderer
        .track_info
        .as_ref()
        .expect("track metadata is parsed and retained");
    assert_eq!(track.title, "Svefn-g-englar");
    assert_eq!(track.album_art_uri.as_deref(), Some(ART));

    mock.stop().await;
}

/// A renderer that reports no metadata must degrade to the previous behavior
/// rather than erroring or emitting empty-string noise.
#[tokio::test]
async fn upnp_without_metadata_degrades_cleanly() {
    let mock = MockUpnpRenderer::start().await;
    mock.set_state("PLAYING").await;
    mock.clear_track().await;

    let bus = create_bus();
    let adapter = UPnPAdapter::new(bus);
    probe(&adapter, &mock).await;

    let np = adapter
        .get_now_playing("mock-upnp-uuid-12345")
        .await
        .expect("renderer is known");

    // Falls back to the renderer's own name, as it always did.
    assert_eq!(np.line1, "Mock UPnP Renderer");
    assert_eq!(np.line2, "");
    assert_eq!(np.line3, "");
    assert!(np.image_key.is_none());

    let renderer = adapter
        .get_renderer("mock-upnp-uuid-12345")
        .await
        .expect("renderer is known");
    assert!(renderer.track_info.is_none());

    mock.stop().await;
}

/// Metadata is only re-parsed when the TrackURI changes, mirroring the
/// OpenHome adapter's guard so polling does not re-parse every tick.
#[tokio::test]
async fn upnp_track_metadata_survives_a_repeat_poll() {
    let mock = MockUpnpRenderer::start().await;
    mock.set_state("PLAYING").await;
    mock.set_track("uri://stable", "Glosoli", "Sigur Ros", "Takk...", ART)
        .await;

    let bus = create_bus();
    let adapter = UPnPAdapter::new(bus);
    probe(&adapter, &mock).await;
    // Second poll with an unchanged TrackURI must not lose what was parsed.
    probe(&adapter, &mock).await;

    let renderer = adapter
        .get_renderer("mock-upnp-uuid-12345")
        .await
        .expect("renderer is known");
    let track = renderer.track_info.as_ref().expect("metadata retained");
    assert_eq!(track.title, "Glosoli");
    assert_eq!(renderer.last_track_uri.as_deref(), Some("uri://stable"));

    mock.stop().await;
}

/// Internet radio and many DLNA servers keep one stream URI for a whole session
/// and change only `TrackMetaData` per track. A guard comparing `TrackURI` alone
/// pins now-playing to the first track forever.
#[tokio::test]
async fn upnp_reparses_when_metadata_changes_under_a_constant_track_uri() {
    let mock = MockUpnpRenderer::start().await;
    mock.set_state("PLAYING").await;
    mock.set_track(
        "radio://stream",
        "First Song",
        "First Artist",
        "Album A",
        ART,
    )
    .await;

    let bus = create_bus();
    let adapter = UPnPAdapter::new(bus);
    probe(&adapter, &mock).await;

    let first = adapter
        .get_now_playing("mock-upnp-uuid-12345")
        .await
        .expect("renderer is known");
    assert_eq!(first.line1, "First Song");

    // Same TrackURI, different metadata — as a radio stream reports a new track.
    mock.set_track(
        "radio://stream",
        "Second Song",
        "Second Artist",
        "Album B",
        "http://10.0.0.5/art/99.jpg",
    )
    .await;
    probe(&adapter, &mock).await;

    let second = adapter
        .get_now_playing("mock-upnp-uuid-12345")
        .await
        .expect("renderer is known");
    assert_eq!(
        second.line1, "Second Song",
        "a constant TrackURI must not freeze now-playing metadata"
    );
    assert_eq!(second.line2, "Second Artist");
    assert_eq!(second.line3, "Album B");
    assert_eq!(
        second.image_key.as_deref(),
        Some("http://10.0.0.5/art/99.jpg")
    );

    mock.stop().await;
}

/// track_metadata must not be advertised as unsupported now that it is read.
#[tokio::test]
async fn upnp_zone_does_not_advertise_track_metadata_as_unsupported() {
    let mock = MockUpnpRenderer::start().await;
    mock.set_track("uri://1", "Track", "Artist", "Album", ART)
        .await;

    let bus = create_bus();
    let adapter = UPnPAdapter::new(bus);
    probe(&adapter, &mock).await;

    let zones = adapter.get_zones().await;
    let zone = zones.first().expect("one zone");
    assert!(
        !zone.unsupported.contains(&"track_metadata".to_string()),
        "clients reading `unsupported` would hide metadata the adapter now reads"
    );
    // Still unsupported: the art URI is parsed, but UHC does not fetch the bytes.
    assert!(zone.unsupported.contains(&"album_art".to_string()));

    mock.stop().await;
}
