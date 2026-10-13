//! Canonical public product identity.
//!
//! User-facing copy takes the product and company names from here. Stable
//! machine identifiers (crate/package name, MCP server key, service names,
//! config paths, env vars, `/knob/*` routes) are deliberately not derived from
//! these and must not change; `tests/brand_contract.rs` pins that.

pub const PRODUCT_NAME: &str = "Unified Hi-Fi Control";
pub const COMPANY_NAME: &str = "Open Horizon Labs";
pub const PRODUCT_BYLINE: &str = "Unified Hi-Fi Control by Open Horizon Labs";
