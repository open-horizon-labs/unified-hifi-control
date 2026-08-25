//! Per-provider capability truth, in three states (issue #398).
//!
//! # The problem this solves
//!
//! `AGENTS.md`'s capability matrix carried `❌` against OpenHome and UPnP volume.
//! Both adapters implement `vol_abs`/`vol_rel` and `POST /openhome/control` has
//! exposed it over HTTP the whole time; only the MCP path declined to call it. A
//! hand-written cell had turned a UHC gap into a claim about the provider, and it
//! stood until someone read the adapters — after which the same false claim was
//! copied into #398's own acceptance criteria.
//!
//! So this module has one job: make that class of error a **test failure** rather
//! than something a careful reader might catch.
//!
//! # Three states, and only two of them can be hand-written
//!
//! [`Support`] has three variants. [`Gap`] — the type of the hand-written table —
//! has **two**, and no way to spell "supported". The only code that can produce
//! [`Support::Supported`] is [`routed`], which asks
//! [`crate::mcp::routing`]'s own functions. A hand-written `✅` is therefore not a
//! matter of discipline here; it does not compile.
//!
//! That guarantee is narrower than it sounds, and the narrowness matters: it says
//! the table cannot disagree with routing. It does **not** say routing is right —
//! `for_volume()` returning `VolumeRoute::OpenHome` makes `volume: supported` true
//! by construction whether or not the adapter call works. What closes that gap is
//! `tests/mcp_contract.rs::every_supported_capability_reaches_that_providers_own_adapter`,
//! which calls every `supported` cell and asserts the response names that
//! provider's own adapter. Read that test before trusting this module.
//!
//! # The rule for `unsupported`, and why it is biased
//!
//! [`Support::Unsupported`] tells a client **never to retry**. It has no expiry.
//! [`Support::NotImplemented`] costs a client one wasted call and self-corrects
//! when the issue it names ships. The two errors are not symmetric, so neither is
//! the rule:
//!
//! > A cell may be `unsupported` only when the provider's protocol demonstrably
//! > lacks it and the fact can be named in one sentence a reader can check.
//! > Otherwise it is `not_implemented`.
//!
//! Every `unsupported` cell therefore carries its `evidence`, and the evidence is
//! emitted to the client in `detail` — so the claim is auditable instead of
//! trusted. Where UHC's `unsupported` claims rest on specification knowledge
//! rather than on a call to a device (all of OpenHome's and UPnP's do; none of
//! LMS's, which come from the live Lyrion 9.1.2 inventories in #402/#403), the
//! evidence says so.
//!
//! # What this module does NOT model, stated rather than implied
//!
//! **Capability is per provider, not per device.** A fixed-volume Roon output —
//! an endpoint feeding an analogue preamp — has no volume control, and this module
//! reports `volume: supported` for it. UHC cannot currently do better: the
//! aggregator's `volume_control: None` conflates "this output has no volume
//! control" with "no volume has been read yet", so deriving from it would mislabel
//! every freshly discovered zone. The wire payload carries the aggregator's
//! `has_volume_control` observation beside the capability so a client can combine
//! the two itself; the ambiguity is documented on that field rather than resolved
//! by guessing.
//!
//! **Capability is per operation family, not per action.** `transport` covers
//! play/pause and `transport_skip` covers next/previous, because UPnP refuses the
//! second (see [`Capability::TransportSkip`]). Other per-action asymmetries almost
//! certainly exist — `stop` is implemented by three adapters and is not a
//! `hifi_control` action at all — and are not modelled.

use serde::Serialize;

use crate::mcp::envelope::{Provider, Refusal};
use crate::mcp::routing::{LibraryRoute, TransportRoute, VolumeRoute, ZoneTarget};

// =============================================================================
// The vocabulary
// =============================================================================

/// What a client can ask a zone to do.
///
/// # This list is deliberately not Roon-shaped
///
/// #392's first rule is that intersecting the surface to the weakest backend is
/// its own dishonesty. So the vocabulary includes every operation LMS has beyond
/// Roon — `queue_reorder`, `queue_remove`, `queue_clear` are `unsupported` for
/// Roon (its public API offers a queue subscription plus `play_from_here` and
/// nothing more) and `not_implemented` for LMS. The matrix therefore says LMS is
/// the richer backend out loud, which is the fact earlier drafts of AGENTS.md
/// hid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Start, stop and toggle playback: `hifi_control` `play`/`pause`/`playpause`.
    Transport,
    /// Move between items: `hifi_control` `next`/`previous`.
    ///
    /// Split from [`Self::Transport`] because UHC's UPnP adapter refuses these
    /// two outright (`src/adapters/upnp.rs`, `REFUSED_TRANSPORT_ACTIONS`) while
    /// accepting play/pause. Reporting one `transport` value would have to lie
    /// about one half or the other.
    TransportSkip,
    /// Set or nudge the level: `hifi_control` `volume_set`/`volume_up`/`volume_down`.
    Volume,
    /// Find content by free text: `hifi_search`.
    Search,
    /// Play the best match for free text: `hifi_play`.
    PlayByQuery,
    /// Play a specific item by an opaque reference, without re-searching (#396).
    PlayByRef,
    /// Walk a library hierarchy with paging (#399 for Roon, #402 for LMS).
    Browse,
    /// Read what is queued.
    QueueRead,
    /// Jump to a queued item.
    QueueJump,
    /// Move a queued item.
    QueueReorder,
    /// Drop one queued item.
    QueueRemove,
    /// Empty the queue.
    QueueClear,
    /// Move an active queue from one zone to another (#507).
    QueueTransfer,
    /// Insert immediately after the current item.
    PlayNext,
    /// Read and set repeat mode.
    RepeatMode,
    /// Read and set shuffle mode.
    ShuffleMode,
    /// List, start and save named playlists.
    SavedPlaylists,
    /// List and play saved favourites.
    Favorites,
    /// Group and ungroup zones for synchronised playback.
    MultiroomSync,
}

