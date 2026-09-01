//! AdapterCoordinator - Centralized lifecycle management for adapters
//!
//! The coordinator serves as a registry of all available adapters and manages their lifecycle.
//! It tracks which adapters are enabled and handles starting/stopping them uniformly.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::adapters::Startable;
use crate::api::AdapterSettings;
use crate::bus::{BusEvent, SharedBus};
use std::sync::Arc;

/// All available adapters in the system.
/// This is the single source of truth for what adapters exist.
/// Note: "lms-cli" is a companion to "lms" and shares its enabled state.
pub const AVAILABLE_ADAPTERS: &[&str] = &["roon", "lms", "lms-cli", "openhome", "upnp", "hqplayer"];

/// Registered adapter with its spawn function
struct RegisteredAdapter {
    /// Adapter prefix (e.g., "lms", "roon")
    #[allow(dead_code)]
    prefix: String,
    /// Whether this adapter is currently enabled
    enabled: bool,
    /// Running task handle (if started)
    handle: Option<JoinHandle<()>>,
    /// Startable adapters own their child tasks internally rather than exposing a JoinHandle here.
    direct_running: bool,
    /// Cancellation token for this adapter
    cancel: CancellationToken,
}

/// AdapterCoordinator manages adapter lifecycle:
/// - Register adapters by prefix
/// - Start only enabled adapters
/// - Coordinate graceful shutdown
pub struct AdapterCoordinator {
    adapters: RwLock<HashMap<String, RegisteredAdapter>>,
    /// Companion workers grouped under their provider's one client-visible
    /// projection. The coordinator owns this topology so every lifecycle path
    /// applies the same cancellation fence.
    companions: RwLock<HashMap<String, Vec<Arc<dyn Startable>>>>,
    bus: SharedBus,
    /// Global shutdown token (parent of all adapter tokens)
    shutdown: CancellationToken,
    /// Timeout for shutdown acknowledgments
    shutdown_timeout: Duration,
}

impl AdapterCoordinator {
    pub fn new(bus: SharedBus) -> Self {
        Self {
            adapters: RwLock::new(HashMap::new()),
            companions: RwLock::new(HashMap::new()),
            bus,
            shutdown: CancellationToken::new(),
            shutdown_timeout: Duration::from_secs(5),
        }
    }

    /// Create with custom shutdown timeout
    pub fn with_shutdown_timeout(bus: SharedBus, timeout: Duration) -> Self {
        Self {
            adapters: RwLock::new(HashMap::new()),
            companions: RwLock::new(HashMap::new()),
            bus,
            shutdown: CancellationToken::new(),
            shutdown_timeout: timeout,
        }
    }

    /// Register all available adapters using settings to determine enabled state.
    /// This is the primary way to initialize the coordinator.
    pub async fn register_from_settings(&self, settings: &AdapterSettings) {
        for &name in AVAILABLE_ADAPTERS {
            let enabled = match name {
                "roon" => settings.roon,
                "lms" => settings.lms,
                // lms-cli shares enabled state with lms (companion adapter)
                "lms-cli" => settings.lms,
                "openhome" => settings.openhome,
                "upnp" => settings.upnp,
                "hqplayer" => settings.hqplayer,
                _ => false,
            };
            self.register(name, enabled).await;
            if enabled {
                info!("Adapter {} enabled", name);
            } else {
                info!("Adapter {} disabled", name);
            }
        }
    }

    /// Start all enabled adapters from the provided list.
    /// This is the single codepath for starting adapters.
    pub async fn start_all_enabled(&self, adapters: &[Arc<dyn Startable>]) {
        let companion_names = self.companion_names().await;
        for adapter in adapters {
            let name = adapter.name();
            if companion_names.iter().any(|companion| companion == name) {
                continue;
            }
            if !self.is_enabled(name).await {
                debug!("Adapter {} is disabled, skipping", name);
                continue;
            }
            if !adapter.can_start().await {
                debug!("Adapter {} cannot start (not configured?), skipping", name);
                continue;
            }
            match self.start_adapter_and_companions(adapter.as_ref()).await {
                Ok(()) => info!("Started adapter: {}", name),
                Err(e) => warn!("Failed to start adapter {}: {}", name, e),
            }
        }
    }

