//! Page components for the Dioxus-based web UI.
//!
//! Each page is a Dioxus component that renders a full page using the Layout component.

pub mod dashboard;
pub mod hqplayer;
pub mod knobs;
pub mod lms;
pub mod settings;
pub mod zone;
pub mod zones;

pub use dashboard::DashboardPage;
pub use hqplayer::HqplayerPage;
pub use knobs::KnobsPage;
pub use lms::LmsPage;
pub use settings::SettingsPage;
pub use zone::ZonePage;
pub use zones::ZonesPage;
