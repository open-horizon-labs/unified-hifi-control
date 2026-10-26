use super::*;
use crate::adapters::lms::LmsSearchResultType;
use crate::mcp::collection_locations::{CollectionLocation, CollectionStep, RoonLocationOrigin};
use crate::mcp::refs::{RefTarget, RoonRefTarget};

fn store() -> (ShortcutStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    (ShortcutStore::open(dir.path().join("shortcuts.json")), dir)
}

fn step(title: &str, subtitle: Option<&str>, position: Option<u32>) -> CollectionStep {
    CollectionStep::roon(title, subtitle.map(str::to_string), position)
}

fn roon_location(steps: Vec<CollectionStep>) -> CollectionLocation {
    CollectionLocation::Roon {
        origin: RoonLocationOrigin::BrowseRoot,
        steps,
    }
}

/// A Roon pin whose rows have no subtitle: enough to tell pins apart by title.
fn roon(trail: &[&str]) -> ShortcutTarget {
    ShortcutTarget::Roon {
        location: roon_location(trail.iter().map(|t| step(t, None, None)).collect()),
    }
}

fn roon_ref(location: Option<CollectionLocation>, trail: &[&str]) -> RefTarget {
    RefTarget::Roon {
        target: RoonRefTarget {
            item_key: "k".into(),
            multi_session_key: "s".into(),
            trail: trail.iter().map(|t| t.to_string()).collect(),
            location,
        },
        title: trail.last().copied().unwrap_or("X").to_string(),
    }
}

#[tokio::test]
async fn pin_survives_a_restart_and_pinning_twice_is_one_shortcut() {
    let (store, dir) = store();
    let target = roon(&["Library", "Albums", "Kind of Blue"]);
    let (first, already) = store
        .pin(target.clone(), "Kind of Blue".into(), None)
        .await
        .unwrap();
    assert!(!already);
    assert_eq!(first.name, "Kind of Blue");

    let (again, already) = store
        .pin(target, "Kind of Blue".into(), Some("Other".into()))
        .await
        .unwrap();
    assert!(already);
    assert_eq!(again.id, first.id);
    assert_eq!(store.list().await.len(), 1);

    let reopened = ShortcutStore::open(dir.path().join("shortcuts.json"));
    assert_eq!(reopened.get(&first.id).await.unwrap().title, "Kind of Blue");
}

#[tokio::test]
async fn deleting_a_shortcut_unbinds_every_button_that_used_it() {
    let (store, _dir) = store();
    let (a, _) = store.pin(roon(&["A"]), "A".into(), None).await.unwrap();
    let (b, _) = store.pin(roon(&["B"]), "B".into(), None).await.unwrap();
    let mut map = BTreeMap::new();
    map.insert("preset_1:press".to_string(), a.id.clone());
    map.insert("next:long".to_string(), b.id.clone());
    store.set_bindings("dial1", map.clone()).await.unwrap();
    store.set_bindings("frame1", map).await.unwrap();

    assert!(store.delete(&a.id).await.unwrap());

    for controller in ["dial1", "frame1"] {
        let left = store.bindings_for(controller).await;
        assert_eq!(left.len(), 1, "{controller}");
        assert_eq!(left["next:long"], b.id);
    }
}

#[tokio::test]
async fn a_map_naming_an_unknown_shortcut_or_a_bad_event_writes_nothing() {
    let (store, _dir) = store();
    let (a, _) = store.pin(roon(&["A"]), "A".into(), None).await.unwrap();

    let mut unknown = BTreeMap::new();
    unknown.insert("preset_1:press".to_string(), "sc_nope".to_string());
    assert!(matches!(
        store.set_bindings("dial1", unknown).await,
        Err(BindingError::UnknownShortcut(_))
    ));

    let mut mixed = BTreeMap::new();
    mixed.insert("preset_1:press".to_string(), a.id.clone());
    mixed.insert("play_pause:press".to_string(), a.id.clone()); // refused: transport press
    assert!(matches!(
        store.set_bindings("dial1", mixed).await,
        Err(BindingError::Event {
            refusal: EventRefusal::TransportPress,
            ..
        })
    ));
    assert!(
        store.bindings_for("dial1").await.is_empty(),
        "a refused map must leave the previous one alone"
    );
}