    /// Start one direct [`Startable`] and update the coordinator's lifecycle
    /// record.  HTTP configuration paths use this rather than owning a shadow
    /// "running" state beside the coordinator.
    pub async fn start_adapter_and_track(&self, adapter: &dyn Startable) -> Result<()> {
        if !adapter.can_start().await {
            return Err(anyhow::anyhow!("adapter cannot start"));
        }
        adapter.start().await?;
        self.set_running(adapter.name(), true).await;
        Ok(())
    }

    /// Stop all adapters from the provided list.
    pub async fn stop_all(&self, adapters: &[Arc<dyn Startable>]) {
        let companion_names = self.companion_names().await;
        for adapter in adapters {
            if companion_names
                .iter()
                .any(|companion| companion == adapter.name())
            {
                continue;
            }
            self.stop_adapter_and_companions_then_flush(
                adapter.as_ref(),
                adapter.name(),
                "coordinator shutdown",
            )
            .await;
        }
    }

    /// Retire an adapter's client-visible projection before stopping its worker.
    ///
    /// `ZoneAggregator` owns zones, so this is the one lifecycle operation for
    /// every stop path: publish `AdapterStopping`, stop the adapter, then update
    /// the coordinator's running state. The caller supplies a precise reason so
    /// observability distinguishes shutdown, reconfiguration, and settings disable.
    pub async fn stop_adapter_and_flush(&self, adapter: &dyn Startable, reason: &str) {
        self.bus.publish(BusEvent::AdapterStopping {
            adapter: adapter.name().to_string(),
            reason: Some(reason.to_string()),
        });
        adapter.stop().await;
        self.set_running(adapter.name(), false).await;
        debug!("Stopped adapter: {}", adapter.name());
    }

    /// Cancel a group of workers which jointly observe one provider, then retire that
    /// provider's projection exactly once.
    ///
    /// LMS is the current user: its HTTP poller and CLI subscription share an
    /// `lms:` projection.  Retiring the projection after only one observer has
    /// stopped lets the other observer re-publish a fact from the old server.
    /// Cancellation is therefore issued to *every* observer before the
    /// aggregator is told the provider has stopped.  The explicit projection
    /// owner is separate from worker names because companion workers need not
    /// have their own zone-ID prefix.
    pub async fn stop_adapters_then_flush(
        &self,
        adapters: &[&dyn Startable],
        projection_adapter: &str,
        reason: &str,
    ) {
        for adapter in adapters {
            adapter.stop().await;
        }
        self.bus.publish(BusEvent::AdapterStopping {
            adapter: projection_adapter.to_string(),
            reason: Some(reason.to_string()),
        });
        for adapter in adapters {
            self.set_running(adapter.name(), false).await;
            debug!("Stopped adapter: {}", adapter.name());
        }
    }

    /// Register a worker that observes the same provider projection as
    /// `provider`. Composition does this once; handlers never need to know a
    /// provider's internal worker topology.
    pub async fn register_companion(&self, provider: &str, companion: Arc<dyn Startable>) {
        self.companions
            .write()
            .await
            .entry(provider.to_string())
            .or_default()
            .push(companion);
    }

    /// Start a provider's primary adapter and every registered companion,
    /// tracking their lifecycle centrally.
    pub async fn start_adapter_and_companions(&self, adapter: &dyn Startable) -> Result<()> {
        self.start_adapter_and_track(adapter).await?;
        let companions = self
            .companions
            .read()
            .await
            .get(adapter.name())
            .cloned()
            .unwrap_or_default();
        for companion in companions {
            if let Err(error) = self.start_adapter_and_track(companion.as_ref()).await {
                self.stop_adapters_then_flush(
                    &[adapter],
                    adapter.name(),
                    "companion start failure",
                )
                .await;
                return Err(error);
            }
        }
        Ok(())
    }

