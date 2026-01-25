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
pub const AVAILABLE_ADAPTERS: &[&str] = &["roon", "lms", "lms-cli", "openhome", "upnp"];

/// Registered adapter with its spawn function
struct RegisteredAdapter {
    /// Adapter prefix (e.g., "lms", "roon")
    #[allow(dead_code)]
    prefix: String,
    /// Whether this adapter is currently enabled
    enabled: bool,
    /// Running task handle (if started)
    handle: Option<JoinHandle<()>>,
    /// Cancellation token for this adapter
    cancel: CancellationToken,
}

/// AdapterCoordinator manages adapter lifecycle:
/// - Register adapters by prefix
/// - Start only enabled adapters
/// - Coordinate graceful shutdown
pub struct AdapterCoordinator {
    adapters: RwLock<HashMap<String, RegisteredAdapter>>,
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
            bus,
            shutdown: CancellationToken::new(),
            shutdown_timeout: Duration::from_secs(5),
        }
    }

    /// Create with custom shutdown timeout
    pub fn with_shutdown_timeout(bus: SharedBus, timeout: Duration) -> Self {
        Self {
            adapters: RwLock::new(HashMap::new()),
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
        for adapter in adapters {
            let name = adapter.name();
            if !self.is_enabled(name).await {
                debug!("Adapter {} is disabled, skipping", name);
                continue;
            }
            if !adapter.can_start().await {
                debug!("Adapter {} cannot start (not configured?), skipping", name);
                continue;
            }
            match adapter.start().await {
                Ok(()) => info!("Started adapter: {}", name),
                Err(e) => warn!("Failed to start adapter {}: {}", name, e),
            }
        }
    }

    /// Stop all adapters from the provided list.
    pub async fn stop_all(&self, adapters: &[Arc<dyn Startable>]) {
        for adapter in adapters {
            adapter.stop().await;
            debug!("Stopped adapter: {}", adapter.name());
        }
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
            .map(|a| a.handle.is_some())
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
                        running: adapter.handle.is_some(),
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

    #[tokio::test]
    async fn test_register_and_check_enabled() {
        let bus = create_bus();
        let coord = AdapterCoordinator::new(bus);

        coord.register("test", true).await;
        assert!(coord.is_enabled("test").await);

        coord.register("disabled", false).await;
        assert!(!coord.is_enabled("disabled").await);
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