impl Capability {
    /// The whole vocabulary, in report order.
    ///
    /// Transport first, then content, then the queue, then player state — roughly
    /// the order a client needs them in.
    pub const ALL: &'static [Self] = &[
        Self::Transport,
        Self::TransportSkip,
        Self::Volume,
        Self::Search,
        Self::PlayByQuery,
        Self::PlayByRef,
        Self::Browse,
        Self::QueueRead,
        Self::QueueJump,
        Self::QueueReorder,
        Self::QueueRemove,
        Self::QueueClear,
        Self::QueueTransfer,
        Self::PlayNext,
        Self::RepeatMode,
        Self::ShuffleMode,
        Self::SavedPlaylists,
        Self::Favorites,
        Self::MultiroomSync,
    ];

    /// The name on the wire. Matches the `serde` rename, and
    /// [`tests::names_match_the_serialized_form`] proves it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::TransportSkip => "transport_skip",
            Self::Volume => "volume",
            Self::Search => "search",
            Self::PlayByQuery => "play_by_query",
            Self::PlayByRef => "play_by_ref",
            Self::Browse => "browse",
            Self::QueueRead => "queue_read",
            Self::QueueJump => "queue_jump",
            Self::QueueReorder => "queue_reorder",
            Self::QueueRemove => "queue_remove",
            Self::QueueClear => "queue_clear",
            Self::QueueTransfer => "queue_transfer",
            Self::PlayNext => "play_next",
            Self::RepeatMode => "repeat_mode",
            Self::ShuffleMode => "shuffle_mode",
            Self::SavedPlaylists => "saved_playlists",
            Self::Favorites => "favorites",
            Self::MultiroomSync => "multiroom_sync",
        }
    }
}

// =============================================================================
// The three states
// =============================================================================

/// One capability's state for one provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Support {
    /// A working MCP call path exists. **Only [`routed`] can produce this.**
    Supported,
    /// The provider's protocol cannot do this. Never retry.
    Unsupported {
        /// The protocol fact this rests on, in one checkable sentence.
        evidence: &'static str,
    },
    /// The provider can; UHC has not wired it to MCP.
    NotImplemented {
        /// The UHC issue that closes it, so "not yet" is checkable.
        tracked_by: &'static str,
        /// Which protocol feature is going unused.
        evidence: &'static str,
    },
}

impl Support {
    /// The wire name of the state.
    pub fn name(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported { .. } => "unsupported",
            Self::NotImplemented { .. } => "not_implemented",
        }
    }

    pub fn evidence(self) -> Option<&'static str> {
        match self {
            Self::Supported => None,
            Self::Unsupported { evidence } | Self::NotImplemented { evidence, .. } => {
                Some(evidence)
            }
        }
    }

    pub fn tracked_by(self) -> Option<&'static str> {
        match self {
            Self::NotImplemented { tracked_by, .. } => Some(tracked_by),
            _ => None,
        }
    }

    /// The [`Refusal`] a tool returns when it declines this capability.
    ///
    /// The classification comes from here rather than from the call site, so a
    /// tool cannot describe a gap one way while the capability report describes it
    /// another. `alternatives` is the caller's, because what a zone can do
    /// *instead* depends on which tool was asked.
    pub fn refusal(self, capability: Capability, alternatives: Vec<String>) -> Option<Refusal> {
        match self {
            Self::Supported => None,
            Self::Unsupported { evidence } => Some(Refusal::ProviderLimitation {
                operation: capability.name().to_string(),
                alternatives,
                detail: evidence.to_string(),
            }),
            Self::NotImplemented {
                tracked_by,
                evidence,
            } => Some(Refusal::NotImplemented {
                operation: capability.name().to_string(),
                tracked_by,
                alternatives,
                detail: evidence.to_string(),
            }),
        }
    }
}

/// The hand-written half of the table.
///
/// **This type cannot express "supported", and that is its whole point.** The
/// error #398 exists to correct was a hand-written cell claiming a provider limit.
/// Making the hand-written vocabulary unable to claim capability at all removes
/// the possibility rather than warning against it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gap {
    /// The provider's protocol cannot. Requires a nameable fact.
    ProviderCannot(&'static str),
    /// The provider can and UHC does not. `(tracked_by, evidence)`.
    NotWired(&'static str, &'static str),
}

impl Gap {
    fn to_support(self) -> Support {
        match self {
            Self::ProviderCannot(evidence) => Support::Unsupported { evidence },
            Self::NotWired(tracked_by, evidence) => Support::NotImplemented {
                tracked_by,
                evidence,
            },
        }
    }
}

// =============================================================================
// The routed half — the only source of `Supported`
// =============================================================================

/// Ask the routing layer whether a working call path exists.
///
/// `None` means "routing does not decide this", and the caller falls through to
/// [`GAPS`]. Every `Some` is `Support::Supported`: a route that refuses is not a
/// capability, it is a gap, and gaps carry evidence this function does not have.
fn routed(target: ZoneTarget, capability: Capability) -> Option<Support> {
    let supported = match capability {
        Capability::Transport => matches!(
            (target, target.for_transport()),
            (ZoneTarget::Roon, TransportRoute::Roon)
                | (ZoneTarget::Lms, TransportRoute::Lms)
                | (ZoneTarget::OpenHome, TransportRoute::OpenHome)
                | (ZoneTarget::Upnp, TransportRoute::Upnp)
                | (ZoneTarget::HqPlayer, TransportRoute::HqPlayer)
                | (ZoneTarget::Spotify, TransportRoute::Spotify)
                | (ZoneTarget::MusicAssistant, TransportRoute::MusicAssistant)
        ),
        // Same route as Transport, minus the adapters that refuse the two skip
        // actions. Read from the adapter's own const so the claim cannot drift
        // from the arm that enforces it.
        Capability::TransportSkip => {
            let routes_to_own_adapter = matches!(
                (target, target.for_transport()),
                (ZoneTarget::Roon, TransportRoute::Roon)
                    | (ZoneTarget::Lms, TransportRoute::Lms)
                    | (ZoneTarget::OpenHome, TransportRoute::OpenHome)
                    | (ZoneTarget::Upnp, TransportRoute::Upnp)
                    | (ZoneTarget::HqPlayer, TransportRoute::HqPlayer)
                    | (ZoneTarget::Spotify, TransportRoute::Spotify)
                    | (ZoneTarget::MusicAssistant, TransportRoute::MusicAssistant)
            );
            let adapter_refuses = target == ZoneTarget::Upnp
                && crate::adapters::upnp::REFUSED_TRANSPORT_ACTIONS.contains(&"next");
            routes_to_own_adapter && !adapter_refuses
        }
        Capability::Volume => matches!(
            (target, target.for_volume()),
            (ZoneTarget::Roon, VolumeRoute::Roon)
                | (ZoneTarget::Lms, VolumeRoute::Lms)
                | (ZoneTarget::OpenHome, VolumeRoute::OpenHome)
                | (ZoneTarget::Upnp, VolumeRoute::Upnp)
                | (ZoneTarget::HqPlayer, VolumeRoute::HqPlayer)
                | (ZoneTarget::Spotify, VolumeRoute::Spotify)
                | (ZoneTarget::MusicAssistant, VolumeRoute::MusicAssistant)
        ),
        Capability::Search | Capability::PlayByQuery => matches!(
            (target, target.for_library()),
            (ZoneTarget::Roon, LibraryRoute::Roon)
                | (ZoneTarget::Lms, LibraryRoute::Lms)
                | (ZoneTarget::Spotify, LibraryRoute::Spotify)
                | (ZoneTarget::MusicAssistant, LibraryRoute::MusicAssistant)
        ),
        Capability::PlayByRef => {
            matches!(
                (target, target.for_library()),
                (ZoneTarget::Spotify, LibraryRoute::Spotify)
                    | (ZoneTarget::MusicAssistant, LibraryRoute::MusicAssistant)
            )
        }
        Capability::RepeatMode | Capability::ShuffleMode => {
            (target == ZoneTarget::Spotify
                && matches!(target.for_transport(), TransportRoute::Spotify))
                || target == ZoneTarget::HqPlayer
                || (target == ZoneTarget::MusicAssistant
                    && matches!(target.for_transport(), TransportRoute::MusicAssistant))
        }
        Capability::QueueRead => matches!(target, ZoneTarget::Spotify | ZoneTarget::MusicAssistant),
        Capability::QueueJump
        | Capability::QueueReorder
        | Capability::QueueRemove
        | Capability::QueueClear
        | Capability::QueueTransfer => target == ZoneTarget::MusicAssistant,
        Capability::PlayNext => target == ZoneTarget::MusicAssistant,
        Capability::MultiroomSync => target == ZoneTarget::MusicAssistant,
        Capability::Browse | Capability::SavedPlaylists | Capability::Favorites => {
            matches!(target, ZoneTarget::Spotify | ZoneTarget::MusicAssistant)
        }
    };
    supported.then_some(Support::Supported)
}

