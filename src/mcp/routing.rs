//! Zone-prefix routing for MCP tools.
//!
//! Every MCP tool that talks to a backend decides which adapter to use from the
//! zone id's prefix. Before issue #394 that decision was open-coded in four
//! places with three *different* rules; this module is the single place it lives.
//!
//! # #398 closed the silent Roon default
//!
//! Until #398, two things here were wrong and #394 deliberately froze them:
//!
//! 1. An unprefixed zone id was treated as Roon.
//! 2. An *unrecognised* prefix (`sonos:foo`) was also treated as Roon by
//!    transport, search and play — silently, with no capability check.
//!
//! Both are gone. [`ZoneTarget::Unprefixed`] and [`ZoneTarget::Unknown`] now
//! route to [`TransportRoute::Refused`] / [`VolumeRoute::Refused`] /
//! [`LibraryRoute::Refused`], and each tool turns that into a refusal naming the
//! accepted prefixes. The two variants stay separate because they need *different
//! sentences*: "you left the prefix off" and "that prefix names no adapter" send a
//! client to different fixes.
//!
//! **The one default that remains is deliberate and documented:** an absent
//! `zone_id` on `hifi_search` ([`ZoneTarget::classify_opt`] with `None`) still
//! means Roon. `None` is not a malformed zone id — there is nothing to route by,
//! LMS `globalsearch` requires a player id, and `hifi_search`'s own description
//! says Roon is the default. It is reported as `scope.provider: "roon"`, so it is
//! visible rather than silent.
//!
//! # There are FIVE zone prefixes, not four
//!
//! `PrefixedZoneId` (`src/bus/events.rs`) lists `roon:`, `lms:`, `openhome:`,
//! `upnp:` **and `hqplayer:`**, and `HqpAdapter` publishes `ZoneDiscovered` with
//! the last one — so HQPlayer zones appear in `hifi_zones`. Before #398,
//! `classify` had no arm for them, so `Unknown` sent every HQPlayer zone to Roon:
//! a zone type UHC advertises, controlled through the wrong adapter. The adapter
//! it should reach has the whole surface (`play`, `pause`, `next`, `previous`,
//! `set_volume`, `volume_up`/`down`, `seek`).
//!
//! #398 recognises the prefix and reports the gap honestly ([`HqPlayer`] routes
//! to `Refused`, and the tools classify it as `not_implemented` tracked by #328).
//! It does **not** wire it: the zone id is `hqplayer:<instance_name>`, so routing
//! needs `HqpInstanceManager` resolution and there is no HQPlayer zone in any test
//! fixture.
//!
//! [`HqPlayer`]: ZoneTarget::HqPlayer
//!
//! # The three rules are not interchangeable
//!
//! Transport, volume and library still disagree, and the disagreement is
//! observable:
//!
//! | zone id        | transport ([`Self::for_transport`]) | volume ([`Self::for_volume`]) | library ([`Self::for_library`]) |
//! |----------------|-------------------------------------|-------------------------------|---------------------------------|
//! | `roon:x`       | Roon                                | Roon                          | Roon                            |
//! | `lms:x`        | LMS                                 | LMS                           | LMS                             |
//! | `openhome:x`   | OpenHome                            | OpenHome (#398 wired it)      | **refused**                     |
//! | `upnp:x`       | UPnP                                | UPnP (#398 wired it)          | **refused**                     |
//! | `hqplayer:x`   | **refused**                         | **refused**                   | **refused**                     |
//! | `x` (bare)     | **refused** (#398)                  | **refused** (#398)            | **refused** (#398)              |
//! | `sonos:x`      | **refused** (#398)                  | **refused**                   | **refused** (#398)              |
//!
//! A single `is_lms()`-style predicate cannot express that. Any attempt to unify
//! these into one rule changes behavior; `tests/mcp_contract.rs`
//! (`volume_routing_differs_from_transport_routing`) fails if you try.
//!
//! # Where capability truth lives
//!
//! [`crate::mcp::capabilities`] derives every `supported` value from the three
//! functions below, so a capability cannot be advertised without a route to back
//! it. The reverse does not hold — a route existing does not make an adapter call
//! succeed — which is why the contract tests prove each `supported` cell by
//! calling it and checking the adapter's own error wording.

use crate::mcp::envelope::Provider;

