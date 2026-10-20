//! OpenHome radio tracks must update without a stream URI change (#423).
#[allow(dead_code, unused_imports, unused_variables)]
mod mock_servers;
use mock_servers::openhome::MockOpenHomeDevice;
use unified_hifi_control::adapters::openhome::OpenHomeAdapter;
use unified_hifi_control::bus::{create_bus, BusEvent};

#[tokio::test]
async fn openhome_updates_metadata_under_a_constant_uri_and_skips_steady_state() {
    let mock = MockOpenHomeDevice::start().await;
    mock.set_state("Playing").await;
    mock.set_track(
        "First",
        "Artist one",
        "Album one",
        "http://example.test/one.jpg",
    )
    .await;
    let bus = create_bus();
    let mut events = bus.subscribe();
    let adapter = OpenHomeAdapter::new(bus);
    let uuid = mock.uuid().await;
    adapter
        .probe_device_for_test(&uuid, &mock.description_url())
        .await
        .unwrap();
    assert_eq!(adapter.get_now_playing(&uuid).await.unwrap().line1, "First");
    while events.try_recv().is_ok() {}

    mock.set_track(
        "Second",
        "Artist two",
        "Album two",
        "http://example.test/two.jpg",
    )
    .await;
    adapter
        .probe_device_for_test(&uuid, &mock.description_url())
        .await
        .unwrap();
    let np = adapter.get_now_playing(&uuid).await.unwrap();
    assert_eq!(np.line1, "Second");
    assert_eq!(np.line2, "Artist two");
    assert_eq!(np.line3, "Album two");
    assert_eq!(np.image_key.as_deref(), Some("http://example.test/two.jpg"));
    let mut changes = 0;
    while let Ok(event) = events.try_recv() {
        if let BusEvent::NowPlayingChanged {
            title,
            artist,
            album,
            image_key,
            ..
        } = event
        {
            assert_eq!(title.as_deref(), Some("Second"));
            assert_eq!(artist.as_deref(), Some("Artist two"));
            assert_eq!(album.as_deref(), Some("Album two"));
            assert_eq!(image_key.as_deref(), Some("http://example.test/two.jpg"));
            changes += 1;
        }
    }
    assert_eq!(changes, 1);
    adapter
        .probe_device_for_test(&uuid, &mock.description_url())
        .await
        .unwrap();
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event, BusEvent::NowPlayingChanged { .. }),
            "unchanged metadata must not be parsed/published again"
        );
    }
    mock.stop().await;
}
