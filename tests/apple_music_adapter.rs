//! Contract tests for the native MusicKit Apple Music adapter.
//!
//! These tests deliberately exercise the Rust/Swift companion boundary rather
//! than MusicKit itself.  MusicKit is only available in a signed macOS target;
//! the companion is tested separately on macOS.

use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use unified_hifi_control::adapters::apple_music::{
    AppleMusicAdapter, CompanionPlatform, ExecutionOwner, MusicKitCommand, MusicKitCompanion,
    MusicKitPlaybackState, MusicKitSnapshot, MusicKitTrack, PlaybackRoute,
};
use unified_hifi_control::adapters::{
    AdapterCommand, AdapterContext, AdapterLogic, LibraryAdapter, Startable,
};
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::api::apple_bridge::{AppleBridgeRegistry, PairedMusicKitCompanion};
use unified_hifi_control::bus::SharedBus;
use unified_hifi_control::bus::{create_bus, BusEvent, PlaybackState};
use unified_hifi_control::mcp::observation_history::PlaybackObservationHistory;

#[derive(Clone)]
struct FakeCompanion {
    snapshot: MusicKitSnapshot,
    commands: Arc<Mutex<Vec<MusicKitCommand>>>,
}

#[derive(Clone)]
struct FlakyCompanion {
    snapshot: MusicKitSnapshot,
    responses: Arc<Mutex<VecDeque<anyhow::Result<Vec<MusicKitSnapshot>>>>>,
}

#[async_trait]
impl MusicKitCompanion for FlakyCompanion {
    async fn snapshot(&self) -> anyhow::Result<MusicKitSnapshot> {
        Ok(self.snapshot.clone())
    }

    async fn execute(&self, _command: MusicKitCommand) -> anyhow::Result<()> {
        Ok(())
    }

    async fn snapshots(&self) -> anyhow::Result<Vec<MusicKitSnapshot>> {
        self.responses
            .lock()
            .map_err(|_| anyhow::anyhow!("flaky companion lock poisoned"))?
            .pop_front()
            .unwrap_or_else(|| Ok(vec![self.snapshot.clone()]))
    }
}

#[async_trait]
impl MusicKitCompanion for FakeCompanion {
    async fn snapshot(&self) -> anyhow::Result<MusicKitSnapshot> {
        Ok(self.snapshot.clone())
    }

    async fn execute(&self, command: MusicKitCommand) -> anyhow::Result<()> {
        self.commands
            .lock()
            .map_err(|_| anyhow::anyhow!("fake companion lock poisoned"))?
            .push(command);
        Ok(())
    }
}

fn snapshot() -> MusicKitSnapshot {
    MusicKitSnapshot {
        player_id: "application".to_string(),
        display_name: "Apple Music".to_string(),
        state: MusicKitPlaybackState::Playing,
        track: Some(MusicKitTrack {
            title: "Song".to_string(),
            artist: "Artist".to_string(),
            album: "Album".to_string(),
            artwork_url: Some("https://example.test/art.jpg".to_string()),
            position_seconds: Some(12.0),
            duration_seconds: Some(180.0),
        }),
        volume: Some(0.75),
        is_muted: false,
    }
}

fn adapter(bus: SharedBus, commands: Arc<Mutex<Vec<MusicKitCommand>>>) -> AppleMusicAdapter {
    AppleMusicAdapter::with_companion(
        bus,
        Arc::new(FakeCompanion {
            snapshot: snapshot(),
            commands,
        }),
        Duration::from_millis(5),
    )
}

#[tokio::test]
async fn companion_snapshot_maps_to_an_applemusic_zone() {
    let bus = create_bus();
    let mut events = bus.subscribe();
    let adapter = adapter(bus.clone(), Arc::new(Mutex::new(Vec::new())));
    let shutdown = CancellationToken::new();

    let run = tokio::spawn({
        let adapter = adapter.clone();
        let shutdown = shutdown.clone();
        async move { adapter.run(AdapterContext { bus, shutdown }).await }
    });

    let event = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if let Ok(BusEvent::ZoneDiscovered { zone }) = events.recv().await {
                break zone;
            }
        }
    })
    .await
    .expect("adapter must publish a zone");

    assert_eq!(event.zone_id, "applemusic:application");
    assert_eq!(event.source, "applemusic");
    assert_eq!(event.state, PlaybackState::Playing);
    assert_eq!(event.zone_name, "Apple Music");
    assert_eq!(
        event.now_playing.as_ref().map(|track| track.title.as_str()),
        Some("Song")
    );
    assert_eq!(
        event.volume_control.as_ref().map(|volume| volume.value),
        Some(75.0)
    );

    shutdown.cancel();
    run.await
        .expect("adapter task must join")
        .expect("adapter must stop cleanly");
}