/// Which adapter a zone id refers to, by prefix alone.
///
/// Classification is purely syntactic: it says nothing about whether the zone
/// exists, is reachable, or supports the operation being attempted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneTarget {
    /// `roon:` prefix.
    Roon,
    /// `lms:` prefix.
    Lms,
    /// `openhome:` prefix.
    OpenHome,
    /// `upnp:` prefix.
    Upnp,
    /// `hqplayer:` prefix — a real zone type `hifi_zones` lists. See the module
    /// docs: recognised by #398, not yet wired to a control path.
    HqPlayer,
    /// `applemusic:` prefix — native MusicKit companion session.
    AppleMusic,
    /// `spotify:` prefix — Spotify Connect controller.
    Spotify,
    /// `musicassistant:` prefix — optional MA peer adapter.
    MusicAssistant,
    /// No `:` at all. Was Roon until #398; now refused with the prefix list.
    Unprefixed,
    /// A prefix that matches no adapter, e.g. `sonos:x`.
    ///
    /// Kept distinct from [`Self::Unprefixed`] because the remedy differs: one
    /// client forgot the prefix, the other invented one.
    Unknown,
}

impl ZoneTarget {
    /// Every provider a zone id can actually name, in the order the capability
    /// report and the AGENTS.md matrix present them.
    ///
    /// The two non-provider variants are absent by construction: asking for the
    /// capabilities of "unknown" is not a question with an answer.
    pub const PROVIDERS: &'static [Self] = &[
        Self::Roon,
        Self::Lms,
        Self::OpenHome,
        Self::Upnp,
        Self::HqPlayer,
        Self::AppleMusic,
        Self::Spotify,
        Self::MusicAssistant,
    ];

    /// Classify a zone id by prefix.
    pub fn classify(zone_id: &str) -> Self {
        if zone_id.starts_with("roon:") {
            Self::Roon
        } else if zone_id.starts_with("lms:") {
            Self::Lms
        } else if zone_id.starts_with("openhome:") {
            Self::OpenHome
        } else if zone_id.starts_with("upnp:") {
            Self::Upnp
        } else if zone_id.starts_with("hqplayer:") {
            Self::HqPlayer
        } else if zone_id.starts_with("applemusic:") {
            Self::AppleMusic
        } else if zone_id.starts_with("spotify:") {
            Self::Spotify
        } else if zone_id.starts_with("musicassistant:") {
            Self::MusicAssistant
        } else if zone_id.contains(':') {
            Self::Unknown
        } else {
            Self::Unprefixed
        }
    }

    /// Classify an optional zone id, as `hifi_search` supplies.
    ///
    /// `None` means "no zone context", which routes to Roon — the one documented
    /// default #398 kept. See the module docs for why it is not the same thing as
    /// an unprefixed id.
    pub fn classify_opt(zone_id: Option<&str>) -> Self {
        zone_id.map_or(Self::Roon, Self::classify)
    }

    /// The prefix this target is spelled with, for refusals and diagnostics.
    ///
    /// `None` for the two variants that name no provider.
    pub fn prefix(self) -> Option<&'static str> {
        Some(match self {
            Self::Roon => "roon:",
            Self::Lms => "lms:",
            Self::OpenHome => "openhome:",
            Self::Upnp => "upnp:",
            Self::HqPlayer => "hqplayer:",
            Self::AppleMusic => "applemusic:",
            Self::Spotify => "spotify:",
            Self::MusicAssistant => "musicassistant:",
            Self::Unprefixed | Self::Unknown => return None,
        })
    }

    /// The lowercase provider label, used in refusal detail and HTTP route names.
    pub fn label(self) -> &'static str {
        match self {
            Self::Roon => "roon",
            Self::Lms => "lms",
            Self::OpenHome => "openhome",
            Self::Upnp => "upnp",
            Self::HqPlayer => "hqplayer",
            Self::AppleMusic => "applemusic",
            Self::Spotify => "spotify",
            Self::MusicAssistant => "musicassistant",
            Self::Unprefixed => "unprefixed",
            Self::Unknown => "unknown",
        }
    }
}

/// Where a transport command (`play`/`pause`/`next`/...) is sent, or that it is
/// refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportRoute {
    Roon,
    Lms,
    OpenHome,
    Upnp,
    HqPlayer,
    AppleMusic,
    Spotify,
    MusicAssistant,
    /// No transport path for this zone id. Carries the target so the caller can
    /// tell "not wired for this provider" from "not a zone id UHC understands".
    Refused(ZoneTarget),
}

