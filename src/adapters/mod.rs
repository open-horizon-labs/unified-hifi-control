//! Audio source adapters (Roon, HQPlayer, LMS, OpenHome, UPnP, Apple Music, Spotify)

pub mod apple_music;
pub mod didl;
pub mod handle;
pub mod hqplayer;
pub mod lms;
pub mod lms_discovery;
pub mod musicassistant;
pub mod openhome;
pub mod roon;
pub mod spotify;
pub mod traits;
pub mod upnp;

pub use handle::*;
pub use lms_discovery::{discover_lms_servers, DiscoveredLms};
pub use traits::*;
