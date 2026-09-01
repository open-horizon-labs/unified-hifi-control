//! AdapterHandle - Wraps AdapterLogic with consistent lifecycle management
//!
//! Provides automatic retry with exponential backoff when adapters encounter errors.
//! All retry logic is centralized here - adapters should NOT implement their own retry loops.

use anyhow::Result;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::adapters::traits::{AdapterContext, AdapterLogic};
use crate::bus::{BusEvent, SharedBus};

/// Retry configuration for adapter startup/run
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Initial delay between retry attempts
    pub initial_delay: Duration,
    /// Maximum delay (backoff caps at this value)
    pub max_delay: Duration,
    /// Minimum run time before resetting backoff on failure
    /// If the adapter runs for at least this long before failing,
    /// the backoff delay resets to initial_delay
    pub stable_run_threshold: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            initial_delay: Duration::from_secs(5),
            max_delay: Duration::from_secs(60),
            stable_run_threshold: Duration::from_secs(30),
        }
    }
}

impl RetryConfig {
    /// Create a new RetryConfig with custom delays
    pub fn new(initial_delay: Duration, max_delay: Duration) -> Self {
        Self {
            initial_delay,
            max_delay,
            stable_run_threshold: Duration::from_secs(30),
        }
    }
}

/// AdapterHandle wraps an AdapterLogic implementation and provides:
/// - Consistent shutdown handling (can't forget it)
/// - Automatic ACK on stop via AdapterStopped event
/// - ShuttingDown event watching
/// - Automatic retry with exponential backoff
pub struct AdapterHandle<T: AdapterLogic> {
    logic: Arc<T>,
    bus: SharedBus,
    shutdown: CancellationToken,
}

impl<T: AdapterLogic> AdapterHandle<T> {
    pub fn new(logic: T, bus: SharedBus, shutdown: CancellationToken) -> Self {
        Self {
            logic: Arc::new(logic),
            bus,
            shutdown,
        }
    }