/// Where a volume command is sent, or that it is refused.
///
/// #398 added the OpenHome and UPnP arms: both adapters implement `vol_abs` and
/// `vol_rel` and `POST /{openhome,upnp}/control` has exposed them over HTTP all
/// along — only this path declined to call them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeRoute {
    Roon,
    Lms,
    OpenHome,
    Upnp,
    HqPlayer,
    AppleMusic,
    Spotify,
    MusicAssistant,
    /// No volume path for this zone id.
    Refused(ZoneTarget),
}

/// Where a library operation (`hifi_search` / `hifi_play`) is sent.
///
/// Roon, LMS, and Spotify expose library/content surfaces; OpenHome and UPnP zones are
/// renderers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LibraryRoute {
    Roon,
    Lms,
    Spotify,
    AppleMusic,
    /// No library path for this zone id.
    Refused(ZoneTarget),
}

/// The zone-id prefixes that name an adapter, for refusals that have to say what
/// a valid zone id looks like.
///
/// Derived from the same list [`ZoneTarget::classify`] matches on (asserted by
/// [`tests::accepted_prefixes_are_exactly_what_classify_recognises`]), so a new
/// adapter cannot be added without this following.
///
/// # `hqplayer:` is here even though nothing is wired to it
///
/// This list answers "what does a zone id look like", not "what can I do with
/// one". `hifi_zones` returns `hqplayer:` ids, so omitting the prefix would tell a
/// client that an id UHC just handed it is invalid — a worse lie than the one
/// #398 is fixing. `hifi_capabilities` is the tool that says which operations a
/// recognised prefix actually supports.
///
/// # The bare unprefixed form is deliberately omitted
///
/// It is not a zone id UHC ever emits (`src/bus/events.rs` prefixes every one),
/// and since #398 it is refused. Every value listed here works as a prefix.
pub const ACCEPTED_ZONE_PREFIXES: &[&str] = &[
    "roon:",
    "lms:",
    "openhome:",
    "upnp:",
    "hqplayer:",
    "applemusic:",
    "spotify:",
    "musicassistant:",
];

/// Every `hifi_control` action, in the order the tool's description lists them.
///
/// **A closed set since #398.** `handle_control` used to end its match with
/// `other => other`, forwarding any string to an adapter — so a typo surfaced as
/// whatever that backend happened to say, or (offline) as a device-lookup failure
/// that never mentioned the action. Now an action absent from this list is
/// refused with the list itself.
///
/// One action per entry, not a comma-joined sentence: this doubles as an
/// envelope's `refusal.accepted`, and each entry has to be something a client can
/// put straight back into `action`.
pub const CONTROL_ACTIONS: &[&str] = &[
    "play",
    "pause",
    "playpause",
    "next",
    "previous",
    "prev",
    "volume_set",
    "volume_up",
    "volume_down",
    "repeat_off",
    "repeat_context",
    "repeat_track",
    "shuffle_on",
    "shuffle_off",
];

/// The transport subset of [`CONTROL_ACTIONS`], for "what can this zone do
/// instead?" on a volume refusal.
///
/// `prev` is omitted as a duplicate of `previous`: an `alternatives` list is
/// advice, and offering the same command twice under two spellings is noise.
pub const TRANSPORT_ACTIONS: &[&str] = &["play", "pause", "playpause", "next", "previous"];

impl ZoneTarget {
    /// Transport routing. Every id UHC cannot place is refused, not defaulted.
    pub fn for_transport(self) -> TransportRoute {
        match self {
            Self::Roon => TransportRoute::Roon,
            Self::Lms => TransportRoute::Lms,
            Self::OpenHome => TransportRoute::OpenHome,
            Self::Upnp => TransportRoute::Upnp,
            Self::AppleMusic => TransportRoute::AppleMusic,
            Self::Spotify => TransportRoute::Spotify,
            Self::MusicAssistant => TransportRoute::MusicAssistant,
            // Recognised, and genuinely not wired. #328.
            Self::HqPlayer => TransportRoute::HqPlayer,
            // #398: was Roon for both of these.
            Self::Unprefixed | Self::Unknown => TransportRoute::Refused(self),
        }
    }