// =============================================================================
// The gap table
// =============================================================================
//
// Shared evidence, named so the reasoning is stated once and every cell that
// depends on it points at the same sentence. A reader disagreeing with one of
// these disagrees with every cell it covers, which is the correct blast radius.

/// UPnP zones are `MediaRenderer` devices. Library operations belong to a
/// `MediaServer`'s `ContentDirectory`, which UHC neither discovers nor models.
const UPNP_RENDERER_HAS_NO_LIBRARY: &str = "UHC discovers UPnP zones as \
    urn:schemas-upnp-org:device:MediaRenderer:1 and speaks only AVTransport:1 and \
    RenderingControl:1. Searching or browsing content is a ContentDirectory:1 (MediaServer) \
    capability, which a renderer does not have and UHC does not discover. Verified from the \
    UPnP AV service definitions, not from a device.";

/// AVTransport carries one current URI plus one `SetNextAVTransportURI`. There is
/// no list, so there is nothing to read, reorder, remove or clear.
const UPNP_HAS_NO_QUEUE: &str = "AVTransport:1 holds a single current transport URI plus one \
    SetNextAVTransportURI; it has no playlist to enumerate or mutate. Verified from the UPnP \
    AV service definitions, not from a device.";

/// OpenHome zones as UHC discovers them are renderers too: it searches for
/// `Product`/`Transport`/`Volume` and speaks nothing that carries a library.
const OPENHOME_RENDERER_HAS_NO_LIBRARY: &str = "UHC discovers OpenHome zones by their \
    av-openhome-org Product, Transport and Volume services (src/adapters/openhome.rs) and the \
    OpenHome service set has no library: content is resolved by a control point against a \
    separate media server. Verified from the OpenHome service definitions, not from a device.";

/// The OpenHome `Playlist:1` service is a real queue — `Read`, `ReadList`,
/// `IdArray`, `Insert`, `DeleteId`, `DeleteAll`, `SeekId`, `SeekIndex`,
/// `SetRepeat`, `SetShuffle`. UHC drives none of it.
const OPENHOME_PLAYLIST_SERVICE_UNUSED: &str = "OpenHome's Playlist:1 service provides \
    Read/ReadList/IdArray, Insert, DeleteId, DeleteAll, SeekId/SeekIndex and \
    SetRepeat/SetShuffle. UHC discovers only Product/Transport/Volume and drives none of it, \
    so this is a UHC gap. Verified from the OpenHome service definitions, not from a device.";

/// Roon's public API offers a queue subscription plus `play_from_here`. Nothing
/// mutates the queue. #392 states this; the pinned fork exposes nothing more.
const ROON_QUEUE_IS_READ_PLUS_JUMP: &str = "The Roon API's transport service exposes a queue \
    subscription and play_from_here and no mutation at all -- no move, remove or clear. The \
    pinned roon-api fork (ohc/main) exposes subscribe_queue and play_from_here and nothing \
    further.";

/// OpenHome's per-room `Playlist:1` has no cross-room action, and Songcast
/// relays audio rather than queue state.
const OPENHOME_HAS_NO_QUEUE_TRANSFER: &str = "OpenHome's Playlist:1 (Read/Insert/DeleteId/\
    DeleteAll) is scoped to one room's renderer; the service set defines no action that moves \
    one room's playlist into another's. Songcast (Sender:1/Receiver:1) relays audio to a group, \
    it does not merge queue state. Verified from the OpenHome service definitions, not from a \
    device.";

/// Playing a *named* item is a different question from *finding* one, and the ship
/// gate's dissent caught this module conflating them.
///
/// Both renderer protocols can be told to play a specific thing: UPnP's
/// `AVTransport:1` has `SetAVTransportURI` and `SetNextAVTransportURI`, OpenHome's
/// `Playlist:1` has `Insert(AfterId, Uri, Metadata)`. What is missing is UHC's
/// ability to *name* one — there is no library to resolve a reference against. That
/// is a UHC gap, so the state is `not_implemented`, even though the practical
/// answer today is still "you cannot do this".
///
/// The distinction matters because a `⛔` here would tell a client the operation is
/// impossible forever, when in fact #396 plus a media-server integration would make
/// it work. `search`, `browse` and `play_by_query` keep their `⛔`, because no action
/// in either service set resolves free text at all.
const PLAY_A_NAMED_URI_EXISTS: &str =
    "the protocol can play a specific item -- UPnP AVTransport:1 \
    takes SetAVTransportURI and SetNextAVTransportURI, OpenHome Playlist:1 takes \
    Insert(AfterId, Uri, Metadata) -- so what is missing is UHC's ability to name one, not the \
    device's ability to play it. Reported as a UHC gap rather than a provider limit, because a \
    reference minted against a media server would work. Verified from the UPnP AV and OpenHome \
    service definitions, not from a device.";

const APPLE_TRANSPORT_NOT_VALIDATED: &str = "the native iPhone companion path is implemented, \
    but SystemMusicPlayer transport and volume behavior remains pending signed physical-device \
    validation (#465).";

