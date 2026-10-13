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

mod artwork_analysis;
pub mod artwork_cache;
pub mod image;
pub mod routes;
pub mod store;

pub use routes::*;
pub use store::KnobStore;
