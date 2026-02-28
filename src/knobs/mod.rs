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
pub mod llm_manifest;
pub mod manifest;
pub mod manifest_routes;
pub mod routes;
pub mod store;
pub mod udp;

pub use manifest_routes::ManifestStore;
pub use routes::*;
pub use store::KnobStore;
