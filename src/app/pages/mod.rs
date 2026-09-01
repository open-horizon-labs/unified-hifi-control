//! Dioxus fullstack page components.
//!
//! These pages use Dioxus signals and server functions instead of inline JavaScript.

mod hqplayer;
mod knobs;
mod lms;
mod settings;
mod zones;

pub use hqplayer::HqPlayer;
pub use knobs::Knobs;
pub use lms::Lms;
pub use settings::Settings;
pub use zones::Zones;
