use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, RwLock};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Retry and stability policy for one managed HQPlayer producer.
///
/// This is deliberately independent from command timeouts. A command retry proves whether one
/// request can finish; this policy controls how a long-lived producer behaves while the daemon is
/// restarting or unavailable. It is not serialized by any HTTP, SSE or MCP payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HqpRecoveryConfig {
    /// Delay between coherent observations while the producer is healthy.
    pub poll_interval: Duration,
    /// Fixed retry delay during the qualified daemon-restart window.
    pub short_retry_delay: Duration,
    /// Elapsed outage time for which the fixed short delay remains appropriate.
    pub restart_window: Duration,
    /// First delay after the restart window has elapsed.
    pub backoff_initial: Duration,
    /// Maximum prolonged-outage delay.
    pub backoff_cap: Duration,
    /// Coherent-health duration required before retry debt is forgiven.
    pub stable_threshold: Duration,
}

impl Default for HqpRecoveryConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(2),
            short_retry_delay: Duration::from_secs(1),
            // This remains an injectable qualification value. Issue #369 records the supported
            // HQPlayer Embedded measurement before this branch is handed off as release-ready.
            restart_window: Duration::from_secs(12),
            backoff_initial: Duration::from_secs(2),
            backoff_cap: Duration::from_secs(60),
            stable_threshold: Duration::from_secs(10),
        }
    }
}

/// Internally observable child state. Existing public wire payloads deliberately remain unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HqpWorkerPhase {
    Recovering,
    Healthy,
    Failed,
    Stopped,
}

/// Internal lifecycle snapshot for diagnostics and conformance tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HqpWorkerStatus {
    pub supervisor_enabled: bool,
    pub phase: HqpWorkerPhase,
}

pub(crate) type SharedWorkerPhase = Arc<RwLock<HqpWorkerPhase>>;
pub(crate) type SharedRecoveryState = Arc<Mutex<RecoveryState>>;

/// Pure outage/stability state. Callers provide the observation instant, making every transition
/// deterministic without sleeping or measuring scheduler wall time in tests.
#[derive(Debug)]
pub(crate) struct RecoveryState {
    config: HqpRecoveryConfig,
    outage_started: Option<Instant>,
    healthy_since: Option<Instant>,
    next_backoff: Duration,
}

impl RecoveryState {
    pub(crate) fn new(mut config: HqpRecoveryConfig) -> Self {
        const MIN_DELAY: Duration = Duration::from_millis(1);
        config.poll_interval = config.poll_interval.max(MIN_DELAY);
        config.backoff_cap = config.backoff_cap.max(MIN_DELAY);
        config.short_retry_delay = config
            .short_retry_delay
            .max(MIN_DELAY)
            .min(config.backoff_cap);
        config.backoff_initial = config
            .backoff_initial
            .max(MIN_DELAY)
            .max(config.short_retry_delay)
            .min(config.backoff_cap);
        Self {
            next_backoff: config.backoff_initial,
            config,
            outage_started: None,
            healthy_since: None,
        }
    }

    pub(crate) fn config(&self) -> HqpRecoveryConfig {
        self.config
    }

    /// Record a failed coherent observation and return the next cancellation-aware wait.
    pub(crate) fn on_failure(&mut self, now: Instant) -> Duration {
        self.healthy_since = None;
        let outage_started = *self.outage_started.get_or_insert(now);
        if now.saturating_duration_since(outage_started) <= self.config.restart_window {
            return self.config.short_retry_delay;
        }

        let delay = self.next_backoff;
        self.next_backoff = self
            .next_backoff
            .checked_mul(2)
            .unwrap_or(self.config.backoff_cap)
            .min(self.config.backoff_cap);
        delay
    }

    /// Record a complete coherent observation. Returns true only when the prior outage debt was
    /// reset after continuous coherent health reached the configured stability threshold.
    pub(crate) fn on_coherent_success(&mut self, now: Instant) -> bool {
        if self.outage_started.is_none() {
            return false;
        }

        let healthy_since = *self.healthy_since.get_or_insert(now);
        if now.saturating_duration_since(healthy_since) < self.config.stable_threshold {
            return false;
        }

        self.outage_started = None;
        self.healthy_since = None;
        self.next_backoff = self.config.backoff_initial;
        true
    }
}

