//! Mock servers for adapter integration testing
//!
//! These mock servers simulate real backend services (Roon, LMS, HQPlayer, UPnP, OpenHome)
//! allowing full integration testing without real hardware.

//!
//! See `README.md` in this directory for what each mock does and does not cover.

pub mod hqplayer;
pub mod lms;
pub mod openhome;
pub mod roon;
pub mod roon_core;
pub mod upnp;

pub use hqplayer::MockHqpServer;
pub use lms::MockLmsServer;
pub use openhome::MockOpenHomeDevice;
pub use roon::MockRoonCore;
pub use roon_core::FakeRoonCore;
pub use upnp::MockUpnpRenderer;
