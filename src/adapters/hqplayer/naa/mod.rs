//! HQPlayer output routing through a UHC-owned NAA relay.
//!
//! * [`outputs`] — public wire types shared by HTTP, MCP and the Dioxus page.
//! * [`relay`] — the owned relay (routes, selection, session, DAC observations, listener
//!   lifecycle). No listener of its own beyond the NAA data socket; no HTTP control surface.
//! * [`protocol`] — the byte-transparent session worker ported from the private PoC.
//!
//! `protocol` and `relay` exist only with the private opt-in `naa-proxy` Cargo feature; the wire
//! types always exist so surfaces can report the feature honestly when it is not compiled in.
//!
//! Native HQPlayer transport coordination (Stop/Play/Seek around a route change) is deliberately
//! not here: it runs on `HqpAdapter`'s exact-instance operation lease in `hqplayer.rs`, so profile
//! and pipeline operations share that lease and Stop can cancel pending output work.

#[cfg(feature = "naa-proxy")]
pub mod coordinator;
#[cfg(feature = "naa-proxy")]
pub mod discovery;
#[cfg(feature = "naa-proxy")]
pub mod frame;
pub mod outputs;
#[cfg(feature = "naa-proxy")]
pub mod protocol;
#[cfg(feature = "naa-proxy")]
pub mod relay;
#[cfg(feature = "naa-proxy")]
pub mod setup;
