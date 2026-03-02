//! Knobs hardware surface support
//!
//! S3 Knob is an ESP32-based physical controller with:
//! - 240x240 LCD display (RGB565 format)
//! - Rotary encoder for volume
//! - Button for play/pause
//! - Battery monitoring
//!
//! This module provides:
//! - Device store (registration, config, status tracking)
//! - Hardware API endpoints (/now_playing, /control, /config)
//! - RGB565 image conversion for LCD display
//! - Manifest-driven protocol for ambient UI surfaces
//! - LLM-driven manifest generation via cloud proxy

pub mod image;
pub mod layout_routes;
pub mod layout_store;
pub mod llm_manifest;
pub mod manifest;
pub mod manifest_routes;
pub mod routes;
pub mod store;
pub mod udp;

pub use layout_store::LayoutStore;
pub use manifest_routes::ManifestStore;
pub use routes::*;
pub use store::KnobStore;

/// Normalize a zone_id: bare IDs without a colon are assumed to be Roon zones.
///
/// Legacy clients send raw Roon zone IDs without a `roon:` prefix.
/// Multi-adapter zone IDs contain a colon (`lms:`, `openhome:`, etc.).
pub(crate) fn normalize_zone_id(zone_id: &str) -> String {
    if !zone_id.contains(':') {
        format!("roon:{}", zone_id)
    } else {
        zone_id.to_string()
    }
}