/// HQPlayer content operations: UHC's XML control protocol coverage does not
/// include them and the protocol's own reach has not been verified here. Reported
/// as a gap rather than a limit, per this module's bias rule.
const HQPLAYER_CONTENT_UNVERIFIED: &str = "UHC's HQPlayer adapter speaks transport, volume, \
    seek and pipeline settings; whether HQPlayer's control protocol reaches content operations \
    has not been verified here. Reported as not-yet-implemented rather than as a provider \
    limit, because an unverified 'never' is the more expensive error.";

/// Every cell routing does not decide.
///
/// Exactly the `(target, capability)` pairs for which [`routed`] returns `None` —
/// asserted both ways by [`tests::the_gap_table_covers_exactly_what_routing_does_not`],
/// so adding a capability or a provider fails until every cell is filled, and a
/// cell that routing has taken over fails until it is deleted.
#[rustfmt::skip]
const GAPS: &[(ZoneTarget, Capability, Gap)] = &[
    // -------------------------------------------------------------------------
    // Roon. Transport, volume, search and play-by-query are routed.
    // -------------------------------------------------------------------------
    (ZoneTarget::Roon, Capability::PlayByRef, Gap::NotWired("#396",
        "RoonAdapter::browse/load/play_item exist and are exposed over HTTP; MCP mints no \
         reference for a search hit to act on.")),
    (ZoneTarget::Roon, Capability::Browse, Gap::NotWired("#399",
        "RoonAdapter::browse() and load() exist and POST /roon/browse exposes them; only the \
         MCP projection is missing.")),
    (ZoneTarget::Roon, Capability::QueueRead, Gap::NotWired("#400",
        "the pinned roon-api fork exposes subscribe_queue(zone, max_items), which nothing in \
         UHC calls.")),
    (ZoneTarget::Roon, Capability::QueueJump, Gap::NotWired("#400",
        "the pinned roon-api fork exposes play_from_here(zone, queue_item_id), which nothing \
         in UHC calls.")),
    (ZoneTarget::Roon, Capability::QueueReorder, Gap::ProviderCannot(ROON_QUEUE_IS_READ_PLUS_JUMP)),
    (ZoneTarget::Roon, Capability::QueueRemove, Gap::ProviderCannot(ROON_QUEUE_IS_READ_PLUS_JUMP)),
    (ZoneTarget::Roon, Capability::QueueClear, Gap::ProviderCannot(ROON_QUEUE_IS_READ_PLUS_JUMP)),
    (ZoneTarget::Roon, Capability::QueueTransfer, Gap::ProviderCannot(ROON_QUEUE_IS_READ_PLUS_JUMP)),
    (ZoneTarget::Roon, Capability::PlayNext, Gap::NotWired("#399",
        "Roon's browse item actions include Play Next alongside Play Now and Queue; UHC's \
         PlayAction models only Play, Queue and Radio, so this arrives with browse rather \
         than with the queue.")),
    (ZoneTarget::Roon, Capability::RepeatMode, Gap::NotWired("#360",
        "the Roon API's transport service takes loop settings (disabled/loop/loop_one); UHC \
         drives none of them from any surface.")),
    (ZoneTarget::Roon, Capability::ShuffleMode, Gap::NotWired("#360",
        "the Roon API's transport service takes a shuffle setting; UHC drives it from no \
         surface.")),
    (ZoneTarget::Roon, Capability::SavedPlaylists, Gap::NotWired("#399",
        "Roon exposes Playlists as a browse hierarchy, so this arrives with browse rather \
         than as its own protocol feature.")),
    (ZoneTarget::Roon, Capability::Favorites, Gap::NotWired("#399",
        "Roon exposes My Favorites and tags as browse hierarchies, so this arrives with \
         browse.")),
    (ZoneTarget::Roon, Capability::MultiroomSync, Gap::NotWired("#360",
        "the Roon API's transport service groups and ungroups outputs; UHC exposes no \
         grouping on any surface.")),

    // -------------------------------------------------------------------------
    // LMS. **Not one ProviderCannot** -- every gap here is UHC's, verified live
    // against Lyrion 9.1.2 in #402/#403. This is the row the issue's third
    // acceptance criterion is about.
    // -------------------------------------------------------------------------
    (ZoneTarget::Lms, Capability::PlayByRef, Gap::NotWired("#396",
        "the native taggedlist queries return durable entity ids (track_id/album_id/artist_id) \
         that playlistcontrol accepts, verified live; MCP discards them and hands back a title. \
         Note the XMLBrowser paths (globalsearch, favorites) return positional breadcrumbs \
         instead, which is #396's safety problem, not a capability gap.")),
    (ZoneTarget::Lms, Capability::Browse, Gap::NotWired("#402",
        "browselibrary items and the native albums/artists/genres/years/playlists/mediafolder \
         queries walk the whole hierarchy with native <start> <n> paging -- all \
         verified live on Lyrion 9.1.2. The adapter never calls any of them.")),
    (ZoneTarget::Lms, Capability::QueueRead, Gap::NotWired("#400",
        "status <player> <start> <n> returns the whole current playlist with playlist_cur_index; \
         verified live.")),
    (ZoneTarget::Lms, Capability::QueueJump, Gap::NotWired("#400",
        "playlist index <n> jumps to a queue position; verified live.")),
    (ZoneTarget::Lms, Capability::QueueReorder, Gap::NotWired("#400",
        "playlist move <from> <to> reorders the queue; verified live. Roon cannot do this and \
         LMS can, which is why this capability exists in the vocabulary at all.")),
    (ZoneTarget::Lms, Capability::QueueRemove, Gap::NotWired("#400",
        "playlist delete <index> removes one queued item; verified live.")),
    (ZoneTarget::Lms, Capability::QueueClear, Gap::NotWired("#400",
        "playlist clear empties the queue; verified live.")),
    (ZoneTarget::Lms, Capability::QueueTransfer, Gap::NotWired("#400",
        "sync <playerid> was verified live (#403) to merge a player into another's sync group by \
         adopting the leader's queue, which destroys the source's queue rather than transferring \
         it; the CLI reference names no dedicated queue-to-queue transfer command. A composite \
         emulation -- read the source's playlist, replay it against the target with \
         playlistcontrol, then playlist clear the source -- is buildable from primitives #400 \
         already verified live, but is not wired.")),
    (ZoneTarget::Lms, Capability::PlayNext, Gap::NotWired("#403",
        "playlistcontrol cmd:insert places an item immediately after the current one, verified \
         live -- and LmsPlayAction::Insert is already modelled in the adapter and simply \
         unreachable from MCP.")),
    (ZoneTarget::Lms, Capability::RepeatMode, Gap::NotWired("#403",
        "playlist repeat <0|1|2> and playlist repeat ? read and write it; verified live. Note \
         the mode lives on the sync master, so setting it changes every member.")),
    (ZoneTarget::Lms, Capability::ShuffleMode, Gap::NotWired("#403",
        "playlist shuffle <0|1|2> and playlist shuffle ? read and write it; verified live. \
         Setting it reshuffles the queue, so it is also a queue mutation.")),
    (ZoneTarget::Lms, Capability::SavedPlaylists, Gap::NotWired("#403",
        "playlists / playlists tracks / playlistcontrol cmd:load playlist_id / playlists new / \
         rename / delete all exist and were verified live. playlist save additionally needs a \
         configured playlistdir, which is unset on a stock install -- so its own answer is \
         three-state at the server level, which is why #403 probes pref playlistdir ?.")),
    (ZoneTarget::Lms, Capability::Favorites, Gap::NotWired("#403",
        "favorites items / favorites playlist play / add / delete / exists all work; verified \
         live. Note LMS favorites have no durable id -- only a url -- so a ref must be minted \
         over the url.")),
    (ZoneTarget::Lms, Capability::MultiroomSync, Gap::NotWired("#403",
        "sync <playerid>, sync -, sync ? and the server-scoped syncgroups ? all work; verified \
         live. Joining is destructive to the target zone's queue, which is why #403 gates it \
         behind confirmation.")),

    // -------------------------------------------------------------------------
    // OpenHome. Transport, skip and (since #398) volume are routed.
    // -------------------------------------------------------------------------
    (ZoneTarget::OpenHome, Capability::Search, Gap::ProviderCannot(OPENHOME_RENDERER_HAS_NO_LIBRARY)),
    (ZoneTarget::OpenHome, Capability::PlayByQuery, Gap::ProviderCannot(OPENHOME_RENDERER_HAS_NO_LIBRARY)),
    (ZoneTarget::OpenHome, Capability::PlayByRef, Gap::NotWired("#396", PLAY_A_NAMED_URI_EXISTS)),
    (ZoneTarget::OpenHome, Capability::Browse, Gap::ProviderCannot(OPENHOME_RENDERER_HAS_NO_LIBRARY)),
    (ZoneTarget::OpenHome, Capability::QueueRead, Gap::NotWired("#392", OPENHOME_PLAYLIST_SERVICE_UNUSED)),
    (ZoneTarget::OpenHome, Capability::QueueJump, Gap::NotWired("#392", OPENHOME_PLAYLIST_SERVICE_UNUSED)),
    (ZoneTarget::OpenHome, Capability::QueueReorder, Gap::NotWired("#392",
        "OpenHome's Playlist:1 has no Move action, but Insert takes an AfterId, so a reorder is \
         DeleteId plus Insert -- composite rather than atomic, and reachable. Verified from the \
         OpenHome service definitions, not from a device.")),
    (ZoneTarget::OpenHome, Capability::QueueRemove, Gap::NotWired("#392", OPENHOME_PLAYLIST_SERVICE_UNUSED)),
    (ZoneTarget::OpenHome, Capability::QueueClear, Gap::NotWired("#392", OPENHOME_PLAYLIST_SERVICE_UNUSED)),
    (ZoneTarget::OpenHome, Capability::QueueTransfer, Gap::ProviderCannot(OPENHOME_HAS_NO_QUEUE_TRANSFER)),
    (ZoneTarget::OpenHome, Capability::PlayNext, Gap::NotWired("#392", OPENHOME_PLAYLIST_SERVICE_UNUSED)),
    (ZoneTarget::OpenHome, Capability::RepeatMode, Gap::NotWired("#392", OPENHOME_PLAYLIST_SERVICE_UNUSED)),
    (ZoneTarget::OpenHome, Capability::ShuffleMode, Gap::NotWired("#392", OPENHOME_PLAYLIST_SERVICE_UNUSED)),
    (ZoneTarget::OpenHome, Capability::SavedPlaylists, Gap::ProviderCannot(
        "the av-openhome-org service set has no playlist storage: Playlist:1 is the live queue \
         (Read/Insert/DeleteId/DeleteAll) with no save or recall action, and stored playlists \
         live on whatever media server the control point uses. Verified from the OpenHome \
         service definitions, not from a device.")),
    (ZoneTarget::OpenHome, Capability::Favorites, Gap::NotWired("#392",
        "OpenHome devices carry stored presets (Radio:1 presets, and a Pins service on newer \
         firmware). UHC discovers neither. Reported as a gap rather than a limit because the \
         per-firmware reach is unverified here.")),
    (ZoneTarget::OpenHome, Capability::MultiroomSync, Gap::NotWired("#392",
        "OpenHome's Sender:1 and Receiver:1 services are Songcast multiroom -- exactly this \
         capability. UHC discovers neither. Verified from the OpenHome service definitions, \
         not from a device.")),

    // -------------------------------------------------------------------------
    // UPnP. Transport and (since #398) volume are routed; skip is not, because
    // the adapter refuses it.
    // -------------------------------------------------------------------------
    (ZoneTarget::Upnp, Capability::TransportSkip, Gap::NotWired("#392",
        "AVTransport:1 -- the service this adapter already speaks -- defines Next and Previous \
         actions, and UHC's adapter refuses them before issuing either \
         (src/adapters/upnp.rs, REFUSED_TRANSPORT_ACTIONS). A renderer with no playlist would \
         reject the call, but that is the device's answer to give, not UHC's to assume.")),
    (ZoneTarget::Upnp, Capability::Search, Gap::ProviderCannot(UPNP_RENDERER_HAS_NO_LIBRARY)),
    (ZoneTarget::Upnp, Capability::PlayByQuery, Gap::ProviderCannot(UPNP_RENDERER_HAS_NO_LIBRARY)),
    (ZoneTarget::Upnp, Capability::PlayByRef, Gap::NotWired("#396", PLAY_A_NAMED_URI_EXISTS)),
    (ZoneTarget::Upnp, Capability::Browse, Gap::ProviderCannot(UPNP_RENDERER_HAS_NO_LIBRARY)),
    (ZoneTarget::Upnp, Capability::QueueRead, Gap::ProviderCannot(UPNP_HAS_NO_QUEUE)),
    (ZoneTarget::Upnp, Capability::QueueJump, Gap::ProviderCannot(UPNP_HAS_NO_QUEUE)),
    (ZoneTarget::Upnp, Capability::QueueReorder, Gap::ProviderCannot(UPNP_HAS_NO_QUEUE)),
    (ZoneTarget::Upnp, Capability::QueueRemove, Gap::ProviderCannot(UPNP_HAS_NO_QUEUE)),
    (ZoneTarget::Upnp, Capability::QueueClear, Gap::ProviderCannot(UPNP_HAS_NO_QUEUE)),
    (ZoneTarget::Upnp, Capability::QueueTransfer, Gap::ProviderCannot(UPNP_HAS_NO_QUEUE)),
    (ZoneTarget::Upnp, Capability::PlayNext, Gap::NotWired("#396", PLAY_A_NAMED_URI_EXISTS)),
    (ZoneTarget::Upnp, Capability::RepeatMode, Gap::NotWired("#392",
        "AVTransport:1's SetPlayMode takes REPEAT_ONE and REPEAT_ALL, so repeat is a protocol \
         feature UHC does not use. Verified from the UPnP AV service definitions, not from a \
         device.")),
    (ZoneTarget::Upnp, Capability::ShuffleMode, Gap::NotWired("#392",
        "AVTransport:1's SetPlayMode takes SHUFFLE and RANDOM, so shuffle is a protocol \
         feature UHC does not use. Verified from the UPnP AV service definitions, not from a \
         device.")),
    (ZoneTarget::Upnp, Capability::SavedPlaylists, Gap::ProviderCannot(
        "AVTransport:1 and RenderingControl:1 store nothing; a MediaRenderer has no playlist \
         storage. Verified from the UPnP AV service definitions, not from a device.")),
    (ZoneTarget::Upnp, Capability::Favorites, Gap::ProviderCannot(
        "AVTransport:1 and RenderingControl:1 store nothing; a MediaRenderer has no favourites. \
         Verified from the UPnP AV service definitions, not from a device.")),
    (ZoneTarget::Upnp, Capability::MultiroomSync, Gap::ProviderCannot(
        "UPnP AV defines no synchronised-playback service; multiroom on UPnP renderers is \
         vendor-specific and outside the two services UHC speaks. Verified from the UPnP AV \
         service definitions, not from a device.")),

    // -------------------------------------------------------------------------
    // Apple Music. The native companion path exists, but transport acceptance
    // remains deliberately gated until a signed iPhone run proves it.
    // -------------------------------------------------------------------------
    (ZoneTarget::AppleMusic, Capability::Transport, Gap::NotWired("#465", APPLE_TRANSPORT_NOT_VALIDATED)),
    (ZoneTarget::AppleMusic, Capability::TransportSkip, Gap::NotWired("#465", APPLE_TRANSPORT_NOT_VALIDATED)),
    (ZoneTarget::AppleMusic, Capability::Volume, Gap::NotWired("#465", APPLE_TRANSPORT_NOT_VALIDATED)),

    // -------------------------------------------------------------------------
    // HQPlayer content remains unverified; transport, volume, and pipeline mode
    // control are routed through HqpInstanceManager.
    // -------------------------------------------------------------------------
    (ZoneTarget::HqPlayer, Capability::Search, Gap::NotWired("#209", HQPLAYER_CONTENT_UNVERIFIED)),
    (ZoneTarget::HqPlayer, Capability::PlayByQuery, Gap::NotWired("#209", HQPLAYER_CONTENT_UNVERIFIED)),
    (ZoneTarget::HqPlayer, Capability::PlayByRef, Gap::NotWired("#209", HQPLAYER_CONTENT_UNVERIFIED)),
    (ZoneTarget::HqPlayer, Capability::Browse, Gap::NotWired("#209", HQPLAYER_CONTENT_UNVERIFIED)),
    (ZoneTarget::HqPlayer, Capability::QueueRead, Gap::NotWired("#209", HQPLAYER_CONTENT_UNVERIFIED)),
    (ZoneTarget::HqPlayer, Capability::QueueJump, Gap::NotWired("#209", HQPLAYER_CONTENT_UNVERIFIED)),
    (ZoneTarget::HqPlayer, Capability::QueueReorder, Gap::NotWired("#209", HQPLAYER_CONTENT_UNVERIFIED)),
    (ZoneTarget::HqPlayer, Capability::QueueRemove, Gap::NotWired("#209", HQPLAYER_CONTENT_UNVERIFIED)),
    (ZoneTarget::HqPlayer, Capability::QueueClear, Gap::NotWired("#209", HQPLAYER_CONTENT_UNVERIFIED)),
    (ZoneTarget::HqPlayer, Capability::QueueTransfer, Gap::NotWired("#209", HQPLAYER_CONTENT_UNVERIFIED)),
    (ZoneTarget::HqPlayer, Capability::PlayNext, Gap::NotWired("#209", HQPLAYER_CONTENT_UNVERIFIED)),
    (ZoneTarget::HqPlayer, Capability::SavedPlaylists, Gap::NotWired("#209", HQPLAYER_CONTENT_UNVERIFIED)),
    (ZoneTarget::HqPlayer, Capability::Favorites, Gap::NotWired("#209", HQPLAYER_CONTENT_UNVERIFIED)),
    (ZoneTarget::HqPlayer, Capability::MultiroomSync, Gap::NotWired("#209", HQPLAYER_CONTENT_UNVERIFIED)),
];

