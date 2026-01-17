use anyhow::Result;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::bus::SharedBus;

// =============================================================================
// Startable - Uniform adapter lifecycle trait
// =============================================================================

/// Trait for adapters that can be started/stopped uniformly.
/// This enables the coordinator to manage all adapters through a single codepath.
#[async_trait]
pub trait Startable: Send + Sync {
    /// Adapter name/prefix (e.g., "lms", "openhome")
    fn name(&self) -> &'static str;

    /// Start the adapter. No-op if already running or can't start.
    async fn start(&self) -> Result<()>;

    /// Stop the adapter gracefully.
    async fn stop(&self);

    /// Whether this adapter can be started (e.g., has required config).
    /// Default: true (most adapters can always start).
    async fn can_start(&self) -> bool {
        true
    }
}

/// Macro to implement Startable trait with minimal boilerplate.
///
/// Adapters must implement:
/// - `async fn start_internal(&self) -> Result<()>`
/// - `async fn stop_internal(&self)`
/// - Optionally: custom `can_start` method (pass as third arg)
///
/// Usage:
/// ```ignore
/// impl_startable!(OpenHomeAdapter, "openhome");
/// impl_startable!(LmsAdapter, "lms", is_configured);  // custom can_start
/// ```
#[macro_export]
macro_rules! impl_startable {
    // With custom can_start method
    ($adapter:ty, $name:literal, $can_start:ident) => {
        #[async_trait::async_trait]
        impl $crate::adapters::Startable for $adapter {
            fn name(&self) -> &'static str {
                $name
            }

            async fn start(&self) -> anyhow::Result<()> {
                self.start_internal().await
            }

            async fn stop(&self) {
                self.stop_internal().await
            }

            async fn can_start(&self) -> bool {
                self.$can_start().await
            }
        }
    };
    // Default can_start (always true)
    ($adapter:ty, $name:literal) => {
        #[async_trait::async_trait]
        impl $crate::adapters::Startable for $adapter {
            fn name(&self) -> &'static str {
                $name
            }

            async fn start(&self) -> anyhow::Result<()> {
                self.start_internal().await
            }

            async fn stop(&self) {
                self.stop_internal().await
            }
        }
    };
}

/// Context passed to adapter logic during execution
pub struct AdapterContext {
    /// Event bus for publishing events
    pub bus: SharedBus,
    /// Cancellation token for shutdown coordination
    pub shutdown: CancellationToken,
}

/// Command that can be sent to an adapter
#[derive(Debug, Clone)]
pub enum AdapterCommand {
    Play,
    Pause,
    PlayPause,
    Stop,
    Next,
    Previous,
    VolumeAbsolute(i32),
    VolumeRelative(i32),
    Mute(bool),
}

/// Response from command execution
#[derive(Debug, Clone)]
pub struct AdapterCommandResponse {
    pub success: bool,
    pub error: Option<String>,
}

/// Adapter-specific logic trait
///
/// Implementors provide discovery and protocol handling.
/// Lifecycle (startup, shutdown, ACK) is handled by AdapterHandle.
#[async_trait]
pub trait AdapterLogic: Send + Sync + 'static {
    /// Unique prefix for zone IDs (e.g., "lms", "roon", "openhome")
    fn prefix(&self) -> &'static str;

    /// Run the adapter's main loop (discovery, polling, etc.)
    /// Should publish ZoneDiscovered/Updated/Removed events to ctx.bus
    /// Returns when ctx.shutdown is triggered or on error
    async fn run(&self, ctx: AdapterContext) -> Result<()>;

    /// Handle a command for a zone owned by this adapter
    /// Called by AdapterHandle when a matching command arrives
    async fn handle_command(
        &self,
        zone_id: &str,
        command: AdapterCommand,
    ) -> Result<AdapterCommandResponse>;

    /// Optional: called before run() for one-time setup
    async fn init(&self) -> Result<()> {
        Ok(())
    }
}
