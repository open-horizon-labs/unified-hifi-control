//! Zone-prefix routing for MCP tools.
//!
//! Every MCP tool that talks to a backend decides which adapter to use from the
//! zone id's prefix. Before issue #394 that decision was open-coded in four
//! places with three *different* rules; this module is the single place it lives.
//!
//! # This preserves today's behavior exactly, including its defects
//!
//! Two things here are known to be wrong, and #394 deliberately does not fix
//! either — correcting them is #398's job:
//!
//! 1. An unprefixed zone id is treated as Roon.
//! 2. An *unrecognised* prefix (`sonos:foo`) is also treated as Roon by
//!    transport, search and play — silently, with no capability check.
//!
//! [`ZoneTarget::Unknown`] exists so #398 has something to change: today every
//! call site folds `Unknown` into Roon, and the fix is to stop doing that. Do
//! not collapse `Unknown` into [`ZoneTarget::Roon`] — that would erase the seam.
//!
//! # The three rules are not interchangeable
//!
//! Transport and volume disagree, and the disagreement is observable:
//!
//! | zone id        | transport ([`Self::for_transport`]) | volume ([`Self::for_volume`]) |
//! |----------------|-------------------------------------|-------------------------------|
//! | `lms:x`        | LMS                                 | LMS                           |
//! | `roon:x`       | Roon                                | Roon                          |
//! | `x` (bare)     | Roon                                | Roon                          |
//! | `openhome:x`   | OpenHome                            | **refused**                   |
//! | `upnp:x`       | UPnP                                | **refused**                   |
//! | `sonos:x`      | **Roon**                            | **refused**                   |
//!
//! A single `is_lms()`-style predicate cannot express that. Any attempt to
//! unify these into one rule changes behavior; `tests/mcp_contract.rs`
//! (`volume_routing_differs_from_transport_routing`) fails if you try.

/// Which adapter a zone id refers to, by prefix alone.
///
/// Classification is purely syntactic: it says nothing about whether the zone
/// exists, is reachable, or supports the operation being attempted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneTarget {
    /// `lms:` prefix.
    Lms,
    /// `openhome:` prefix.
    OpenHome,
    /// `upnp:` prefix.
    Upnp,
    /// Explicit `roon:` prefix, or a bare id with no prefix at all.
    ///
    /// The bare-id case is today's silent default and belongs to #398.
    Roon,
    /// A prefix that matches no adapter, e.g. `sonos:x`.
    ///
    /// Kept distinct from [`Self::Roon`] on purpose: every call site currently
    /// routes it to Roon anyway, and #398's fix is to stop. Merging the two
    /// variants would remove the only place that distinction is visible.
    Unknown,
}

impl ZoneTarget {
    /// Classify a zone id by prefix.
    pub fn classify(zone_id: &str) -> Self {
        if zone_id.starts_with("lms:") {
            Self::Lms
        } else if zone_id.starts_with("openhome:") {
            Self::OpenHome
        } else if zone_id.starts_with("upnp:") {
            Self::Upnp
        } else if zone_id.starts_with("roon:") || !zone_id.contains(':') {
            // Bare ids land here. This is #398's defect, frozen by #394.
            Self::Roon
        } else {
            Self::Unknown
        }
    }

    /// Classify an optional zone id, as `hifi_search` supplies.
    ///
    /// `None` means "no zone context", which today routes to Roon exactly as a
    /// bare id does.
    pub fn classify_opt(zone_id: Option<&str>) -> Self {
        zone_id.map_or(Self::Roon, Self::classify)
    }
}

/// Where a transport command (`play`/`pause`/`next`/...) is sent.
///
/// Four adapters, with Roon as the catch-all — including for unrecognised
/// prefixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportRoute {
    Lms,
    OpenHome,
    Upnp,
    Roon,
}

/// Where a volume command is sent, or that it is refused.
///
/// Narrower than [`TransportRoute`]: only LMS and Roon expose volume control,
/// and everything else — including unrecognised prefixes — is refused rather
/// than defaulted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeRoute {
    Lms,
    Roon,
    /// No volume control for this zone type.
    Unsupported,
}

/// Where a library operation (`hifi_search` / `hifi_play`) is sent.
///
/// Only LMS is distinguished; everything else goes to Roon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryRoute {
    Lms,
    Roon,
}

impl ZoneTarget {
    /// Transport routing. `Unknown` falls through to Roon — today's behavior.
    pub fn for_transport(self) -> TransportRoute {
        match self {
            Self::Lms => TransportRoute::Lms,
            Self::OpenHome => TransportRoute::OpenHome,
            Self::Upnp => TransportRoute::Upnp,
            // Unknown prefixes default to Roon. #398 changes this, #394 does not.
            Self::Roon | Self::Unknown => TransportRoute::Roon,
        }
    }

