//! Adversarial checks of the URI providers' shared, lifecycle-owned reliable endpoint.
use anyhow::Result;
use async_trait::async_trait;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::Notify;
use unified_hifi_control::{
    adapters::{
        library_commands::LibraryCommandWorker, LibraryAdapter, LibrarySearchResult, Startable,
    },
    aggregator::ZoneAggregator,
    bus::{
        create_bus,
        runtime::{
            build_runtime, CommandDeadlines, CommandLane, CommandRequest, CommandStatus,
            RuntimeCommand,
        },
        PrefixedZoneId,
    },
    library_playback::{self, DurableTarget, LibraryPlayRequest},
};

#[derive(Default)]
struct NativeRecorder {
    calls: AtomicUsize,
    block_first: bool,
    entered: Notify,
    release: Notify,
}
#[async_trait]
impl LibraryAdapter for NativeRecorder {
    async fn search(&self, _: &str, _: usize) -> Result<Vec<LibrarySearchResult>> {
        Ok(vec![])
    }
    async fn play_uri(&self, _: &str, _: &str) -> Result<String> {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        if index == 0 && self.block_first {
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok("accepted".into())
    }
}
fn target(provider: &str) -> DurableTarget {
    match provider {
        "spotify" => DurableTarget::Spotify {
            uri: "spotify:album:one".into(),
        },
        "musicassistant" => DurableTarget::MusicAssistant {
            uri: "library://album/one".into(),
        },
        _ => unreachable!(),
    }
}
fn request(provider: &str, expires_in: std::time::Duration) -> CommandRequest {
    let now = tokio::time::Instant::now();
    CommandRequest {
        target: PrefixedZoneId::parse(format!("{provider}:room")).unwrap(),
        command: RuntimeCommand::LibraryPlay(Box::new(LibraryPlayRequest {
            target: target(provider),
            title: "One".into(),
        })),
        correlation_id: None,
        lane: CommandLane::Interactive,
        deadlines: CommandDeadlines {
            dispatch_by: now + expires_in,
            confirm_by: now + std::time::Duration::from_secs(2),
        },
    }
}

#[tokio::test]
async fn each_uri_provider_requires_its_live_endpoint_and_withdraws_it_on_stop() {
    for provider in ["spotify", "musicassistant"] {
        let runtime = build_runtime(Arc::new(ZoneAggregator::new(create_bus())), 4, 4);
        let recorder = Arc::new(NativeRecorder::default());
        let worker = LibraryCommandWorker::new(
            "test-library",
            provider,
            recorder.clone(),
            runtime.commands.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let zone = format!("{provider}:room");
        assert!(
            library_playback::play(Some(&runtime.commands), &zone, &target(provider), "One")
                .await
                .is_err()
        );
        assert_eq!(recorder.calls.load(Ordering::SeqCst), 0);
        worker.start().await.unwrap();
        library_playback::play(Some(&runtime.commands), &zone, &target(provider), "One")
            .await
            .unwrap();
        assert_eq!(recorder.calls.load(Ordering::SeqCst), 1);
        worker.stop().await;
        assert!(
            library_playback::play(Some(&runtime.commands), &zone, &target(provider), "One")
                .await
                .is_err()
        );
        assert_eq!(
            recorder.calls.load(Ordering::SeqCst),
            1,
            "a retained adapter must not bypass retired admission"
        );
    }
}

#[tokio::test]
async fn an_expired_queued_item_never_reaches_the_native_provider() {
    let runtime = build_runtime(Arc::new(ZoneAggregator::new(create_bus())), 4, 4);
    let recorder = Arc::new(NativeRecorder {
        block_first: true,
        ..Default::default()
    });
    let worker = LibraryCommandWorker::new(
        "test-library",
        "spotify",
        recorder.clone(),
        runtime.commands.clone(),
        tokio_util::sync::CancellationToken::new(),
    );
    worker.start().await.unwrap();
    let mut first = runtime
        .commands
        .submit(request("spotify", std::time::Duration::from_secs(1)))
        .await
        .unwrap();
    recorder.entered.notified().await;
    let mut expired = runtime
        .commands
        .submit(request("spotify", std::time::Duration::from_millis(5)))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    recorder.release.notify_one();
    assert_eq!(
        first.wait_for_native_result().await,
        CommandStatus::AwaitingProjection,
        "native acceptance must not fabricate a projection confirmation"
    );
    assert!(matches!(
        expired.wait_for_native_result().await,
        CommandStatus::NotDispatched { .. }
    ));
    assert_eq!(recorder.calls.load(Ordering::SeqCst), 1);
    worker.stop().await;
}

#[tokio::test]
async fn stopping_a_provider_drops_queued_work_instead_of_replaying_it_after_restart() {
    let runtime = build_runtime(Arc::new(ZoneAggregator::new(create_bus())), 4, 4);
    let recorder = Arc::new(NativeRecorder {
        block_first: true,
        ..Default::default()
    });
    let worker = LibraryCommandWorker::new(
        "test-library",
        "musicassistant",
        recorder.clone(),
        runtime.commands.clone(),
        tokio_util::sync::CancellationToken::new(),
    );
    worker.start().await.unwrap();
    let _first = runtime
        .commands
        .submit(request("musicassistant", std::time::Duration::from_secs(1)))
        .await
        .unwrap();
    recorder.entered.notified().await;
    let _queued = runtime
        .commands
        .submit(request("musicassistant", std::time::Duration::from_secs(1)))
        .await
        .unwrap();
    worker.stop().await;
    assert!(!runtime
        .commands
        .has_endpoint(&PrefixedZoneId::musicassistant("room")));
    worker.start().await.unwrap();
    library_playback::play(
        Some(&runtime.commands),
        "musicassistant:room",
        &target("musicassistant"),
        "One",
    )
    .await
    .unwrap();
    assert_eq!(
        recorder.calls.load(Ordering::SeqCst),
        2,
        "only the interrupted native attempt and the fresh request may reach the provider"
    );
    worker.stop().await;
}

struct ProviderOwner {
    gateway: unified_hifi_control::bus::runtime::CommandGateway,
    endpoint_was_live_at_stop: std::sync::atomic::AtomicBool,
}
#[async_trait]
impl Startable for ProviderOwner {
    fn name(&self) -> &'static str {
        "spotify"
    }
    async fn start(&self) -> Result<()> {
        Ok(())
    }
    async fn stop(&self) {
        self.endpoint_was_live_at_stop.store(
            self.gateway.has_endpoint(&PrefixedZoneId::spotify("room")),
            Ordering::SeqCst,
        );
    }
}
struct FailedCompanion;
#[async_trait]
impl Startable for FailedCompanion {
    fn name(&self) -> &'static str {
        "failed-companion"
    }
    async fn start(&self) -> Result<()> {
        anyhow::bail!("deliberate start failure")
    }
    async fn stop(&self) {}
}

