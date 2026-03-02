//! Dioxus fullstack page components.
//!
//! These pages use Dioxus signals and server functions instead of inline JavaScript.

mod configurator;
mod dials;
mod hqplayer;
mod lms;
mod settings;
mod zones;

pub use configurator::Configurator;
pub use dials::Dials;
pub use hqplayer::HqPlayer;
pub use lms::Lms;
pub use settings::Settings;
pub use zones::Zones;