#[test]
fn transport_keys_carry_a_shortcut_only_on_a_long_press_and_volume_never() {
    // The controller runs a transport key's normal action on the press, so a press or double can
    // never be "the shortcut": the normal action has already happened.
    for key in ["play_pause", "play", "pause", "next", "previous"] {
        assert_eq!(
            event_key(key, "press"),
            Err(EventRefusal::TransportPress),
            "{key}"
        );
        assert_eq!(
            event_key(key, "double"),
            Err(EventRefusal::TransportPress),
            "{key}"
        );
        assert_eq!(event_key(key, "long"), Ok(format!("{key}:long")));
    }
    // Volume and mute: nothing, ever.
    for key in ["volume_up", "volume_down", "mute"] {
        for gesture in ["press", "long", "double"] {
            assert_eq!(
                event_key(key, gesture),
                Err(EventRefusal::ReservedKey),
                "{key}:{gesture}"
            );
        }
    }
    // Everything else takes all three.
    for gesture in ["press", "long", "double"] {
        assert_eq!(event_key("home", gesture), Ok(format!("home:{gesture}")));
        assert_eq!(
            event_key("usage_0192", gesture),
            Ok(format!("usage_0192:{gesture}"))
        );
    }
    assert_eq!(event_key("preset_1", "press"), Ok("preset_1:press".into()));
    assert_eq!(event_key("Preset", "press"), Err(EventRefusal::BadKey));
    assert_eq!(event_key("", "press"), Err(EventRefusal::BadKey));
    assert_eq!(event_key("a", "tap"), Err(EventRefusal::BadGesture));
}

#[tokio::test]
async fn an_unreadable_file_is_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shortcuts.json");
    std::fs::write(&path, b"{ not json").unwrap();
    let store = ShortcutStore::open(path.clone());

    let refused = store.pin(roon(&["A"]), "A".into(), None).await;
    assert!(refused.is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"{ not json");
}

#[test]
fn only_targets_with_a_durable_handle_can_be_pinned() {
    use crate::adapters::lms::LmsPlayTarget;

    let walk = vec![
        step("Local Library", None, Some(0)),
        step("Albums", None, Some(2)),
        step("Kind of Blue", Some("Miles Davis"), Some(5)),
    ];
    let with_location = roon_ref(
        Some(roon_location(walk.clone())),
        &["Local Library", "Albums", "Kind of Blue"],
    );
    assert_eq!(
        ShortcutTarget::from_ref(&with_location),
        Ok(ShortcutTarget::Roon {
            location: roon_location(walk)
        })
    );

    // A search hit knows only its own title: no location, so it can never be found again.
    let search_hit = roon_ref(None, &["Kind of Blue"]);
    assert!(matches!(
        ShortcutTarget::from_ref(&search_hit),
        Err(PinRefusal::NoDurableHandle(_))
    ));

    // The tempting wrong patch: keep the title trail, which exists on every Roon ref. Titles are not
    // an identity (two albums share one, and a trail replay takes the first), so a ref with a trail
    // but no location must be refused, not stored.
    let trail_only = roon_ref(None, &["Local Library", "Albums", "Greatest Hits"]);
    assert!(matches!(
        ShortcutTarget::from_ref(&trail_only),
        Err(PinRefusal::NoDurableHandle(_))
    ));

    // A location too short to be a path is refused as well.
    let too_short = roon_ref(
        Some(roon_location(vec![step("Greatest Hits", None, None)])),
        &["Greatest Hits"],
    );
    assert!(matches!(
        ShortcutTarget::from_ref(&too_short),
        Err(PinRefusal::NoDurableHandle(_))
    ));

    // A positional LMS breadcrumb can play the wrong item with no error; never pin it.
    let lms_breadcrumb = RefTarget::Lms {
        target: LmsPlayTarget::GlobalSearchItem {
            item_id: "abc.0.1".into(),
        },
        title: "X".into(),
    };
    assert!(matches!(
        ShortcutTarget::from_ref(&lms_breadcrumb),
        Err(PinRefusal::NoDurableHandle(_))
    ));

    let lms_album = RefTarget::Lms {
        target: LmsPlayTarget::Library {
            kind: LmsSearchResultType::Album,
            id: 42,
        },
        title: "X".into(),
    };
    assert!(ShortcutTarget::from_ref(&lms_album).is_ok());

    let apple = RefTarget::AppleMusic {
        companion_id: "c".into(),
        handle: "h".into(),
        title: "X".into(),
    };
    assert!(matches!(
        ShortcutTarget::from_ref(&apple),
        Err(PinRefusal::Unsupported(_))
    ));
}

