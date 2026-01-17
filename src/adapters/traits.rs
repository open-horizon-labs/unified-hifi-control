use anyhow::Result;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::bus::SharedBus;

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