#[test]
fn transient_companion_states_use_the_shared_unknown_state() {
    let mut snapshot = snapshot();
    snapshot.state = MusicKitPlaybackState::Unknown;

    let zone = AppleMusicAdapter::zone_from_snapshot(&snapshot)
        .expect("unknown is a valid transient companion state");
    assert_eq!(zone.state, PlaybackState::Unknown);
}

#[tokio::test]
async fn companion_snapshot_flows_through_aggregator_and_flushes_on_stop() {
    let bus = create_bus();
    let observations = PlaybackObservationHistory::new_for_test();
    let aggregator = Arc::new(ZoneAggregator::new_with_observation_history(
        bus.clone(), observations,
    ));
    let aggregator_task = {
        let aggregator = aggregator.clone();
        tokio::spawn(async move { aggregator.run().await })
    };
    let adapter = adapter(bus.clone(), Arc::new(Mutex::new(Vec::new())));
    let shutdown = CancellationToken::new();
    let adapter_task = {
        let adapter = adapter.clone();
        let adapter_bus = bus.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            adapter
                .run(AdapterContext {
                    bus: adapter_bus,
                    shutdown,
                })
                .await
        })
    };

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if aggregator.get_zone("applemusic:application").await.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("aggregator must receive the companion zone");

    let zone = aggregator
        .get_zone("applemusic:application")
        .await
        .expect("Apple Music zone must be addressable through the aggregator");
    assert_eq!(zone.source, "applemusic");
    assert_eq!(
        zone.now_playing.as_ref().map(|track| track.title.as_str()),
        Some("Song")
    );
    let observed = aggregator
        .observed_playback_history("applemusic:application", 10)
        .await;
    assert!(!observed.is_empty());
    assert_eq!(observed[0].title.as_deref(), Some("Song"));
    assert_eq!(observed[0].reference, None);
    assert_eq!(observed[0].confidence, "observed_unresolved");

    shutdown.cancel();
    adapter_task
        .await
        .expect("adapter task must join")
        .expect("adapter must stop cleanly");
    adapter.stop().await;
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if aggregator
                .get_zone("applemusic:application")
                .await
                .is_none()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("stopping the adapter must flush its aggregator zones");
    assert!(aggregator
        .observed_playback_history("applemusic:application", 10)
        .await
        .is_empty());

    bus.publish(BusEvent::ShuttingDown { reason: None });
    aggregator_task
        .await
        .expect("aggregator task must join");
}

#[tokio::test]
async fn transient_companion_failure_retains_zone_until_recovery() {
    let bus = create_bus();
    let mut events = bus.subscribe();
    let responses = Arc::new(Mutex::new(VecDeque::from([
        Ok(vec![snapshot()]),
        Err(anyhow::anyhow!("temporary timeout")),
        Ok(vec![snapshot()]),
    ])));
    let adapter = AppleMusicAdapter::with_companion(
        bus.clone(),
        Arc::new(FlakyCompanion {
            snapshot: snapshot(),
            responses,
        }),
        Duration::from_millis(5),
    );
    let shutdown = CancellationToken::new();
    let task = {
        let adapter = adapter.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move { adapter.run(AdapterContext { bus, shutdown }).await })
    };

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if matches!(events.recv().await.expect("bus event"), BusEvent::ZoneDiscovered { .. }) {
                break;
            }
        }
    })
    .await
    .expect("initial snapshot must discover the zone");

    let mut removed = false;
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match events.recv().await.expect("bus event") {
                BusEvent::ZoneRemoved { .. } => removed = true,
                BusEvent::ZoneUpdated { zone_id, state, .. }
                    if zone_id.to_string() == "applemusic:application" && state == "unknown" =>
                {
                    break;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("temporary failure must mark the retained zone unknown");
    assert!(!removed, "a transient refresh failure must not remove the zone");

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if matches!(
                events.recv().await.expect("bus event"),
                BusEvent::ZoneUpdated { state, .. } if state == "playing"
            ) {
                break;
            }
        }
    })
    .await
    .expect("a later snapshot must recover the zone");

    shutdown.cancel();
    task.await
        .expect("adapter task must join")
        .expect("adapter must stop cleanly");
}

