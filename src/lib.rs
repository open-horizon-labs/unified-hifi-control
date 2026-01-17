//! Unified Hi-Fi Control - Rust Implementation
//!
//! A source-agnostic hi-fi control bridge for hardware surfaces and Home Assistant.
//!
//! This library provides:
//! - Roon audio system control
//! - HQPlayer upsampling engine control
//! - Logitech Media Server (LMS) control
//! - MQTT integration for Home Assistant
//! - Server-Sent Events for real-time updates
//! - Web UI for daily control (Pico CSS)

pub mod adapters;
pub mod aggregator;
pub mod api;
pub mod bus;
pub mod config;
pub mod coordinator;
pub mod firmware;
pub mod knobs;
pub mod mdns;
pub mod ui;