// =============================================================================
// The public question
// =============================================================================

/// What does this provider support for this capability?
///
/// Routing answers first; anything it does not decide comes from [`GAPS`].
pub fn support(target: ZoneTarget, capability: Capability) -> Support {
    if let Some(supported) = routed(target, capability) {
        return supported;
    }
    if target == ZoneTarget::AppleMusic {
        if matches!(
            capability,
            Capability::Transport | Capability::TransportSkip | Capability::Volume
        ) {
            return Support::NotImplemented {
                tracked_by: "#465",
                evidence: APPLE_TRANSPORT_NOT_VALIDATED,
            };
        }
        let tracked_by = match capability {
            Capability::Search | Capability::PlayByQuery | Capability::PlayByRef => "#481",
            Capability::Browse | Capability::SavedPlaylists | Capability::Favorites => "#482",
            Capability::QueueRead
            | Capability::QueueJump
            | Capability::QueueReorder
            | Capability::QueueRemove
            | Capability::QueueClear
            | Capability::PlayNext => "#483",
            _ => "#462",
        };
        return Support::NotImplemented {
            tracked_by,
            evidence: "the native companion content bridge is specified but not enabled; this capability remains pending its approved owner-scoped transport and companion validation.",
        };
    }
    if matches!(
        target,
        ZoneTarget::AppleMusic | ZoneTarget::Spotify | ZoneTarget::MusicAssistant
    ) {
        return Support::NotImplemented {
            tracked_by: "#462",
            evidence: "the adapter's initial contract covers transport, skip and volume; library, browse, queue and playlist operations are separate follow-on capability steps and are not wired yet.",
        };
    }
    match GAPS
        .iter()
        .find(|(t, c, _)| *t == target && *c == capability)
    {
        Some((_, _, gap)) => gap.to_support(),
        // Unreachable: `the_gap_table_covers_exactly_what_routing_does_not`
        // fails if any cell is missing. If it ever happens anyway, report the
        // non-foreclosing state — inventing a provider limitation out of a
        // missing table row is the one outcome this module must never produce.
        None => Support::NotImplemented {
            tracked_by: "#392",
            evidence: "no capability entry exists for this provider and capability, which is a \
                       UHC bug; reported as not-yet-implemented rather than as a provider \
                       limitation because a missing row is not evidence about a provider.",
        },
    }
}