    /// Volume routing. Unlike transport, anything that is not LMS or Roon is
    /// refused rather than defaulted.
    pub fn for_volume(self) -> VolumeRoute {
        match self {
            Self::Lms => VolumeRoute::Lms,
            Self::Roon => VolumeRoute::Roon,
            Self::OpenHome | Self::Upnp | Self::Unknown => VolumeRoute::Unsupported,
        }
    }

    /// Library routing for `hifi_search` and `hifi_play`.
    pub fn for_library(self) -> LibraryRoute {
        match self {
            Self::Lms => LibraryRoute::Lms,
            // Everything else, Unknown included, goes to Roon today.
            Self::Roon | Self::OpenHome | Self::Upnp | Self::Unknown => LibraryRoute::Roon,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_prefixes() {
        assert_eq!(ZoneTarget::classify("lms:aa:bb:cc"), ZoneTarget::Lms);
        assert_eq!(ZoneTarget::classify("openhome:uuid"), ZoneTarget::OpenHome);
        assert_eq!(ZoneTarget::classify("upnp:uuid"), ZoneTarget::Upnp);
        assert_eq!(ZoneTarget::classify("roon:1601a5d4"), ZoneTarget::Roon);
    }

    /// Frozen defect, not a design choice: see the module docs and #398.
    #[test]
    fn bare_ids_classify_as_roon_today() {
        assert_eq!(ZoneTarget::classify("1601a5d4"), ZoneTarget::Roon);
        assert_eq!(ZoneTarget::classify(""), ZoneTarget::Roon);
        assert_eq!(ZoneTarget::classify_opt(None), ZoneTarget::Roon);
    }

    #[test]
    fn unrecognised_prefixes_are_distinguishable_from_roon() {
        assert_eq!(ZoneTarget::classify("sonos:abc"), ZoneTarget::Unknown);
        assert_eq!(ZoneTarget::classify("chromecast:abc"), ZoneTarget::Unknown);
        // The distinction is the seam #398 needs; do not collapse it.
        assert_ne!(
            ZoneTarget::classify("sonos:abc"),
            ZoneTarget::classify("roon:abc")
        );
    }

    /// Transport's catch-all is Roon, for bare ids AND unknown prefixes.
    #[test]
    fn transport_routes_everything_unrecognised_to_roon() {
        assert_eq!(
            ZoneTarget::classify("lms:x").for_transport(),
            TransportRoute::Lms
        );
        assert_eq!(
            ZoneTarget::classify("openhome:x").for_transport(),
            TransportRoute::OpenHome
        );
        assert_eq!(
            ZoneTarget::classify("upnp:x").for_transport(),
            TransportRoute::Upnp
        );
        assert_eq!(
            ZoneTarget::classify("roon:x").for_transport(),
            TransportRoute::Roon
        );
        assert_eq!(
            ZoneTarget::classify("bare").for_transport(),
            TransportRoute::Roon
        );
        assert_eq!(
            ZoneTarget::classify("sonos:x").for_transport(),
            TransportRoute::Roon
        );
    }

    /// Volume refuses where transport defaults. This asymmetry is the reason
    /// routing cannot be one predicate.
    #[test]
    fn volume_refuses_where_transport_defaults_to_roon() {
        assert_eq!(ZoneTarget::classify("lms:x").for_volume(), VolumeRoute::Lms);
        assert_eq!(ZoneTarget::classify("roon:x").for_volume(), VolumeRoute::Roon);
        assert_eq!(ZoneTarget::classify("bare").for_volume(), VolumeRoute::Roon);

        for zone_id in ["openhome:x", "upnp:x", "sonos:x"] {
            assert_eq!(
                ZoneTarget::classify(zone_id).for_volume(),
                VolumeRoute::Unsupported,
                "{zone_id} must be refused for volume"
            );
        }

        // For the two prefixed adapters the asymmetry is "own adapter for
        // transport, refused for volume".
        for zone_id in ["openhome:x", "upnp:x"] {
            assert_ne!(
                ZoneTarget::classify(zone_id).for_transport(),
                TransportRoute::Roon,
                "{zone_id} must reach its own adapter for transport"
            );
        }
    }

    /// `sonos:x` is the case that proves the two rules differ: refused for
    /// volume, routed to Roon for transport.
    #[test]
    fn unknown_prefix_is_refused_for_volume_but_routed_to_roon_for_transport() {
        let target = ZoneTarget::classify("sonos:x");
        assert_eq!(target.for_volume(), VolumeRoute::Unsupported);
        assert_eq!(target.for_transport(), TransportRoute::Roon);
    }

    #[test]
    fn library_routes_only_lms_away_from_roon() {
        assert_eq!(
            ZoneTarget::classify("lms:x").for_library(),
            LibraryRoute::Lms
        );
        for zone_id in ["roon:x", "bare", "openhome:x", "upnp:x", "sonos:x"] {
            assert_eq!(
                ZoneTarget::classify(zone_id).for_library(),
                LibraryRoute::Roon,
                "{zone_id} must route to Roon for search/play"
            );
        }
        assert_eq!(
            ZoneTarget::classify_opt(None).for_library(),
            LibraryRoute::Roon
        );
    }
}