    /// Volume routing.
    pub fn for_volume(self) -> VolumeRoute {
        match self {
            Self::Roon => VolumeRoute::Roon,
            Self::Lms => VolumeRoute::Lms,
            // #398 wired both: the adapters implement vol_abs/vol_rel.
            Self::OpenHome => VolumeRoute::OpenHome,
            Self::Upnp => VolumeRoute::Upnp,
            Self::AppleMusic => VolumeRoute::AppleMusic,
            Self::Spotify => VolumeRoute::Spotify,
            Self::MusicAssistant => VolumeRoute::MusicAssistant,
            Self::HqPlayer => VolumeRoute::HqPlayer,
            Self::Unprefixed | Self::Unknown => VolumeRoute::Refused(self),
        }
    }

    /// Library routing for `hifi_search` and `hifi_play`.
    pub fn for_library(self) -> LibraryRoute {
        match self {
            Self::Roon => LibraryRoute::Roon,
            Self::Lms => LibraryRoute::Lms,
            Self::Spotify => LibraryRoute::Spotify,
            Self::AppleMusic => LibraryRoute::AppleMusic,
            // OpenHome and UPnP zones are renderers with no library; before #398
            // both were sent to Roon, which searched a library the zone could not
            // play from.
            Self::OpenHome | Self::Upnp | Self::HqPlayer | Self::MusicAssistant => {
                LibraryRoute::Refused(self)
            }
            Self::Unprefixed | Self::Unknown => LibraryRoute::Refused(self),
        }
    }

    /// The provider for an envelope [`Scope`](crate::mcp::envelope::Scope).
    ///
    /// **The single source of `scope.provider`, for every tool and every path.**
    ///
    /// Deliberately derived from identification rather than from the route taken.
    /// An earlier draft asked the *route* — so `sonos:x` reported `roon`, because
    /// that is where transport sent it. The #395 execute-gate dissent blocked
    /// that: it turns a silent default into a positive claim that a Sonos zone is
    /// a Roon zone, in the one field a capability matrix is built on.
    ///
    /// Both non-provider variants report `unknown`. They are distinguished in the
    /// refusal's sentence, not here: `Provider` is the envelope's published
    /// vocabulary and "UHC identified no provider" is one fact, however the client
    /// arrived at it.
    pub fn provider(self) -> Provider {
        match self {
            Self::Roon => Provider::Roon,
            Self::Lms => Provider::Lms,
            Self::OpenHome => Provider::OpenHome,
            Self::Upnp => Provider::Upnp,
            Self::HqPlayer => Provider::HqPlayer,
            Self::AppleMusic => Provider::AppleMusic,
            Self::Spotify => Provider::Spotify,
            Self::MusicAssistant => Provider::MusicAssistant,
            Self::Unprefixed | Self::Unknown => Provider::Unknown,
        }
    }
}

// =============================================================================
// Refusal sentences for an unplaceable zone id
// =============================================================================

/// The human sentence for a zone id UHC cannot place.
///
/// Two shapes, because the fixes differ. Both name every accepted prefix and
/// point at `hifi_zones`, so a client recovers in one call rather than guessing.
///
/// Callers pass this to `Envelope::refused` alongside
/// [`unplaceable_zone_refusal`], so the prose and its classification are written
/// side by side.
pub fn unplaceable_zone_text(zone_id: &str, target: ZoneTarget) -> String {
    let prefixes = ACCEPTED_ZONE_PREFIXES.join(", ");
    match target {
        ZoneTarget::Unknown => {
            let prefix = zone_id
                .split_once(':')
                .map(|(p, _)| format!("{p}:"))
                .unwrap_or_default();
            format!(
                "Zone id '{zone_id}' uses the prefix '{prefix}', which names no adapter. \
                 Accepted prefixes: {prefixes}. Call hifi_zones for valid zone ids."
            )
        }
        // Unprefixed is the load-bearing case: it used to work by defaulting to
        // Roon, so the sentence has to carry the repair, not just the rule.
        _ => format!(
            "Zone id '{zone_id}' has no provider prefix. Accepted prefixes: {prefixes}. \
             If this is a Roon zone id, call again with 'roon:{zone_id}'. Call hifi_zones \
             for valid zone ids."
        ),
    }
}

