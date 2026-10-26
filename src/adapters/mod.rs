//! Audio source adapters (Roon, HQPlayer, LMS, OpenHome, UPnP, Apple Music, Spotify)

pub mod apple_music;
pub mod denonavr;
pub mod devices;
// Playback-device families (#774, alpha).
pub mod anthem;
pub mod arcam;
pub mod bluos;
pub mod didl;
pub mod eiscp;
pub mod eversolo;
pub mod handle;
pub mod hegel;
pub mod homeassistant_devices;
pub mod hqplayer;
pub mod library_commands;
pub mod lms;
pub mod lms_discovery;
pub mod lyngdorf;
pub mod musicassistant;
pub mod musiccast;
pub mod openhome;
pub mod roon;
pub mod sonos;
pub mod spotify;
pub mod streammagic;
pub mod traits;
pub mod trinnov;
pub mod upnp;
pub mod wiim;

pub use handle::*;
pub use lms_discovery::{discover_lms_servers, DiscoveredLms};
pub use traits::*;

/// Every playback-device family adapter (#774, alpha). Add a family here; the adapter set and
/// the device service pick it up.
pub fn device_families(
    bus: crate::bus::SharedBus,
) -> Vec<std::sync::Arc<dyn devices::DeviceFamily>> {
    vec![
        std::sync::Arc::new(denonavr::DenonAvrAdapter::new(bus.clone())),
        std::sync::Arc::new(wiim::WiimAdapter::new(bus.clone())),
        std::sync::Arc::new(sonos::SonosAdapter::new(bus.clone())),
        std::sync::Arc::new(musiccast::MusicCastAdapter::new(bus.clone())),
        std::sync::Arc::new(bluos::BluosAdapter::new(bus.clone())),
        std::sync::Arc::new(streammagic::StreamMagicAdapter::new(bus.clone())),
        std::sync::Arc::new(trinnov::TrinnovAdapter::new(bus.clone())),
        std::sync::Arc::new(eiscp::EiscpAdapter::new(bus.clone())),
        std::sync::Arc::new(arcam::ArcamAdapter::new(bus.clone())),
        std::sync::Arc::new(hegel::HegelAdapter::new(bus.clone())),
        std::sync::Arc::new(lyngdorf::LyngdorfAdapter::new(bus.clone())),
        std::sync::Arc::new(anthem::AnthemAdapter::new(bus.clone())),
        std::sync::Arc::new(eversolo::EversoloAdapter::new(bus.clone())),
        std::sync::Arc::new(homeassistant_devices::HomeAssistantDevices::new(bus)),
    ]
}