#[tokio::test]
async fn coordinator_withdraws_library_admission_before_stopping_the_provider() {
    let bus = create_bus();
    let runtime = build_runtime(Arc::new(ZoneAggregator::new(bus.clone())), 4, 4);
    let coordinator = unified_hifi_control::coordinator::AdapterCoordinator::new(bus);
    let provider = Arc::new(ProviderOwner {
        gateway: runtime.commands.clone(),
        endpoint_was_live_at_stop: false.into(),
    });
    let worker = Arc::new(LibraryCommandWorker::new(
        "test-library",
        "spotify",
        Arc::new(NativeRecorder::default()),
        runtime.commands.clone(),
        tokio_util::sync::CancellationToken::new(),
    ));
    coordinator.register_companion("spotify", worker).await;
    coordinator
        .start_adapter_and_companions(provider.as_ref())
        .await
        .unwrap();
    coordinator
        .stop_adapter_and_companions_then_flush(provider.as_ref(), "spotify", "test")
        .await;
    assert!(
        !provider.endpoint_was_live_at_stop.load(Ordering::SeqCst),
        "native admission must close before its provider owner is stopped"
    );
}

#[tokio::test]
async fn coordinator_rolls_back_started_library_endpoints_if_a_later_companion_fails() {
    let bus = create_bus();
    let runtime = build_runtime(Arc::new(ZoneAggregator::new(bus.clone())), 4, 4);
    let coordinator = unified_hifi_control::coordinator::AdapterCoordinator::new(bus);
    let provider = Arc::new(ProviderOwner {
        gateway: runtime.commands.clone(),
        endpoint_was_live_at_stop: false.into(),
    });
    let worker = Arc::new(LibraryCommandWorker::new(
        "test-library",
        "spotify",
        Arc::new(NativeRecorder::default()),
        runtime.commands.clone(),
        tokio_util::sync::CancellationToken::new(),
    ));
    coordinator
        .register_companion("spotify", worker.clone())
        .await;
    coordinator
        .register_companion("spotify", Arc::new(FailedCompanion))
        .await;
    let result = coordinator
        .start_adapter_and_companions(provider.as_ref())
        .await;
    let endpoint_survived = runtime
        .commands
        .has_endpoint(&PrefixedZoneId::spotify("room"));
    worker.stop().await; // Clean up the defective implementation before failing the assertion.
    assert!(result.is_err());
    assert!(
        !endpoint_survived,
        "partial startup must not leave a native dispatch path alive"
    );
}
