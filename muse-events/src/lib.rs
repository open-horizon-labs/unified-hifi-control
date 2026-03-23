//! Shared wire protocol types for the Muse ecosystem.
//!
//! This crate defines the types that cross boundaries between:
//! - UHC (unified-hifi-control) - the event producer
//! - Memex - SSE event consumer
//! - muse-ingest - batch event consumer
//!
//! # Modules
//! - [`zone`] - Zone and playback state types
//! - [`events`] - SSE wire protocol events (MuseEvent)
//! - [`ingest`] - Ingest wire protocol types (IngestEvent, IngestRequest)

pub mod events;
pub mod ingest;
pub mod zone;

// Re-export commonly used types at crate root
pub use events::MuseEvent;
pub use ingest::{IngestEvent, IngestRequest};
pub use zone::{
    NowPlaying, PlaybackState, TrackMetadata, VolumeControl, VolumeScale, Zone, ZoneState,
};
