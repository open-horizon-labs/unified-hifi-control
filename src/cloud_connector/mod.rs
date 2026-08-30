//! Zero-trust, outbound-only connector primitives for HiPhi Cloud.
//!
//! The connector is opt-in and outbound-only. It speaks semantic messages:
//! provider identifiers and LAN URLs stay behind the aggregator boundary.

pub mod artwork;
pub mod commands;
pub mod config;
pub mod identity;
pub mod pairing;
pub mod protocol;
#[cfg(not(test))]
pub mod runtime;
pub mod session;
pub mod state;
pub mod transport;

pub use artwork::{ArtLane, ArtRequest, ArtResponse};
pub use commands::{CommandGrantVerifier, CommandLedger, CommandOutcome, VerifiedCommand};
pub use config::{CloudConnectorConfig, ConfigError};
pub use identity::{InstallationIdentity, ZoneHandleMap};
pub use protocol::{
    parse_relay_message, AllowedAction, ArtworkChunk, ArtworkRelayRequest, ArtworkRelayResponse,
    CommandEnvelope, CommandPayload, ConnectorMessage, RelayMessage, StateSnapshot, WireEnvelope,
    WireType,
};
pub use session::{
    verify_installation_session_grant, InstallationGrantRequest, InstallationSessionProof,
    SessionError, SessionGrantError, SessionProof, SessionVerifier,
    VerifiedInstallationSessionGrant,
};
pub use state::{SemanticStateInput, StateError, StateProjection, StateStore};
pub use transport::{
    Backoff, ConnectionState, ConnectorRunLoop, EndpointError, EpochGuardError, RelayEndpoint,
    RunLoopEvent, SessionEpochGuard,
};
