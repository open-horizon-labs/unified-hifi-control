//! Exercise the production notifier loop without the SDK's HTTP retirement timers.
use super::*;
use async_trait::async_trait;
use rust_mcp_sdk::{
    auth::AuthInfo,
    error::{McpSdkError, SdkResult},
    schema::{
        ClientMessage, InitializeRequestParams, InitializeResult, MessageFromServer,
        NotificationParams, RequestId, ServerMessage,
    },
    task_store::{ClientTaskStore, ServerTaskStore},
};
use std::{sync::Weak, time::Duration};
use tokio::sync::{mpsc, oneshot, RwLockReadGuard};

// Each observed attempt can succeed, fail, or stay pending until cancellation.
struct ControlledServer {
    attempts: mpsc::UnboundedSender<oneshot::Sender<bool>>,
}

#[async_trait]
impl McpServer for ControlledServer {
    async fn notify_resource_list_changed(&self, _: Option<NotificationParams>) -> SdkResult<()> {
        let (complete, result) = oneshot::channel();
        self.attempts
            .send(complete)
            .expect("test owns attempt receiver");
        if result.await.expect("test completes or retains attempt") {
            Ok(())
        } else {
            Err(McpSdkError::Internal {
                description: "controlled send failure".into(),
            })
        }
    }

    // The notifier must not use any other SDK operation.
    async fn start(self: Arc<Self>) -> SdkResult<()> {
        unreachable!()
    }
    async fn set_client_details(&self, _: InitializeRequestParams) -> SdkResult<()> {
        unreachable!()
    }
    fn server_info(&self) -> &InitializeResult {
        unreachable!()
    }
    fn client_info(&self) -> Option<InitializeRequestParams> {
        unreachable!()
    }
    async fn auth_info(&self) -> RwLockReadGuard<'_, Option<AuthInfo>> {
        unreachable!()
    }
    async fn auth_info_cloned(&self) -> Option<AuthInfo> {
        unreachable!()
    }
    async fn update_auth_info(&self, _: Option<AuthInfo>) {
        unreachable!()
    }
    async fn wait_for_initialization(&self) {
        unreachable!()
    }
    fn task_store(&self) -> Option<Arc<ServerTaskStore>> {
        unreachable!()
    }
    fn client_task_store(&self) -> Option<Arc<ClientTaskStore>> {
        unreachable!()
    }
    async fn stderr_message(&self, _: String) -> SdkResult<()> {
        unreachable!()
    }
    fn session_id(&self) -> Option<String> {
        unreachable!()
    }
    async fn send(
        &self,
        _: MessageFromServer,
        _: Option<RequestId>,
        _: Option<Duration>,
    ) -> SdkResult<Option<ClientMessage>> {
        unreachable!()
    }
    async fn send_batch(
        &self,
        _: Vec<ServerMessage>,
        _: Option<Duration>,
    ) -> SdkResult<Option<Vec<ClientMessage>>> {
        unreachable!()
    }
}

struct Probe {
    runtime: Weak<ControlledServer>,
    attempts: mpsc::UnboundedReceiver<oneshot::Sender<bool>>,
}

struct Fixture {
    bus: SharedBus,
    aggregator: Arc<ZoneAggregator>,
    baselines: Arc<McpResourceBaselines>,
    shutdown: CancellationToken,
}

impl Fixture {
    fn new() -> Self {
        let bus = crate::bus::create_bus();
        Self {
            aggregator: Arc::new(ZoneAggregator::new(bus.clone())),
            bus,
            baselines: Arc::new(McpResourceBaselines::default()),
            shutdown: CancellationToken::new(),
        }
    }

    async fn start(&self) -> Probe {
        // A deliberately stale served snapshot: independent of ambient settings.
        let generation = self
            .baselines
            .inner
            .lock()
            .await
            .served("session", "stale".into())
            .expect("new or evicted session");
        let (attempts, received) = mpsc::unbounded_channel();
        let runtime = Arc::new(ControlledServer { attempts });
        let weak = Arc::downgrade(&runtime);
        spawn_list_changed_notifier(
            self.bus.clone(),
            runtime,
            self.aggregator.clone(),
            self.baselines.clone(),
            "session".into(),
            generation,
            self.shutdown.clone(),
        );
        Probe {
            runtime: weak,
            attempts: received,
        }
    }