/// Own one replaceable observer child for the lifetime of a configured instance.
///
/// Manager map membership now means this supervisor is live, not merely that a historical child
/// handle was retained. Retry state is shared across replacement children so a crashing observer
/// cannot manufacture a fresh fast-retry budget. With the current release `panic = "abort"` policy,
/// panic replacement is available only in unwind builds; issue #372 owns that production choice.
pub(crate) async fn supervise_worker<Run, RunFuture, Cleanup, CleanupFuture>(
    name: String,
    shutdown: CancellationToken,
    phase: SharedWorkerPhase,
    recovery: SharedRecoveryState,
    mut run: Run,
    mut cleanup: Cleanup,
) where
    Run: FnMut(CancellationToken, SharedWorkerPhase, SharedRecoveryState) -> RunFuture,
    RunFuture: Future<Output = ()> + Send + 'static,
    Cleanup: FnMut() -> CleanupFuture,
    CleanupFuture: Future<Output = ()> + Send,
{
    loop {
        if shutdown.is_cancelled() {
            break;
        }

        *phase.write().await = HqpWorkerPhase::Recovering;
        let child_shutdown = shutdown.child_token();
        let mut child = tokio::spawn(run(child_shutdown.clone(), phase.clone(), recovery.clone()));

        let (completion, unexpected) = tokio::select! {
            _ = shutdown.cancelled() => {
                child_shutdown.cancel();
                (child.await, false)
            }
            result = &mut child => (result, true),
        };

        if !unexpected {
            match completion {
                Ok(()) => tracing::debug!("HQPlayer worker {name} joined during shutdown"),
                Err(error) => {
                    tracing::warn!(
                        "HQPlayer worker {name} failed while joining during shutdown: {error}"
                    );
                    // The observer's normal shutdown path disconnects itself. An abnormal join
                    // result bypassed that guarantee, so clear any still-reachable state here.
                    cleanup().await;
                }
            }
            break;
        }

        if completion.is_ok() && shutdown.is_cancelled() {
            // A cancellation-aware observer may finish in the same scheduler turn as the parent
            // token, allowing its join branch to win the select. Its normal exit path already
            // disconnected; treat this as an ordinary shutdown, not a failed producer epoch.
            tracing::debug!("HQPlayer worker {name} joined during shutdown");
            break;
        }

        match completion {
            Ok(()) => tracing::warn!("HQPlayer worker {name} exited unexpectedly"),
            Err(error) if error.is_panic() => {
                tracing::error!("HQPlayer worker {name} panicked: {error}")
            }
            Err(error) => tracing::warn!("HQPlayer worker {name} failed to join: {error}"),
        }
        *phase.write().await = HqpWorkerPhase::Failed;
        let delay = recovery.lock().await.on_failure(Instant::now());

        // A panic can bypass the observer's final disconnect. Clear reachable state before any
        // replacement is allowed to publish a new producer epoch. Cleanup also remains mandatory
        // when manager shutdown races this already-observed unexpected completion; cancellation
        // suppresses only replacement and backoff.
        cleanup().await;
        if shutdown.is_cancelled() {
            break;
        }

        tracing::warn!("HQPlayer worker {name} will be replaced after {:?}", delay);
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep(delay) => {}
        }
    }

    *phase.write().await = HqpWorkerPhase::Stopped;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{Mutex, Notify, RwLock};
    use tokio::time::Instant;
    use tokio_util::sync::CancellationToken;

    fn policy() -> HqpRecoveryConfig {
        HqpRecoveryConfig {
            poll_interval: Duration::from_secs(2),
            short_retry_delay: Duration::from_millis(10),
            restart_window: Duration::from_millis(100),
            backoff_initial: Duration::from_millis(20),
            backoff_cap: Duration::from_millis(80),
            stable_threshold: Duration::from_millis(50),
        }
    }

    #[test]
    fn restart_window_then_exponential_backoff_is_exact_and_capped() {
        let mut recovery = RecoveryState::new(policy());
        let start = Instant::now();

        assert_eq!(recovery.on_failure(start), Duration::from_millis(10));
        assert_eq!(
            recovery.on_failure(start + Duration::from_millis(100)),
            Duration::from_millis(10),
            "the measured restart window includes its boundary"
        );
        assert_eq!(
            recovery.on_failure(start + Duration::from_millis(101)),
            Duration::from_millis(20)
        );
        assert_eq!(
            recovery.on_failure(start + Duration::from_millis(110)),
            Duration::from_millis(40)
        );
        assert_eq!(
            recovery.on_failure(start + Duration::from_millis(120)),
            Duration::from_millis(80)
        );
        assert_eq!(
            recovery.on_failure(start + Duration::from_millis(130)),
            Duration::from_millis(80),
            "prolonged outages stay at the cap"
        );
    }

    #[test]
    fn only_stable_coherent_health_resets_retry_debt() {
        let mut recovery = RecoveryState::new(policy());
        let start = Instant::now();

        assert_eq!(recovery.on_failure(start), Duration::from_millis(10));
        assert_eq!(
            recovery.on_failure(start + Duration::from_millis(101)),
            Duration::from_millis(20)
        );
        assert_eq!(
            recovery.on_failure(start + Duration::from_millis(110)),
            Duration::from_millis(40)
        );

        assert!(
            !recovery.on_coherent_success(start + Duration::from_millis(120)),
            "one coherent observation is healthy but not stable"
        );
        assert_eq!(
            recovery.on_failure(start + Duration::from_millis(130)),
            Duration::from_millis(80),
            "brief health must preserve the existing outage debt"
        );

        assert!(!recovery.on_coherent_success(start + Duration::from_millis(140)));
        assert!(
            recovery.on_coherent_success(start + Duration::from_millis(190)),
            "coherent health spanning the threshold resets the outage"
        );
        assert_eq!(
            recovery.on_failure(start + Duration::from_millis(191)),
            Duration::from_millis(10),
            "the next outage starts in the short restart window"
        );
    }

    #[test]
    fn zero_duration_configuration_cannot_create_a_busy_retry_loop() {
        let zero = HqpRecoveryConfig {
            poll_interval: Duration::ZERO,
            short_retry_delay: Duration::ZERO,
            restart_window: Duration::ZERO,
            backoff_initial: Duration::ZERO,
            backoff_cap: Duration::ZERO,
            stable_threshold: Duration::ZERO,
        };
        let mut recovery = RecoveryState::new(zero);
        let config = recovery.config();

        assert!(config.poll_interval >= Duration::from_millis(1));
        assert!(config.short_retry_delay >= Duration::from_millis(1));
        assert!(config.backoff_initial >= Duration::from_millis(1));
        assert!(config.backoff_cap >= Duration::from_millis(1));
        assert!(recovery.on_failure(Instant::now()) >= Duration::from_millis(1));
    }

    #[tokio::test]
    async fn unexpected_normal_exit_is_cleaned_and_replaced_exactly_once() {
        let shutdown = CancellationToken::new();
        let phase = Arc::new(RwLock::new(HqpWorkerPhase::Recovering));
        let recovery = Arc::new(Mutex::new(RecoveryState::new(policy())));
        let starts = Arc::new(AtomicUsize::new(0));
        let second_started = Arc::new(Notify::new());
        let cleanups = Arc::new(AtomicUsize::new(0));

        let task = tokio::spawn(supervise_worker(
            "rig".to_string(),
            shutdown.clone(),
            phase.clone(),
            recovery,
            {
                let starts = starts.clone();
                let second_started = second_started.clone();
                move |child_shutdown, _, _| {
                    let starts = starts.clone();
                    let second_started = second_started.clone();
                    async move {
                        if starts.fetch_add(1, Ordering::SeqCst) == 0 {
                            return;
                        }
                        second_started.notify_one();
                        child_shutdown.cancelled().await;
                    }
                }
            },
            {
                let cleanups = cleanups.clone();
                move || {
                    let cleanups = cleanups.clone();
                    async move {
                        cleanups.fetch_add(1, Ordering::SeqCst);
                    }
                }
            },
        ));

        second_started.notified().await;
        shutdown.cancel();
        task.await.expect("supervisor joins");

        assert_eq!(starts.load(Ordering::SeqCst), 2);
        assert_eq!(cleanups.load(Ordering::SeqCst), 1);
        assert_eq!(*phase.read().await, HqpWorkerPhase::Stopped);
    }

    #[tokio::test]
    async fn unwind_panic_is_joined_cleaned_and_replaced_exactly_once() {
        let shutdown = CancellationToken::new();
        let phase = Arc::new(RwLock::new(HqpWorkerPhase::Recovering));
        let recovery = Arc::new(Mutex::new(RecoveryState::new(policy())));
        let starts = Arc::new(AtomicUsize::new(0));
        let replacement_started = Arc::new(Notify::new());
        let cleanups = Arc::new(AtomicUsize::new(0));

        let task = tokio::spawn(supervise_worker(
            "rig".to_string(),
            shutdown.clone(),
            phase,
            recovery,
            {
                let starts = starts.clone();
                let replacement_started = replacement_started.clone();
                move |child_shutdown, _, _| {
                    let starts = starts.clone();
                    let replacement_started = replacement_started.clone();
                    async move {
                        if starts.fetch_add(1, Ordering::SeqCst) == 0 {
                            panic!("injected observer panic");
                        }
                        replacement_started.notify_one();
                        child_shutdown.cancelled().await;
                    }
                }
            },
            {
                let cleanups = cleanups.clone();
                move || {
                    let cleanups = cleanups.clone();
                    async move {
                        cleanups.fetch_add(1, Ordering::SeqCst);
                    }
                }
            },
        ));

        replacement_started.notified().await;
        shutdown.cancel();
        task.await.expect("supervisor joins");

        assert_eq!(starts.load(Ordering::SeqCst), 2);
        assert_eq!(cleanups.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn panic_while_shutdown_is_joining_still_clears_reachable_state() {
        let shutdown = CancellationToken::new();
        let phase = Arc::new(RwLock::new(HqpWorkerPhase::Recovering));
        let recovery = Arc::new(Mutex::new(RecoveryState::new(policy())));
        let starts = Arc::new(AtomicUsize::new(0));
        let child_started = Arc::new(Notify::new());
        let release_panic = Arc::new(Notify::new());
        let cleanups = Arc::new(AtomicUsize::new(0));
        let state_reachable = Arc::new(AtomicBool::new(true));

        let task = tokio::spawn(supervise_worker(
            "rig".to_string(),
            shutdown.clone(),
            phase.clone(),
            recovery,
            {
                let starts = starts.clone();
                let child_started = child_started.clone();
                let release_panic = release_panic.clone();
                move |_, _, _| {
                    let starts = starts.clone();
                    let child_started = child_started.clone();
                    let release_panic = release_panic.clone();
                    async move {
                        starts.fetch_add(1, Ordering::SeqCst);
                        child_started.notify_one();
                        release_panic.notified().await;
                        panic!("injected observer panic concurrent with shutdown");
                    }
                }
            },
            {
                let cleanups = cleanups.clone();
                let state_reachable = state_reachable.clone();
                move || {
                    let cleanups = cleanups.clone();
                    let state_reachable = state_reachable.clone();
                    async move {
                        cleanups.fetch_add(1, Ordering::SeqCst);
                        state_reachable.store(false, Ordering::SeqCst);
                    }
                }
            },
        ));

        child_started.notified().await;
        shutdown.cancel();
        tokio::task::yield_now().await;
        release_panic.notify_one();
        task.await.expect("supervisor joins");

        assert_eq!(starts.load(Ordering::SeqCst), 1, "shutdown forbids respawn");
        assert_eq!(
            cleanups.load(Ordering::SeqCst),
            1,
            "an abnormal child result must be cleaned even after shutdown wins"
        );
        assert!(
            !state_reachable.load(Ordering::SeqCst),
            "stale adapter and zone state must not remain reachable"
        );
        assert_eq!(*phase.read().await, HqpWorkerPhase::Stopped);
    }

    #[tokio::test]
    async fn repeated_child_exits_share_and_escalate_one_outage_debt() {
        let mut escalating_policy = policy();
        escalating_policy.short_retry_delay = Duration::from_millis(1);
        escalating_policy.restart_window = Duration::ZERO;
        escalating_policy.backoff_initial = Duration::from_millis(2);
        escalating_policy.backoff_cap = Duration::from_millis(4);
        let shutdown = CancellationToken::new();
        let phase = Arc::new(RwLock::new(HqpWorkerPhase::Recovering));
        let recovery = Arc::new(Mutex::new(RecoveryState::new(escalating_policy)));
        let starts = Arc::new(AtomicUsize::new(0));
        let fourth_started = Arc::new(Notify::new());
        let cleanups = Arc::new(AtomicUsize::new(0));

        let task = tokio::spawn(supervise_worker(
            "rig".to_string(),
            shutdown.clone(),
            phase,
            recovery.clone(),
            {
                let starts = starts.clone();
                let fourth_started = fourth_started.clone();
                move |child_shutdown, _, _| {
                    let starts = starts.clone();
                    let fourth_started = fourth_started.clone();
                    async move {
                        if starts.fetch_add(1, Ordering::SeqCst) < 3 {
                            return;
                        }
                        fourth_started.notify_one();
                        child_shutdown.cancelled().await;
                    }
                }
            },
            {
                let cleanups = cleanups.clone();
                move || {
                    let cleanups = cleanups.clone();
                    async move {
                        cleanups.fetch_add(1, Ordering::SeqCst);
                    }
                }
            },
        ));

        tokio::time::timeout(Duration::from_millis(100), fourth_started.notified())
            .await
            .expect("three bounded replacement delays complete");
        {
            let recovery = recovery.lock().await;
            assert!(
                recovery.outage_started.is_some(),
                "replacement children retain the original outage epoch"
            );
            assert_eq!(
                recovery.next_backoff,
                Duration::from_millis(4),
                "replacement children share retry debt through the configured cap"
            );
        }
        assert_eq!(starts.load(Ordering::SeqCst), 4);
        assert_eq!(cleanups.load(Ordering::SeqCst), 3);

        shutdown.cancel();
        task.await.expect("supervisor joins");
    }

    #[tokio::test]
    async fn cancellation_interrupts_replacement_backoff_without_respawn() {
        let mut long_policy = policy();
        long_policy.short_retry_delay = Duration::from_secs(60 * 60);
        let shutdown = CancellationToken::new();
        let phase = Arc::new(RwLock::new(HqpWorkerPhase::Recovering));
        let recovery = Arc::new(Mutex::new(RecoveryState::new(long_policy)));
        let starts = Arc::new(AtomicUsize::new(0));
        let cleaned = Arc::new(Notify::new());

        let task = tokio::spawn(supervise_worker(
            "rig".to_string(),
            shutdown.clone(),
            phase.clone(),
            recovery,
            {
                let starts = starts.clone();
                move |_, _, _| {
                    let starts = starts.clone();
                    async move {
                        starts.fetch_add(1, Ordering::SeqCst);
                    }
                }
            },
            {
                let cleaned = cleaned.clone();
                move || {
                    let cleaned = cleaned.clone();
                    async move {
                        cleaned.notify_one();
                    }
                }
            },
        ));

        cleaned.notified().await;
        assert_eq!(*phase.read().await, HqpWorkerPhase::Failed);
        shutdown.cancel();
        tokio::time::timeout(Duration::from_millis(100), task)
            .await
            .expect("cancellation interrupts the one-hour replacement delay")
            .expect("supervisor joins");

        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(*phase.read().await, HqpWorkerPhase::Stopped);
    }
}