    /// Cancel a provider's primary worker and its registered companions, then
    /// retire their shared projection once.
    pub async fn stop_adapter_and_companions_then_flush(
        &self,
        adapter: &dyn Startable,
        projection_adapter: &str,
        reason: &str,
    ) {
        let companions = self
            .companions
            .read()
            .await
            .get(projection_adapter)
            .cloned()
            .unwrap_or_default();
        let mut observers: Vec<&dyn Startable> = vec![adapter];
        observers.extend(
            companions
                .iter()
                .map(|companion| companion.as_ref() as &dyn Startable),
        );
        self.stop_adapters_then_flush(&observers, projection_adapter, reason)
            .await;
    }

    async fn companion_names(&self) -> Vec<String> {
        self.companions
            .read()
            .await
            .values()
            .flatten()
            .map(|companion| companion.name().to_string())
            .collect()
    }

    /// Register an adapter without starting it
    pub async fn register(&self, prefix: &str, enabled: bool) {
        let mut adapters = self.adapters.write().await;
        adapters.insert(
            prefix.to_string(),
            RegisteredAdapter {
                prefix: prefix.to_string(),
                enabled,
                handle: None,
                direct_running: false,
                cancel: self.shutdown.child_token(),
            },
        );
        debug!("Registered adapter: {} (enabled: {})", prefix, enabled);
    }

    /// Start an adapter with the given spawn function
    /// The spawn function receives (bus, cancel_token) and should spawn the adapter task
    pub async fn start_adapter<F, Fut>(&self, prefix: &str, spawn_fn: F) -> Result<()>
    where
        F: FnOnce(SharedBus, CancellationToken) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut adapters = self.adapters.write().await;

        let adapter = adapters
            .get_mut(prefix)
            .ok_or_else(|| anyhow::anyhow!("Adapter {} not registered", prefix))?;

        if !adapter.enabled {
            debug!("Adapter {} is disabled, not starting", prefix);
            return Ok(());
        }

        if adapter.handle.is_some() {
            debug!("Adapter {} already running", prefix);
            return Ok(());
        }

        let bus = self.bus.clone();
        let cancel = adapter.cancel.clone();

        let handle = tokio::spawn(spawn_fn(bus, cancel));
        adapter.handle = Some(handle);

