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
//! - Web UI (Dioxus + Tailwind CSS + DioxusLabs components)

// =============================================================================
// Lints - Enforce code quality and consistency
// =============================================================================

// Deny truly dangerous patterns (these will fail the build)
#![deny(unsafe_code)]
#![deny(unused_must_use)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![deny(clippy::todo)]
#![deny(clippy::unimplemented)]

// Note: clippy::pedantic, clippy::nursery, and clippy::cargo are NOT enabled
// because they have hundreds of existing violations. Enable incrementally.

// Adaptive-control producer contract (shared: server, WASM consumers, MCP).
// Deliberately not feature-gated - see src/adaptive/mod.rs for the dependency rule.
pub mod adaptive;
#[cfg(feature = "server")]
pub mod tls;

pub mod brand;

// Dioxus UI app (shared between server SSR and WASM client)
pub mod app;

// Dioxus components (official dx components)
pub mod components;

// Device identity catalog is shared by the server and controller management UI.
#[path = "firmware/catalog.rs"]
pub mod firmware_catalog;

// Server-only modules (excluded from WASM build)
#[cfg(feature = "server")]
pub mod adapters;
#[cfg(feature = "server")]
pub mod aggregator;
#[cfg(feature = "server")]
pub mod api;
#[cfg(feature = "server")]
pub mod bus;
#[cfg(feature = "server")]
pub mod cloud_connector;
#[cfg(feature = "server")]
pub mod config;
#[cfg(feature = "server")]
pub mod coordinator;
// Playback devices (amplifiers/receivers) linked to zones (#774). Alpha.
#[cfg(feature = "server")]
pub mod devices;
#[cfg(feature = "server")]
pub mod embedded;
#[cfg(feature = "server")]
pub mod firmware;
#[cfg(feature = "server")]
pub mod knobs;
#[cfg(feature = "server")]
pub mod mcp;
#[cfg(feature = "server")]
pub mod mdns;
#[cfg(feature = "server")]
pub mod shortcuts;
// Optional MQTT publisher: exposes UHC zones to Home Assistant (#508).
#[cfg(feature = "server")]
pub mod mqtt;
#[cfg(feature = "server")]
pub mod music_context;
// Adaptive producer publication: internal bus + aggregator-owned state (#324).
// Server-only: it necessarily touches both `crate::bus` and `crate::adaptive`.
#[cfg(feature = "server")]
pub mod producers;
// The single decision point for zone-list membership and order (see module docs).
#[cfg(feature = "server")]
pub mod zone_list;

// Capability ownership for commands to composed zones.
#[cfg(feature = "server")]
pub mod zone_control;

#[cfg(feature = "server")]
pub mod command_dispatch;
#[cfg(feature = "server")]
pub(crate) mod provider_control;
#[cfg(feature = "server")]
pub mod volume;