#[tokio::test]
async fn two_albums_with_one_title_by_different_artists_are_two_pins() {
    let (store, _dir) = store();
    let walk = |artist: &str| {
        roon_location(vec![
            step("Local Library", None, Some(0)),
            step("Albums", None, Some(2)),
            step("Greatest Hits", Some(artist), Some(0)),
        ])
    };
    let a = ShortcutTarget::Roon {
        location: walk("Artist A"),
    };
    let b = ShortcutTarget::Roon {
        location: walk("Artist B"),
    };
    let (first, _) = store
        .pin(a.clone(), "Greatest Hits".into(), None)
        .await
        .unwrap();
    let (second, already) = store.pin(b, "Greatest Hits".into(), None).await.unwrap();
    assert!(!already, "a different artist is a different item");
    assert_ne!(first.id, second.id);
    // The same item pinned again is still one pin.
    let (again, already) = store.pin(a, "Greatest Hits".into(), None).await.unwrap();
    assert!(already && again.id == first.id);
}

#[test]
fn a_stored_target_carries_no_session_or_ref_token() {
    let json = serde_json::to_string(&roon(&["Library", "Albums"])).unwrap();
    assert!(!json.contains("item_key"));
    assert!(!json.contains("session"));
    assert!(!json.contains("ref_"));
}

#[tokio::test]
async fn recent_events_keep_the_distinct_keys_a_remote_sends_most_recent_first() {
    let (store, _dir) = store();
    for event in ["home:press", "back:long", "home:press", "usage_0192:double"] {
        store.record_event("dial1", event, "unmapped").await;
    }
    let seen: Vec<_> = store
        .recent_events("dial1")
        .await
        .into_iter()
        .map(|e| e.event)
        .collect();
    // A repeated key moves up; it does not crowd the others out.
    assert_eq!(seen, ["usage_0192:double", "home:press", "back:long"]);

    for n in 0..20 {
        store
            .record_event("dial1", &format!("key_{n}:press"), "unmapped")
            .await;
    }
    assert_eq!(store.recent_events("dial1").await.len(), 8);
    assert!(store.recent_events("someone-else").await.is_empty());
}

#[tokio::test]
async fn button_history_does_not_follow_a_rebinding_or_zone_change() {
    let (store, _dir) = store();
    let result = LastPress {
        at: Utc::now(),
        code: "accepted",
        ok: true,
        message: "Sent".into(),
    };
    store
        .record_binding_press("dial", "home:press", "pin-a", Some("roon:room-a"), result)
        .await;
    assert!(store
        .last_binding_press("dial", "home:press", "pin-a", Some("roon:room-a"))
        .await
        .is_some());
    assert!(store
        .last_binding_press("dial", "home:long", "pin-a", Some("roon:room-a"))
        .await
        .is_none());
    assert!(store
        .last_binding_press("dial", "home:press", "pin-b", Some("roon:room-a"))
        .await
        .is_none());
    assert!(store
        .last_binding_press("dial", "home:press", "pin-a", Some("roon:room-b"))
        .await
        .is_none());
}