/// The structured half of [`unplaceable_zone_text`].
///
/// `invalid`, not `unsupported`: nothing about a provider was learned, and the
/// client can fix this by itself. `accepted` is the prefix list because that is
/// what the client resends with.
pub fn unplaceable_zone_refusal(target: ZoneTarget) -> crate::mcp::envelope::Refusal {
    let detail = match target {
        ZoneTarget::Unknown => {
            "This zone id's prefix names no adapter, so UHC cannot say which backend it \
             belongs to or what it supports. hifi_zones lists every zone id that exists; \
             hifi_capabilities says what each provider supports."
        }
        _ => {
            "Every zone id UHC publishes carries a provider prefix, so an unprefixed id \
             cannot be placed. Until #398 it was assumed to be Roon, which silently sent \
             typos and non-Roon ids to the Roon adapter."
        }
    };
    crate::mcp::envelope::Refusal::InvalidParameter {
        parameter: "zone_id",
        accepted: ACCEPTED_ZONE_PREFIXES
            .iter()
            .map(|p| (*p).to_string())
            .collect(),
        detail: detail.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_every_known_prefix() {
        assert_eq!(ZoneTarget::classify("roon:1601a5d4"), ZoneTarget::Roon);
        assert_eq!(ZoneTarget::classify("lms:aa:bb:cc"), ZoneTarget::Lms);
        assert_eq!(ZoneTarget::classify("openhome:uuid"), ZoneTarget::OpenHome);
        assert_eq!(ZoneTarget::classify("upnp:uuid"), ZoneTarget::Upnp);
        assert_eq!(
            ZoneTarget::classify("hqplayer:desktop"),
            ZoneTarget::HqPlayer
        );
    }

    /// The two unplaceable shapes must stay distinguishable: they get different
    /// sentences, and collapsing them loses the repair hint for bare ids.
    #[test]
    fn unplaceable_ids_are_two_distinct_cases() {
        assert_eq!(ZoneTarget::classify("1601a5d4"), ZoneTarget::Unprefixed);
        assert_eq!(ZoneTarget::classify(""), ZoneTarget::Unprefixed);
        assert_eq!(ZoneTarget::classify("sonos:abc"), ZoneTarget::Unknown);
        assert_eq!(ZoneTarget::classify("chromecast:abc"), ZoneTarget::Unknown);
        assert_ne!(
            ZoneTarget::classify("1601a5d4"),
            ZoneTarget::classify("sonos:abc")
        );
    }

    /// An absent `zone_id` is not an unplaceable one. This is the single default
    /// #398 kept, and it is documented in `hifi_search`'s own description.
    #[test]
    fn an_absent_zone_id_is_still_roon() {
        assert_eq!(ZoneTarget::classify_opt(None), ZoneTarget::Roon);
        assert_eq!(
            ZoneTarget::classify_opt(None).for_library(),
            LibraryRoute::Roon
        );
    }

    /// Nothing unplaceable reaches an adapter any more, on any of the three
    /// rules. This is #398's central behavior change.
    #[test]
    fn unplaceable_ids_are_refused_on_every_route() {
        for zone_id in ["1601a5d4bare", "sonos:abc", ""] {
            let target = ZoneTarget::classify(zone_id);
            assert!(
                matches!(target.for_transport(), TransportRoute::Refused(_)),
                "{zone_id} must not reach a transport adapter"
            );
            assert!(
                matches!(target.for_volume(), VolumeRoute::Refused(_)),
                "{zone_id} must not reach a volume adapter"
            );
            assert!(
                matches!(target.for_library(), LibraryRoute::Refused(_)),
                "{zone_id} must not reach a library adapter"
            );
        }
    }

    /// Each recognised prefix reaches its own adapter for transport.
    #[test]
    fn transport_routes_each_prefix_to_its_own_adapter() {
        assert_eq!(
            ZoneTarget::classify("roon:x").for_transport(),
            TransportRoute::Roon
        );
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
        // Recognised but not wired — and it must carry which provider it was, or
        // the refusal cannot say #328.
        assert_eq!(
            ZoneTarget::classify("hqplayer:x").for_transport(),
            TransportRoute::Refused(ZoneTarget::HqPlayer)
        );
    }

    /// Volume reaches four adapters since #398, where it reached two.
    #[test]
    fn volume_reaches_openhome_and_upnp_since_398() {
        assert_eq!(
            ZoneTarget::classify("roon:x").for_volume(),
            VolumeRoute::Roon
        );
        assert_eq!(ZoneTarget::classify("lms:x").for_volume(), VolumeRoute::Lms);
        assert_eq!(
            ZoneTarget::classify("openhome:x").for_volume(),
            VolumeRoute::OpenHome
        );
        assert_eq!(
            ZoneTarget::classify("upnp:x").for_volume(),
            VolumeRoute::Upnp
        );
    }

    /// Volume and transport still differ, and the difference has moved: it used
    /// to be OpenHome/UPnP, and now it is only the library rule that narrows.
    #[test]
    fn library_routes_roon_lms_and_spotify() {
        assert_eq!(
            ZoneTarget::classify("roon:x").for_library(),
            LibraryRoute::Roon
        );
        assert_eq!(
            ZoneTarget::classify("lms:x").for_library(),
            LibraryRoute::Lms
        );
        assert_eq!(
            ZoneTarget::classify("spotify:x").for_library(),
            LibraryRoute::Spotify
        );
        for zone_id in ["openhome:x", "upnp:x", "hqplayer:x"] {
            assert!(
                matches!(
                    ZoneTarget::classify(zone_id).for_library(),
                    LibraryRoute::Refused(_)
                ),
                "{zone_id} has no library and must not be sent to Roon's"
            );
            // ...while transport for the same id does reach its own adapter, or
            // is refused naming its own provider. Either way, never Roon.
            assert_ne!(
                ZoneTarget::classify(zone_id).for_transport(),
                TransportRoute::Roon,
                "{zone_id} must never be routed to Roon"
            );
        }
    }

    /// The prefix list a refusal prints must be exactly what `classify`
    /// recognises. Divergence would print advice that does not work.
    #[test]
    fn accepted_prefixes_are_exactly_what_classify_recognises() {
        for prefix in ACCEPTED_ZONE_PREFIXES {
            let target = ZoneTarget::classify(&format!("{prefix}abc"));
            assert_eq!(
                target.prefix(),
                Some(*prefix),
                "{prefix} is advertised but classify does not place it"
            );
        }
        let recognised: Vec<&str> = ZoneTarget::PROVIDERS
            .iter()
            .filter_map(|t| t.prefix())
            .collect();
        assert_eq!(
            recognised, ACCEPTED_ZONE_PREFIXES,
            "every provider's prefix must be advertised, in the same order"
        );
    }

    /// Both refusal sentences must name every accepted prefix, and the
    /// unprefixed one must carry the repaired id.
    #[test]
    fn refusal_text_names_every_prefix_and_repairs_a_bare_id() {
        let bare = unplaceable_zone_text("1601a5d4", ZoneTarget::Unprefixed);
        for prefix in ACCEPTED_ZONE_PREFIXES {
            assert!(bare.contains(prefix), "{bare} must name {prefix}");
        }
        assert!(
            bare.contains("'roon:1601a5d4'"),
            "a bare id must be told the exact repaired id: {bare}"
        );

        let unknown = unplaceable_zone_text("sonos:abc", ZoneTarget::Unknown);
        assert!(
            unknown.contains("'sonos:'"),
            "an unrecognised prefix must be quoted back: {unknown}"
        );
        for prefix in ACCEPTED_ZONE_PREFIXES {
            assert!(unknown.contains(prefix), "{unknown} must name {prefix}");
        }
    }

    /// Every action the tool's description advertises is in the closed set, and
    /// nothing else is.
    #[test]
    fn control_actions_cover_the_documented_set() {
        for action in [
            "play",
            "pause",
            "playpause",
            "next",
            "previous",
            "prev",
            "volume_set",
            "volume_up",
            "volume_down",
        ] {
            assert!(
                CONTROL_ACTIONS.contains(&action),
                "{action} is documented but not in the closed set"
            );
        }
        assert!(!CONTROL_ACTIONS.contains(&"frobnicate"));
        assert!(!CONTROL_ACTIONS.contains(&"stop"));
        // Every transport alternative must be a real action.
        for action in TRANSPORT_ACTIONS {
            assert!(
                CONTROL_ACTIONS.contains(action),
                "{action} is offered as an alternative but is not accepted"
            );
        }
    }
}
