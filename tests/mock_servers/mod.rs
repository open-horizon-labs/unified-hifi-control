#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Mock servers for adapter integration testing
//!
//! These mock servers simulate real backend services (Roon, LMS, HQPlayer, UPnP, OpenHome)
//! allowing full integration testing without real hardware.

pub mod hqplayer;
pub mod lms;
pub mod openhome;
pub mod roon;
pub mod upnp;

pub use hqplayer::MockHqpServer;
pub use lms::MockLmsServer;
pub use openhome::MockOpenHomeDevice;
pub use roon::MockRoonCore;
pub use upnp::MockUpnpRenderer;
