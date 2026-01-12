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

pub mod store;
pub mod routes;
pub mod image;

pub use store::KnobStore;
pub use routes::*;