    fn candidates(&self) {
        for _ in 0..32 {
            self.bus.publish(BusEvent::ZonesFlushed {
                adapter: "roon".into(),
                zone_ids: vec![],
            });
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

// Keep this task runnable so paused time cannot auto-advance to the next retry.
// The loop has only in-memory lock awaits and synchronous settings reads here.
async fn poll_ready() {
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
}

impl Probe {
    async fn attempt(&mut self) -> oneshot::Sender<bool> {
        poll_ready().await;
        self.attempts.try_recv().expect("notifier attempted send")
    }

    async fn quiet(&mut self) {
        poll_ready().await;
        assert!(matches!(
            self.attempts.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    async fn exited(&mut self) {
        poll_ready().await;
        assert!(
            self.runtime.upgrade().is_none(),
            "notifier must release runtime"
        );
        assert!(matches!(
            self.attempts.try_recv(),
            Err(mpsc::error::TryRecvError::Disconnected)
        ));
    }
}

#[tokio::test(start_paused = true)]
async fn failed_sends_are_paced_despite_candidate_storm_then_success_coalesces() {
    let fixture = Fixture::new();
    let mut probe = fixture.start().await;
    probe.attempt().await.send(false).unwrap();
    poll_ready().await;
    fixture.candidates();
    probe.quiet().await;
    tokio::time::advance(Duration::from_millis(999)).await;
    fixture.candidates();
    probe.quiet().await;
    tokio::time::advance(Duration::from_millis(1)).await;
    fixture.candidates();
    probe.attempt().await.send(true).unwrap();
    poll_ready().await;
    tokio::time::advance(Duration::from_secs(5)).await;
    fixture.candidates();
    probe.quiet().await;
    fixture.shutdown.cancel();
    probe.exited().await;
    assert_eq!(fixture.bus.subscriber_count(), 0);
}

#[tokio::test(start_paused = true)]
async fn blocked_send_times_out_then_shutdown_cancels_retry_and_releases_session() {
    let fixture = Fixture::new();
    let mut probe = fixture.start().await;
    let first = probe.attempt().await;
    tokio::time::advance(Duration::from_millis(1999)).await;
    poll_ready().await;
    assert!(!first.is_closed());
    tokio::time::advance(Duration::from_millis(1)).await;
    poll_ready().await;
    assert!(first.is_closed(), "timeout must drop the send future");
    fixture.candidates();
    probe.quiet().await;
    tokio::time::advance(Duration::from_millis(999)).await;
    fixture.candidates();
    probe.quiet().await;
    tokio::time::advance(Duration::from_millis(1)).await;
    fixture.candidates();
    let retry = probe.attempt().await;
    fixture.shutdown.cancel();
    probe.exited().await;
    assert!(retry.is_closed(), "shutdown must cancel a blocked send");
    assert_eq!(
        fixture.baselines.inner.lock().await.revision("session"),
        None
    );
    assert_eq!(fixture.bus.subscriber_count(), 0);
}

#[tokio::test(start_paused = true)]
async fn idle_notifier_retires_and_relisting_starts_a_working_replacement() {
    let fixture = Fixture::new();
    let mut old = fixture.start().await;
    old.attempt().await.send(true).unwrap();
    poll_ready().await;
    tokio::time::advance(Duration::from_secs(30 * 60)).await;
    old.exited().await;
    assert_eq!(fixture.bus.subscriber_count(), 0);
    let mut replacement = fixture.start().await;
    replacement.attempt().await.send(true).unwrap();
    poll_ready().await;
    fixture.shutdown.cancel();
    replacement.exited().await;
}

#[tokio::test(start_paused = true)]
async fn evicted_inflight_notifier_cannot_remove_its_working_replacement() {
    let fixture = Fixture::new();
    let mut old = fixture.start().await;
    let pending = old.attempt().await;
    tokio::time::advance(Duration::from_millis(1)).await;
    {
        let mut store = fixture.baselines.inner.lock().await;
        for i in 0..256 {
            store.served(&format!("other-{i}"), "stale".into());
        }
    }
    let mut replacement = fixture.start().await;
    replacement.attempt().await.send(true).unwrap();
    poll_ready().await;
    // Old failure arrives after replacement has claimed its own invalidation.
    pending.send(false).unwrap();
    poll_ready().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    old.exited().await;
    replacement.quiet().await;
    assert_eq!(fixture.bus.subscriber_count(), 1);
    assert!(
        fixture
            .baselines
            .inner
            .lock()
            .await
            .served("session", "fresh".into())
            .is_none(),
        "old cleanup must not delete the replacement entry"
    );
    fixture.shutdown.cancel();
    replacement.exited().await;
}