/// Every capability for one provider, in [`Capability::ALL`] order.
pub fn support_table(target: ZoneTarget) -> Vec<(Capability, Support)> {
    Capability::ALL
        .iter()
        .map(|c| (*c, support(target, *c)))
        .collect()
}

// =============================================================================
// The wire shape
// =============================================================================

/// One capability's state, as `hifi_capabilities` reports it.
///
/// `support` rather than `state`: `state` is already a zone's playback state in
/// every other MCP payload, and one field name meaning two things in one surface
/// is how a client comes to parse the wrong one.
#[derive(Debug, Serialize)]
pub struct McpCapability {
    pub capability: &'static str,
    pub support: &'static str,
    /// Only ever set for `not_implemented` — a provider limitation has no issue
    /// that would close it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracked_by: Option<&'static str>,
    /// The fact the state rests on. Absent for `supported`, where the call path
    /// is the evidence and the contract tests prove it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<&'static str>,
}

/// One provider's whole table.
#[derive(Debug, Serialize)]
pub struct McpProviderCapabilities {
    pub provider: Provider,
    pub capabilities: Vec<McpCapability>,
}

/// A zone, with just enough to join it to a provider's table.
#[derive(Debug, Serialize)]
pub struct McpCapabilityZone {
    pub zone_id: String,
    /// Absent when the prefix is valid but the aggregator holds no such zone —
    /// which is how a client tells a typo from a zone that is offline.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone_name: Option<String>,
    pub provider: Provider,
    /// Whether the aggregator currently holds a volume control for this zone.
    ///
    /// **An observation, not a capability.** `false` means either "this output
    /// has no volume control" or "no volume has been read yet", and UHC cannot
    /// tell those apart — which is exactly why `volume`'s capability state is
    /// per provider and this is reported separately for the client to weigh.
    /// Absent when the aggregator holds no such zone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_volume_control: Option<bool>,
}

