//! Shared UI components for the Dioxus fullstack web UI.

pub mod bootstrap_prompt;
pub mod error_alert;
pub mod form_inputs;
pub mod hqp_controls;
pub mod layout;
pub mod nav;
pub mod volume;
pub mod zones_strip;

pub use bootstrap_prompt::BootstrapPrompt;
pub use error_alert::ErrorAlert;
pub use form_inputs::{PowerModeInput, ToggleInput};
pub use hqp_controls::{HqpControlsCompact, HqpMatrixSelect, HqpProfileSelect};
pub use layout::Layout;
pub use nav::Nav;
pub use volume::{VolumeControlsCompact, VolumeControlsFull};
pub use zones_strip::ZonesStrip;
