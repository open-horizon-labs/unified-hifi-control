//! Mock servers for adapter integration testing
//!
//! These mock servers simulate real backend services (Roon, LMS, HQPlayer, UPnP, OpenHome)
//! allowing full integration testing without real hardware.

pub mod lms;
pub mod hqplayer;
pub mod upnp;
pub mod openhome;
pub mod roon;

pub use lms::MockLmsServer;
pub use hqplayer::MockHqpServer;
pub use upnp::MockUpnpRenderer;
pub use openhome::MockOpenHomeDevice;
pub use roon::MockRoonCore;