/// `hifi_capabilities`' payload.
///
/// Two sections rather than one, because capability state is a function of
/// provider: inlining ~18 identical entries per zone would both bloat the answer
/// and imply a per-zone precision UHC does not have. A single-zone query returns
/// one zone and one provider, so the join is trivial there.
#[derive(Debug, Serialize)]
pub struct McpCapabilityReport {
    pub zones: Vec<McpCapabilityZone>,
    pub providers: Vec<McpProviderCapabilities>,
}

/// Project one provider's table onto the wire.
pub fn provider_capabilities(target: ZoneTarget) -> McpProviderCapabilities {
    McpProviderCapabilities {
        provider: target.provider(),
        capabilities: support_table(target)
            .into_iter()
            .map(|(capability, support)| McpCapability {
                capability: capability.name(),
                support: support.name(),
                tracked_by: support.tracked_by(),
                detail: support.evidence(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LMS's every gap must cite the live check. Enforced by
    /// [`every_lms_gap_cites_live_verification`].
    const LMS_EVIDENCE_MARKER: &str = "verified live";

    /// **The table must cover exactly the cells routing does not.** Both
    /// directions: a missing cell means `support` would fall through to the
    /// defensive default, and a stale cell means a hand-written claim is sitting
    /// unread behind a routed one.
    #[test]
    fn the_gap_table_covers_exactly_what_routing_does_not() {
        let mut missing = Vec::new();
        let mut stale = Vec::new();
        for target in ZoneTarget::PROVIDERS {
            for capability in Capability::ALL {
                let routed = routed(*target, *capability).is_some();
                let listed = !routed
                    && (GAPS.iter().any(|(t, c, _)| t == target && c == capability)
                        || (matches!(
                            target,
                            &ZoneTarget::AppleMusic
                                | &ZoneTarget::Spotify
                                | &ZoneTarget::MusicAssistant
                        ) && matches!(
                            capability,
                            Capability::Browse
                                | Capability::QueueJump
                                | Capability::QueueReorder
                                | Capability::QueueRemove
                                | Capability::QueueClear
                                | Capability::QueueTransfer
                                | Capability::PlayNext
                                | Capability::SavedPlaylists
                                | Capability::Favorites
                                | Capability::MultiroomSync
                        ) || (matches!(target, &ZoneTarget::AppleMusic)
                            && matches!(
                                capability,
                                Capability::Search
                                    | Capability::PlayByQuery
                                    | Capability::PlayByRef
                                    | Capability::QueueRead
                                    | Capability::RepeatMode
                                    | Capability::ShuffleMode
                            ))));
                if !routed && !listed {
                    missing.push((target.label(), capability.name()));
                }
                if routed && listed {
                    stale.push((target.label(), capability.name()));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "GAPS has no entry for {missing:?}. Every cell routing does not decide needs one, \
             or the capability report falls back to a defensive default that says nothing."
        );
        assert!(
            stale.is_empty(),
            "GAPS still lists {stale:?}, which routing now decides. Delete the entries: a \
             hand-written cell shadowed by a routed one is unreadable and unread."
        );

        // No duplicates, or `find` would silently pick the first.
        let mut seen = std::collections::BTreeSet::new();
        for (target, capability, _) in GAPS {
            assert!(
                seen.insert((target.label(), capability.name())),
                "GAPS lists {}/{} twice",
                target.label(),
                capability.name()
            );
        }
    }

    /// #398's third acceptance criterion, at the model level: LMS's protocol
    /// supports every capability in this vocabulary, so not one cell may be a
    /// provider limitation.
    #[test]
    fn lms_has_no_provider_limitation() {
        let claimed: Vec<&str> = support_table(ZoneTarget::Lms)
            .into_iter()
            .filter(|(_, s)| matches!(s, Support::Unsupported { .. }))
            .map(|(c, _)| c.name())
            .collect();
        assert!(
            claimed.is_empty(),
            "LMS is claimed protocol-incapable of {claimed:?}. #402/#403 verified every one of \
             these against live Lyrion 9.1.2."
        );
    }

    /// And each of those gaps must cite that live verification rather than
    /// asserting a gap on faith.
    #[test]
    fn every_lms_gap_cites_live_verification() {
        for (capability, support) in support_table(ZoneTarget::Lms) {
            if let Support::NotImplemented { evidence, .. } = support {
                assert!(
                    evidence.contains(LMS_EVIDENCE_MARKER)
                        || evidence.contains("verified against live"),
                    "lms/{}: evidence must cite the live check, got {evidence:?}",
                    capability.name()
                );
            }
        }
    }

    /// OpenHome/UPnP volume: the case that produced the AGENTS.md error.
    #[test]
    fn openhome_and_upnp_volume_is_supported_not_a_provider_limit() {
        for target in [ZoneTarget::OpenHome, ZoneTarget::Upnp] {
            assert_eq!(
                support(target, Capability::Volume),
                Support::Supported,
                "{}/volume: the adapter implements vol_abs/vol_rel and #398 wired MCP to it",
                target.label()
            );
        }
    }

    /// Every provider limitation must carry evidence a reader can check, and say
    /// where the evidence came from when it is not a live call.
    #[test]
    fn every_provider_limitation_names_a_checkable_fact() {
        let mut count = 0;
        for target in ZoneTarget::PROVIDERS {
            for (capability, support) in support_table(*target) {
                if let Support::Unsupported { evidence } = support {
                    count += 1;
                    assert!(
                        evidence.len() > 60,
                        "{}/{}: 'never' needs a real fact, got {evidence:?}",
                        target.label(),
                        capability.name()
                    );
                    // A limitation derived from a specification rather than from
                    // a device must say so, because that is the weaker claim and
                    // the reader is entitled to know which they are reading.
                    assert!(
                        evidence.contains("Verified from")
                            || evidence.contains("The Roon API")
                            || evidence.contains("the av-openhome-org service set"),
                        "{}/{}: evidence must state its provenance, got {evidence:?}",
                        target.label(),
                        capability.name()
                    );
                }
            }
        }
        assert!(
            count > 0,
            "a three-state model with no second state is two states"
        );
    }

    /// Every gap must name an issue that could close it.
    #[test]
    fn every_gap_names_a_tracking_issue() {
        for target in ZoneTarget::PROVIDERS {
            for (capability, support) in support_table(*target) {
                if let Support::NotImplemented { tracked_by, .. } = support {
                    assert!(
                        tracked_by.starts_with('#')
                            && tracked_by[1..].chars().all(|c| c.is_ascii_digit()),
                        "{}/{}: tracked_by must be a UHC issue reference, got {tracked_by:?}",
                        target.label(),
                        capability.name()
                    );
                }
            }
        }
    }

    /// HQPlayer transport and volume are routed through its instance manager.
    #[test]
    fn hqplayer_transport_and_volume_are_supported_since_328() {
        for capability in [
            Capability::Transport,
            Capability::TransportSkip,
            Capability::Volume,
        ] {
            assert_eq!(
                support(ZoneTarget::HqPlayer, capability),
                Support::Supported
            );
        }
    }

    /// UPnP's skip refusal is read from the adapter's own const, so the two
    /// cannot disagree.
    #[test]
    fn upnp_skip_follows_the_adapters_own_refusal_list() {
        assert!(
            crate::adapters::upnp::REFUSED_TRANSPORT_ACTIONS.contains(&"next"),
            "this test's premise is that the adapter refuses next; if that changed, the \
             capability must have flipped with it"
        );
        assert!(matches!(
            support(ZoneTarget::Upnp, Capability::TransportSkip),
            Support::NotImplemented { .. }
        ));
        // ...while play/pause does reach it.
        assert_eq!(
            support(ZoneTarget::Upnp, Capability::Transport),
            Support::Supported
        );
    }

    /// `name()` and the `serde` rename must agree, or the wire and the docs
    /// disagree about what a capability is called.
    #[test]
    fn names_match_the_serialized_form() {
        for capability in Capability::ALL {
            let serialized = serde_json::to_string(capability).expect("capability serializes");
            assert_eq!(
                serialized,
                format!("\"{}\"", capability.name()),
                "{:?}: name() and serde disagree",
                capability
            );
        }
    }

    /// Every capability in the vocabulary appears exactly once.
    #[test]
    fn the_vocabulary_has_no_duplicates() {
        let mut seen = std::collections::BTreeSet::new();
        for capability in Capability::ALL {
            assert!(
                seen.insert(capability.name()),
                "{} appears twice in Capability::ALL",
                capability.name()
            );
        }
        assert_eq!(seen.len(), 19, "the vocabulary changed size: {seen:?}");
    }

    /// A refusal built from a capability state must carry the same
    /// classification the capability report does. Two places describing one gap
    /// differently is the defect this module exists to remove.
    #[test]
    fn refusals_carry_the_same_classification_as_the_report() {
        let gap = support(ZoneTarget::HqPlayer, Capability::Search);
        match gap.refusal(Capability::Search, vec![]) {
            Some(Refusal::NotImplemented { tracked_by, .. }) => assert_eq!(tracked_by, "#209"),
            other => panic!("expected a not_implemented refusal, got {other:?}"),
        }

        let limit = support(ZoneTarget::Upnp, Capability::Search);
        assert!(matches!(
            limit.refusal(Capability::Search, vec![]),
            Some(Refusal::ProviderLimitation { .. })
        ));

        assert!(Support::Supported
            .refusal(Capability::Transport, vec![])
            .is_none());
    }

    #[test]
    fn spotify_repeat_and_shuffle_are_supported() {
        assert_eq!(
            support(ZoneTarget::Spotify, Capability::RepeatMode),
            Support::Supported
        );
        assert_eq!(
            support(ZoneTarget::Spotify, Capability::ShuffleMode),
            Support::Supported
        );
        assert!(matches!(
            support(ZoneTarget::AppleMusic, Capability::RepeatMode),
            Support::NotImplemented { .. }
        ));
    }

    #[test]
    fn spotify_library_capabilities_follow_library_routing() {
        assert_eq!(
            support(ZoneTarget::Spotify, Capability::Search),
            Support::Supported
        );
        assert_eq!(
            support(ZoneTarget::Spotify, Capability::PlayByQuery),
            Support::Supported
        );
        assert_eq!(
            support(ZoneTarget::Spotify, Capability::PlayByRef),
            Support::Supported
        );
        assert!(matches!(
            support(ZoneTarget::Spotify, Capability::Browse),
            Support::Supported
        ));
    }

    #[test]
    fn apple_transport_capabilities_wait_for_physical_validation() {
        for capability in [
            Capability::Transport,
            Capability::TransportSkip,
            Capability::Volume,
        ] {
            assert!(matches!(
                support(ZoneTarget::AppleMusic, capability),
                Support::NotImplemented {
                    tracked_by: "#465",
                    ..
                }
            ));
        }
    }
}
