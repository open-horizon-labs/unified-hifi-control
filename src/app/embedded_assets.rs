//! Embedded static assets for single-binary distribution.
//!
//! All CSS and images are compiled into the binary using include_str!/include_bytes!.
//! Images are served as base64 data URLs to avoid external file dependencies.

use base64::{engine::general_purpose::STANDARD, Engine};
use std::sync::LazyLock;

// ============================================================================
// CSS Assets (embedded as strings)
// ============================================================================

/// DioxusLabs components theme (CSS variables for dark/light mode)
pub const DX_THEME_CSS: &str = include_str!("../../public/dx-components-theme.css");

/// Tailwind CSS utilities
pub const TAILWIND_CSS: &str = include_str!("../../public/tailwind.css");

// ============================================================================
// Image Assets (embedded as base64 data URLs)
// ============================================================================

/// Favicon bytes
const FAVICON_BYTES: &[u8] = include_bytes!("../../public/favicon.ico");

/// Apple touch icon bytes
const APPLE_TOUCH_ICON_BYTES: &[u8] = include_bytes!("../../public/apple-touch-icon.png");

/// Logo image bytes
const LOGO_BYTES: &[u8] = include_bytes!("../../public/hifi-logo.png");

/// Favicon as data URL (lazily encoded)
pub static FAVICON_DATA_URL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "data:image/x-icon;base64,{}",
        STANDARD.encode(FAVICON_BYTES)
    )
});

/// Apple touch icon as data URL (lazily encoded)
pub static APPLE_TOUCH_ICON_DATA_URL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "data:image/png;base64,{}",
        STANDARD.encode(APPLE_TOUCH_ICON_BYTES)
    )
});

/// Logo as data URL (lazily encoded)
pub static LOGO_DATA_URL: LazyLock<String> =
    LazyLock::new(|| format!("data:image/png;base64,{}", STANDARD.encode(LOGO_BYTES)));
