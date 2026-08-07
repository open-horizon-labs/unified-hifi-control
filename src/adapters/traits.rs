use anyhow::Result;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::bus::{RepeatMode, SharedBus};

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
    /// Set the provider's repeat mode for the selected zone.
    SetRepeat(RepeatMode),
    /// Enable or disable shuffle for the selected zone.
    SetShuffle(bool),
}

/// Response from command execution
#[derive(Debug, Clone)]
pub struct AdapterCommandResponse {
    pub success: bool,
    pub error: Option<String>,
}

/// A provider library hit that can be addressed by a stable provider URI.
///
/// The MCP layer deliberately keeps provider-specific fields out of its wire
/// result. Adapters translate their native result into this small contract and
/// retain the URI for play-by-reference.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LibrarySearchResult {
    pub title: String,
    pub subtitle: Option<String>,
    pub uri: String,
}

/// Optional content-library surface implemented by adapters that can search
/// and play a provider URI. Transport-only adapters do not implement this
/// trait; the adapter registry routes only providers registered here.
#[async_trait]
pub trait LibraryAdapter: Send + Sync + 'static {
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<LibrarySearchResult>>;

    async fn play_uri(&self, zone_id: &str, uri: &str) -> Result<String>;

    /// Add one URI to a provider's playback queue when the provider exposes a
    /// safe queue-add operation. Transport-only libraries keep the default
    /// refusal rather than pretending queue support exists.
    async fn queue_uri(&self, _zone_id: &str, _uri: &str) -> Result<()> {
        Err(anyhow::anyhow!(
            "queue add is not implemented for this provider"
        ))
    }

    /// Read a provider queue as JSON for provider-neutral MCP projection.
    async fn read_queue(&self, _zone_id: &str) -> Result<serde_json::Value> {
        Err(anyhow::anyhow!(
            "queue read is not implemented for this provider"
        ))
    }

    /// Provider-specific catalog/library operation used by the provider-aware
    /// MCP surface. Transport-only adapters keep the honest default refusal.
    async fn content(
        &self,
        _operation: &str,
        _params: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        Err(anyhow::anyhow!(
            "content operation is not implemented for this provider"
        ))
    }
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