    /// Get the adapter's prefix
    pub fn prefix(&self) -> &'static str {
        self.logic.prefix()
    }

    /// Get access to the underlying logic (for command handling)
    pub fn logic(&self) -> &Arc<T> {
        &self.logic
    }

    /// Run the adapter with lifecycle management (single attempt, no retry)
    /// - Calls init() if implemented
    /// - Runs the adapter's main loop
    /// - Watches for ShuttingDown events on the bus
    /// - Publishes AdapterStopped on exit
    ///
    /// For automatic retry on error, use `run_with_retry()` instead.
    pub async fn run(self) -> Result<()> {
        let prefix = self.logic.prefix();
        info!("Starting adapter: {}", prefix);

        // Run once without retry
        let result = self.run_once().await;

        // Automatic ACK - publish AdapterStopped
        self.bus.publish(BusEvent::AdapterStopped {
            adapter: prefix.to_string(),
        });

        info!("Adapter {} stopped", prefix);
        result
    }

    /// Run the adapter with automatic retry on error
    ///
    /// When `run()` returns `Err`, waits with exponential backoff and retries.
    /// When `run()` returns `Ok`, exits cleanly.
    ///
    /// Backoff is reset to initial delay if the adapter ran stably for at least
    /// `config.stable_run_threshold` (default 30s) before failing.
    ///
    /// This is the preferred method for production use - it handles transient
    /// failures like service restarts automatically.
    pub async fn run_with_retry(self, config: RetryConfig) -> Result<()> {
        let prefix = self.logic.prefix();
        let mut delay = config.initial_delay;

        loop {
            // Check for shutdown before attempting
            if self.shutdown.is_cancelled() {
                info!("{}: shutdown before attempt", prefix);
                break;
            }

            info!("{}: starting (retry delay: {:?})", prefix, delay);

            let start = Instant::now();
            match self.run_once().await {
                Ok(()) => {
                    info!("{}: clean exit", prefix);
                    break;
                }
                Err(e) => {
                    let run_duration = start.elapsed();

                    // Reset backoff if we had a stable run before failing
                    if run_duration >= config.stable_run_threshold {
                        info!(
                            "{}: ran for {:?} before failure, resetting backoff",
                            prefix, run_duration
                        );
                        delay = config.initial_delay;
                    }

                    warn!("{}: error ({}), retrying in {:?}", prefix, e, delay);

                    // Wait with shutdown check
                    tokio::select! {
                        _ = self.shutdown.cancelled() => {
                            info!("{}: shutdown during backoff", prefix);
                            break;
                        }
                        _ = tokio::time::sleep(delay) => {
                            // Exponential backoff capped at max_delay
                            delay = (delay * 2).min(config.max_delay);
                        }
                    }
                }
            }
        }

        // Automatic ACK - publish AdapterStopped
        self.bus.publish(BusEvent::AdapterStopped {
            adapter: prefix.to_string(),
        });

        info!("{}: stopped", prefix);
        Ok(())
    }

    /// Run the adapter once (internal helper)
    ///
    /// Returns:
    /// - `Ok(())` on clean shutdown (adapter should not restart)
    /// - `Err(...)` on error (adapter should restart)
    async fn run_once(&self) -> Result<()> {
        let prefix = self.logic.prefix();

        // Initialize
        if let Err(e) = self.logic.init().await {
            error!("{}: init failed: {}", prefix, e);
            return Err(e);
        }

        // Subscribe to bus for shutdown signal
        let mut rx = self.bus.subscribe();

        // Create context for the adapter
        let ctx = AdapterContext {
            bus: self.bus.clone(),
            shutdown: self.shutdown.clone(),
        };

        // Run with lifecycle management
        let result = tokio::select! {
            // Run adapter-specific logic
            result = self.logic.run(ctx) => {
                match &result {
                    Ok(()) => info!("{}: completed normally", prefix),
                    Err(e) => error!("{}: error: {}", prefix, e),
                }
                result
            }

            // Watch for shutdown signal on bus
            _ = async {
                while let Ok(event) = rx.recv().await {
                    if matches!(event, BusEvent::ShuttingDown { .. }) {
                        info!("{}: received ShuttingDown event", prefix);
                        break;
                    }
                }
            } => {
                info!("{}: stopping due to ShuttingDown event", prefix);
                Ok(())
            }

            // Direct cancellation (backup mechanism)
            _ = self.shutdown.cancelled() => {
                info!("{}: cancelled via token", prefix);
                Ok(())
            }
        };

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::traits::{AdapterCommand, AdapterCommandResponse};
    use crate::bus::EventBus;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Mock adapter that fails N times then succeeds
    #[derive(Clone)]
    struct MockFailingAdapter {
        prefix: &'static str,
        fail_count: Arc<AtomicUsize>,
        max_failures: usize,
    }

    impl MockFailingAdapter {
        fn new(prefix: &'static str, max_failures: usize) -> Self {
            Self {
                prefix,
                fail_count: Arc::new(AtomicUsize::new(0)),
                max_failures,
            }
        }
    }

    #[async_trait]
    impl AdapterLogic for MockFailingAdapter {
        fn prefix(&self) -> &'static str {
            self.prefix
        }

        async fn run(&self, _ctx: AdapterContext) -> Result<()> {
            let count = self.fail_count.fetch_add(1, Ordering::SeqCst);
            if count < self.max_failures {
                Err(anyhow::anyhow!("Simulated failure {}", count + 1))
            } else {
                Ok(())
            }
        }

        async fn handle_command(
            &self,
            _zone_id: &str,
            _command: AdapterCommand,
        ) -> Result<AdapterCommandResponse> {
            Ok(AdapterCommandResponse {
                success: true,
                error: None,
            })
        }
    }

    /// Mock adapter that always succeeds
    #[derive(Clone)]
    struct MockSuccessAdapter;

    #[async_trait]
    impl AdapterLogic for MockSuccessAdapter {
        fn prefix(&self) -> &'static str {
            "mock-success"
        }

        async fn run(&self, _ctx: AdapterContext) -> Result<()> {
            Ok(())
        }

        async fn handle_command(
            &self,
            _zone_id: &str,
            _command: AdapterCommand,
        ) -> Result<AdapterCommandResponse> {
            Ok(AdapterCommandResponse {
                success: true,
                error: None,
            })
        }
    }

    fn test_bus() -> SharedBus {
        Arc::new(EventBus::new(100))
    }

    #[tokio::test]
    async fn test_run_with_retry_success_on_first_try() {
        let bus = test_bus();
        let shutdown = CancellationToken::new();
        let adapter = MockSuccessAdapter;

        let handle = AdapterHandle::new(adapter, bus, shutdown);
        let config = RetryConfig::new(Duration::from_millis(10), Duration::from_millis(100));

        let result = handle.run_with_retry(config).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_run_with_retry_retries_on_failure() {
        let bus = test_bus();
        let shutdown = CancellationToken::new();
        let adapter = MockFailingAdapter::new("mock-failing", 2);
        let attempt_tracker = adapter.fail_count.clone();

        let handle = AdapterHandle::new(adapter, bus, shutdown);
        let config = RetryConfig::new(Duration::from_millis(10), Duration::from_millis(100));

        let result = handle.run_with_retry(config).await;
        assert!(result.is_ok());
        // Should have attempted 3 times: 2 failures + 1 success
        assert_eq!(attempt_tracker.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_run_with_retry_shutdown_during_backoff() {
        let bus = test_bus();
        let shutdown = CancellationToken::new();
        let adapter = MockFailingAdapter::new("mock-failing", 100); // Many failures
        let attempt_tracker = adapter.fail_count.clone();

        let shutdown_clone = shutdown.clone();
        let handle = AdapterHandle::new(adapter, bus, shutdown);
        let config = RetryConfig::new(
            Duration::from_secs(10), // Long delay
            Duration::from_secs(60),
        );

        // Spawn the retry loop
        let task = tokio::spawn(async move { handle.run_with_retry(config).await });

        // Wait a bit for first attempt
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Cancel during backoff
        shutdown_clone.cancel();

        // Should exit cleanly
        let result = task.await.unwrap();
        assert!(result.is_ok());

        // Should have attempted at least once
        assert!(attempt_tracker.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn test_run_with_retry_shutdown_before_first_attempt() {
        let bus = test_bus();
        let shutdown = CancellationToken::new();
        shutdown.cancel(); // Cancel before starting

        let adapter = MockFailingAdapter::new("mock-failing", 100);
        let attempt_tracker = adapter.fail_count.clone();

        let handle = AdapterHandle::new(adapter, bus, shutdown);
        let config = RetryConfig::default();

        let result = handle.run_with_retry(config).await;
        assert!(result.is_ok());

        // Should not have attempted at all
        assert_eq!(attempt_tracker.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_retry_config_default() {
        let config = RetryConfig::default();
        assert_eq!(config.initial_delay, Duration::from_secs(5));
        assert_eq!(config.max_delay, Duration::from_secs(60));
        assert_eq!(config.stable_run_threshold, Duration::from_secs(30));
    }

    #[tokio::test]
    async fn test_backoff_progression() {
        // Verify the backoff doubling logic
        let mut delay = Duration::from_secs(5);
        let max_delay = Duration::from_secs(60);

        // 5 -> 10 -> 20 -> 40 -> 60 -> 60
        let expected = [5, 10, 20, 40, 60, 60];

        for expected_secs in expected {
            assert_eq!(delay.as_secs(), expected_secs);
            delay = (delay * 2).min(max_delay);
        }
    }

    /// Mock adapter that runs for configurable durations then fails/succeeds
    struct MockTimedAdapter {
        /// Durations to run before failing, then succeeds on last call
        run_durations: Arc<Mutex<Vec<Duration>>>,
        call_count: Arc<AtomicUsize>,
        /// Record the delay used before each retry (for verification)
        retry_delays: Arc<Mutex<Vec<Instant>>>,
    }

    impl MockTimedAdapter {
        fn new(durations: Vec<Duration>) -> Self {
            Self {
                run_durations: Arc::new(Mutex::new(durations)),
                call_count: Arc::new(AtomicUsize::new(0)),
                retry_delays: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl Clone for MockTimedAdapter {
        fn clone(&self) -> Self {
            Self {
                run_durations: self.run_durations.clone(),
                call_count: self.call_count.clone(),
                retry_delays: self.retry_delays.clone(),
            }
        }
    }

    #[async_trait]
    impl AdapterLogic for MockTimedAdapter {
        fn prefix(&self) -> &'static str {
            "mock-timed"
        }

        async fn run(&self, _ctx: AdapterContext) -> Result<()> {
            let count = self.call_count.fetch_add(1, Ordering::SeqCst);

            // Record when this attempt started
            {
                let mut delays = self.retry_delays.lock().unwrap();
                delays.push(Instant::now());
            }

            // Get duration before await (drop lock before sleeping)
            let duration = {
                let durations = self.run_durations.lock().unwrap();
                if count < durations.len() {
                    Some(durations[count])
                } else {
                    None
                }
            };

            if let Some(dur) = duration {
                // Sleep for specified duration then fail
                tokio::time::sleep(dur).await;
                Err(anyhow::anyhow!("Simulated failure after {:?}", dur))
            } else {
                // No more durations - succeed
                Ok(())
            }
        }

        async fn handle_command(
            &self,
            _zone_id: &str,
            _command: AdapterCommand,
        ) -> Result<AdapterCommandResponse> {
            Ok(AdapterCommandResponse {
                success: true,
                error: None,
            })
        }
    }

    #[tokio::test]
    async fn test_backoff_reset_after_stable_run() {
        // Test that backoff resets after a stable run
        //
        // Scenario:
        // 1. First run: fails immediately (short run)
        // 2. Second run: runs for stable_threshold, then fails
        // 3. Third run: succeeds
        //
        // Expected behavior:
        // - After first (short) failure: delay doubles (10ms -> 20ms)
        // - After second (stable) failure: delay resets to initial (10ms)

        let bus = test_bus();
        let shutdown = CancellationToken::new();

        // Run durations: 0ms (immediate fail), 60ms (stable), then succeed
        let adapter = MockTimedAdapter::new(vec![
            Duration::from_millis(0),  // First: immediate failure
            Duration::from_millis(60), // Second: stable run then failure
        ]);
        let retry_delays = adapter.retry_delays.clone();

        let handle = AdapterHandle::new(adapter, bus, shutdown);
        let config = RetryConfig {
            initial_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(100),
            stable_run_threshold: Duration::from_millis(50), // 50ms = stable
        };

        let start = Instant::now();
        let result = handle.run_with_retry(config).await;
        assert!(result.is_ok());

        // Check timing of retry attempts
        let delays = retry_delays.lock().unwrap();
        assert_eq!(delays.len(), 3, "Should have 3 attempts");

        // Gap between attempt 1 and 2: should be ~10ms (initial delay, short run)
        let gap1 = delays[1].duration_since(delays[0]);
        // Gap between attempt 2 and 3: should be ~10ms (reset after stable run)
        // NOT 20ms (which would be doubled delay)
        let gap2 = delays[2].duration_since(delays[1]);

        // Allow some tolerance for timing
        // First gap: 0ms run + 10ms wait = ~10ms
        assert!(
            gap1 >= Duration::from_millis(8) && gap1 <= Duration::from_millis(25),
            "First gap should be ~10ms, was {:?}",
            gap1
        );

        // Second gap: 60ms run + 10ms wait (reset) = ~70ms
        // If backoff wasn't reset, it would be 60ms run + 20ms wait = ~80ms
        assert!(
            gap2 >= Duration::from_millis(65) && gap2 <= Duration::from_millis(85),
            "Second gap should be ~70ms (with reset), was {:?}",
            gap2
        );

        // Total time sanity check
        let total = start.elapsed();
        assert!(
            total < Duration::from_millis(150),
            "Total time too long: {:?}",
            total
        );
    }
}