        info!("Started adapter: {}", prefix);
        Ok(())
    }

    /// Enable/disable an adapter
    pub async fn set_enabled(&self, prefix: &str, enabled: bool) {
        let mut adapters = self.adapters.write().await;
        if let Some(adapter) = adapters.get_mut(prefix) {
            adapter.enabled = enabled;
            debug!("Adapter {} enabled: {}", prefix, enabled);
        }
    }

    /// Record lifecycle state for Startable adapters that own their worker tasks internally.
    pub async fn set_running(&self, prefix: &str, running: bool) {
        if let Some(adapter) = self.adapters.write().await.get_mut(prefix) {
            adapter.direct_running = running;
        }
    }

    /// Check if an adapter is enabled
    pub async fn is_enabled(&self, prefix: &str) -> bool {
        let adapters = self.adapters.read().await;
        adapters.get(prefix).map(|a| a.enabled).unwrap_or(false)
    }

    /// Check if an adapter is running
    pub async fn is_running(&self, prefix: &str) -> bool {
        let adapters = self.adapters.read().await;
        adapters
            .get(prefix)
            .map(|a| a.handle.is_some() || a.direct_running)
            .unwrap_or(false)
    }

    /// Stop a single adapter
    pub async fn stop_adapter(&self, prefix: &str) -> Result<()> {
        // Extract handle while holding lock, reset token immediately to avoid race
        // with concurrent start_adapter cloning a cancelled token
        let handle = {
            let mut adapters = self.adapters.write().await;

            let adapter = adapters
                .get_mut(prefix)
                .ok_or_else(|| anyhow::anyhow!("Adapter {} not registered", prefix))?;

            if adapter.handle.is_none() {
                debug!("Adapter {} not running", prefix);
                return Ok(());
            }

            info!("Stopping adapter: {}", prefix);

            // Cancel the adapter's token
            adapter.cancel.cancel();

            // Reset token immediately so concurrent start_adapter gets fresh token
            adapter.cancel = self.shutdown.child_token();

            // Take the handle - lock released after this block
            adapter.handle.take()
        };

        // Wait for the task to complete with timeout (lock not held)
        if let Some(handle) = handle {
            match tokio::time::timeout(self.shutdown_timeout, handle).await {
                Ok(Ok(())) => {
                    info!("Adapter {} stopped cleanly", prefix);
                }
                Ok(Err(e)) => {
                    error!("Adapter {} task panicked: {}", prefix, e);
                }
                Err(_) => {
                    warn!("Adapter {} did not stop within timeout, abandoning", prefix);
                }
            }
        }

        Ok(())
    }

    /// Graceful shutdown of all adapters
    /// 1. Publish ShuttingDown event
    /// 2. Wait for AdapterStopped ACKs
    /// 3. Cancel any remaining tasks
    pub async fn shutdown(&self) {
        info!("Coordinator initiating shutdown");

        // Get list of running adapters
        let running: Vec<String> = {
            let adapters = self.adapters.read().await;
            adapters
                .iter()
                .filter(|(_, a)| a.handle.is_some())
                .map(|(prefix, _)| prefix.clone())
                .collect()
        };

        if running.is_empty() {
            info!("No adapters running, shutdown complete");
            return;
        }

        info!("Shutting down {} adapter(s): {:?}", running.len(), running);

        // Publish ShuttingDown event
        self.bus.publish(BusEvent::ShuttingDown {
            reason: Some("Coordinator shutdown".to_string()),
        });

        // Wait for AdapterStopped ACKs with timeout
        let acks_received = self.wait_for_acks(&running).await;

        if acks_received < running.len() {
            warn!(
                "Only received {}/{} shutdown ACKs, forcing remaining",
                acks_received,
                running.len()
            );
        }

        // Cancel global token (catches any stragglers)
        self.shutdown.cancel();

        // Collect all task handles (release lock before awaiting)
        let handles: Vec<(String, tokio::task::JoinHandle<()>)> = {
            let mut adapters = self.adapters.write().await;
            adapters
                .iter_mut()
                .filter_map(|(prefix, adapter)| adapter.handle.take().map(|h| (prefix.clone(), h)))
                .collect()
        };

        // Wait for all task handles (lock not held)
        for (prefix, handle) in handles {
            match tokio::time::timeout(Duration::from_secs(1), handle).await {
                Ok(Ok(())) => debug!("Adapter {} task joined", prefix),
                Ok(Err(e)) => warn!("Adapter {} task panicked: {}", prefix, e),
                Err(_) => warn!("Adapter {} task did not join, abandoning", prefix),
            }
        }

        info!("Coordinator shutdown complete");
    }

    /// Wait for AdapterStopped events from running adapters
    async fn wait_for_acks(&self, expected: &[String]) -> usize {
        let mut rx = self.bus.subscribe();
        let mut received: Vec<String> = Vec::new();

        let deadline = tokio::time::Instant::now() + self.shutdown_timeout;

        while received.len() < expected.len() {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }

            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(BusEvent::AdapterStopped { adapter })) => {
                    if expected.contains(&adapter) && !received.contains(&adapter) {
                        debug!("Received ACK from adapter: {}", adapter);
                        received.push(adapter);
                    }
                }
                Ok(Ok(_)) => {
                    // Other event, continue waiting
                }
                Ok(Err(_)) => {
                    // Channel closed
                    break;
                }
                Err(_) => {
                    // Timeout
                    break;
                }
            }
        }

        received.len()
    }

    /// Get list of registered adapter prefixes
    pub async fn registered_adapters(&self) -> Vec<String> {
        self.adapters.read().await.keys().cloned().collect()
    }

    /// Get adapter status for debugging/monitoring
    pub async fn adapter_status(&self) -> HashMap<String, AdapterStatus> {
        let adapters = self.adapters.read().await;
        adapters
            .iter()
            .map(|(prefix, adapter)| {
                (
                    prefix.clone(),
                    AdapterStatus {
                        prefix: prefix.clone(),
                        enabled: adapter.enabled,
                        running: adapter.handle.is_some() || adapter.direct_running,
                    },
                )
            })
            .collect()
    }
}

