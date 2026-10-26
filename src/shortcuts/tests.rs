use super::*;
use crate::adapters::lms::LmsSearchResultType;
use crate::mcp::refs::{RefTarget, RoonRefTarget};

fn store() -> (ShortcutStore, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    (ShortcutStore::open(dir.path().join("shortcuts.json")), dir)
}

fn roon(trail: &[&str]) -> ShortcutTarget {
    ShortcutTarget::Roon {
        trail: trail.iter().map(|s| s.to_string()).collect(),
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

    let roon_with_trail = RefTarget::Roon {
        target: RoonRefTarget {
            item_key: "k".into(),
            multi_session_key: "s".into(),
            trail: vec!["Library".into(), "Albums".into()],
        },
        title: "X".into(),
    };
    assert_eq!(
        ShortcutTarget::from_ref(&roon_with_trail),
        Ok(roon(&["Library", "Albums"]))
    );

    // A search hit's trail is just its own title: not a path from the root, so re-walking it can
    // never find the item. Pinning it would succeed now and fail on every press.
    let roon_search_hit = RefTarget::Roon {
        target: RoonRefTarget {
            item_key: "k".into(),
            multi_session_key: "s".into(),
            trail: vec!["Kind of Blue".into()],
        },
        title: "Kind of Blue".into(),
    };
    assert!(matches!(
        ShortcutTarget::from_ref(&roon_search_hit),
        Err(PinRefusal::NoDurableHandle(_))
    ));

    // The tempting wrong patch: pin the item_key and session, which only live as long as the
    // Core's browse session. A trail-less Roon ref must be refused, not stored.
    let roon_no_trail = RefTarget::Roon {
        target: RoonRefTarget {
            item_key: "k".into(),
            multi_session_key: "s".into(),
            trail: Vec::new(),
        },
        title: "X".into(),
    };
    assert!(matches!(
        ShortcutTarget::from_ref(&roon_no_trail),
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