#[tokio::test]
async fn adapter_translates_unified_commands_to_musickit_commands() {
    let bus = create_bus();
    let commands = Arc::new(Mutex::new(Vec::new()));
    let adapter = adapter(bus, commands.clone());

    let response = adapter
        .handle_command("applemusic:application", AdapterCommand::Play)
        .await
        .expect("command must be handled");
    assert!(response.success);

    let response = adapter
        .handle_command("applemusic:application", AdapterCommand::VolumeAbsolute(42))
        .await
        .expect("volume command must be handled");
    assert!(response.success);

    let recorded = commands.lock().expect("fake companion lock").clone();
    assert_eq!(
        recorded,
        vec![
            MusicKitCommand::Play,
            MusicKitCommand::SetVolume { value: 0.42 }
        ]
    );
}

#[tokio::test]
async fn volume_commands_are_refused_without_a_companion_volume_observation() {
    let bus = create_bus();
    let commands = Arc::new(Mutex::new(Vec::new()));
    let mut snapshot = snapshot();
    snapshot.volume = None;
    let adapter = AppleMusicAdapter::with_companion(
        bus,
        Arc::new(FakeCompanion {
            snapshot,
            commands: commands.clone(),
        }),
        Duration::from_millis(5),
    );

    let response = adapter
        .handle_command("applemusic:application", AdapterCommand::VolumeAbsolute(42))
        .await
        .expect("missing volume should be classified, not panic");
    assert!(!response.success);
    assert!(response
        .error
        .as_deref()
        .is_some_and(|error| error.contains("unavailable")));
    assert!(commands.lock().expect("fake companion lock").is_empty());
}

#[tokio::test]
async fn adapter_rejects_zone_owned_by_another_provider() {
    let bus = create_bus();
    let adapter = adapter(bus, Arc::new(Mutex::new(Vec::new())));

    let response = adapter
        .handle_command("spotify:device", AdapterCommand::Play)
        .await
        .expect("invalid owner should be a classified response");
    assert!(!response.success);
    assert!(response
        .error
        .as_deref()
        .is_some_and(|error| error.contains("applemusic")));
}

#[tokio::test]
async fn paired_companion_truthfully_refuses_content_until_bridge_contract_exists() {
    let bus = create_bus();
    let adapter = AppleMusicAdapter::with_companion(
        bus,
        Arc::new(PairedMusicKitCompanion::new(AppleBridgeRegistry::default())),
        Duration::from_millis(5),
    );

    let error = adapter
        .search("Miles Davis", 10)
        .await
        .expect_err("content must remain gated until the approved bridge transport exists");
    assert!(error
        .to_string()
        .contains("content operations are not implemented"));
}

#[tokio::test]
async fn stopping_adapter_flushes_apple_music_zones() {
    let bus = create_bus();
    let mut events = bus.subscribe();
    let adapter = adapter(bus, Arc::new(Mutex::new(Vec::new())));

    adapter.stop().await;

    let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .expect("stop must publish a lifecycle event")
        .expect("bus must remain open");
    assert!(matches!(
        event,
        BusEvent::AdapterStopping { adapter, reason }
            if adapter == "applemusic" && reason.as_deref() == Some("requested")
    ));
}

#[test]
fn execution_owner_is_the_only_controllable_zone() {
    let iphone = ExecutionOwner::new("muness-iphone", CompanionPlatform::IPhone)
        .expect("valid companion id");
    let mac =
        ExecutionOwner::new("studio-mac", CompanionPlatform::Mac).expect("valid companion id");

    assert_eq!(iphone.zone_id().to_string(), "applemusic:muness-iphone");
    assert_eq!(mac.zone_id().to_string(), "applemusic:studio-mac");
    assert_ne!(iphone.zone_id(), mac.zone_id());
}

#[test]
fn airplay_route_is_destination_observation_not_a_zone() {
    let route = PlaybackRoute::AirPlay {
        route_id: "homepod:kitchen".to_string(),
        display_name: "Kitchen HomePod".to_string(),
    };

    assert!(route.is_destination_only());
    assert!(!PlaybackRoute::Unknown.is_destination_only());
    assert!(!PlaybackRoute::LocalOutput {
        display_name: "iPhone Speaker".to_string(),
    }
    .is_destination_only());
}

#[test]
fn companion_ids_cannot_collide_with_prefixed_zone_ids() {
    assert!(ExecutionOwner::new("", CompanionPlatform::IPhone).is_err());
    assert!(ExecutionOwner::new("applemusic:fake", CompanionPlatform::Mac).is_err());
}