/// Status information for an adapter
#[derive(Debug, Clone)]
pub struct AdapterStatus {
    pub prefix: String,
    pub enabled: bool,
    pub running: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::create_bus;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    struct DirectStartable {
        started: AtomicBool,
    }

    struct RecordingStartable {
        name: &'static str,
        stopped: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl Startable for RecordingStartable {
        fn name(&self) -> &'static str {
            self.name
        }

        async fn start(&self) -> Result<()> {
            Ok(())
        }

        async fn stop(&self) {
            self.stopped.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait::async_trait]
    impl Startable for DirectStartable {
        fn name(&self) -> &'static str {
            "direct"
        }

        async fn start(&self) -> Result<()> {
            self.started.store(true, Ordering::SeqCst);
            Ok(())
        }

        async fn stop(&self) {
            self.started.store(false, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn test_register_and_check_enabled() {
        let bus = create_bus();
        let coord = AdapterCoordinator::new(bus);

        coord.register("test", true).await;
        assert!(coord.is_enabled("test").await);

        coord.register("disabled", false).await;
        assert!(!coord.is_enabled("disabled").await);
    }

    /// Issue #162: the public settings model has carried an `hqplayer` enable switch while the
    /// lifecycle registry silently omitted it. A configured producer therefore could not enter the
    /// coordinator-owned start/stop path at all.
    ///
    /// **Label: client-red.** The settings surface already promises this toggle controls the adapter.
    #[tokio::test]
    async fn hqplayer_setting_registers_the_managed_adapter() {
        let bus = create_bus();
        let coord = AdapterCoordinator::new(bus);
        let settings = AdapterSettings {
            hqplayer: true,
            ..Default::default()
        };

        coord.register_from_settings(&settings).await;

        assert!(
            coord
                .registered_adapters()
                .await
                .iter()
                .any(|a| a == "hqplayer"),
            "HQPlayer must be a coordinator-owned lifecycle, not an opportunistic page read"
        );
        assert!(coord.is_enabled("hqplayer").await);
    }

    #[tokio::test]
    async fn direct_startable_lifecycle_is_reported_as_running() {
        let bus = create_bus();
        let coord = AdapterCoordinator::new(bus);
        coord.register("direct", true).await;

        let adapter = Arc::new(DirectStartable {
            started: AtomicBool::new(false),
        });
        let adapters: Vec<Arc<dyn Startable>> = vec![adapter.clone()];

        coord.start_all_enabled(&adapters).await;
        assert!(adapter.started.load(Ordering::SeqCst));
        assert!(coord.is_running("direct").await);
        assert!(coord.adapter_status().await["direct"].running);

        coord.stop_all(&adapters).await;
        assert!(!adapter.started.load(Ordering::SeqCst));
        assert!(!coord.is_running("direct").await);
        assert!(!coord.adapter_status().await["direct"].running);
    }

    #[tokio::test]
    async fn grouped_lifecycle_cancels_every_lms_observer_and_retires_one_projection() {
        let bus = create_bus();
        let mut events = bus.subscribe();
        let coord = AdapterCoordinator::new(bus);
        coord.register("lms", true).await;
        coord.register("lms-cli", true).await;
        coord.set_running("lms", true).await;
        coord.set_running("lms-cli", true).await;

        let polling_stopped = Arc::new(AtomicBool::new(false));
        let cli_stopped = Arc::new(AtomicBool::new(false));
        let polling = Arc::new(RecordingStartable {
            name: "lms",
            stopped: polling_stopped.clone(),
        });
        let cli = Arc::new(RecordingStartable {
            name: "lms-cli",
            stopped: cli_stopped.clone(),
        });
        coord.register_companion("lms", cli.clone()).await;

        coord
            .stop_adapter_and_companions_then_flush(
                polling.as_ref(),
                "lms",
                "test LMS reconfiguration",
            )
            .await;

        assert!(polling_stopped.load(Ordering::SeqCst));
        assert!(cli_stopped.load(Ordering::SeqCst));
        assert!(!coord.is_running("lms").await);
        assert!(!coord.is_running("lms-cli").await);
        assert!(matches!(
            events.recv().await,
            Ok(BusEvent::AdapterStopping { adapter, .. }) if adapter == "lms"
        ));
    }

    #[tokio::test]
    async fn test_start_adapter() {
        let bus = create_bus();
        let coord = AdapterCoordinator::new(bus.clone());

        coord.register("test", true).await;

        let started = Arc::new(AtomicBool::new(false));
        let started_clone = started.clone();

        coord
            .start_adapter("test", move |_bus, cancel| {
                let started = started_clone.clone();
                async move {
                    started.store(true, Ordering::SeqCst);
                    cancel.cancelled().await;
                }
            })
            .await
            .unwrap();

        // Give task time to start
        tokio::time::sleep(Duration::from_millis(10)).await;

        assert!(started.load(Ordering::SeqCst));
        assert!(coord.is_running("test").await);
    }

    #[tokio::test]
    async fn test_disabled_adapter_not_started() {
        let bus = create_bus();
        let coord = AdapterCoordinator::new(bus.clone());

        coord.register("disabled", false).await;

        let started = Arc::new(AtomicBool::new(false));
        let started_clone = started.clone();

        coord
            .start_adapter("disabled", move |_bus, _cancel| {
                let started = started_clone.clone();
                async move {
                    started.store(true, Ordering::SeqCst);
                }
            })
            .await
            .unwrap();

        tokio::time::sleep(Duration::from_millis(10)).await;

        assert!(!started.load(Ordering::SeqCst));
        assert!(!coord.is_running("disabled").await);
    }

    #[tokio::test]
    async fn test_shutdown_sends_event() {
        let bus = create_bus();
        let coord =
            AdapterCoordinator::with_shutdown_timeout(bus.clone(), Duration::from_millis(100));

        let mut rx = bus.subscribe();

        coord.register("test", true).await;
        coord
            .start_adapter("test", |bus, cancel| async move {
                let mut rx = bus.subscribe();
                loop {
                    tokio::select! {
                        event = rx.recv() => {
                            if let Ok(BusEvent::ShuttingDown { .. }) = event {
                                bus.publish(BusEvent::AdapterStopped {
                                    adapter: "test".to_string(),
                                });
                                break;
                            }
                        }
                        _ = cancel.cancelled() => break,
                    }
                }
            })
            .await
            .unwrap();

        // Give adapter time to start
        tokio::time::sleep(Duration::from_millis(10)).await;

        coord.shutdown().await;

        // Check that ShuttingDown was published
        let mut saw_shutting_down = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, BusEvent::ShuttingDown { .. }) {
                saw_shutting_down = true;
                break;
            }
        }
        assert!(saw_shutting_down);
    }

    #[tokio::test]
    async fn test_adapter_status() {
        let bus = create_bus();
        let coord = AdapterCoordinator::new(bus);

        coord.register("a", true).await;
        coord.register("b", false).await;

        let status = coord.adapter_status().await;
        assert_eq!(status.len(), 2);
        assert!(status["a"].enabled);
        assert!(!status["a"].running);
        assert!(!status["b"].enabled);
    }
}
