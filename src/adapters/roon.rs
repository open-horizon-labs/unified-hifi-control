//! Roon adapter using rust-roon-api
//!
//! Connects to Roon Core via SOOD discovery and WebSocket protocol.

use anyhow::Result;
use async_trait::async_trait;
use roon_api::{
    browse::{
        Action as BrowseAction, Browse, BrowseOpts, BrowseResult, Item as BrowseItem, ItemHint,
        LoadOpts, LoadResult,
    },
    image::{Args as ImageArgs, Format as ImageFormat, Image, Scale, Scaling},
    status::{self, Status},
    transport::{self, volume, Control, Transport, Zone as RoonZone},
    CoreEvent, Info, Parsed, RoonApi, RoonApiError, Services, Svc,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, Mutex, RwLock, Semaphore};
use tokio::time::timeout_at;
use tokio_util::sync::CancellationToken;

use crate::adapters::handle::{AdapterHandle, RetryConfig};
use crate::adapters::traits::{
    AdapterCommand, AdapterCommandResponse, AdapterContext, AdapterLogic, LibraryAdapter,
    LibrarySearchResult,
};
use crate::bus::{
    runtime::{
        CommandEndpoint, CommandGateway, CommandId, NativeResult, ProjectionCommit,
        ProjectionEntry, ProjectionIngress, ProjectionKind, ProjectionPayload, ProjectionSource,
        ProjectionUpdate, RuntimeCommand,
    },
    BusEvent, Command, NowPlaying as BusNowPlaying, PlaybackState, PrefixedZoneId, SharedBus,
    VolumeControl as BusVolumeControl, Zone as BusZone,
};
use crate::config::get_config_file_path;
use crate::knobs::KnobStore;

const ROON_STATE_FILE: &str = "roon_state.json";

/// Timeout for browse/load requests
const BROWSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Default search result limit
const DEFAULT_SEARCH_LIMIT: usize = 50;

/// Category names returned by Roon search - these are containers, not playable items
const CATEGORY_NAMES: &[&str] = &[
    "Albums",
    "Tracks",
    "Artists",
    "Works",
    "Composers",
    "Genres",
    "Tags",
];

/// Search source - where to search
#[derive(Debug, Clone, Copy, Default)]
pub enum SearchSource {
    #[default]
    Library,
    Tidal,
    Qobuz,
}

/// Play action - what to do with the selected item
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PlayAction {
    #[default]
    Play,
    Queue,
    Radio,
}

impl PlayAction {
    /// Parse from string, defaulting to Play for unknown values
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "queue" | "add" => Self::Queue,
            "radio" | "start_radio" => Self::Radio,
            _ => Self::Play,
        }
    }

    /// Whether an action row's title (from a Roon `hint: action`/`action_list`
    /// item) expresses this intent.
    ///
    /// #545: a real Core does not always offer the generic three-item menu
    /// `tests/mock_servers/roon_core.rs::play_actions()` models ("Play Now",
    /// "Queue", "Start Radio") -- browsing a playlist or album entry can put
    /// a context verb ("Play Playlist", "Play Album", "Shuffle") directly
    /// alongside real content one level up from that menu. The live repro
    /// that drove this fix hit exactly that: the adapter searched for the
    /// literal string "Play Now" among `["Play Playlist", <track titles>]`
    /// and never found it, even though "Play Playlist" was right there and
    /// the Core had already acted on it (see `resolve_item_key`'s docs for
    /// why the play still succeeded despite the error). Matching by prefix/
    /// substring instead of an exact literal is what makes this find the
    /// right row regardless of which noun Roon puts after the verb.
    fn matches_title(self, title: &str) -> bool {
        let lower = title.to_lowercase();
        match self {
            // "Play Now", "Play Album", "Play Playlist", "Play Artist",
            // "Play Track", "Play Radio Station", ... -- every context verb
            // observed or documented for immediate playback starts with
            // "play".
            Self::Play => lower.starts_with("play"),
            Self::Queue => lower.contains("queue"),
            Self::Radio => lower.contains("radio"),
        }
    }

    /// The canonical verb text `tests/mock_servers/roon_core.rs::play_actions()`
    /// models this intent with, for error messages only (not for matching --
    /// see [`Self::matches_title`], which a real Core's context verbs may
    /// not equal literally).
    fn canonical_verb(self) -> &'static str {
        match self {
            Self::Play => "Play Now",
            Self::Queue => "Queue",
            Self::Radio => "Start Radio",
        }
    }
}

/// Pick the action row (from a level's `hint: action`/`action_list` items)
/// that best matches the requested [`PlayAction`].
///
/// Only [`PlayAction::Play`] falls back to the first action-hinted row when
/// none matches by name, per #545's acceptance criteria ("intent mapping or
/// first play-action") -- Roon's vocabulary for "play this" is genuinely
/// varied ("Play Now", "Play Album", "Play Playlist", ...) so a Play request
/// should still succeed against an unrecognised verb. Queue and Radio stay
/// strict: asking to queue or start radio when the Core only offers a Play
/// verb is a real "not available" case, not something to silently downgrade
/// to Play -- see `an_action_the_core_does_not_offer_is_reported_with_what_
/// is_available` (`tests/roon_protocol.rs`), which predates #545 and still
/// expects that refusal.
fn find_action_item(items: &[BrowseItem], action: PlayAction) -> Option<&BrowseItem> {
    let candidates: Vec<&BrowseItem> = items
        .iter()
        .filter(|item| {
            matches!(
                item.hint,
                Some(ItemHint::Action) | Some(ItemHint::ActionList)
            )
        })
        .collect();
    if let Some(found) = candidates
        .iter()
        .find(|item| action.matches_title(&item.title))
    {
        return Some(found);
    }
    if matches!(action, PlayAction::Play) {
        return candidates.first().copied();
    }
    None
}

/// Find the first playable item in a list (hint is Action or ActionList)
fn find_playable_item(items: &[BrowseItem]) -> Option<&BrowseItem> {
    items.iter().find(|item| {
        matches!(
            item.hint,
            Some(ItemHint::Action) | Some(ItemHint::ActionList)
        )
    })
}

/// Check if an item is a category (Albums, Tracks, etc.) rather than playable content
///
/// `pub(crate)` since #396's ship-gate re-review: a real Roon Core's search
/// results include these grouping rows alongside the actual top hit (e.g.
/// searching "kind of blue" returns "Kind of Blue" *and* "Albums (32
/// Results)", "Tracks (34 Results)", ... all with the same `hint: "list"` and
/// a real `item_key`) -- `tests/mock_servers/roon_core.rs`'s search model
/// never included these grouping rows, so no test caught that
/// `crate::mcp::tools::library::handle_search` was minting a ref for them
/// indistinguishably from real content. Resolving one lands in
/// `resolve_item_key`'s "guess the first item" fallback, which can silently
/// play unrelated content -- see that function's call site in
/// `src/mcp/tools/library.rs` for how this is now excluded from minting.
pub(crate) fn is_category(item: &BrowseItem) -> bool {
    CATEGORY_NAMES.contains(&item.title.as_str())
}

/// A second, title-independent signal for the same "this is a grouping row,
/// not addressable content" question `is_category` answers by name match.
///
/// #396's ship-gate re-review verified live (against nuc14, Roon 2.70, a
/// Library search) that `is_category`'s exact title list is correct for
/// Library results, but has **no live evidence for TIDAL or Qobuz sources**,
/// which may group results under different labels `CATEGORY_NAMES` does not
/// contain. This catches that case by a different, source-independent
/// pattern observed in the same live data: every grouping row's subtitle was
/// exactly `"<N> Results"` (`"7 Results"`, `"32 Results"`, `"19 Results"`),
/// while every real hit's subtitle was something else (an artist name, an
/// album title). **This pattern is also unverified for TIDAL/Qobuz** -- it
/// is an inference from one source, not a second confirmed fact — but unlike
/// guessing at a wire *filter* shape (see `LmsAdapter::assert_library_id_exists`'s
/// own history in this same issue), a false miss here only fails to exclude
/// a row `is_category` didn't already catch: it cannot mint a *worse* ref
/// than the title check alone would have, only sometimes a few more of the
/// same already-accepted risk. So it is included as a strictly-additive
/// second layer rather than withheld pending verification.
fn looks_like_a_result_count_grouping(item: &BrowseItem) -> bool {
    let Some(subtitle) = item.subtitle.as_deref() else {
        return false;
    };
    // Case and singular/plural tolerant: verified live only for Library's
    // exact "<N> Results" shape (see #431), and TIDAL/Qobuz may reasonably
    // format the same fact as "<N> result", "<n> RESULTS", or with
    // incidental surrounding whitespace. Still requires the two-token
    // "<count> <word>" shape, so this cannot start matching real content
    // whose subtitle merely contains the word "results" or starts with a
    // digit -- see `result_count_grouping_does_not_swallow_real_subtitles`.
    let Some((count, word)) = subtitle.trim().split_once(' ') else {
        return false;
    };
    count.parse::<u64>().is_ok() && matches!(word.to_lowercase().as_str(), "results" | "result")
}

/// Whether an item is a grouping/category row rather than addressable
/// content, by either signal above. This is what `hifi_search`'s ref-minting
/// (`src/mcp/tools/library.rs`) consults; `is_category` alone remains what
/// `try_navigate_to_playable` uses, unchanged, since this function's second
/// check is new to #396 and was never exercised by that existing path.
pub(crate) fn is_ungrounded_grouping(item: &BrowseItem) -> bool {
    is_category(item) || looks_like_a_result_count_grouping(item)
}

/// Strip "roon:" prefix from zone/output IDs.
/// MCP and aggregator use prefixed IDs (e.g., "roon:zone_123"), but Roon API expects bare IDs.
fn strip_roon_prefix(id: &str) -> &str {
    id.strip_prefix("roon:").unwrap_or(id)
}

/// Require a properly-prefixed Roon zone id and return its bare id.
///
/// Unlike [`strip_roon_prefix`] (used by the pre-existing transport paths,
/// which tolerate a bare id for backward compatibility), the multiroom
/// grouping surface (#509) is new and follows the stricter validation the
/// Music Assistant and LMS adapters already apply to their own multiroom
/// parameters (`musicassistant_player_id`, `lms_player_id`).
fn roon_zone_id(zone_id: &str, parameter: &str) -> Result<String> {
    zone_id
        .strip_prefix("roon:")
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!("{parameter} must be a Roon zone id"))
}

/// [`roon_zone_id`] over a list, rejecting duplicates the same way the
/// Music Assistant and LMS multiroom contracts do.
fn roon_zone_ids(zone_ids: &[String], parameter: &str) -> Result<Vec<String>> {
    let mut ids = Vec::with_capacity(zone_ids.len());
    for zone_id in zone_ids {
        let id = roon_zone_id(zone_id, parameter)?;
        if ids.contains(&id) {
            return Err(anyhow::anyhow!(
                "{parameter} must not contain duplicate zones"
            ));
        }
        ids.push(id);
    }
    Ok(ids)
}

/// Pending image request - stores the oneshot sender to deliver the result
type ImageRequest = oneshot::Sender<Option<ImageData>>;

/// Pending browse request - stores the oneshot sender to deliver the result
type BrowseRequest = oneshot::Sender<Result<BrowseResult>>;

/// Pending load request - stores the oneshot sender to deliver the result
type LoadRequest = oneshot::Sender<Result<LoadResult>>;

// =============================================================================
// Core-reported browse rejections (issue #405)
// =============================================================================
//
// Roon Core answers an unusable browse or load request with a named error
// instead of a result. The pinned fork surfaces that as
// `Parsed::Error(RoonApiError::Browse*)`, carrying both the originating
// `req_id` and the request's `multi_session_key` (see the fork's
// `src/browse.rs:174-188`).
//
// Before #405 the event loop dropped those messages, so the waiting caller sat
// out the full `BROWSE_TIMEOUT` and then reported "Browse request timed out" -
// the same thing an unreachable Core produces. The types below carry the
// Core's actual answer back to the caller, promptly and distinguishably.
//
// Two things about the delivery path, both established by reading the pinned
// fork and neither observable from here, so they are recorded rather than
// assumed:
//
// - Services are tried in order with a `break` on the first that claims a
//   message (fork `src/lib.rs:597-631`), and Transport is tried before Browse.
//   `Transport::parse_msg` gates every push on its own subscription ids, so it
//   returns an empty `Vec` for a browse message and declines to claim it
//   (fork `src/transport.rs:451-540`). If that ever changes, browse errors get
//   swallowed inside the dependency and nothing here can see it.
// - `Browse::parse_msg` matches four literal error names against `msg["name"]`
//   (fork `src/browse.rs:174-188`). A Core that spells one differently yields
//   `Parsed::None`, which the fork drops, and the caller times out as it did
//   before - with this module's tests still green. That half is wire-level and
//   is #408's to pin.

/// Which browse/load rejection Roon Core reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoonBrowseErrorKind {
    /// The `item_key` is not valid in the browse session it was used in: it has
    /// expired, the session was reset, or it was minted somewhere else.
    /// Recoverable - re-acquire the key (search or browse again) and retry.
    ///
    /// The recovery instruction in [`RoonBrowseErrorKind::explain`] is written
    /// for a caller that holds a key - `play_item` and `/roon/browse`. `search`
    /// mints and consumes its own keys internally, so a rejection there reaches
    /// a client that never held one; retrying the search is still the right
    /// move, but a caller-facing surface may want to reword it (#395, #396).
    InvalidItemKey,
    /// The `level` / `pop_levels` in the request do not exist in this session.
    /// Recoverable - browse again from a level the caller still holds.
    InvalidLevels,
    /// The `zone_or_output_id` in the request is unknown to the Core. Not
    /// fixed by retrying: the caller has to name a zone that exists.
    ZoneNotFound,
    /// The Core reported an unexpected failure for this request.
    UnexpectedError,
}

impl RoonBrowseErrorKind {
    /// Stable machine-readable tag for logs and callers.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidItemKey => "invalid_item_key",
            Self::InvalidLevels => "invalid_levels",
            Self::ZoneNotFound => "zone_not_found",
            Self::UnexpectedError => "unexpected_error",
        }
    }

    /// True when the caller can recover by re-acquiring its reference and
    /// retrying immediately, rather than by waiting for the Core.
    pub fn is_recoverable(self) -> bool {
        matches!(self, Self::InvalidItemKey | Self::InvalidLevels)
    }

    /// Human-readable explanation, including the recovery instruction where
    /// there is one. Deliberately never contains the word "timeout" - callers
    /// and operators must be able to tell these apart at a glance.
    fn explain(self) -> &'static str {
        match self {
            Self::InvalidItemKey => {
                "Roon Core rejected the item key: it is no longer valid in this \
                 browse session - search or browse again to get a fresh key"
            }
            Self::InvalidLevels => {
                "Roon Core rejected the browse level: this browse session is not \
                 at that level - browse again from a level you still hold"
            }
            Self::ZoneNotFound => {
                "Roon Core does not recognise the zone or output in this browse \
                 request"
            }
            Self::UnexpectedError => {
                "Roon Core reported an unexpected error for this browse request"
            }
        }
    }
}

/// A browse or load request that Roon Core answered with a rejection.
///
/// Distinct from a timeout: the Core answered, and it answered "no". Returned
/// inside the `anyhow::Error` that `browse`, `load`, `search`, `play_item` and
/// `search_and_play` already return, so no signature changes; recover it with
/// [`RoonBrowseError::from_error`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoonBrowseError {
    /// What the Core rejected.
    pub kind: RoonBrowseErrorKind,
    /// The `multi_session_key` the rejected request ran in, as reported by the
    /// Core. `None` only if the request carried no session key.
    pub session_key: Option<String>,
}

impl RoonBrowseError {
    fn new(kind: RoonBrowseErrorKind, session_key: Option<&str>) -> Self {
        Self {
            kind,
            session_key: session_key.map(str::to_string),
        }
    }

    /// Recover the typed rejection from an error returned by the browse-backed
    /// adapter methods. `None` means the failure was something else - a
    /// timeout, a lost Core, or Browse not being connected - so callers can
    /// keep those classes apart without matching on message text.
    pub fn from_error(err: &anyhow::Error) -> Option<&Self> {
        err.downcast_ref::<Self>()
    }
}

impl std::error::Error for RoonBrowseError {}

impl std::fmt::Display for RoonBrowseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.kind.explain())?;
        if let Some(session_key) = &self.session_key {
            write!(f, " (browse session '{}')", session_key)?;
        }
        Ok(())
    }
}

/// Where a Core rejection ended up. Reported for logging and asserted by the
/// correlation tests; the caller-visible effect is the resolved oneshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorRouting {
    /// Delivered to the pending browse holding this `req_id`.
    Browse,
    /// Delivered to the pending load holding this `req_id`.
    Load,
    /// Delivered to the pending image request holding this `req_id`.
    Image,
    /// The waiter was found and removed, but its receiver was already gone.
    ReceiverGone,
    /// No request with this `req_id` is pending - it was already resolved, or
    /// the caller timed out and cleaned up. Dropping the rejection is correct.
    NoWaiter,
    /// A request with this `req_id` is pending but carries a different session
    /// or image key, so it was left alone to hit its own timeout rather than
    /// being resolved with someone else's rejection.
    KeyMismatch,
}

/// Deliver a Core rejection to a waiting browse or load request.
/// Returns whether the waiter was still listening.
fn deliver_browse_rejection<T>(
    sender: oneshot::Sender<Result<T>>,
    kind: RoonBrowseErrorKind,
    session_key: Option<&str>,
) -> bool {
    sender
        .send(Err(anyhow::Error::new(RoonBrowseError::new(
            kind,
            session_key,
        ))))
        .is_ok()
}

// =============================================================================
// Browse/load result correlation (issue #416)
// =============================================================================
//
// #405 made `req_id` primary for the *rejection* arms and left the success arms
// scanning `pending_browses` / `pending_loads` for the first entry whose session
// key matched. A session key names a browse *session*, not a request - `search()`
// reuses one across six requests and `/roon/browse` lets a caller supply its own -
// so with two requests in flight under one key, first-match-wins handed a caller
// the other's list as a success. #416 applies #405's argument to those arms.
//
// The `req_id` is available here without a fork change, which #405's PR did not
// realise: the fork's event channel is
// `Receiver<(CoreEvent, Option<(serde_json::Value, Parsed)>)>` and for browse
// results it forwards the *raw MOO message* next to the parsed value (fork
// `src/lib.rs:620-630`). That message carries `request_id` - the fork's own
// `Browse::parse_msg` reads `msg["request_id"]` off it to find the session key
// (fork `src/browse.rs:157`). UHC was discarding it with `if let Some((_,
// parsed)) = msg`.

/// The MOO `request_id` of the raw message a [`Parsed`] value was decoded from.
///
/// `None` means the message cannot be correlated - see
/// [`ResultRouting::Uncorrelated`] for what that costs and why guessing is worse.
///
/// Only the string form is accepted, because that is the only form that can reach
/// here: MOO carries the id in a `Request-Id` header, which the fork copies in as a
/// string (fork `src/moo.rs:354-355`), and both `Browse::parse_msg` and
/// `Image::parse_msg` do `msg["request_id"].as_str().unwrap().parse::<usize>()`
/// before producing anything - so a non-string id panics inside the dependency long
/// before it gets here. Accepting a numeric id would be unreachable code pretending
/// to be robustness.
fn moo_request_id(raw_msg: &serde_json::Value) -> Option<usize> {
    raw_msg
        .get("request_id")
        .and_then(|value| value.as_str())
        .and_then(|id| id.parse::<usize>().ok())
}

/// Where a browse/load result ended up. Reported for logging and asserted by the
/// correlation tests; the caller-visible effect is the resolved oneshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultRouting {
    /// Delivered to the request holding this `req_id`.
    Delivered,
    /// The waiter was found and removed, but its receiver was already gone.
    ReceiverGone,
    /// No request with this `req_id` is pending - it was already resolved, or the
    /// caller timed out and cleaned up. Dropping the result is correct.
    NoWaiter,
    /// A request with this `req_id` is pending but carries a different session
    /// key, so it was left alone to hit its own timeout rather than being
    /// resolved with someone else's result.
    KeyMismatch,
    /// The message carried no usable `request_id`, so nothing can say which
    /// request it answers. The waiter is left to its own `BROWSE_TIMEOUT`.
    ///
    /// Unreachable with the pinned fork - a `Parsed::BrowseResult` only exists
    /// because `Browse::parse_msg` already parsed `msg["request_id"]` as a
    /// `usize`, and the fork forwards that same message. It is a variant rather
    /// than an assumption because the alternative, falling back to the
    /// session-key scan, is precisely the silent cross-delivery #416 removes. A
    /// fork that started nulling the raw message for browse - as it already does
    /// for images (fork `src/lib.rs:634-639`) - would then reintroduce the bug
    /// quietly. This makes it a loud 10s timeout with a `warn` instead.
    Uncorrelated,
}

/// Route a browse or load *result* to the exact request waiting on it.
///
/// Same rule as [`RoonState::route_browse_rejection`], for the same reasons, and
/// deliberately the same shape so the two cannot drift:
///
/// - **`req_id` is authoritative.** `Browse::browse`/`Browse::load` return the
///   request id UHC stores as its map key (fork `src/browse.rs:126-154`) and `Moo`
///   allocates those ids from one monotonic per-connection counter (fork
///   `src/moo.rs:124-135`), so an id names exactly one submitted request. It is
///   also unique across both maps, but the type system already prevents a
///   `BrowseResult` from reaching a `pending_loads` waiter, so each kind searches
///   only its own map.
///
///   That the id *in the arriving message* is one of those ids is not a claim
///   about Roon's wire protocol - it is structural. `Browse::parse_msg` returns
///   `Parsed::None` unless `msg["request_id"]` is a key in the fork's own
///   `session_keys` map (fork `src/browse.rs:157-162`), which holds exactly the
///   ids `browse()`/`load()` handed back. So the existence of a
///   `Parsed::BrowseResult` at all proves its id was issued for a browse or a
///   load. Which is why `NoWaiter` needs no further explanation: if UHC's map does
///   not hold that id, UHC removed it - resolved already, or timed out and cleaned
///   up - and dropping the result is right.
///
/// - **The session key is still checked**, because request ids are unique only
///   within one connection: every `Moo` starts its counter at 0 (fork
///   `src/moo.rs:92`). On a mismatch the waiter is left alone and falls back to
///   `BROWSE_TIMEOUT`, because being resolved with another caller's *success* is
///   the worst outcome available here. Keep this check. It costs one string
///   comparison and nothing near this code says request ids are only
///   per-connection unique.
///
///   **What makes it reachable is not the reconnect story #405 attached to it.**
///   That story - a stale entry still holding an id a fresh connection reissues -
///   is hard to reach on this path: `CoreEvent::Lost` clears all three pending
///   maps and drops the Browse service, `run_roon_loop` clears them again on the
///   way out, and even if an entry survived, the colliding `insert` would
///   overwrite it (resolving that caller promptly with "cancelled") so the map
///   would hold the *new* request's key by the time a result arrived. The case
///   that does not depend on a race is **more than one live connection**: the fork
///   keeps its Moos in a `Vec` and will register with every Core that answers
///   discovery, each with its own counter from 0, while UHC's `Registered` arm
///   keeps only the last Core's Browse service. Two Cores on one LAN therefore
///   means two overlapping id spaces on one channel. Not observed - recorded
///   because it is the reason to keep the guard, and it is a better reason than
///   the one it replaces. #405's suggestion of stamping entries with a connection
///   generation would close it properly.
///
/// Resolution is exactly-once: the entry is removed before the send, and every
/// mutation of these maps happens synchronously under the one state `RwLock`, so a
/// caller's timeout cleanup and this routing interleave but never overlap. A
/// `KeyMismatch` deliberately does not remove - that entry belongs to a caller
/// still inside its own timeout, whose cleanup owns it.
fn route_pending_result<T>(
    pending: &mut HashMap<usize, (Option<String>, oneshot::Sender<Result<T>>)>,
    req_id: Option<usize>,
    session_key: Option<&str>,
    result: T,
) -> ResultRouting {
    let Some(req_id) = req_id else {
        return ResultRouting::Uncorrelated;
    };

    let matches = matches!(
        pending.get(&req_id),
        Some((pending_key, _)) if pending_key.as_deref() == session_key
    );
    if matches {
        if let Some((_, sender)) = pending.remove(&req_id) {
            return if sender.send(Ok(result)).is_ok() {
                ResultRouting::Delivered
            } else {
                ResultRouting::ReceiverGone
            };
        }
    }

    if pending.contains_key(&req_id) {
        return ResultRouting::KeyMismatch;
    }

    ResultRouting::NoWaiter
}

/// Log what became of a browse/load result. `KeyMismatch` and `Uncorrelated` are
/// `warn` because each one costs a caller the full `BROWSE_TIMEOUT`, and before
/// #416 both were invisible.
fn log_result_routing(
    routing: ResultRouting,
    what: &str,
    req_id: Option<usize>,
    session_key: Option<&str>,
) {
    match routing {
        ResultRouting::Delivered => tracing::debug!(
            "Roon {} result delivered to req_id {:?} (session {:?})",
            what,
            req_id,
            session_key
        ),
        ResultRouting::ReceiverGone => tracing::debug!(
            "Roon {} result arrived after its caller gave up: req_id {:?} (session {:?})",
            what,
            req_id,
            session_key
        ),
        ResultRouting::NoWaiter => tracing::debug!(
            "Roon {} result with nothing waiting on it (already resolved or timed \
             out): req_id {:?} (session {:?})",
            what,
            req_id,
            session_key
        ),
        ResultRouting::KeyMismatch => tracing::warn!(
            "Roon {} result not routed - req_id {:?} is pending under a different \
             session key, so it was left for its own timeout (result was for \
             session {:?})",
            what,
            req_id,
            session_key
        ),
        ResultRouting::Uncorrelated => tracing::warn!(
            "Roon {} result carried no request id, so it could not be correlated \
             and was dropped; the caller will time out (session {:?})",
            what,
            session_key
        ),
    }
}

/// Image data returned from Roon
#[derive(Debug, Clone)]
pub struct ImageData {
    pub content_type: String,
    pub data: Vec<u8>,
}

/// Get the Roon state file path in the config subdirectory
/// Issue #76: Organize config files into unified-hifi/ subdirectory
fn get_roon_state_path() -> PathBuf {
    get_config_file_path(ROON_STATE_FILE)
}

/// Maximum relative volume step per call (prevents wild jumps)
const MAX_RELATIVE_STEP: f32 = 10.0;

/// Default volume range when output info unavailable
const DEFAULT_VOLUME_MIN: f32 = 0.0;
const DEFAULT_VOLUME_MAX: f32 = 100.0;

// =============================================================================
// SAFETY CRITICAL: Volume range handling
// =============================================================================
//
// Bug (catastrophe): Hardcoded 0-100 range causes dB values like -12 to be
// clamped to 0 (maximum volume), risking equipment damage.
//
// Fix: Use zone's actual volume range (e.g., -64 to 0 dB).
// See tests/volume_safety.rs for regression protection.

/// Clamp value to range (f32 for fractional step support)
#[inline]
pub fn clamp(value: f32, min: f32, max: f32) -> f32 {
    value.max(min).min(max)
}

/// Get volume range from output, with safe defaults
///
/// Returns (min, max) tuple. For dB zones this might be (-64, 0),
/// for percentage zones (0, 100).
pub fn get_volume_range(output: Option<&Output>) -> (f32, f32) {
    let Some(output) = output else {
        return (DEFAULT_VOLUME_MIN, DEFAULT_VOLUME_MAX);
    };

    let Some(ref vol) = output.volume else {
        return (DEFAULT_VOLUME_MIN, DEFAULT_VOLUME_MAX);
    };

    let min = vol.min.unwrap_or(DEFAULT_VOLUME_MIN);
    let max = vol.max.unwrap_or(DEFAULT_VOLUME_MAX);

    (min, max)
}

/// Zone information exposed via API
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Zone {
    pub zone_id: String,
    pub display_name: String,
    pub state: String,
    pub is_next_allowed: bool,
    pub is_previous_allowed: bool,
    pub is_pause_allowed: bool,
    pub is_play_allowed: bool,
    pub now_playing: Option<NowPlaying>,
    pub outputs: Vec<Output>,
}

/// Output information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Output {
    pub output_id: String,
    pub display_name: String,
    pub volume: Option<VolumeInfo>,
    /// Output ids this output can be grouped with, straight from the Core
    /// (`transport::Output::can_group_with_output_ids`). Roon only groups
    /// outputs that share a streaming protocol (RAAT with RAAT, AirPlay with
    /// AirPlay, etc); this is the Core's own compatibility list, so grouping
    /// (#509) refuses upfront against it instead of guessing at protocol
    /// names the wire format never exposes.
    #[serde(default)]
    pub can_group_with_output_ids: Vec<String>,
}

/// Volume information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeInfo {
    pub value: Option<f32>,
    pub min: Option<f32>,
    pub max: Option<f32>,
    pub is_muted: Option<bool>,
    /// Volume step size from Roon API (varies per zone)
    pub step: Option<f32>,
}

/// Now playing information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NowPlaying {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub image_key: Option<String>,
    pub seek_position: Option<i64>,
    pub length: Option<u32>,
}

/// Roon connection status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoonStatus {
    pub connected: bool,
    pub core_name: Option<String>,
    pub core_version: Option<String>,
    pub zone_count: usize,
}

/// Internal state
#[derive(Default)]
struct RoonState {
    connected: bool,
    core_name: Option<String>,
    core_version: Option<String>,
    zones: HashMap<String, Zone>,
    transport: Option<Transport>,
    image: Option<Image>,
    browse: Option<Browse>,
    /// Pending image requests: request_id -> (image_key, oneshot sender)
    pending_images: HashMap<usize, (String, ImageRequest)>,
    /// Pending browse requests: request_id -> (session_key, oneshot sender)
    pending_browses: HashMap<usize, (Option<String>, BrowseRequest)>,
    /// Pending load requests: request_id -> (session_key, oneshot sender)
    pending_loads: HashMap<usize, (Option<String>, LoadRequest)>,
}

async fn clear_roon_runtime_state(state: &Arc<RwLock<RoonState>>) {
    let mut state = state.write().await;
    state.connected = false;
    state.core_name = None;
    state.core_version = None;
    state.zones.clear();
    state.transport = None;
    state.image = None;
    state.browse = None;
    state.pending_images.clear();
    state.pending_browses.clear();
    state.pending_loads.clear();
}

impl RoonState {
    /// Route a Core browse/load rejection to the exact request waiting on it.
    ///
    /// Correlation is by `req_id`, cross-checked against the session key:
    ///
    /// - `req_id` is authoritative. The fork hands back the same request id
    ///   that `Browse::browse`/`Browse::load` returned when the request was
    ///   submitted (fork `src/browse.rs:126-154`, read back off the wire at
    ///   `:157`), and `Moo` allocates request ids from one monotonic
    ///   per-connection counter (fork `src/moo.rs:124-135`). So a `req_id`
    ///   names exactly one submitted request, and `pending_browses` and
    ///   `pending_loads` cannot both hold it. That is strictly better than the
    ///   session-key scan the success arms are forced into: a session key is
    ///   shared by every request in a browse session (`search()` reuses one
    ///   across six requests, and `/roon/browse` lets a caller supply its own),
    ///   so scanning for it cannot say *which* request was refused - nor even
    ///   whether it was a browse or a load.
    ///
    /// - The session key is checked because that counter restarts at 0 on every
    ///   reconnect (fork `src/moo.rs:92`): a rejection arriving on a fresh
    ///   connection can carry a request id that an older, not-yet-timed-out
    ///   entry still occupies. On a mismatch the waiter is left alone and falls
    ///   back to `BROWSE_TIMEOUT` - the pre-#405 behavior - because resolving
    ///   the wrong caller is worse than resolving them late. Do not remove this
    ///   check as redundant: `req_id` uniqueness is per-connection only, and
    ///   nothing near this code says so.
    ///
    /// The pair is not a unique key, and this narrows the reconnect window
    /// rather than closing it. Session keys are not unique either - a caller
    /// that reuses one across a reconnect (`/roon/browse` accepts a
    /// caller-supplied `session_key`) can produce a stale entry and a fresh
    /// request that agree on both. Both callers get an error either way, and
    /// neither is ever told a wrong *success*; today both simply time out. If a
    /// long-lived browse tree is ever built on one session key (#399), stamp the
    /// pending entries with a connection generation instead.
    ///
    /// Resolution is exactly-once because every path removes the entry before
    /// sending *and* every mutation of these maps is synchronous while the one
    /// state `RwLock` is held - so a caller's timeout cleanup and this routing
    /// can interleave but never overlap. A repeated rejection finds nothing; a
    /// caller that timed out first leaves nothing to find. A `KeyMismatch`
    /// deliberately does **not** remove: that entry belongs to a caller still
    /// inside its `BROWSE_TIMEOUT`, whose own cleanup owns it. Removing it here
    /// would be the strand, not the fix.
    ///
    /// Both maps are searched for an entry where *both* the request id and the
    /// session key match before any mismatch is reported. A mismatched entry in
    /// one map must not hide a matching entry in the other - that too is only
    /// reachable through a reconnect id collision, but reporting a mismatch
    /// early would cost a legitimate waiter its prompt answer.
    fn route_browse_rejection(
        &mut self,
        req_id: usize,
        session_key: Option<&str>,
        kind: RoonBrowseErrorKind,
    ) -> ErrorRouting {
        let browse_matches = matches!(
            self.pending_browses.get(&req_id),
            Some((pending_key, _)) if pending_key.as_deref() == session_key
        );
        if browse_matches {
            if let Some((_, sender)) = self.pending_browses.remove(&req_id) {
                return if deliver_browse_rejection(sender, kind, session_key) {
                    ErrorRouting::Browse
                } else {
                    ErrorRouting::ReceiverGone
                };
            }
        }

        let load_matches = matches!(
            self.pending_loads.get(&req_id),
            Some((pending_key, _)) if pending_key.as_deref() == session_key
        );
        if load_matches {
            if let Some((_, sender)) = self.pending_loads.remove(&req_id) {
                return if deliver_browse_rejection(sender, kind, session_key) {
                    ErrorRouting::Load
                } else {
                    ErrorRouting::ReceiverGone
                };
            }
        }

        if self.pending_browses.contains_key(&req_id) || self.pending_loads.contains_key(&req_id) {
            return ErrorRouting::KeyMismatch;
        }

        ErrorRouting::NoWaiter
    }

    /// Route a browse result to the browse request that asked for it (issue #416).
    ///
    /// See [`route_pending_result`] for the correlation rule and why it matches
    /// [`RoonState::route_browse_rejection`]'s.
    fn route_browse_result(
        &mut self,
        req_id: Option<usize>,
        session_key: Option<&str>,
        result: BrowseResult,
    ) -> ResultRouting {
        route_pending_result(&mut self.pending_browses, req_id, session_key, result)
    }

    /// Route a load result to the load request that asked for it (issue #416).
    fn route_load_result(
        &mut self,
        req_id: Option<usize>,
        session_key: Option<&str>,
        result: LoadResult,
    ) -> ResultRouting {
        route_pending_result(&mut self.pending_loads, req_id, session_key, result)
    }

    /// Route a Core image rejection to the request waiting on it.
    ///
    /// Same rule as [`RoonState::route_browse_rejection`], with the image key
    /// standing in for the session key. `get_image` reads `None` as "Image not
    /// found", so the caller gets an answer instead of a 10s timeout.
    ///
    /// Accepted lossiness: `pending_images` senders carry `Option<ImageData>`,
    /// so `ImageNotFound` and `ImageUnexpectedError` both arrive at the caller
    /// as "Image not found". The distinction survives only in the log line the
    /// event loop writes. Widening `ImageRequest` to a `Result` would carry it
    /// through, and is not worth the churn for cover art.
    fn route_image_rejection(&mut self, req_id: usize, image_key: &str) -> ErrorRouting {
        if let Some((pending_key, _)) = self.pending_images.get(&req_id) {
            if pending_key != image_key {
                return ErrorRouting::KeyMismatch;
            }
            if let Some((_, sender)) = self.pending_images.remove(&req_id) {
                return if sender.send(None).is_ok() {
                    ErrorRouting::Image
                } else {
                    ErrorRouting::ReceiverGone
                };
            }
        }

        ErrorRouting::NoWaiter
    }
}

/// One serialized reliable ingress for Roon's authoritative Core callbacks.
///
/// Roon transport commands do not produce a readback response.  The Core sends complete zone
/// callbacks afterwards instead.  A single provider endpoint therefore admits one native command
/// at a time, arms this bridge *before* dispatch, and waits for the matching zone callback before
/// it allows another command through.  That temporal fence is the strongest correlation the
/// protocol exposes: it never invents a synchronous readback, and it never treats an update for a
/// different zone as confirmation.
#[derive(Clone)]
pub struct RoonRuntimeBridge {
    ingress: ProjectionIngress,
    commands: CommandGateway,
    publication: Arc<Mutex<u64>>,
    publication_gate: Arc<Semaphore>,
    pending: Arc<Mutex<Option<PendingRoonCommand>>>,
}

struct PendingRoonCommand {
    target: PrefixedZoneId,
    command_id: CommandId,
    expectation: RoonObservationExpectation,
    observed: oneshot::Sender<()>,
}

#[derive(Clone, Debug)]
enum RoonObservationExpectation {
    Playback(PlaybackState),
    TrackChanged(Option<(String, String, String, Option<String>)>),
    PreviousApplied {
        track: Option<(String, String, String, Option<String>)>,
        seek_position: Option<f64>,
    },
    Volume(f32),
}

impl RoonObservationExpectation {
    fn matches(&self, zone: &BusZone) -> bool {
        match self {
            Self::Playback(expected) => zone.state == *expected,
            Self::TrackChanged(previous) => {
                let observed = roon_track_identity(zone);
                observed.is_some() && &observed != previous
            }
            Self::PreviousApplied {
                track,
                seek_position,
            } => {
                let observed_track = roon_track_identity(zone);
                if observed_track.is_some() && &observed_track != track {
                    return true;
                }

                matches!(
                    (seek_position, zone.now_playing.as_ref().and_then(|np| np.seek_position)),
                    (Some(before), Some(after)) if *before > 2.0 && after <= 2.0
                )
            }
            Self::Volume(expected) => zone
                .volume_control
                .as_ref()
                .is_some_and(|volume| (volume.value - expected).abs() <= 0.01),
        }
    }
}

fn roon_track_identity(zone: &BusZone) -> Option<(String, String, String, Option<String>)> {
    zone.now_playing.as_ref().map(|now_playing| {
        (
            now_playing.title.clone(),
            now_playing.artist.clone(),
            now_playing.album.clone(),
            now_playing.image_key.clone(),
        )
    })
}

impl RoonRuntimeBridge {
    pub fn new(ingress: ProjectionIngress, commands: CommandGateway) -> Self {
        Self {
            ingress,
            commands,
            publication: Arc::new(Mutex::new(0)),
            publication_gate: Arc::new(Semaphore::new(1)),
            pending: Arc::new(Mutex::new(None)),
        }
    }

    fn commands(&self) -> CommandGateway {
        self.commands.clone()
    }

    /// Start associating the next matching Core zone callback with one native command.
    ///
    /// The endpoint serializes calls to this method; refusing a second arm is intentional defence
    /// in depth, because Roon does not attach the originating command id to a callback.
    async fn arm(
        &self,
        target: PrefixedZoneId,
        command_id: CommandId,
        expectation: RoonObservationExpectation,
    ) -> Result<oneshot::Receiver<()>> {
        let (observed, receiver) = oneshot::channel();
        let mut pending = self.pending.lock().await;
        if pending.is_some() {
            anyhow::bail!("Roon command confirmation is already awaiting a Core zone callback")
        }
        *pending = Some(PendingRoonCommand {
            target,
            command_id,
            expectation,
            observed,
        });
        Ok(receiver)
    }

    /// Forget an unobserved command after a native dispatch failure or confirmation timeout.
    async fn disarm(&self, command_id: CommandId) {
        let mut pending = self.pending.lock().await;
        if pending
            .as_ref()
            .is_some_and(|pending| pending.command_id == command_id)
        {
            pending.take();
        }
    }

    /// Commit a full authoritative zone callback, associating it with an armed command only when
    /// the callback is for that exact zone.  The projection actor decides whether the commit is
    /// actually accepted; only a committed projection can resolve the operation.
    async fn publish_zone(&self, zone: BusZone) -> Result<()> {
        let matched = {
            let mut pending = self.pending.lock().await;
            if pending.as_ref().is_some_and(|pending| {
                pending.target.as_str() == zone.zone_id && pending.expectation.matches(&zone)
            }) {
                pending.take()
            } else {
                None
            }
        };
        self.publish(
            ProjectionPayload::Zone(Box::new(zone.clone())),
            format!("zone:{}", zone.zone_id),
            matched.as_ref().map(|pending| pending.command_id),
            matched.map(|pending| pending.observed),
        )
        .await
    }

    async fn publish_removed(&self, zone_id: PrefixedZoneId) -> Result<()> {
        self.publish(
            ProjectionPayload::ZoneRemoved {
                zone_id: zone_id.clone(),
            },
            format!("zone:{zone_id}"),
            None,
            None,
        )
        .await
    }

    async fn publish(
        &self,
        payload: ProjectionPayload,
        key: String,
        caused_by: Option<CommandId>,
        observed: Option<oneshot::Sender<()>>,
    ) -> Result<()> {
        // Allocate and admit in one serialized lane.  Do not hold the data mutex over the await:
        // the semaphore establishes source order while retaining the no-await-under-lock rule.
        let _publication_permit = self
            .publication_gate
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow::anyhow!("Roon reliable publication lane closed"))?;
        let sequence = {
            let mut sequence = self.publication.lock().await;
            *sequence = sequence.saturating_add(1);
            *sequence
        };
        let commit = self
            .ingress
            .submit(ProjectionUpdate {
                source: ProjectionSource {
                    adapter: "roon".to_string(),
                    instance: None,
                    epoch: 0,
                },
                sequence,
                kind: ProjectionKind::Snapshot,
                caused_by,
                entries: vec![ProjectionEntry { key, payload }],
            })
            .await
            .map_err(|_| anyhow::anyhow!("Roon reliable projection ingress stopped"))?;
        if matches!(commit, ProjectionCommit::Committed { .. }) {
            if let Some(observed) = observed {
                if observed.send(()).is_err() {
                    tracing::debug!(
                        "Roon command observer was dropped after its projection committed"
                    );
                }
            }
        }
        Ok(())
    }
}

/// Roon adapter
#[derive(Clone)]
pub struct RoonAdapter {
    state: Arc<RwLock<RoonState>>,
    bus: SharedBus,
    /// Wrapped in RwLock to allow creating fresh token on restart
    shutdown: Arc<RwLock<CancellationToken>>,
    /// Base URL for Roon extension display (e.g., "http://hostname:3000")
    base_url: Arc<RwLock<Option<String>>>,
    /// Whether the adapter has been started
    started: Arc<std::sync::atomic::AtomicBool>,
    /// Knob store for displaying controller count in Roon extension
    knob_store: Option<KnobStore>,
    /// Present only in the composed server.  It routes complete Core callbacks into the
    /// aggregator-owned projection and owns command/callback correlation.
    runtime_bridge: Option<Arc<RoonRuntimeBridge>>,
    /// Where pairing state (`roon_state.json`) is loaded from and saved to.
    ///
    /// Defaults to `get_roon_state_path()`, i.e. the production config directory
    /// (honouring `UHC_CONFIG_DIR`). Overridable via [`Self::with_state_path_for_tests`]
    /// so each test instance gets its own file instead of racing every other
    /// `RoonAdapter` in the test binary over one shared path (issue #554).
    state_path: PathBuf,
}

impl RoonAdapter {
    /// Create a disconnected Roon adapter (stub, used when disabled)
    pub fn new_disconnected(bus: SharedBus) -> Self {
        Self {
            state: Arc::new(RwLock::new(RoonState::default())),
            bus,
            shutdown: Arc::new(RwLock::new(CancellationToken::new())),
            base_url: Arc::new(RwLock::new(None)),
            started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            knob_store: None,
            runtime_bridge: None,
            state_path: get_roon_state_path(),
        }
    }

    /// Create Roon adapter ready to start
    ///
    /// `base_url` is shown in Roon Settings → Extensions (e.g., "http://hostname:3000")
    /// `knob_store` is used to display controller count in Roon extension status
    pub fn new_configured(bus: SharedBus, base_url: String, knob_store: KnobStore) -> Self {
        Self::new_configured_with_runtime(bus, base_url, knob_store, None)
    }

    /// Compose Roon with the private reliable command/projection lanes while keeping every public
    /// HTTP/MCP route exactly as it is.
    pub fn new_configured_with_runtime(
        bus: SharedBus,
        base_url: String,
        knob_store: KnobStore,
        runtime_bridge: Option<Arc<RoonRuntimeBridge>>,
    ) -> Self {
        Self {
            state: Arc::new(RwLock::new(RoonState::default())),
            bus,
            shutdown: Arc::new(RwLock::new(CancellationToken::new())),
            base_url: Arc::new(RwLock::new(Some(base_url))),
            started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            knob_store: Some(knob_store),
            runtime_bridge,
            state_path: get_roon_state_path(),
        }
    }

    /// Test seam (issue #554): point this adapter's pairing-state persistence at a
    /// caller-owned path instead of the shared production config directory.
    ///
    /// `tests/roon_protocol.rs` spawns many `RoonAdapter`s in one test binary; every
    /// one of them registers against its own fake Core and read-modify-writes
    /// `roon_state.json` on pairing. Sharing one file (even a tempdir one) meant two
    /// adapters could interleave load->save and permanently drop each other's token.
    /// Each test now gets its own file via this seam, so production behaviour
    /// (`get_roon_state_path()`, honouring `UHC_CONFIG_DIR`) is unchanged.
    #[doc(hidden)]
    pub fn with_state_path_for_tests(mut self, path: PathBuf) -> Self {
        self.state_path = path;
        self
    }

    /// Create and immediately start Roon adapter (legacy API for compatibility)
    pub async fn new(bus: SharedBus, base_url: String, knob_store: KnobStore) -> Result<Self> {
        let adapter = Self::new_configured(bus, base_url, knob_store);
        adapter.start_internal().await?;
        Ok(adapter)
    }

    /// Start the Roon event loop (internal - use Startable trait)
    async fn start_internal(&self) -> Result<()> {
        use std::sync::atomic::Ordering;

        // Check if already started
        if self.started.swap(true, Ordering::SeqCst) {
            return Ok(()); // Already started
        }

        // Verify configuration
        {
            let url = self.base_url.read().await;
            if url.is_none() {
                self.started.store(false, Ordering::SeqCst);
                return Err(anyhow::anyhow!("Roon base_url not configured"));
            }
        }

        // Create fresh cancellation token for this run (previous token may be cancelled)
        let shutdown = {
            let mut token = self.shutdown.write().await;
            *token = CancellationToken::new();
            token.clone()
        };

        // Create AdapterHandle and spawn run_with_retry
        let handle = AdapterHandle::new(self.clone(), self.bus.clone(), shutdown);
        let config = RetryConfig::new(Duration::from_secs(1), Duration::from_secs(60));

        tokio::spawn(async move {
            if let Err(e) = handle.run_with_retry(config).await {
                tracing::error!("Roon adapter exited with error: {}", e);
            }
        });

        Ok(())
    }

    /// Test seam (issue #408): run the Roon event loop against a Core at a known
    /// address, skipping SOOD multicast discovery.
    ///
    /// **Not for production use** — `AdapterLogic::run` is the production entry
    /// point and always discovers. This exists so `tests/roon_protocol.rs` can
    /// drive the *real* event loop (including the pending-browse/load correlation
    /// maps) against `tests/mock_servers/roon_core.rs`. Driving the real loop is
    /// the whole point: a test that reimplemented the loop would assert nothing
    /// about this file.
    ///
    /// SOOD-based discovery against a fake was rejected deliberately: it needs UDP
    /// multicast, which is process-global, cannot be scoped to one test, and is why
    /// this repo's `/hqp/discover` tests already pass in CI while failing on a
    /// developer machine.
    ///
    /// Returns when the Core is lost or `stop()` is called.
    #[doc(hidden)]
    pub async fn run_event_loop_against_core_for_tests(
        &self,
        ip: std::net::IpAddr,
        port: &str,
    ) -> Result<()> {
        let base_url = self
            .base_url
            .read()
            .await
            .clone()
            .unwrap_or_else(|| "http://test.invalid".to_string());
        let shutdown = self.shutdown.read().await.clone();

        run_roon_loop(
            self.state.clone(),
            self.bus.clone(),
            base_url,
            shutdown,
            self.knob_store.clone(),
            self.runtime_bridge.clone(),
            CoreConnect::Direct {
                ip,
                port: port.to_string(),
            },
            self.state_path.clone(),
        )
        .await
    }

    /// Stop the Roon adapter (internal - use Startable trait)
    async fn stop_internal(&self) {
        use std::sync::atomic::Ordering;

        // Cancel background tasks
        self.shutdown.read().await.cancel();

        // The discovery/event-loop task may take time to observe cancellation. Clear the
        // user-visible runtime cache synchronously so an explicit disable cannot continue to
        // report a connected Core and stale zones during that shutdown window.
        clear_roon_runtime_state(&self.state).await;

        // Reset started flag so we can restart later
        self.started.store(false, Ordering::SeqCst);

        tracing::info!("Roon adapter stopped");
    }

    /// Check if adapter is configured (has base_url)
    async fn is_configured(&self) -> bool {
        self.base_url.read().await.is_some()
    }

    /// Get connection status
    pub async fn get_status(&self) -> RoonStatus {
        let state = self.state.read().await;
        RoonStatus {
            connected: state.connected,
            core_name: state.core_name.clone(),
            core_version: state.core_version.clone(),
            zone_count: state.zones.len(),
        }
    }

    /// Get all zones
    pub async fn get_zones(&self) -> Vec<Zone> {
        let state = self.state.read().await;
        state.zones.values().cloned().collect()
    }

    /// Get specific zone
    pub async fn get_zone(&self, zone_id: &str) -> Option<Zone> {
        let zone_id = strip_roon_prefix(zone_id);
        let state = self.state.read().await;
        state.zones.get(zone_id).cloned()
    }

    /// Control playback
    pub async fn control(&self, zone_id: &str, action: &str) -> Result<()> {
        let zone_id = strip_roon_prefix(zone_id);

        // Clone transport while holding lock, then release before await
        let transport = {
            let state = self.state.read().await;
            state
                .transport
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Not connected to Roon"))?
        };

        let control = match action {
            "play" => Control::Play,
            "pause" => Control::Pause,
            "play_pause" => Control::PlayPause,
            "stop" => Control::Stop,
            "previous" => Control::Previous,
            "next" => Control::Next,
            _ => return Err(anyhow::anyhow!("Unknown action: {}", action)),
        };

        transport.control(zone_id, &control).await;
        Ok(())
    }

    /// Change volume
    ///
    /// SAFETY CRITICAL: For absolute volume, we must clamp to the output's actual
    /// volume range. dB-based zones (like HQPlayer) use ranges like -64 to 0.
    /// Naively clamping to 0-100 would send -12 dB → 0 (MAX VOLUME), risking
    /// equipment damage. See tests/volume_safety.rs for regression protection.
    pub async fn change_volume(&self, zone_id: &str, value: f32, relative: bool) -> Result<()> {
        let zone_id = strip_roon_prefix(zone_id);

        // Clone transport and gather volume info while holding lock, then release before await
        let (transport, output_id, mode, final_value) = {
            let state = self.state.read().await;
            let transport = state
                .transport
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Not connected to Roon"))?;

            // Look up zone and get first output
            let zone = state
                .zones
                .get(zone_id)
                .ok_or_else(|| anyhow::anyhow!("Zone not found: {}", zone_id))?;
            let output = zone
                .outputs
                .first()
                .ok_or_else(|| anyhow::anyhow!("Zone has no outputs"))?;
            let output_id = output.output_id.clone();

            if relative {
                // Relative volume changes - clamp step size to prevent wild jumps
                let clamped_step = clamp(value, -MAX_RELATIVE_STEP, MAX_RELATIVE_STEP);
                (
                    transport,
                    output_id,
                    volume::ChangeMode::Relative,
                    clamped_step,
                )
            } else {
                // Absolute volume - MUST use output's actual range
                let (min, max) = get_volume_range(Some(output));
                let clamped_value = clamp(value, min, max);

                tracing::debug!(
                    "Volume change: zone={}, output={}, requested={}, clamped={}, range={}..{}",
                    zone_id,
                    output_id,
                    value,
                    clamped_value,
                    min,
                    max
                );

                (
                    transport,
                    output_id,
                    volume::ChangeMode::Absolute,
                    clamped_value,
                )
            }
        };

        // Roon transport API now takes f64 to support fractional dB steps
        transport
            .change_volume(&output_id, &mode, final_value as f64)
            .await;
        Ok(())
    }

    /// Mute/unmute
    pub async fn mute(&self, zone_id: &str, mute: bool) -> Result<()> {
        let zone_id = strip_roon_prefix(zone_id);

        // Clone transport and get output_id while holding lock, then release before await
        let (transport, output_id) = {
            let state = self.state.read().await;
            let transport = state
                .transport
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Not connected to Roon"))?;

            // Look up zone and get first output
            let zone = state
                .zones
                .get(zone_id)
                .ok_or_else(|| anyhow::anyhow!("Zone not found: {}", zone_id))?;
            let output_id = zone
                .outputs
                .first()
                .ok_or_else(|| anyhow::anyhow!("Zone has no outputs"))?
                .output_id
                .clone();

            (transport, output_id)
        };

        let how = if mute {
            volume::Mute::Mute
        } else {
            volume::Mute::Unmute
        };
        transport.mute(&output_id, &how).await;
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Multiroom grouping (#509): `Transport::group_outputs` / `ungroup_outputs`
    // wired to the `multiroom_status` / `multiroom_set_members` /
    // `multiroom_ungroup` contract the Music Assistant adapter already
    // implements (`src/adapters/musicassistant.rs`).
    //
    // Roon groups *outputs*, not zones. Grouping merges every named zone's
    // outputs into a single zone; the Core keeps the leader's zone_id and
    // retires the other zones (their outputs move under the leader's
    // zone_id in the next zone update). This adapter resolves each zone
    // argument to one representative output -- the same first-output
    // resolution `change_volume` (above) already uses for multi-output
    // zones -- and lets the ordinary zone-event pipeline in `run()`
    // (`Parsed::Zones` / `Parsed::ZonesRemoved`, already unconditional for
    // every zone change) publish the resulting `ZoneUpdated` /
    // `ZoneRemoved` bus events. No special-case bus code is needed for
    // grouping: from the bus's perspective a merge is just an ordinary zone
    // update plus zone removal.
    //
    // Because member zone ids do not survive a merge, this adapter reports
    // (and accepts back) *outputs* as members once a group exists: for a
    // zone with more than one output, `multiroom_status` treats the first
    // output as the "leader" and the rest as members, addressed as
    // `roon:<output_id>` rather than a zone id. That is a stable-but-
    // arbitrary UHC display convention (mirroring the same call LMS's
    // adapter documents for its own leaderless sync groups), not a Roon
    // fact: `resolve_roon_output` accepts either a live zone id or a bare
    // output id, so a caller can always name a group member whether or not
    // it currently has its own zone.

    /// Resolve `id` (bare, no "roon:" prefix) to one Roon output: either the
    /// first output of a zone with this id, or -- for a member of an
    /// existing group, whose original zone id was retired by the merge --
    /// the output with this id directly.
    async fn resolve_roon_output(&self, id: &str) -> Result<Output> {
        let state = self.state.read().await;
        if let Some(zone) = state.zones.get(id) {
            return zone
                .outputs
                .first()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Zone has no outputs: {id}"));
        }
        state
            .zones
            .values()
            .find_map(|zone| zone.outputs.iter().find(|o| o.output_id == id).cloned())
            .ok_or_else(|| anyhow::anyhow!("Roon zone or output was not found: {id}"))
    }

    /// Report every Roon zone with more than one output as a group.
    ///
    /// `can_set_members` is always `true`: every Roon output accepts
    /// `group_outputs`/`ungroup_outputs` unconditionally, so there is no
    /// per-group capability to check (unlike Music Assistant's per-leader
    /// `SET_MEMBERS` feature flag).
    pub async fn multiroom_status(&self) -> Result<Value> {
        let state = self.state.read().await;
        let groups: Vec<Value> = state
            .zones
            .values()
            .filter(|zone| zone.outputs.len() > 1)
            .filter_map(|zone| {
                let (_leader_output, member_outputs) = zone.outputs.split_first()?;
                Some(json!({
                    "leader_zone_id": PrefixedZoneId::roon(&zone.zone_id).to_string(),
                    "member_zone_ids": member_outputs
                        .iter()
                        .map(|o| PrefixedZoneId::roon(&o.output_id).to_string())
                        .collect::<Vec<_>>(),
                    "can_set_members": true,
                }))
            })
            .collect();
        Ok(json!({ "groups": groups }))
    }

    /// Group `add_zone_ids` (and any current members of `leader_zone_id`'s
    /// group) together via `group_outputs`, and ungroup `remove_zone_ids` via
    /// `ungroup_outputs`, then return the current `multiroom_status`.
    ///
    /// Roon confirms grouping asynchronously through the ordinary zone
    /// subscription rather than a synchronous RPC reply (the same is true of
    /// `change_volume`/`mute`/`control` above), so -- unlike the Music
    /// Assistant and LMS adapters, whose underlying protocols are
    /// request/response -- the status returned here is a snapshot of
    /// current knowledge, not a guaranteed post-condition. Callers that need
    /// the authoritative outcome should watch the zone bus, exactly as they
    /// already must for every other Roon transport command.
    pub async fn set_group_members(
        &self,
        leader_zone_id: &str,
        add_zone_ids: &[String],
        remove_zone_ids: &[String],
    ) -> Result<Value> {
        if add_zone_ids.is_empty() && remove_zone_ids.is_empty() {
            return Err(anyhow::anyhow!(
                "Roon group membership needs at least one member"
            ));
        }
        let leader_id = roon_zone_id(leader_zone_id, "leader_zone_id")?;
        let add_ids = roon_zone_ids(add_zone_ids, "member_zone_ids_to_add")?;
        let remove_ids = roon_zone_ids(remove_zone_ids, "member_zone_ids_to_remove")?;
        for id in add_ids.iter().chain(remove_ids.iter()) {
            if id == &leader_id {
                return Err(anyhow::anyhow!(
                    "Roon group leader cannot be its own member input"
                ));
            }
        }

        let leader_output = self
            .resolve_roon_output(&leader_id)
            .await
            .map_err(|_| anyhow::anyhow!("Roon group leader was not found"))?;
        let mut add_outputs = Vec::with_capacity(add_ids.len());
        for id in &add_ids {
            add_outputs.push(
                self.resolve_roon_output(id)
                    .await
                    .map_err(|_| anyhow::anyhow!("Roon group member was not found"))?,
            );
        }
        let mut remove_outputs = Vec::with_capacity(remove_ids.len());
        for id in &remove_ids {
            remove_outputs.push(
                self.resolve_roon_output(id)
                    .await
                    .map_err(|_| anyhow::anyhow!("Roon group member was not found"))?,
            );
        }

        // Roon only groups outputs that share a streaming protocol. The Core
        // does not expose a queryable protocol name, but `can_group_with_output_ids`
        // *is* the Core's own compatibility list for the leader's output, so a
        // mismatch here is refused cleanly instead of being sent to the Core
        // to fail (or be silently ignored) in some undocumented way.
        for output in &add_outputs {
            if !leader_output
                .can_group_with_output_ids
                .iter()
                .any(|id| id == &output.output_id)
            {
                return Err(anyhow::anyhow!(
                    "Cannot group '{}' with '{}': they do not share a compatible streaming protocol",
                    output.display_name,
                    leader_output.display_name
                ));
            }
        }

        let transport = {
            let state = self.state.read().await;
            state
                .transport
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Not connected to Roon"))?
        };

        if !add_outputs.is_empty() {
            let mut output_ids: Vec<&str> = vec![leader_output.output_id.as_str()];
            output_ids.extend(add_outputs.iter().map(|o| o.output_id.as_str()));
            transport.group_outputs(output_ids).await;
        }
        for output in &remove_outputs {
            transport
                .ungroup_outputs(vec![output.output_id.as_str()])
                .await;
        }

        self.multiroom_status().await
    }

    /// Ungroup each of `member_zone_ids` via `ungroup_outputs`, then return
    /// the current `multiroom_status`. See [`Self::set_group_members`] for
    /// why the returned status is a snapshot rather than a guaranteed
    /// post-condition.
    pub async fn ungroup_members(&self, member_zone_ids: &[String]) -> Result<Value> {
        let member_ids = roon_zone_ids(member_zone_ids, "member_zone_ids")?;
        if member_ids.is_empty() {
            return Err(anyhow::anyhow!("Roon ungroup needs at least one member"));
        }
        let mut outputs = Vec::with_capacity(member_ids.len());
        for id in &member_ids {
            outputs.push(
                self.resolve_roon_output(id)
                    .await
                    .map_err(|_| anyhow::anyhow!("Roon group member was not found"))?,
            );
        }

        let transport = {
            let state = self.state.read().await;
            state
                .transport
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Not connected to Roon"))?
        };
        for output in &outputs {
            transport
                .ungroup_outputs(vec![output.output_id.as_str()])
                .await;
        }

        self.multiroom_status().await
    }

    /// Get album art image
    pub async fn get_image(
        &self,
        image_key: &str,
        width: Option<u32>,
        height: Option<u32>,
    ) -> Result<ImageData> {
        let (tx, rx) = oneshot::channel();

        // Clone image service while holding lock, then release before await
        let image = {
            let state = self.state.read().await;
            state
                .image
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Image service not available"))?
        };

        // Build scaling args
        let scaling = match (width, height) {
            (Some(w), Some(h)) => Some(Scaling::new(Scale::Fit, w, h)),
            (Some(w), None) => Some(Scaling::new(Scale::Fit, w, w)),
            (None, Some(h)) => Some(Scaling::new(Scale::Fit, h, h)),
            (None, None) => Some(Scaling::new(Scale::Fit, 300, 300)),
        };

        // Request the image (lock not held)
        let args = ImageArgs::new(scaling, Some(ImageFormat::Jpeg));
        let req_id = image.get_image(image_key, args).await;

        let req_id = match req_id {
            Some(id) => {
                // Re-acquire lock to insert pending request
                let mut state = self.state.write().await;
                state.pending_images.insert(id, (image_key.to_string(), tx));
                id
            }
            None => return Err(anyhow::anyhow!("Failed to request image")),
        };

        tracing::debug!("Requested image {} with req_id {}", image_key, req_id);

        // Wait for response with timeout
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), rx).await;

        // Clean up pending request on timeout or cancellation
        if !matches!(result, Ok(Ok(Some(_)))) {
            let mut state = self.state.write().await;
            state.pending_images.remove(&req_id);
        }

        match result {
            Ok(Ok(Some(data))) => Ok(data),
            Ok(Ok(None)) => Err(anyhow::anyhow!("Image not found")),
            Ok(Err(_)) => Err(anyhow::anyhow!("Image request cancelled")),
            Err(_) => Err(anyhow::anyhow!("Image request timed out")),
        }
    }

    // =========================================================================
    // Browse API methods (consolidated from RoonBrowseAdapter)
    // =========================================================================

    /// Check if browse service is connected
    pub async fn is_browse_connected(&self) -> bool {
        let state = self.state.read().await;
        state.connected && state.browse.is_some()
    }

    /// Browse the Roon library hierarchy
    pub async fn browse(&self, mut opts: BrowseOpts) -> Result<BrowseResult> {
        // Require session key to avoid concurrent request collisions
        let session_key = opts
            .multi_session_key
            .clone()
            .ok_or_else(|| anyhow::anyhow!("multi_session_key is required for browse requests"))?;

        if let Some(zone) = opts.zone_or_output_id.as_deref() {
            opts.zone_or_output_id = Some(strip_roon_prefix(zone).to_string());
        }
        let (tx, rx) = oneshot::channel();

        let browse = {
            let state = self.state.read().await;
            state.browse.clone().ok_or_else(|| {
                anyhow::anyhow!("Browse service not available - not connected to Roon")
            })?
        };

        let req_id = browse.browse(&opts).await;

        let req_id = match req_id {
            Some(id) => {
                let mut state = self.state.write().await;
                // This `insert` overwrites any entry already holding `id`, dropping
                // that caller's sender - which resolves it promptly with "Browse
                // request cancelled" instead of leaving it to a 10s timeout. That
                // needs two overlapping request-id spaces to happen at all (see
                // `route_pending_result`), and the outcome is an improvement on what
                // the evicted caller had, so it is left alone. Do not "fix" it by
                // symmetry with `pending_images`, where the same shape is a real
                // defect because there two callers collide whenever they want the
                // same image key.
                state
                    .pending_browses
                    .insert(id, (Some(session_key.clone()), tx));
                id
            }
            None => return Err(anyhow::anyhow!("Failed to initiate browse request")),
        };

        tracing::debug!("Browse request initiated with req_id {}", req_id);

        let result = tokio::time::timeout(BROWSE_TIMEOUT, rx).await;

        if result.is_err() {
            let mut state = self.state.write().await;
            state.pending_browses.remove(&req_id);
        }

        match result {
            Ok(Ok(data)) => data,
            Ok(Err(_)) => Err(anyhow::anyhow!("Browse request cancelled")),
            Err(_) => Err(anyhow::anyhow!("Browse request timed out")),
        }
    }

    /// Load items from the current browse position (for pagination)
    pub async fn load(&self, opts: LoadOpts) -> Result<LoadResult> {
        // Require session key to avoid concurrent request collisions
        let session_key = opts
            .multi_session_key
            .clone()
            .ok_or_else(|| anyhow::anyhow!("multi_session_key is required for load requests"))?;

        let (tx, rx) = oneshot::channel();

        let browse = {
            let state = self.state.read().await;
            state.browse.clone().ok_or_else(|| {
                anyhow::anyhow!("Browse service not available - not connected to Roon")
            })?
        };

        let req_id = browse.load(&opts).await;

        let req_id = match req_id {
            Some(id) => {
                let mut state = self.state.write().await;
                state
                    .pending_loads
                    .insert(id, (Some(session_key.clone()), tx));
                id
            }
            None => return Err(anyhow::anyhow!("Failed to initiate load request")),
        };

        tracing::debug!("Load request initiated with req_id {}", req_id);

        let result = tokio::time::timeout(BROWSE_TIMEOUT, rx).await;

        if result.is_err() {
            let mut state = self.state.write().await;
            state.pending_loads.remove(&req_id);
        }

        match result {
            Ok(Ok(data)) => data,
            Ok(Err(_)) => Err(anyhow::anyhow!("Load request cancelled")),
            Err(_) => Err(anyhow::anyhow!("Load request timed out")),
        }
    }

    /// Search the Roon library, TIDAL, or Qobuz.
    ///
    /// Thin wrapper over [`Self::search_with_session`] that drops the session
    /// key, preserving this method's exact pre-#396 signature and behavior --
    /// `/roon/search` (`src/api/mod.rs`) calls this one, unchanged.
    pub async fn search(
        &self,
        query: &str,
        zone_id: Option<&str>,
        limit: Option<usize>,
        source: SearchSource,
    ) -> Result<Vec<BrowseItem>> {
        self.search_with_session(query, zone_id, limit, source)
            .await
            .map(|(_session_key, items)| items)
    }

    /// [`Self::search`], plus the `multi_session_key` the search minted.
    ///
    /// #396: a returned `item_key`'s scope is that session, not the Core in
    /// general -- see `tests/mock_servers/roon_core.rs`'s `ItemKeyScope` docs
    /// for the third-party evidence this rests on. A ref minted from these
    /// results must record the session key here so [`Self::play_ref`] can
    /// resolve it inside the same session rather than a fresh, unrelated one.
    pub async fn search_with_session(
        &self,
        query: &str,
        zone_id: Option<&str>,
        limit: Option<usize>,
        source: SearchSource,
    ) -> Result<(String, Vec<BrowseItem>)> {
        let session_key = format!(
            "search_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );

        let source_name = match source {
            SearchSource::Library => "Library",
            SearchSource::Tidal => "TIDAL",
            SearchSource::Qobuz => "Qobuz",
        };

        // Step 1: Navigate to root
        let root_opts = BrowseOpts {
            multi_session_key: Some(session_key.clone()),
            zone_or_output_id: zone_id.map(|z| z.to_string()),
            pop_all: true,
            ..Default::default()
        };
        self.browse(root_opts).await?;

        let root_load = LoadOpts {
            multi_session_key: Some(session_key.clone()),
            count: Some(10),
            ..Default::default()
        };
        let root_items = self.load(root_load).await?;

        // Find source item
        let source_item = root_items
            .items
            .iter()
            .find(|item| item.title == source_name)
            .ok_or_else(|| anyhow::anyhow!("{} not found in browse root", source_name))?;

        let source_key = source_item
            .item_key
            .clone()
            .ok_or_else(|| anyhow::anyhow!("{} has no item_key", source_name))?;

        // Step 2: Browse into source
        let source_opts = BrowseOpts {
            multi_session_key: Some(session_key.clone()),
            item_key: Some(source_key),
            zone_or_output_id: zone_id.map(|z| z.to_string()),
            ..Default::default()
        };
        self.browse(source_opts).await?;

        let source_load = LoadOpts {
            multi_session_key: Some(session_key.clone()),
            count: Some(10),
            ..Default::default()
        };
        let source_items = self.load(source_load).await?;

        // Find Search item
        let search_item = source_items
            .items
            .iter()
            .find(|item| item.title == "Search")
            .ok_or_else(|| anyhow::anyhow!("Search not found in {}", source_name))?;

        let search_key = search_item
            .item_key
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Search has no item_key"))?;

        // Step 3: Search with query
        let search_opts = BrowseOpts {
            multi_session_key: Some(session_key.clone()),
            item_key: Some(search_key),
            input: Some(query.to_string()),
            zone_or_output_id: zone_id.map(|z| z.to_string()),
            ..Default::default()
        };
        let search_result = self.browse(search_opts).await?;

        // Step 4: Load search results
        if let Some(list) = &search_result.list {
            if list.count > 0 {
                let load_opts = LoadOpts {
                    multi_session_key: Some(session_key.clone()),
                    count: Some(limit.unwrap_or(DEFAULT_SEARCH_LIMIT)),
                    ..Default::default()
                };
                let load_result = self.load(load_opts).await?;
                return Ok((session_key, load_result.items));
            }
        }

        Ok((session_key, vec![]))
    }

    // =========================================================================
    // hifi_collections (#531): the same browse/load machinery `hifi_search`
    // already drives, exposed as hierarchy walking rather than a query.
    // =========================================================================

    /// One `hifi_collections browse` step: enter `item_key` within
    /// `session_key` (a fresh session and the collection root when both are
    /// `None`), then load one page. Returns the (possibly newly minted)
    /// session key, the page's items, and the level's total count for
    /// `next_offset`.
    ///
    /// Deliberately never combines `pop_all` with `item_key` in the same
    /// request, and never sends `pop_all` when resuming an existing
    /// session -- see [`Self::play_ref`]'s doc comment for the live evidence
    /// that combination hangs a real Core.
    pub async fn browse_collection(
        &self,
        zone_id: &str,
        item_key: Option<&str>,
        session_key: Option<&str>,
        offset: usize,
        limit: usize,
    ) -> Result<(String, Vec<BrowseItem>, usize)> {
        let bare_zone_id = strip_roon_prefix(zone_id);
        let session_key = session_key.map(ToOwned::to_owned).unwrap_or_else(|| {
            format!(
                "collections_{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            )
        });

        let mut opts = BrowseOpts {
            multi_session_key: Some(session_key.clone()),
            zone_or_output_id: Some(bare_zone_id.to_string()),
            ..Default::default()
        };
        match item_key {
            Some(key) => opts.item_key = Some(key.to_string()),
            // Only reset the browse stack when there is nowhere to navigate
            // from -- i.e. a brand new session at the collection root.
            None => opts.pop_all = true,
        }
        self.browse(opts).await?;

        let load = self
            .load(LoadOpts {
                multi_session_key: Some(session_key.clone()),
                offset,
                count: Some(limit),
                ..Default::default()
            })
            .await?;

        // #545: a level must never hand back the very item it was just
        // entered through as one of its own children -- browsing that child
        // would just re-enter the same node, which is how "browse a
        // playlist, see the playlist again as a child of itself" nesting
        // was reported. Whatever upstream Roon behavior produces this
        // (a self-referential header row, a session bug) is not something
        // this adapter can distinguish from here, but the guard is correct
        // either way: nothing can legitimately contain itself.
        let items = match item_key {
            Some(parent_key) => load
                .items
                .into_iter()
                .filter(|item| item.item_key.as_deref() != Some(parent_key))
                .collect(),
            None => load.items,
        };

        Ok((session_key, items, load.list.count))
    }

    /// Peek one level into a navigable (`hint: list`) item to learn whether
    /// Roon also exposes it as directly playable, and whether that peek
    /// finds any real content besides the play action(s) themselves.
    ///
    /// Returns `(playable, has_other_content)`.
    ///
    /// #545: Roon puts play actions one level *below* the browsable item
    /// itself (see `resolve_item_key`'s docs on `enter_item`) -- there is no
    /// hint on the item's own row that says "you can also play this without
    /// navigating in". The only way to know is to look: browse `item_key` in
    /// a disposable session (so this never disturbs the caller's own browse
    /// position -- `browse_collection`'s session may still be paging through
    /// the level this item came from) and check the first few rows of what
    /// comes back.
    ///
    /// Bounded to a small peek (`PEEK_COUNT`) rather than the whole level:
    /// every fixture and the live #545 repro both put the action row(s)
    /// first, so a short prefix answers both questions at a fraction of the
    /// cost of loading the full page, and `list.count` (not the prefix
    /// length) is what `has_other_content` is actually judged against.
    async fn peek_playability(&self, zone_id: &str, item_key: &str) -> Result<(bool, bool)> {
        const PEEK_COUNT: usize = 5;
        let peek_session = format!(
            "peek_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let browse_result = self
            .browse(BrowseOpts {
                multi_session_key: Some(peek_session.clone()),
                item_key: Some(item_key.to_string()),
                zone_or_output_id: Some(zone_id.to_string()),
                ..Default::default()
            })
            .await?;
        if !matches!(browse_result.action, BrowseAction::List) {
            // Not expected for a `hint: list` item -- nothing to peek into.
            return Ok((false, false));
        }
        let loaded = self
            .load(LoadOpts {
                multi_session_key: Some(peek_session),
                count: Some(PEEK_COUNT),
                ..Default::default()
            })
            .await?;
        let action_rows_seen = loaded
            .items
            .iter()
            .filter(|item| {
                matches!(
                    item.hint,
                    Some(ItemHint::Action) | Some(ItemHint::ActionList)
                )
            })
            .count();
        let playable = action_rows_seen > 0;
        let has_other_content = loaded.list.count > action_rows_seen;
        Ok((playable, has_other_content))
    }

    /// Enter a named top-level browse node (`"Playlists"`, ...) in a fresh
    /// session and load one page of its contents in a single call --
    /// `hifi_collections playlists`'s whole job, since Roon exposes playlists
    /// as a browse hierarchy rather than its own protocol feature (see the
    /// capability table's note on this).
    pub async fn browse_named_root_node(
        &self,
        zone_id: &str,
        node_title: &str,
        offset: usize,
        limit: usize,
    ) -> Result<(String, Vec<BrowseItem>, usize)> {
        let bare_zone_id = strip_roon_prefix(zone_id);
        let session_key = format!(
            "collections_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );

        self.browse(BrowseOpts {
            multi_session_key: Some(session_key.clone()),
            zone_or_output_id: Some(bare_zone_id.to_string()),
            pop_all: true,
            ..Default::default()
        })
        .await?;

        let root_items = self
            .load(LoadOpts {
                multi_session_key: Some(session_key.clone()),
                count: Some(50),
                ..Default::default()
            })
            .await?;

        let node = root_items
            .items
            .iter()
            .find(|item| item.title.eq_ignore_ascii_case(node_title))
            .ok_or_else(|| anyhow::anyhow!("{node_title} not found in Roon browse root"))?;
        let node_key = node
            .item_key
            .clone()
            .ok_or_else(|| anyhow::anyhow!("{node_title} has no item_key"))?;

        self.browse(BrowseOpts {
            multi_session_key: Some(session_key.clone()),
            item_key: Some(node_key),
            zone_or_output_id: Some(bare_zone_id.to_string()),
            ..Default::default()
        })
        .await?;

        let items = self
            .load(LoadOpts {
                multi_session_key: Some(session_key.clone()),
                offset,
                count: Some(limit),
                ..Default::default()
            })
            .await?;

        Ok((session_key, items.items, items.list.count))
    }

    /// Search and play the first matching result
    pub async fn search_and_play(
        &self,
        query: &str,
        zone_id: &str,
        source: SearchSource,
        action: PlayAction,
    ) -> Result<String> {
        let session_key = format!(
            "play_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );

        let source_name = match source {
            SearchSource::Library => "Library",
            SearchSource::Tidal => "TIDAL",
            SearchSource::Qobuz => "Qobuz",
        };

        let bare_zone_id = strip_roon_prefix(zone_id);

        // Navigate to root
        self.browse(BrowseOpts {
            multi_session_key: Some(session_key.clone()),
            zone_or_output_id: Some(bare_zone_id.to_string()),
            pop_all: true,
            ..Default::default()
        })
        .await?;

        let root_items = self
            .load(LoadOpts {
                multi_session_key: Some(session_key.clone()),
                count: Some(10),
                ..Default::default()
            })
            .await?;

        // Find source
        let source_item = root_items
            .items
            .iter()
            .find(|item| item.title == source_name)
            .ok_or_else(|| anyhow::anyhow!("{} not found", source_name))?;

        let source_key = source_item
            .item_key
            .clone()
            .ok_or_else(|| anyhow::anyhow!("{} has no item_key", source_name))?;

        // Browse into source
        self.browse(BrowseOpts {
            multi_session_key: Some(session_key.clone()),
            item_key: Some(source_key),
            zone_or_output_id: Some(bare_zone_id.to_string()),
            ..Default::default()
        })
        .await?;

        let source_items = self
            .load(LoadOpts {
                multi_session_key: Some(session_key.clone()),
                count: Some(10),
                ..Default::default()
            })
            .await?;

        // Find Search
        let search_item = source_items
            .items
            .iter()
            .find(|item| item.title == "Search")
            .ok_or_else(|| anyhow::anyhow!("Search not found in {}", source_name))?;

        let search_key = search_item
            .item_key
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Search has no item_key"))?;

        // Search with query
        self.browse(BrowseOpts {
            multi_session_key: Some(session_key.clone()),
            item_key: Some(search_key),
            input: Some(query.to_string()),
            zone_or_output_id: Some(bare_zone_id.to_string()),
            ..Default::default()
        })
        .await?;

        let search_results = self
            .load(LoadOpts {
                multi_session_key: Some(session_key.clone()),
                count: Some(20),
                ..Default::default()
            })
            .await?;

        // Find first playable item
        if let Some(playable) = find_playable_item(&search_results.items) {
            let playable_title = playable.title.clone();
            let playable_key = playable
                .item_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Playable item has no item_key"))?;

            return self
                .execute_play_action(
                    &session_key,
                    bare_zone_id,
                    &playable_title,
                    &playable_key,
                    action,
                )
                .await;
        }

        // Try navigating deeper
        if let Some(result) = self
            .try_navigate_to_playable(&session_key, bare_zone_id, &search_results.items, action)
            .await?
        {
            return Ok(result);
        }

        // Try category fallback
        if let Some(result) = self
            .try_category_playable(&session_key, bare_zone_id, &search_results.items, action)
            .await?
        {
            return Ok(result);
        }

        Err(anyhow::anyhow!("No playable results found for '{}'", query))
    }

    /// Try to navigate into the first non-category item to find playable content
    async fn try_navigate_to_playable(
        &self,
        session_key: &str,
        zone_id: &str,
        items: &[BrowseItem],
        action: PlayAction,
    ) -> Result<Option<String>> {
        let first = match items.first() {
            Some(item) if !is_category(item) && item.item_key.is_some() => item,
            _ => return Ok(None),
        };

        let first_title = first.title.clone();
        let first_key = first
            .item_key
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Item has no key"))?;

        self.browse(BrowseOpts {
            multi_session_key: Some(session_key.to_string()),
            item_key: Some(first_key),
            zone_or_output_id: Some(zone_id.to_string()),
            ..Default::default()
        })
        .await?;

        let inner_items = self
            .load(LoadOpts {
                multi_session_key: Some(session_key.to_string()),
                count: Some(20),
                ..Default::default()
            })
            .await?;

        if let Some(playable) = find_playable_item(&inner_items.items) {
            let play_key = playable
                .item_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Item has no key"))?;
            return Ok(Some(
                self.execute_play_action(session_key, zone_id, &first_title, &play_key, action)
                    .await?,
            ));
        }

        // Try one more level
        if let Some(inner_first) = inner_items.items.first() {
            if matches!(inner_first.hint, Some(ItemHint::List)) {
                if let Some(inner_key) = &inner_first.item_key {
                    self.browse(BrowseOpts {
                        multi_session_key: Some(session_key.to_string()),
                        item_key: Some(inner_key.clone()),
                        zone_or_output_id: Some(zone_id.to_string()),
                        ..Default::default()
                    })
                    .await?;

                    let deeper = self
                        .load(LoadOpts {
                            multi_session_key: Some(session_key.to_string()),
                            count: Some(20),
                            ..Default::default()
                        })
                        .await?;

                    if let Some(playable) = find_playable_item(&deeper.items) {
                        let play_key = playable
                            .item_key
                            .clone()
                            .ok_or_else(|| anyhow::anyhow!("Item has no key"))?;
                        return Ok(Some(
                            self.execute_play_action(
                                session_key,
                                zone_id,
                                &first_title,
                                &play_key,
                                action,
                            )
                            .await?,
                        ));
                    }
                }
            }
        }

        Ok(None)
    }

    /// Try to find playable content in Albums or Tracks category
    async fn try_category_playable(
        &self,
        session_key: &str,
        zone_id: &str,
        items: &[BrowseItem],
        action: PlayAction,
    ) -> Result<Option<String>> {
        let category = items
            .iter()
            .find(|item| item.title == "Albums" || item.title == "Tracks");

        let cat = match category {
            Some(c) if c.item_key.is_some() => c,
            _ => return Ok(None),
        };

        let cat_key = cat
            .item_key
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Category has no key"))?;

        self.browse(BrowseOpts {
            multi_session_key: Some(session_key.to_string()),
            item_key: Some(cat_key),
            zone_or_output_id: Some(zone_id.to_string()),
            ..Default::default()
        })
        .await?;

        let category_items = self
            .load(LoadOpts {
                multi_session_key: Some(session_key.to_string()),
                count: Some(20),
                ..Default::default()
            })
            .await?;

        if let Some(playable) = find_playable_item(&category_items.items) {
            let title = playable.title.clone();
            let key = playable
                .item_key
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Playable item has no item_key"))?;

            return Ok(Some(
                self.execute_play_action(session_key, zone_id, &title, &key, action)
                    .await?,
            ));
        }

        // Try first item in category
        if let Some(first_item) = category_items.items.first() {
            if let Some(first_key) = &first_item.item_key {
                let first_title = first_item.title.clone();

                self.browse(BrowseOpts {
                    multi_session_key: Some(session_key.to_string()),
                    item_key: Some(first_key.clone()),
                    zone_or_output_id: Some(zone_id.to_string()),
                    ..Default::default()
                })
                .await?;

                let deeper_items = self
                    .load(LoadOpts {
                        multi_session_key: Some(session_key.to_string()),
                        count: Some(20),
                        ..Default::default()
                    })
                    .await?;

                if let Some(playable) = find_playable_item(&deeper_items.items) {
                    let key = playable
                        .item_key
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("Item has no key"))?;
                    return Ok(Some(
                        self.execute_play_action(session_key, zone_id, &first_title, &key, action)
                            .await?,
                    ));
                }
            }
        }

        Ok(None)
    }

    /// Play an item by its item_key, in a session minted just for this call.
    ///
    /// **This bets that `item_key`s are portable across `multi_session_key`s.**
    /// That bet is unproven in this repo (zero other callers, no test before
    /// #408) and public evidence points against it -- see
    /// `tests/mock_servers/roon_core.rs`'s `ItemKeyScope` docs. It is kept,
    /// unchanged, only because `/roon/play_item` (`src/api/mod.rs`) is
    /// existing, exposed HTTP surface with its own contract; #396's ref
    /// resolution path is [`Self::play_ref`] below, which re-enters the
    /// session that actually minted the key instead of taking this bet.
    pub async fn play_item(
        &self,
        item_key: &str,
        zone_id: &str,
        action: PlayAction,
    ) -> Result<String> {
        if item_key.is_empty() {
            return Err(anyhow::anyhow!("item_key cannot be empty"));
        }
        if item_key.len() > 500 {
            return Err(anyhow::anyhow!("item_key appears malformed (too long)"));
        }

        let session_key = format!(
            "play_item_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );

        let bare_zone_id = strip_roon_prefix(zone_id);
        self.resolve_item_key(&session_key, item_key, bare_zone_id, action, None)
            .await
    }

    /// Resolve a `hifi_play_ref` (#396): play an item_key **inside the session
    /// that minted it**, rather than a fresh, unrelated one.
    ///
    /// # `pop_all` was here, and it broke this against a real Core
    ///
    /// An earlier version of this method reset the shared session's browse
    /// stack with `pop_all: true` before entering `item_key`, on the theory
    /// (borrowed from `tests/mock_servers/roon_core.rs`'s model, which does
    /// not actually enforce this) that two refs minted by one search sharing
    /// one `multi_session_key` needed that reset to resolve independently.
    /// Verified live against a real Core (nuc14, Roon 2.70) that this is
    /// wrong on both counts:
    ///
    /// 1. **Combining `pop_all: true` with `item_key` in one browse request
    ///    hangs** -- the Core never answers, and the caller waits out the
    ///    full `BROWSE_TIMEOUT`. This is exactly the failure #396 shipped
    ///    with and #401-adjacent live testing caught.
    /// 2. **Sending them as two separate requests does not help**: `pop_all`
    ///    invalidates every item_key minted at the levels it discards, so
    ///    the *very key being resolved* becomes `InvalidItemKey` immediately
    ///    after the reset.
    /// 3. **The reset was solving a problem that does not exist.** Without
    ///    any `pop_all`, resolving one ref from a search, navigating it all
    ///    the way down to its action list, and then resolving a *second*,
    ///    unrelated ref minted by the same search (sharing the same session
    ///    key) still succeeds immediately -- verified live, twice, at
    ///    different navigation depths. Item keys minted by one search
    ///    remain independently resolvable regardless of where else that
    ///    session has since navigated, as long as nothing pops it.
    ///
    /// So this is now `play_item`'s exact resolution logic, unchanged, with
    /// only `multi_session_key` swapped for the caller-supplied minting
    /// session instead of a fresh one.
    ///
    /// `fallback_title` is the ref's own title (`hifi_play_ref`'s `title`
    /// field, e.g. a playlist or album name) -- seeded into the result
    /// message before browsing finds anything more specific, so a caller
    /// that plays a top-level item whose own action fires immediately (see
    /// [`Self::resolve_item_key`]'s docs) still gets a human-readable
    /// message instead of the raw `item_key`.
    pub async fn play_ref(
        &self,
        item_key: &str,
        multi_session_key: &str,
        zone_id: &str,
        action: PlayAction,
        fallback_title: &str,
    ) -> Result<String> {
        if item_key.is_empty() {
            return Err(anyhow::anyhow!("item_key cannot be empty"));
        }
        if item_key.len() > 500 {
            return Err(anyhow::anyhow!("item_key appears malformed (too long)"));
        }

        let bare_zone_id = strip_roon_prefix(zone_id);
        self.resolve_item_key(
            multi_session_key,
            item_key,
            bare_zone_id,
            action,
            Some(fallback_title),
        )
        .await
    }

    /// Browse `item_key` within `session_key` and report what came back:
    /// either the Core already invoked it (see below), or a new list to
    /// search/descend.
    ///
    /// # Browsing an `Action` item invokes it -- it does not open a menu
    ///
    /// Confirmed by `tests/mock_servers/roon_core.rs::handle_browse`'s own
    /// "Invoking an action does not produce a list" case (its comment there
    /// records that nothing in this repo read `BrowseResult::action` before
    /// #545, so this was never caught): a `hint: action` item's browse
    /// response carries `action: none`/`message`, not `action: list`, and
    /// the load-bearing fact is which *response* came back, not the item's
    /// own `hint` -- a `hint: action_list` item behaves the same way once
    /// its own children resolve to a single directly-invokable action.
    /// Checking `browse_result.action` here, instead of unconditionally
    /// calling `load()` afterward the way this method's predecessor did, is
    /// what fixes #545's play-matcher bug: the old code always reloaded and
    /// searched the *current* list for a literal `"Play Now"` -- which,
    /// after an `Action` item's browse, is still whatever list was active
    /// beforehand (the Core pushed nothing new), one level too deep, mixed
    /// with unrelated titles (track names). The play had already happened;
    /// only the bookkeeping afterward was wrong, which is why it "worked"
    /// while also reporting an error.
    async fn enter_item(
        &self,
        session_key: &str,
        zone_id: &str,
        item_key: &str,
    ) -> Result<Option<Vec<BrowseItem>>> {
        let browse_result = self
            .browse(BrowseOpts {
                multi_session_key: Some(session_key.to_string()),
                item_key: Some(item_key.to_string()),
                zone_or_output_id: Some(zone_id.to_string()),
                ..Default::default()
            })
            .await?;
        if !matches!(browse_result.action, BrowseAction::List) {
            return Ok(None);
        }
        let items = self
            .load(LoadOpts {
                multi_session_key: Some(session_key.to_string()),
                count: Some(20),
                ..Default::default()
            })
            .await?;
        Ok(Some(items.items))
    }

    /// Shared body of [`Self::play_item`] and [`Self::play_ref`]: browse into
    /// `item_key` within `session_key`, find (or navigate to) a playable
    /// action, and invoke it.
    ///
    /// Deliberately never sends `pop_all` -- see [`Self::play_ref`]'s doc
    /// comment for why combining it with `item_key`, or even sequencing it
    /// beforehand, breaks against a real Core.
    async fn resolve_item_key(
        &self,
        session_key: &str,
        item_key: &str,
        bare_zone_id: &str,
        action: PlayAction,
        fallback_title: Option<&str>,
    ) -> Result<String> {
        let mut container_title = fallback_title.unwrap_or(item_key).to_string();
        let mut next_key = item_key.to_string();

        // Bounded "no action row here -- descend into the first item and try
        // again" loop, for a plain container that needs one more hop before
        // reaching real content (mirrors `try_navigate_to_playable`'s same
        // one-hop assumption for search). #545's live repro needed zero hops
        // (the playlist's own item_key already led to a mixed action+track
        // level); this stays bounded rather than unbounded purely as a
        // safety margin against an unexpected Core shape looping forever.
        for _ in 0..4 {
            let Some(items) = self
                .enter_item(session_key, bare_zone_id, &next_key)
                .await?
            else {
                // `next_key` itself was directly invokable -- see
                // `enter_item`'s docs. Nothing else to do.
                return Ok(container_title);
            };

            if let Some(matched) = find_action_item(&items, action) {
                let verb = matched.title.clone();
                let key = matched
                    .item_key
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("Action has no item_key"))?;

                match self.enter_item(session_key, bare_zone_id, &key).await? {
                    // `matched` was itself directly invokable (the common
                    // case: a `hint: action` row like "Play Playlist", or a
                    // `hint: action_list` row whose own browse turns out to
                    // invoke rather than list -- Roon does not promise which,
                    // so this is decided by the response either way).
                    None => return Ok(format!("{verb}: {container_title}")),
                    // `matched` was an `action_list` wrapper needing one more
                    // descent (the "double-nested action_list" case: e.g.
                    // "Play Album" opens a menu of "Play Now"/"Queue"/"Start
                    // Radio"). Search that menu for the same intent and
                    // invoke it; do not check its own response further --
                    // this is as deep as any known Roon shape nests.
                    Some(inner_items) => {
                        let Some(inner_matched) = find_action_item(&inner_items, action) else {
                            let available: Vec<_> = inner_items.iter().map(|i| &i.title).collect();
                            return Err(anyhow::anyhow!(
                                "Action '{}' not available. Available: {:?}",
                                action.canonical_verb(),
                                available
                            ));
                        };
                        let inner_verb = inner_matched.title.clone();
                        let inner_key = inner_matched
                            .item_key
                            .clone()
                            .ok_or_else(|| anyhow::anyhow!("Action has no item_key"))?;
                        self.browse(BrowseOpts {
                            multi_session_key: Some(session_key.to_string()),
                            item_key: Some(inner_key),
                            zone_or_output_id: Some(bare_zone_id.to_string()),
                            ..Default::default()
                        })
                        .await?;
                        return Ok(format!("{inner_verb}: {verb}"));
                    }
                }
            }

            // No row at this level directly names the requested intent --
            // before giving up, peek inside any `action_list` wrapper here
            // (e.g. "Play Album" wrapping "Play Now"/"Queue"/"Start Radio",
            // #545's own `album()` fixture shape). Peeking is safe: browsing
            // a `hint: action_list` item never invokes anything by itself
            // (only `hint: action` does -- see `enter_item`'s docs), it just
            // opens the menu, so trying each wrapper in turn has no
            // side effect beyond the one that actually matches.
            for wrapper in items
                .iter()
                .filter(|item| matches!(item.hint, Some(ItemHint::ActionList)))
            {
                let Some(wrapper_key) = wrapper.item_key.clone() else {
                    continue;
                };
                let Some(inner_items) = self
                    .enter_item(session_key, bare_zone_id, &wrapper_key)
                    .await?
                else {
                    continue;
                };
                if let Some(inner_matched) = find_action_item(&inner_items, action) {
                    let inner_verb = inner_matched.title.clone();
                    let inner_key = inner_matched
                        .item_key
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("Action has no item_key"))?;
                    self.browse(BrowseOpts {
                        multi_session_key: Some(session_key.to_string()),
                        item_key: Some(inner_key),
                        zone_or_output_id: Some(bare_zone_id.to_string()),
                        ..Default::default()
                    })
                    .await?;
                    return Ok(format!("{inner_verb}: {}", wrapper.title));
                }
            }

            // This level *does* offer at least one action (directly, or
            // inside a wrapper this just peeked and found no match in),
            // just not one matching the requested intent (e.g. Queue/Radio
            // requested against a level that only offers "Play Playlist")
            // -- that is a real "not available" refusal, not an invitation
            // to descend into a bare `action` row and invoke the wrong
            // thing. See `roon_play_ref_does_not_silently_substitute_
            // queue_for_an_unavailable_action` (`tests/roon_protocol.rs`).
            if items.iter().any(|item| {
                matches!(
                    item.hint,
                    Some(ItemHint::Action) | Some(ItemHint::ActionList)
                )
            }) {
                let available: Vec<_> = items.iter().map(|i| &i.title).collect();
                return Err(anyhow::anyhow!(
                    "Action '{}' not available. Available: {:?}",
                    action.canonical_verb(),
                    available
                ));
            }

            // No action row at this level at all -- a plain container.
            // Descend into the first item and look again.
            match items.first().and_then(|item| item.item_key.clone()) {
                Some(key) => {
                    container_title = items[0].title.clone();
                    next_key = key;
                }
                None => break,
            }
        }

        Err(anyhow::anyhow!(
            "Could not find playable content for item_key '{}'",
            item_key
        ))
    }

    /// Invoke the action row at `item_key` (`item_title` is that row's own
    /// title, e.g. "Play Album"), matching `action`'s intent rather than an
    /// exact literal. Shared by `search_and_play`'s fallbacks below --
    /// `resolve_item_key` above inlines the same `enter_item`+
    /// [`find_action_item`] steps itself, since it also needs to thread a
    /// human-readable container title through that this method's older,
    /// narrower call sites never had.
    async fn execute_play_action(
        &self,
        session_key: &str,
        zone_id: &str,
        item_title: &str,
        item_key: &str,
        action: PlayAction,
    ) -> Result<String> {
        let Some(items) = self.enter_item(session_key, zone_id, item_key).await? else {
            // #545: `item_key` itself was directly invokable -- see
            // `enter_item`'s docs. None of this method's pre-#545 call sites
            // could hit this (they only ever passed an `action_list` item's
            // key, which always produced a menu) but a future caller might.
            return Ok(item_title.to_string());
        };

        let Some(matched) = find_action_item(&items, action) else {
            let available: Vec<_> = items.iter().map(|i| &i.title).collect();
            return Err(anyhow::anyhow!(
                "Action '{}' not available. Available: {:?}",
                action.canonical_verb(),
                available
            ));
        };
        let verb = matched.title.clone();
        let key = matched
            .item_key
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Action has no item_key"))?;

        match self.enter_item(session_key, zone_id, &key).await? {
            None => Ok(format!("{verb}: {item_title}")),
            // Double-nested action_list: `matched` was itself a wrapper
            // (e.g. an inner "Play Album" opening "Play Now"/"Queue"/"Start
            // Radio") -- search that menu for the same intent.
            Some(inner_items) => {
                let Some(inner_matched) = find_action_item(&inner_items, action) else {
                    let available: Vec<_> = inner_items.iter().map(|i| &i.title).collect();
                    return Err(anyhow::anyhow!(
                        "Action '{}' not available. Available: {:?}",
                        action.canonical_verb(),
                        available
                    ));
                };
                let inner_verb = inner_matched.title.clone();
                let inner_key = inner_matched
                    .item_key
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("Action has no item_key"))?;
                self.browse(BrowseOpts {
                    multi_session_key: Some(session_key.to_string()),
                    item_key: Some(inner_key),
                    zone_or_output_id: Some(zone_id.to_string()),
                    ..Default::default()
                })
                .await?;
                Ok(format!("{inner_verb}: {verb}"))
            }
        }
    }
}

/// Content-library surface: the multiroom grouping operations (#509) and
/// the `hifi_collections` browse operations (#531) are both implemented
/// here. Roon's own search/play surfaces are reached through their
/// dedicated paths (`RoonAdapter::search`, `RoonAdapter::play_item`, ...)
/// rather than through this trait's `search`/`play_uri` -- a Roon playable
/// target is an `(item_key, multi_session_key)` pair, not a portable URI
/// (see `RoonRefTarget`'s docs) -- which is exactly why `hifi_search`/
/// `hifi_play`/`hifi_play_ref` already call `RoonAdapter` directly
/// (pre-existing debt, unaffected by this registration). `content` is what
/// lets the provider-neutral MCP registry (`AdapterRegistry::library_content`)
/// dispatch `multiroom_status`, `multiroom_set_members`,
/// `multiroom_ungroup`, `collections_browse` and `collections_playlists` to
/// Roon the same way it already dispatches the multiroom operations to
/// Music Assistant (`src/adapters/musicassistant.rs`).
#[async_trait]
impl LibraryAdapter for RoonAdapter {
    async fn search(&self, _query: &str, _limit: usize) -> Result<Vec<LibrarySearchResult>> {
        Err(anyhow::anyhow!(
            "Roon search is not reachable through the generic library registry; hifi_search \
             calls RoonAdapter directly because a Roon item_key is scoped to the \
             multi_session_key that minted it, not a flat URI"
        ))
    }

    async fn play_uri(&self, _zone_id: &str, _uri: &str) -> Result<String> {
        Err(anyhow::anyhow!(
            "Roon playback is not reachable through the generic library registry; \
             hifi_play/hifi_play_ref call RoonAdapter directly for the same reason as search"
        ))
    }

    /// `operation` is one of `multiroom_status`, `multiroom_set_members`,
    /// `multiroom_ungroup`, `collections_browse` or `collections_playlists`
    /// (never `collections_favorites` -- Roon's favorites capability is not
    /// wired, see `crate::mcp::capabilities`). The collections response is
    /// this adapter's own shape, not `hifi_collections`' wire shape:
    /// `{session_key, items: [{title, subtitle, item_key, navigable}],
    /// next_offset}`. `crate::mcp::tools::collections::handle_roon` mints
    /// the opaque ref (a path for `navigable`, a playable ref otherwise)
    /// pairing `item_key` with `session_key` -- this method's job ends at
    /// handing back an already session-scoped, already grouping-filtered
    /// page, not at deciding what a client may address.
    async fn content(&self, operation: &str, params: &Value) -> Result<Value> {
        match operation {
            "multiroom_status" => self.multiroom_status().await,
            "multiroom_set_members" => {
                let leader = params
                    .get("leader_zone_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        anyhow::anyhow!("Roon group operation requires leader_zone_id")
                    })?;
                let add = params
                    .get("member_zone_ids_to_add")
                    .or_else(|| params.get("member_zone_ids"))
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        anyhow::anyhow!("Roon group operation requires member_zone_ids")
                    })?
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>();
                let remove = params
                    .get("member_zone_ids_to_remove")
                    .and_then(Value::as_array)
                    .map(|members| {
                        members
                            .iter()
                            .filter_map(Value::as_str)
                            .map(ToOwned::to_owned)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                self.set_group_members(leader, &add, &remove).await
            }
            "multiroom_ungroup" => {
                let members = params
                    .get("member_zone_ids")
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow::anyhow!("Roon ungroup requires member_zone_ids"))?
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>();
                self.ungroup_members(&members).await
            }
            "collections_browse" | "collections_playlists" => {
                let zone_id = params
                    .get("zone_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("zone_id is required"))?;
                let limit = params
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(20)
                    .clamp(1, 50) as usize;
                let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
                let (session_key, items, total) = if operation == "collections_browse" {
                    let item_key = params.get("item_key").and_then(Value::as_str);
                    let session_key = params.get("session_key").and_then(Value::as_str);
                    self.browse_collection(zone_id, item_key, session_key, offset, limit)
                        .await?
                } else {
                    self.browse_named_root_node(zone_id, "Playlists", offset, limit)
                        .await?
                };
                let mut mapped: Vec<Value> = Vec::new();
                for item in items {
                    if is_ungrounded_grouping(&item) {
                        continue;
                    }
                    // #545: Header/Action/ActionList rows are the
                    // implementation detail `resolve_item_key` walks to
                    // invoke a play action, not addressable content -- a
                    // real Core mixes them into an otherwise-ordinary level
                    // (e.g. a playlist's own browse: `["Play Playlist",
                    // <track>, <track>, ...]`), and rendering them as plain
                    // browse rows is what put a stray "Play Album"/"Play
                    // Playlist" entry in the list next to real content.
                    if matches!(
                        item.hint,
                        Some(ItemHint::Header)
                            | Some(ItemHint::Action)
                            | Some(ItemHint::ActionList)
                    ) {
                        continue;
                    }
                    let list_hinted = matches!(item.hint, Some(ItemHint::List));
                    let (playable, has_other_content) = if list_hinted {
                        match item.item_key.as_deref() {
                            Some(key) => self
                                .peek_playability(zone_id, key)
                                .await
                                .unwrap_or((false, true)),
                            None => (false, true),
                        }
                    } else {
                        (item.item_key.is_some(), true)
                    };
                    // A pure leaf whose only content one level down is an
                    // action list (a track: "Play Track" and nothing else)
                    // has nothing left to navigate into once that action row
                    // is filtered out above -- classify it as playable only,
                    // not also navigable, so "Open" doesn't land on an empty
                    // level. A container that offers both real content *and*
                    // a play action (an album, a playlist) stays navigable
                    // too.
                    let navigable = list_hinted && (!playable || has_other_content);
                    mapped.push(serde_json::json!({
                        "title": item.title,
                        "subtitle": item.subtitle,
                        "item_key": item.item_key,
                        "navigable": navigable,
                        "playable": playable,
                        // #549: Roon's own image_key, resolved through the
                        // same `RoonAdapter::get_image` now-playing art
                        // already uses -- `hifi_collections` mints an
                        // opaque ref over this rather than handing it to a
                        // client directly.
                        "image_key": item.image_key,
                    }));
                }
                let next_offset = (total > offset + limit).then_some((offset + limit) as u64);
                Ok(serde_json::json!({
                    "session_key": session_key,
                    "items": mapped,
                    "next_offset": next_offset,
                }))
            }
            _ => Err(anyhow::anyhow!(
                "Roon content operation `{operation}` is not supported"
            )),
        }
    }
}

#[async_trait]
impl AdapterLogic for RoonAdapter {
    fn prefix(&self) -> &'static str {
        "roon"
    }

    async fn run(&self, ctx: AdapterContext) -> Result<()> {
        let base_url = {
            let url = self.base_url.read().await;
            url.clone()
                .ok_or_else(|| anyhow::anyhow!("Roon base_url not configured"))?
        };

        // Roon callbacks are the provider's authoritative state observation.  Register one
        // endpoint for transport/volume writes before consuming that callback stream; it waits
        // for a matching full-zone callback rather than manufacturing a readback response.
        let command_shutdown = ctx.shutdown.child_token();
        let command_join = self.runtime_bridge.as_ref().and_then(|bridge| {
            match bridge.commands().register_provider("roon", 16) {
                Ok(endpoint) => {
                    let adapter = self.clone();
                    let bridge = bridge.clone();
                    let shutdown = command_shutdown.clone();
                    Some(tokio::spawn(async move {
                        run_roon_command_endpoint(adapter, endpoint, shutdown, bridge).await;
                    }))
                }
                Err(error) => {
                    tracing::error!(?error, "Roon reliable endpoint registration failed");
                    None
                }
            }
        });

        let result = run_roon_loop(
            self.state.clone(),
            ctx.bus,
            base_url,
            ctx.shutdown.clone(),
            self.knob_store.clone(),
            self.runtime_bridge.clone(),
            CoreConnect::Discovery,
            self.state_path.clone(),
        )
        .await;
        command_shutdown.cancel();
        if let Some(join) = command_join {
            if let Err(error) = join.await {
                tracing::warn!(%error, "Roon reliable command endpoint failed to join");
            }
        }
        clear_roon_runtime_state(&self.state).await;
        result
    }

    async fn handle_command(
        &self,
        zone_id: &str,
        command: AdapterCommand,
    ) -> Result<AdapterCommandResponse> {
        let zone_id = strip_roon_prefix(zone_id);

        let result = match command {
            AdapterCommand::Play => self.control(zone_id, "play").await,
            AdapterCommand::Pause => self.control(zone_id, "pause").await,
            AdapterCommand::PlayPause => self.control(zone_id, "play_pause").await,
            AdapterCommand::Stop => self.control(zone_id, "stop").await,
            AdapterCommand::Next => self.control(zone_id, "next").await,
            AdapterCommand::Previous => self.control(zone_id, "previous").await,
            AdapterCommand::VolumeAbsolute(value) => {
                self.change_volume(zone_id, value as f32, false).await
            }
            AdapterCommand::VolumeRelative(delta) => {
                self.change_volume(zone_id, delta as f32, true).await
            }
            AdapterCommand::Mute(mute) => self.mute(zone_id, mute).await,
            AdapterCommand::SetRepeat(_) | AdapterCommand::SetShuffle(_) => {
                return Ok(AdapterCommandResponse {
                    success: false,
                    error: Some(
                        "Repeat and shuffle are not implemented by the Roon adapter".to_string(),
                    ),
                });
            }
        };

        match result {
            Ok(()) => Ok(AdapterCommandResponse {
                success: true,
                error: None,
            }),
            Err(e) => Ok(AdapterCommandResponse {
                success: false,
                error: Some(e.to_string()),
            }),
        }
    }
}

/// Consume Roon's provider endpoint.  `transport.control` and `change_volume` only establish
/// that a command was sent to the Core; the confirmed result is a subsequent full-zone callback
/// committed through [`RoonRuntimeBridge`].  Roon's callback has no request id, so this worker
/// deliberately holds the provider lane until its matching observation arrives or times out.
async fn run_roon_command_endpoint(
    adapter: RoonAdapter,
    mut endpoint: CommandEndpoint,
    shutdown: CancellationToken,
    bridge: Arc<RoonRuntimeBridge>,
) {
    loop {
        let work = tokio::select! {
            _ = shutdown.cancelled() => break,
            work = endpoint.recv() => work,
        };
        let Some(work) = work else { break };
        let permit = match work.begin_dispatch() {
            Ok(permit) => permit,
            Err(_) => continue,
        };
        let command_id = permit.id();
        let target = permit.request().target.clone();
        let confirm_by = permit.request().deadlines.confirm_by;
        let command = permit.request().command.clone();
        // Arm before native I/O: the Core is allowed to notify immediately after accepting a
        // command, and arming afterwards would create a real race that looks like a timeout.
        let expectation = match roon_expectation(&adapter, target.raw_id(), &command).await {
            Ok(expectation) => expectation,
            Err(error) => {
                permit.complete_native(NativeResult::Failed(error.to_string()));
                continue;
            }
        };
        let mut observed = match bridge.arm(target.clone(), command_id, expectation).await {
            Ok(observed) => observed,
            Err(error) => {
                permit.complete_native(NativeResult::Failed(error.to_string()));
                continue;
            }
        };
        let native = match command {
            RuntimeCommand::Control(command) => {
                execute_roon_runtime_command(&adapter, target.raw_id(), command).await
            }
            RuntimeCommand::Hqplayer(_) => Err(anyhow::anyhow!(
                "HQPlayer runtime command routed to Roon endpoint"
            )),
        };
        if let Err(error) = native {
            bridge.disarm(command_id).await;
            permit.complete_native(NativeResult::Failed(error.to_string()));
            continue;
        }
        permit.complete_native(NativeResult::Accepted);

        // The command ledger independently transitions this command to Indeterminate at the same
        // deadline.  This await only keeps later provider writes out of Roon's uncorrelatable
        // callback window; it never turns a timeout into a failed native operation.
        match timeout_at(confirm_by, &mut observed).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => tracing::warn!(
                command_id = command_id.get(),
                "Roon command observation waiter closed before its projection committed"
            ),
            Err(_) => {
                bridge.disarm(command_id).await;
                tracing::debug!(
                    command_id = command_id.get(),
                    "Roon command remains indeterminate until a later Core observation arrives"
                );
            }
        }
    }
}

async fn roon_expectation(
    adapter: &RoonAdapter,
    zone_id: &str,
    command: &RuntimeCommand,
) -> Result<RoonObservationExpectation> {
    let zone = adapter
        .get_zone(zone_id)
        .await
        .ok_or_else(|| anyhow::anyhow!("Zone not found: {zone_id}"))?;
    match command {
        RuntimeCommand::Control(Command::Play) => {
            Ok(RoonObservationExpectation::Playback(PlaybackState::Playing))
        }
        RuntimeCommand::Control(Command::Pause) => {
            Ok(RoonObservationExpectation::Playback(PlaybackState::Paused))
        }
        RuntimeCommand::Control(Command::Stop) => {
            Ok(RoonObservationExpectation::Playback(PlaybackState::Stopped))
        }
        RuntimeCommand::Control(Command::PlayPause) => {
            let expected = if PlaybackState::from(zone.state.as_str()) == PlaybackState::Playing {
                PlaybackState::Paused
            } else {
                PlaybackState::Playing
            };
            Ok(RoonObservationExpectation::Playback(expected))
        }
        RuntimeCommand::Control(Command::Next) => {
            let previous = zone.now_playing.map(|now_playing| {
                (
                    now_playing.title,
                    now_playing.artist,
                    now_playing.album,
                    now_playing.image_key,
                )
            });
            Ok(RoonObservationExpectation::TrackChanged(previous))
        }
        RuntimeCommand::Control(Command::Previous) => {
            let track = zone.now_playing.as_ref().map(|now_playing| {
                (
                    now_playing.title.clone(),
                    now_playing.artist.clone(),
                    now_playing.album.clone(),
                    now_playing.image_key.clone(),
                )
            });
            let seek_position = zone
                .now_playing
                .as_ref()
                .and_then(|now_playing| now_playing.seek_position.map(|position| position as f64));
            Ok(RoonObservationExpectation::PreviousApplied {
                track,
                seek_position,
            })
        }
        RuntimeCommand::Control(Command::VolumeAbsolute {
            value,
            output_id: None,
        }) => {
            let output = zone
                .outputs
                .first()
                .ok_or_else(|| anyhow::anyhow!("Zone has no outputs"))?;
            let (min, max) = get_volume_range(Some(output));
            Ok(RoonObservationExpectation::Volume(clamp(*value, min, max)))
        }
        RuntimeCommand::Control(Command::VolumeRelative {
            delta,
            output_id: None,
        }) => {
            let output = zone
                .outputs
                .first()
                .ok_or_else(|| anyhow::anyhow!("Zone has no outputs"))?;
            let volume = output
                .volume
                .as_ref()
                .and_then(|volume| volume.value)
                .ok_or_else(|| anyhow::anyhow!("Zone volume is unavailable"))?;
            let (min, max) = get_volume_range(Some(output));
            let delta = clamp(*delta, -MAX_RELATIVE_STEP, MAX_RELATIVE_STEP);
            Ok(RoonObservationExpectation::Volume(clamp(
                volume + delta,
                min,
                max,
            )))
        }
        RuntimeCommand::Control(_) | RuntimeCommand::Hqplayer(_) => {
            anyhow::bail!("Roon command has no authoritative observation predicate")
        }
    }
}

async fn execute_roon_runtime_command(
    adapter: &RoonAdapter,
    zone_id: &str,
    command: Command,
) -> Result<()> {
    match command {
        Command::Play => adapter.control(zone_id, "play").await,
        Command::Pause => adapter.control(zone_id, "pause").await,
        Command::PlayPause => adapter.control(zone_id, "play_pause").await,
        Command::Stop => adapter.control(zone_id, "stop").await,
        Command::Next => adapter.control(zone_id, "next").await,
        Command::Previous => adapter.control(zone_id, "previous").await,
        Command::VolumeAbsolute {
            value,
            output_id: None,
        } => adapter.change_volume(zone_id, value, false).await,
        Command::VolumeRelative {
            delta,
            output_id: None,
        } => adapter.change_volume(zone_id, delta, true).await,
        Command::Mute { .. }
        | Command::MuteToggle { .. }
        | Command::Seek { .. }
        | Command::SeekRelative { .. }
        | Command::Shuffle { .. }
        | Command::Repeat { .. }
        | Command::VolumeAbsolute { .. }
        | Command::VolumeRelative { .. } => anyhow::bail!(
            "Roon command was not resolved to a supported native operation before dispatch"
        ),
    }
}

/// Convert Roon zone to our Zone struct
fn convert_zone(roon_zone: &RoonZone) -> Zone {
    let now_playing = roon_zone.now_playing.as_ref().map(|np| NowPlaying {
        title: np.three_line.line1.clone(),
        artist: np.three_line.line2.clone(),
        album: np.three_line.line3.clone(),
        image_key: np.image_key.clone(),
        seek_position: np.seek_position,
        length: np.length,
    });

    // Log output volume info for debugging
    for o in &roon_zone.outputs {
        tracing::debug!(
            "Zone '{}' output '{}': volume={:?}",
            roon_zone.display_name,
            o.display_name,
            o.volume
                .as_ref()
                .map(|v| format!("value={:?} min={:?} max={:?}", v.value, v.min, v.max))
        );
    }

    let outputs = roon_zone
        .outputs
        .iter()
        .map(|o| Output {
            output_id: o.output_id.clone(),
            display_name: o.display_name.clone(),
            volume: o.volume.as_ref().map(|v| VolumeInfo {
                value: v.value,
                min: v.min,
                max: v.max,
                is_muted: v.is_muted,
                step: v.step,
            }),
            can_group_with_output_ids: o.can_group_with_output_ids.clone(),
        })
        .collect();

    let state_str = match roon_zone.state {
        transport::State::Playing => "playing",
        transport::State::Paused => "paused",
        transport::State::Loading => "loading",
        transport::State::Stopped => "stopped",
    };

    Zone {
        zone_id: roon_zone.zone_id.clone(),
        display_name: roon_zone.display_name.clone(),
        state: state_str.to_string(),
        is_next_allowed: roon_zone.is_next_allowed,
        is_previous_allowed: roon_zone.is_previous_allowed,
        is_pause_allowed: roon_zone.is_pause_allowed,
        is_play_allowed: roon_zone.is_play_allowed,
        now_playing,
        outputs,
    }
}

/// Convert local Zone to bus Zone for ZoneDiscovered event
fn roon_zone_to_bus_zone(zone: &Zone) -> BusZone {
    // Get volume from first output (if available)
    // Use prefixed output_id for consistent aggregator matching
    let volume_control = zone.outputs.first().and_then(|o| {
        o.volume.as_ref().map(|v| {
            // Use get_volume_range for consistent defaults with change_volume
            let (default_min, default_max) = get_volume_range(Some(o));
            let min = v.min.unwrap_or(default_min);
            let max = v.max.unwrap_or(default_max);
            // Default to min (safest - for dB zones 0=max, for percent zones 0=min)
            let value = v.value.unwrap_or(min);
            // Infer scale from range: if max <= 0, it's dB; otherwise percentage
            let scale = if max <= 0.0 {
                crate::bus::VolumeScale::Decibel
            } else {
                crate::bus::VolumeScale::Percentage
            };
            BusVolumeControl {
                value,
                min,
                max,
                step: v.step.unwrap_or(1.0),
                is_muted: v.is_muted.unwrap_or(false),
                scale,
                output_id: Some(format!("roon:{}", o.output_id)),
            }
        })
    });

    let now_playing = zone.now_playing.as_ref().map(|np| BusNowPlaying {
        title: np.title.clone(),
        artist: np.artist.clone(),
        album: np.album.clone(),
        image_key: np.image_key.clone(),
        seek_position: np.seek_position.map(|p| p as f64),
        duration: np.length.map(|l| l as f64),
        metadata: None,
        repeat_mode: None,
        shuffle: None,
    });

    BusZone {
        zone_id: format!("roon:{}", zone.zone_id),
        zone_name: zone.display_name.clone(),
        state: PlaybackState::from(zone.state.as_str()),
        volume_control,
        now_playing,
        source: "roon".to_string(),
        is_controllable: true,
        is_seekable: zone.now_playing.is_some(),
        last_updated: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        is_play_allowed: zone.is_play_allowed,
        is_pause_allowed: zone.is_pause_allowed,
        is_next_allowed: zone.is_next_allowed,
        is_previous_allowed: zone.is_previous_allowed,
    }
}

/// How the event loop obtains its connection to a Roon Core.
///
/// Production always uses [`CoreConnect::Discovery`]; `Direct` exists only for
/// `RoonAdapter::run_event_loop_against_core_for_tests` (issue #408).
enum CoreConnect {
    /// SOOD multicast discovery, then WebSocket to whatever answers.
    Discovery,
    /// WebSocket straight to a known address, no discovery.
    Direct { ip: std::net::IpAddr, port: String },
}

/// Main Roon event loop
#[allow(clippy::too_many_arguments)]
async fn run_roon_loop(
    state: Arc<RwLock<RoonState>>,
    bus: SharedBus,
    base_url: String,
    shutdown: CancellationToken,
    knob_store: Option<KnobStore>,
    runtime_bridge: Option<Arc<RoonRuntimeBridge>>,
    connect: CoreConnect,
    state_path: PathBuf,
) -> Result<()> {
    tracing::info!("Starting Roon discovery...");

    // Flag to signal that the loop needs to restart (e.g., core lost, channel closed)
    let restart_needed = Arc::new(AtomicBool::new(false));

    // Ensure config subdirectory exists for state persistence
    // Issue #76: State files now go into unified-hifi/ subdirectory
    // Issue #554: path is caller-supplied (production default: get_roon_state_path()),
    // so concurrent tests can each own a private file instead of racing over one.
    if let Some(parent) = state_path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
            tracing::info!("Created config subdirectory: {:?}", parent);
        }
    }
    let state_path_str = state_path.to_string_lossy().to_string();
    tracing::info!("Roon state file: {}", state_path_str);

    // Extension info - Issue #169: Use UHC_VERSION for consistent version display
    // Use same extension ID as Node.js for seamless migration
    let info = Info::new(
        "com.muness.unified-hifi-control".to_string(),
        "Unified Hi-Fi Control",
        env!("UHC_VERSION"),
        Some("Muness Castle"),
        "",
        Some(env!("CARGO_PKG_REPOSITORY")),
    );

    // Create API instance
    let mut roon = RoonApi::new(info);

    // Create Status service - this is what makes extension visible in Roon Settings
    let (svc, status) = Status::new(&roon);

    // Services we want from Roon Core
    let services = vec![
        Services::Transport(Transport::new()),
        Services::Image(Image::new()),
        Services::Browse(Browse::new()),
        Services::Status(status),
    ];

    // Register Status as a provided service (this enables the pairing UI)
    let mut provided: HashMap<String, Svc> = HashMap::new();
    provided.insert(status::SVCNAME.to_owned(), svc);

    // State persistence callback - use proper path
    let state_path_clone = state_path_str.clone();
    let get_roon_state = move || RoonApi::load_roon_state(&state_path_clone);

    // Start discovery (or, for tests, connect straight to a known Core)
    let (mut handles, mut core_rx) = match connect {
        CoreConnect::Discovery => {
            let started = roon
                .start_discovery(Box::new(get_roon_state), provided, Some(services))
                .await
                .ok_or_else(|| anyhow::anyhow!("Failed to start Roon discovery"))?;
            tracing::info!(
                "Roon discovery started, waiting for core (authorize in Roon → Settings → Extensions)..."
            );
            started
        }
        CoreConnect::Direct { ip, ref port } => {
            let connected = roon
                .ws_connect(
                    Box::new(get_roon_state),
                    provided,
                    Some(services),
                    &ip,
                    port,
                )
                .await
                .ok_or_else(|| {
                    anyhow::anyhow!("Failed to connect to Roon Core at {}:{}", ip, port)
                })?;
            tracing::info!("Connected directly to Roon Core at {}:{}", ip, port);
            connected
        }
    };

    // Event processing task
    let state_for_events = state.clone();
    let bus_for_events = bus.clone();
    let state_path_for_events = state_path_str.clone();
    let base_url_for_events = base_url;
    let shutdown_for_events = shutdown.clone();
    let restart_needed_for_events = restart_needed.clone();
    let knob_store_for_events = knob_store;
    let runtime_bridge_for_events = runtime_bridge;
    handles.spawn(async move {
        loop {
            // Use select! to allow cancellation and handle channel close
            // Issue #128: Without this, the loop would spin on channel close
            // causing high CPU and preventing graceful shutdown
            let event_result = tokio::select! {
                _ = shutdown_for_events.cancelled() => {
                    tracing::info!("Roon event handler shutdown requested");
                    break;
                }
                result = core_rx.recv() => result
            };

            let Some((event, msg)) = event_result else {
                // Channel closed - exit gracefully to allow reconnection
                tracing::info!("Roon event channel closed, exiting handler");
                restart_needed_for_events.store(true, std::sync::atomic::Ordering::SeqCst);
                break;
            };

            match event {
                CoreEvent::Registered(mut core, _token) => {
                    let core_name = core.display_name.clone();
                    let core_version = core.display_version.clone();

                    tracing::info!("Roon Core found: {} (version {})", core_name, core_version);

                    // Update status shown in Roon Settings → Extensions
                    // Issue #169: Show version and controller count
                    if let Some(status) = core.get_status() {
                        let knob_count = if let Some(ref store) = knob_store_for_events {
                            store.list().await.len()
                        } else {
                            0
                        };
                        let message = if knob_count > 0 {
                            format!(
                                "v{} • {} controller{} • {}",
                                env!("UHC_VERSION"),
                                knob_count,
                                if knob_count == 1 { "" } else { "s" },
                                base_url_for_events
                            )
                        } else {
                            format!("v{} • {}", env!("UHC_VERSION"), base_url_for_events)
                        };
                        status.set_status(message, false).await;
                    }

                    // Get transport and image services BEFORE acquiring lock
                    let transport = core.get_transport().cloned();
                    let image = core.get_image().cloned();
                    let browse = core.get_browse().cloned();

                    // Subscribe to zones BEFORE acquiring lock (async operation)
                    if let Some(ref t) = transport {
                        t.subscribe_zones().await;
                    }

                    // Now acquire lock and update state synchronously
                    {
                        let mut s = state_for_events.write().await;
                        s.connected = true;
                        s.core_name = Some(core_name.clone());
                        s.core_version = Some(core_version.clone());
                        s.transport = transport;
                        s.image = image.clone();
                        s.browse = browse.clone();
                    }

                    if image.is_some() {
                        tracing::info!("Roon Image service available");
                    }

                    if browse.is_some() {
                        tracing::info!("Roon Browse service available");
                    }

                    // Publish connected event (after lock released)
                    bus_for_events.publish(BusEvent::RoonConnected {
                        core_name: core_name.clone(),
                        version: core_version.clone(),
                    });
                }
                CoreEvent::Lost(mut core) => {
                    let lost_core_name = core.display_name.clone();
                    let lost_core_version = core.display_version.clone();

                    tracing::warn!(
                        "Roon Core lost: {} (version {})",
                        lost_core_name,
                        lost_core_version
                    );

                    // Update status shown in Roon Settings → Extensions
                    if let Some(status) = core.get_status() {
                        status
                            .set_status("Disconnected - searching...".to_string(), true)
                            .await;
                    }

                    // Losing the Core invalidates every zone it supplied. Capture their
                    // prefixed identities before clearing the operational cache, then retire
                    // the canonical projection. A later reconnect commonly reuses the same
                    // zone IDs, so merely clearing local state leaves controllers displaying
                    // controllable ghosts until some unrelated update happens.
                    let removed_zone_ids: Vec<PrefixedZoneId> = state_for_events
                        .read()
                        .await
                        .zones
                        .keys()
                        .map(PrefixedZoneId::roon)
                        .collect();
                    clear_roon_runtime_state(&state_for_events).await;
                    for zone_id in removed_zone_ids {
                        if let Some(bridge) = runtime_bridge_for_events.as_ref() {
                            if let Err(error) = bridge.publish_removed(zone_id).await {
                                tracing::warn!(%error, "Roon Core-loss projection removal failed");
                            }
                        } else {
                            bus_for_events.publish(BusEvent::ZoneRemoved { zone_id });
                        }
                    }

                    // Publish disconnected event
                    bus_for_events.publish(BusEvent::RoonDisconnected);

                    // Signal restart needed and break
                    restart_needed_for_events.store(true, std::sync::atomic::Ordering::SeqCst);
                    break;
                }
                _ => {}
            }

            // Handle parsed messages
            // `raw_msg` is the MOO message the `Parsed` value was decoded from. It
            // carries the `request_id` the browse/load result arms correlate on
            // (issue #416); before them it was discarded here.
            if let Some((raw_msg, parsed)) = msg {
                match parsed {
                    Parsed::RoonState(roon_state) => {
                        // Persist pairing state to data directory
                        if let Err(e) = RoonApi::save_roon_state(&state_path_for_events, roon_state)
                        {
                            tracing::warn!("Failed to save Roon state: {}", e);
                        } else {
                            tracing::debug!("Roon state saved to {}", state_path_for_events);
                        }
                    }
                    Parsed::Zones(zones) => {
                        // Roon sends full zone facts here. In the reliable composition they are
                        // the one projection source; legacy partial BusEvents are deliberately
                        // suppressed so the aggregator cannot merge two independent lanes.
                        let complete_zones = {
                            let mut s = state_for_events.write().await;
                            let mut complete_zones = Vec::new();
                            for zone in zones {
                            tracing::debug!(
                                "Zone update: {} ({}) - now_playing: {:?}",
                                zone.display_name,
                                zone.zone_id,
                                zone.now_playing.as_ref().map(|np| &np.three_line.line1)
                            );
                            let converted = convert_zone(&zone);
                            let is_new = !s.zones.contains_key(&zone.zone_id);
                            let old_zone = s.zones.get(&zone.zone_id).cloned();

                            // Check if zone gained volume_control (old had none, new has some)
                            let old_had_volume = old_zone
                                .as_ref()
                                .and_then(|oz| oz.outputs.first())
                                .and_then(|o| o.volume.as_ref())
                                .is_some();
                            let new_has_volume = converted
                                .outputs
                                .first()
                                .and_then(|o| o.volume.as_ref())
                                .is_some();
                            let gained_volume = !old_had_volume && new_has_volume;

                            if runtime_bridge_for_events.is_none() && (is_new || gained_volume) {
                                // New zone or zone gained volume - emit ZoneDiscovered
                                // This ensures aggregator gets the full zone with volume_control
                                if gained_volume {
                                    tracing::info!(
                                        "Zone '{}' gained volume control, re-emitting ZoneDiscovered",
                                        converted.display_name
                                    );
                                }
                                let bus_zone = roon_zone_to_bus_zone(&converted);
                                bus_for_events.publish(BusEvent::ZoneDiscovered { zone: bus_zone });
                            } else if runtime_bridge_for_events.is_none() {
                                // Existing zone - emit ZoneUpdated
                                // Use prefixed zone_id to match ZoneDiscovered format
                                let prefixed_zone_id = PrefixedZoneId::roon(&converted.zone_id);
                                bus_for_events.publish(BusEvent::ZoneUpdated {
                                    zone_id: prefixed_zone_id.clone(),
                                    display_name: converted.display_name.clone(),
                                    state: converted.state.clone(),
                                });
                            }

                            // Publish now playing changed if present
                            // Use prefixed zone_id to match aggregator's stored format
                            let prefixed_zone_id = PrefixedZoneId::roon(&converted.zone_id);
                            if runtime_bridge_for_events.is_none() {
                                if let Some(ref np) = converted.now_playing {
                                bus_for_events.publish(BusEvent::NowPlayingChanged {
                                    zone_id: prefixed_zone_id.clone(),
                                    title: Some(np.title.clone()),
                                    artist: Some(np.artist.clone()),
                                    album: Some(np.album.clone()),
                                    image_key: np.image_key.clone(),
                                });
                                }
                            }

                            // Publish volume changed for each output with changed volume
                            if runtime_bridge_for_events.is_none() {
                            for output in &converted.outputs {
                                if let Some(ref vol) = output.volume {
                                    let old_vol = old_zone.as_ref().and_then(|oz| {
                                        oz.outputs
                                            .iter()
                                            .find(|o| o.output_id == output.output_id)
                                            .and_then(|o| o.volume.as_ref())
                                    });

                                    // Emit if volume changed or this is a new zone
                                    let vol_changed = old_vol
                                        .map(|ov| {
                                            ov.value != vol.value || ov.is_muted != vol.is_muted
                                        })
                                        .unwrap_or(true);

                                    // Handle VolumeChanged emission safely:
                                    // - vol.value can be None transiently
                                    // - Using unwrap_or(0.0) would set dB zones to max volume, risking damage
                                    // - But we still want to emit mute changes using last known value
                                    if vol_changed {
                                        // Try current value, then last known value from old_vol
                                        let value_to_use =
                                            vol.value.or_else(|| old_vol.and_then(|ov| ov.value));

                                        if let Some(value) = value_to_use {
                                            bus_for_events.publish(BusEvent::VolumeChanged {
                                                output_id: format!("roon:{}", output.output_id),
                                                value,
                                                is_muted: vol.is_muted.unwrap_or(false),
                                            });
                                        }
                                    }
                                }
                            }
                            }

                            s.zones.insert(zone.zone_id.clone(), converted.clone());
                            if runtime_bridge_for_events.is_some() {
                                complete_zones.push(roon_zone_to_bus_zone(&converted));
                            }
                            }
                            complete_zones
                        };
                        if let Some(bridge) = runtime_bridge_for_events.as_ref() {
                            for zone in complete_zones {
                                if let Err(error) = bridge.publish_zone(zone).await {
                                    tracing::warn!(%error, "Roon zone callback could not enter reliable projection");
                                }
                            }
                        }
                    }
                    Parsed::ZonesSeek(zones_seek) => {
                        let complete_zones = {
                            let mut s = state_for_events.write().await;
                            let mut complete_zones = Vec::new();
                            for seek in zones_seek {
                                if let Some(zone) = s.zones.get_mut(&seek.zone_id) {
                                if let Some(np) = &mut zone.now_playing {
                                    np.seek_position = seek.seek_position;

                                    // Publish seek position changed
                                    // Use prefixed zone_id to match aggregator's stored format
                                    if runtime_bridge_for_events.is_none() {
                                    if let Some(pos) = seek.seek_position {
                                        bus_for_events.publish(BusEvent::SeekPositionChanged {
                                            zone_id: PrefixedZoneId::roon(&seek.zone_id),
                                            position: pos,
                                        });
                                    }
                                    } else {
                                        complete_zones.push(roon_zone_to_bus_zone(zone));
                                    }
                                }
                            }
                            }
                            complete_zones
                        };
                        if let Some(bridge) = runtime_bridge_for_events.as_ref() {
                            for zone in complete_zones {
                                if let Err(error) = bridge.publish_zone(zone).await {
                                    tracing::warn!(%error, "Roon seek callback could not enter reliable projection");
                                }
                            }
                        }
                    }
                    Parsed::ZonesRemoved(zone_ids) => {
                        let removed_zones = {
                            let mut s = state_for_events.write().await;
                            let mut removed_zones = Vec::new();
                            for zone_id in zone_ids {
                            tracing::debug!("Zone removed: {}", zone_id);
                            s.zones.remove(&zone_id);

                            // Publish zone removed event
                            // Use prefixed zone_id to match aggregator's stored format
                            let prefixed_zone_id = PrefixedZoneId::roon(&zone_id);
                            if runtime_bridge_for_events.is_none() {
                                bus_for_events.publish(BusEvent::ZoneRemoved {
                                    zone_id: prefixed_zone_id,
                                });
                            } else {
                                removed_zones.push(prefixed_zone_id);
                            }
                            }
                            removed_zones
                        };
                        if let Some(bridge) = runtime_bridge_for_events.as_ref() {
                            for zone_id in removed_zones {
                                if let Err(error) = bridge.publish_removed(zone_id).await {
                                    tracing::warn!(%error, "Roon removal callback could not enter reliable projection");
                                }
                            }
                        }
                    }
                    // Issue #416 checked these two arms and deliberately left them
                    // scanning, because `pending_images` shares the *shape* of the
                    // browse defect without sharing the defect:
                    //
                    // - The `req_id` is not available here. For image responses the
                    //   fork replaces the raw message with `Value::Null` before
                    //   sending (fork `src/lib.rs:634-639`), so unlike the browse
                    //   arms there is nothing to read a `request_id` off. Correcting
                    //   these would need a fork change.
                    // - It cannot cross-deliver anyway. `Image::get_image`
                    //   deduplicates in-flight requests by image key (fork
                    //   `src/image.rs:77-110`): a second request for a key already in
                    //   flight is handed the *first* request's `req_id` rather than
                    //   being sent. So `pending_images` cannot hold two entries with
                    //   one image key, and the scan below can only ever match the
                    //   one request that asked for this key. The correlation key is
                    //   the content's identity here, which is exactly what a session
                    //   key is not.
                    //
                    // What that dedup *does* cost is a lost waiter, not a wrong
                    // result: two concurrent `get_image` calls for one key resolve
                    // to the same `req_id`, so the second `pending_images.insert`
                    // evicts the first caller's sender and that caller gets "Image
                    // request cancelled" while the second gets the image. Different
                    // defect, different fix (fan-out per key), out of #416's scope -
                    // recorded here rather than silently left.
                    Parsed::Jpeg((image_key, data)) => {
                        tracing::debug!(
                            "Received JPEG image: {} ({} bytes)",
                            image_key,
                            data.len()
                        );
                        let mut s = state_for_events.write().await;
                        // Find pending request by matching image_key
                        if let Some(req_id) = s
                            .pending_images
                            .iter()
                            .find(|(_, (key, _))| key == &image_key)
                            .map(|(k, _)| *k)
                        {
                            if let Some((_key, sender)) = s.pending_images.remove(&req_id) {
                                if sender
                                    .send(Some(ImageData {
                                        content_type: "image/jpeg".to_string(),
                                        data,
                                    }))
                                    .is_err()
                                {
                                    tracing::debug!(
                                        "Image request cancelled (receiver dropped): {}",
                                        image_key
                                    );
                                }
                            }
                        }
                    }
                    Parsed::Png((image_key, data)) => {
                        tracing::debug!("Received PNG image: {} ({} bytes)", image_key, data.len());
                        let mut s = state_for_events.write().await;
                        // Find pending request by matching image_key
                        if let Some(req_id) = s
                            .pending_images
                            .iter()
                            .find(|(_, (key, _))| key == &image_key)
                            .map(|(k, _)| *k)
                        {
                            if let Some((_key, sender)) = s.pending_images.remove(&req_id) {
                                if sender
                                    .send(Some(ImageData {
                                        content_type: "image/png".to_string(),
                                        data,
                                    }))
                                    .is_err()
                                {
                                    tracing::debug!(
                                        "Image request cancelled (receiver dropped): {}",
                                        image_key
                                    );
                                }
                            }
                        }
                    }
                    // Issue #416: correlate on the `req_id` in the raw MOO message,
                    // with the session key as the reconnect guard - the same rule
                    // as the Parsed::Error arm below, deliberately. `Parsed`
                    // itself carries no req_id for results, so this arm reads it
                    // off `raw_msg`; scanning for the first pending request whose
                    // session key matched is what silently handed one caller
                    // another's list.
                    Parsed::BrowseResult(result, session_key) => {
                        tracing::debug!(
                            "Roon BrowseResult action={:?}, session_key={:?}",
                            result.action,
                            session_key
                        );
                        let req_id = moo_request_id(&raw_msg);
                        let routing = {
                            let mut s = state_for_events.write().await;
                            s.route_browse_result(req_id, session_key.as_deref(), result)
                        };
                        log_result_routing(routing, "browse", req_id, session_key.as_deref());
                    }
                    Parsed::LoadResult(result, session_key) => {
                        tracing::debug!(
                            "Roon LoadResult {} items, session_key={:?}",
                            result.items.len(),
                            session_key
                        );
                        let req_id = moo_request_id(&raw_msg);
                        let routing = {
                            let mut s = state_for_events.write().await;
                            s.route_load_result(req_id, session_key.as_deref(), result)
                        };
                        log_result_routing(routing, "load", req_id, session_key.as_deref());
                    }
                    // Issue #405: the Core answered with a rejection rather than
                    // a result. Route it to the request that is waiting on it -
                    // dropping it here is what made a stale item key look like
                    // an unreachable Core, ten seconds late.
                    //
                    // Matched exhaustively on purpose: no wildcard, so a fork
                    // bump that adds a variant fails to compile here instead of
                    // silently resuming the swallow this arm exists to end. If
                    // you are here because of that error, route the new variant.
                    Parsed::Error(err) => {
                        let routing = {
                            let mut s = state_for_events.write().await;
                            match &err {
                                RoonApiError::BrowseInvalidItemKey((req_id, session_key)) => s
                                    .route_browse_rejection(
                                        *req_id,
                                        session_key.as_deref(),
                                        RoonBrowseErrorKind::InvalidItemKey,
                                    ),
                                RoonApiError::BrowseInvalidLevels((req_id, session_key)) => s
                                    .route_browse_rejection(
                                        *req_id,
                                        session_key.as_deref(),
                                        RoonBrowseErrorKind::InvalidLevels,
                                    ),
                                RoonApiError::BrowseZoneNotFound((req_id, session_key)) => s
                                    .route_browse_rejection(
                                        *req_id,
                                        session_key.as_deref(),
                                        RoonBrowseErrorKind::ZoneNotFound,
                                    ),
                                RoonApiError::BrowseUnexpectedError((req_id, session_key)) => s
                                    .route_browse_rejection(
                                        *req_id,
                                        session_key.as_deref(),
                                        RoonBrowseErrorKind::UnexpectedError,
                                    ),
                                RoonApiError::ImageNotFound((req_id, image_key))
                                | RoonApiError::ImageUnexpectedError((req_id, image_key)) => {
                                    s.route_image_rejection(*req_id, image_key)
                                }
                            }
                        };

                        match routing {
                            ErrorRouting::NoWaiter => tracing::debug!(
                                "Roon reported an error with nothing waiting on it \
                                 (already resolved or timed out): {}",
                                err
                            ),
                            ErrorRouting::KeyMismatch => tracing::warn!(
                                "Roon error not routed - a pending request holds that \
                                 request id under a different session/image key, so it \
                                 was left for its own timeout: {}",
                                err
                            ),
                            ErrorRouting::ReceiverGone => tracing::debug!(
                                "Roon error arrived after its caller gave up: {}",
                                err
                            ),
                            ErrorRouting::Browse | ErrorRouting::Load | ErrorRouting::Image => {
                                tracing::debug!("Roon error routed to its {:?} request: {}", routing, err)
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    });

    // Wait for handles - abort all when one signals restart needed
    while handles.join_next().await.is_some() {
        // Check if any task signaled restart needed
        if restart_needed.load(std::sync::atomic::Ordering::SeqCst) {
            tracing::info!("Restart signaled, aborting remaining Roon tasks");
            handles.abort_all();
            break;
        }
    }

    // Clear state before returning
    {
        let mut s = state.write().await;
        s.connected = false;
        s.transport = None;
        s.image = None;
        s.browse = None;
        s.zones.clear();
        s.pending_images.clear();
        s.pending_browses.clear();
        s.pending_loads.clear();
    }

    // Check if restart is needed
    if restart_needed.load(std::sync::atomic::Ordering::SeqCst) {
        Err(anyhow::anyhow!("Roon core lost, restart needed"))
    } else {
        Ok(())
    }
}

// Startable trait implementation via macro
crate::impl_startable!(RoonAdapter, "roon", is_configured);

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a test zone with specified volume parameters
    fn make_test_zone(
        output_id: &str,
        volume_value: Option<f32>,
        volume_min: Option<f32>,
        volume_max: Option<f32>,
    ) -> Zone {
        Zone {
            zone_id: "test-zone".to_string(),
            display_name: "Test Zone".to_string(),
            state: "stopped".to_string(),
            is_next_allowed: true,
            is_previous_allowed: true,
            is_pause_allowed: false,
            is_play_allowed: true,
            now_playing: None,
            outputs: vec![Output {
                output_id: output_id.to_string(),
                display_name: "Test Output".to_string(),
                volume: Some(VolumeInfo {
                    value: volume_value,
                    min: volume_min,
                    max: volume_max,
                    is_muted: None,
                    step: None,
                }),
                can_group_with_output_ids: Vec::new(),
            }],
        }
    }

    #[test]
    fn roon_zone_to_bus_zone_db_scale_value_none_defaults_to_min() {
        // dB zone: max <= 0 means dB scale
        let zone = make_test_zone("output-1", None, Some(-80.0), Some(0.0));
        let bus_zone = roon_zone_to_bus_zone(&zone);

        let vc = bus_zone.volume_control.expect("should have volume_control");
        assert_eq!(vc.value, -80.0, "value should default to min for dB zones");
        assert_eq!(vc.min, -80.0);
        assert_eq!(vc.max, 0.0);
        assert_eq!(vc.scale, crate::bus::VolumeScale::Decibel);
        assert_eq!(vc.step, 1.0, "step should default to 1.0");
        assert!(!vc.is_muted, "is_muted should default to false");
        assert_eq!(vc.output_id, Some("roon:output-1".to_string()));
    }

    #[test]
    fn roon_zone_to_bus_zone_percent_scale_value_none_defaults_to_min() {
        // Percentage zone: max > 0 means percentage scale
        let zone = make_test_zone("output-2", None, Some(0.0), Some(100.0));
        let bus_zone = roon_zone_to_bus_zone(&zone);

        let vc = bus_zone.volume_control.expect("should have volume_control");
        assert_eq!(
            vc.value, 0.0,
            "value should default to min for percentage zones"
        );
        assert_eq!(vc.min, 0.0);
        assert_eq!(vc.max, 100.0);
        assert_eq!(vc.scale, crate::bus::VolumeScale::Percentage);
        assert_eq!(vc.step, 1.0, "step should default to 1.0");
        assert!(!vc.is_muted, "is_muted should default to false");
        assert_eq!(vc.output_id, Some("roon:output-2".to_string()));
    }

    #[test]
    fn roon_zone_to_bus_zone_preserves_actual_value() {
        // When value is Some, it should be preserved
        let zone = make_test_zone("output-3", Some(-30.0), Some(-80.0), Some(0.0));
        let bus_zone = roon_zone_to_bus_zone(&zone);

        let vc = bus_zone.volume_control.expect("should have volume_control");
        assert_eq!(vc.value, -30.0, "actual value should be preserved");
    }

    #[test]
    fn roon_zone_to_bus_zone_no_volume_returns_none() {
        // Zone with output but no volume should have volume_control = None
        let zone = Zone {
            zone_id: "test-zone".to_string(),
            display_name: "Test Zone".to_string(),
            state: "stopped".to_string(),
            is_next_allowed: true,
            is_previous_allowed: true,
            is_pause_allowed: false,
            is_play_allowed: true,
            now_playing: None,
            outputs: vec![Output {
                output_id: "output-no-vol".to_string(),
                display_name: "No Volume Output".to_string(),
                volume: None,
                can_group_with_output_ids: Vec::new(),
            }],
        };
        let bus_zone = roon_zone_to_bus_zone(&zone);

        assert!(
            bus_zone.volume_control.is_none(),
            "should be None when output has no volume"
        );
    }

    fn test_bus_zone_with_track(title: &str, seek_position: f64) -> BusZone {
        let mut zone =
            roon_zone_to_bus_zone(&make_test_zone("output", Some(5.0), Some(0.0), Some(100.0)));
        zone.now_playing = Some(BusNowPlaying {
            title: title.to_string(),
            artist: "John Coltrane".to_string(),
            album: "Blue Train".to_string(),
            image_key: Some("cover".to_string()),
            seek_position: Some(seek_position),
            duration: Some(645.0),
            metadata: None,
            repeat_mode: None,
            shuffle: None,
        });
        zone
    }

    #[test]
    fn roon_previous_confirms_same_track_restart_or_track_change() {
        let previous = RoonObservationExpectation::PreviousApplied {
            track: Some((
                "Blue Train".to_string(),
                "John Coltrane".to_string(),
                "Blue Train".to_string(),
                Some("cover".to_string()),
            )),
            seek_position: Some(37.0),
        };

        assert!(previous.matches(&test_bus_zone_with_track("Blue Train", 0.0)));
        assert!(!previous.matches(&test_bus_zone_with_track("Blue Train", 20.0)));
        assert!(previous.matches(&test_bus_zone_with_track("Moment's Notice", 0.0)));
    }

    #[test]
    fn roon_next_still_requires_a_track_identity_change() {
        let next = RoonObservationExpectation::TrackChanged(Some((
            "Blue Train".to_string(),
            "John Coltrane".to_string(),
            "Blue Train".to_string(),
            Some("cover".to_string()),
        )));

        assert!(!next.matches(&test_bus_zone_with_track("Blue Train", 0.0)));
        assert!(next.matches(&test_bus_zone_with_track("Moment's Notice", 0.0)));
    }

    #[tokio::test]
    async fn explicit_stop_clears_cached_roon_connection_state() {
        let adapter = RoonAdapter::new_disconnected(crate::bus::create_bus());
        {
            let mut state = adapter.state.write().await;
            state.connected = true;
            state.core_name = Some("nuc14".to_string());
            state.core_version = Some("test".to_string());
            state.zones.insert(
                "test-zone".to_string(),
                make_test_zone("output", Some(5.0), Some(0.0), Some(100.0)),
            );
        }

        adapter.stop_internal().await;

        let cleared = adapter.state.read().await;
        assert!(!cleared.connected);
        assert!(cleared.core_name.is_none());
        assert!(cleared.core_version.is_none());
        assert!(cleared.zones.is_empty());
    }

    /// A Roon control has no synchronous state readback.  Its confirmation must
    /// therefore wait for the Core's subsequent full-zone callback, and that
    /// callback must name the same zone -- a callback for another zone cannot
    /// accidentally confirm the write.
    #[tokio::test]
    async fn roon_runtime_confirms_only_the_matching_authoritative_zone_callback() {
        use crate::bus::runtime::{
            build_runtime, CommandDeadlines, CommandLane, CommandRequest, CommandStatus,
            NativeResult, ProjectionCommit, ProjectionCommitter, RuntimeCommand,
        };
        use std::sync::atomic::{AtomicU64, Ordering};

        #[derive(Default)]
        struct Committer(AtomicU64);

        #[async_trait::async_trait]
        impl ProjectionCommitter for Committer {
            async fn commit_projection(
                &self,
                _update: crate::bus::runtime::ProjectionUpdate,
            ) -> ProjectionCommit {
                ProjectionCommit::Committed {
                    revision: self.0.fetch_add(1, Ordering::Relaxed) + 1,
                }
            }
        }

        let parts = build_runtime(Arc::new(Committer::default()), 2, 4);
        let bridge =
            RoonRuntimeBridge::new(parts.projection_ingress.clone(), parts.commands.clone());
        let actor = tokio::spawn(parts.projection_actor.run());
        let mut endpoint = parts
            .commands
            .register_provider("roon", 1)
            .expect("the test owns the Roon endpoint");
        let now = tokio::time::Instant::now();
        let mut ticket = parts
            .commands
            .submit(CommandRequest {
                target: PrefixedZoneId::roon("target"),
                command: RuntimeCommand::Control(Command::Play),
                correlation_id: None,
                lane: CommandLane::Interactive,
                deadlines: CommandDeadlines {
                    dispatch_by: now + Duration::from_secs(1),
                    confirm_by: now + Duration::from_secs(5),
                },
            })
            .await
            .expect("command admission");
        let work = endpoint.recv().await.expect("admitted work");
        let permit = work.begin_dispatch().expect("dispatch permit");
        let command_id = permit.id();
        let mut observed = bridge
            .arm(
                PrefixedZoneId::roon("target"),
                command_id,
                RoonObservationExpectation::Playback(PlaybackState::Playing),
            )
            .await
            .expect("first command can arm the confirmation bridge");
        permit.complete_native(NativeResult::Accepted);

        bridge
            .publish_zone(roon_zone_to_bus_zone(&make_test_zone(
                "other-output",
                Some(20.0),
                Some(0.0),
                Some(100.0),
            )))
            .await
            .expect("unrelated callback still projects");
        assert_eq!(ticket.status(), CommandStatus::AwaitingProjection);
        assert!(
            observed.try_recv().is_err(),
            "wrong-zone callback must not confirm"
        );

        let mut target_zone = make_test_zone("target-output", Some(25.0), Some(0.0), Some(100.0));
        target_zone.zone_id = "target".to_string();
        bridge
            .publish_zone(roon_zone_to_bus_zone(&target_zone))
            .await
            .expect("same-zone callback with the wrong state still projects");
        assert_eq!(ticket.status(), CommandStatus::AwaitingProjection);
        assert!(
            observed.try_recv().is_err(),
            "same-zone callback without the requested effect must not confirm"
        );

        target_zone.state = "playing".to_string();
        bridge
            .publish_zone(roon_zone_to_bus_zone(&target_zone))
            .await
            .expect("matching state callback projects");
        observed.await.expect("matching callback acknowledgement");
        assert!(matches!(
            ticket.wait_for_observable_result().await,
            CommandStatus::Confirmed { .. }
        ));
        actor.abort();
    }

    // =========================================================================
    // Core-reported browse rejections (issue #405)
    // =========================================================================
    //
    // Test strategy: these unit-scope the correlation logic. They do NOT drive
    // a Roon protocol mock - `tests/mock_servers/roon.rs` holds state and
    // speaks no protocol, and building a protocol-level Roon fake is #408's
    // job. Duplicating it here would conflict with that work. What is scoped
    // here is exactly the concurrency-sensitive part: which waiter a rejection
    // resolves when several requests are in flight, and what happens to the
    // ones it must not touch.
    //
    // Coverage boundary, stated so the next agent does not over-trust it: the
    // tests exercise the real routing functions, the real `oneshot` channels
    // and the real `BROWSE_TIMEOUT`. They do not exercise the fork's
    // `parse_msg`, nor the three-line result match in `browse()`/`load()` that
    // hands the oneshot payload back to the caller. In particular, nothing here
    // proves that a real Core's error names match the four literals the fork
    // matches on - if they did not, these tests would stay green and the
    // ten-second hang would be live. That half is #408's.
    //
    // Which evidence supports which claim, since it is easy to conflate:
    // `a_rejected_item_key_resolves_far_inside_the_browse_timeout` is the
    // promptness proof; `browse_rejection_resolves_only_the_matching_waiter`
    // and `a_mismatched_entry_in_one_map_does_not_hide_a_match_in_the_other`
    // are the correlation proofs. A fast suite alone would not have caught an
    // implementation that promptly resolved the *wrong* waiter.

    use std::time::Instant;
    use tokio::sync::oneshot::error::TryRecvError;

    /// Queue a pending browse the way `browse()` does, returning its receiver.
    fn pending_browse(
        state: &mut RoonState,
        req_id: usize,
        session_key: &str,
    ) -> oneshot::Receiver<Result<BrowseResult>> {
        let (tx, rx) = oneshot::channel();
        state
            .pending_browses
            .insert(req_id, (Some(session_key.to_string()), tx));
        rx
    }

    /// Queue a pending load the way `load()` does, returning its receiver.
    fn pending_load(
        state: &mut RoonState,
        req_id: usize,
        session_key: &str,
    ) -> oneshot::Receiver<Result<LoadResult>> {
        let (tx, rx) = oneshot::channel();
        state
            .pending_loads
            .insert(req_id, (Some(session_key.to_string()), tx));
        rx
    }

    /// Queue a pending image request the way `get_image()` does.
    fn pending_image(
        state: &mut RoonState,
        req_id: usize,
        image_key: &str,
    ) -> oneshot::Receiver<Option<ImageData>> {
        let (tx, rx) = oneshot::channel();
        state
            .pending_images
            .insert(req_id, (image_key.to_string(), tx));
        rx
    }

    /// The rejection a receiver was resolved with, or a description of why it
    /// was not resolved.
    fn rejection_kind<T>(rx: &mut oneshot::Receiver<Result<T>>) -> RoonBrowseErrorKind {
        match rx.try_recv() {
            Ok(Err(err)) => match RoonBrowseError::from_error(&err) {
                Some(rejection) => rejection.kind,
                None => unreachable!("resolved with an untyped error: {err}"),
            },
            Ok(Ok(_)) => unreachable!("resolved with a success result"),
            Err(TryRecvError::Empty) => unreachable!("waiter was never resolved"),
            Err(TryRecvError::Closed) => unreachable!("waiter's sender was dropped"),
        }
    }

    fn assert_still_waiting<T>(rx: &mut oneshot::Receiver<Result<T>>, what: &str) {
        assert!(
            matches!(rx.try_recv(), Err(TryRecvError::Empty)),
            "{what} must still be waiting - it was resolved or its sender dropped"
        );
    }

    #[test]
    fn browse_rejection_resolves_only_the_matching_waiter() {
        // Several browse and load requests in flight, as the adapter routinely
        // has during search() and play_item(). One is rejected by the Core.
        let mut state = RoonState::default();
        let mut first = pending_browse(&mut state, 11, "search_a");
        let mut rejected = pending_browse(&mut state, 12, "search_b");
        let mut third = pending_browse(&mut state, 13, "search_c");
        let mut load_a = pending_load(&mut state, 14, "search_a");
        let mut load_b = pending_load(&mut state, 15, "search_b");

        let routing =
            state.route_browse_rejection(12, Some("search_b"), RoonBrowseErrorKind::InvalidItemKey);

        assert_eq!(routing, ErrorRouting::Browse);
        assert_eq!(
            rejection_kind(&mut rejected),
            RoonBrowseErrorKind::InvalidItemKey
        );
        assert_still_waiting(&mut first, "browse 11");
        assert_still_waiting(&mut third, "browse 13");
        assert_still_waiting(&mut load_a, "load 14");
        // load 15 shares the rejected request's session key: proof that
        // correlation is by req_id, not by scanning for a session key.
        assert_still_waiting(&mut load_b, "load 15");

        // Only the resolved entry leaves the map; nothing is stranded.
        assert!(!state.pending_browses.contains_key(&12));
        assert_eq!(state.pending_browses.len(), 2);
        assert_eq!(state.pending_loads.len(), 2);
    }

    #[test]
    fn load_rejection_resolves_the_load_not_a_browse_sharing_its_session() {
        // The positive invariant: the refused request is identified by req_id,
        // so a sibling sharing its session key in the *other* map is untouched.
        // (Correlating by session key instead could not even tell a browse from
        // a load here - it would have to pick a map to scan first.)
        let mut state = RoonState::default();
        let mut browse = pending_browse(&mut state, 20, "play_item_1");
        let mut load = pending_load(&mut state, 21, "play_item_1");

        let routing = state.route_browse_rejection(
            21,
            Some("play_item_1"),
            RoonBrowseErrorKind::InvalidItemKey,
        );

        assert_eq!(routing, ErrorRouting::Load);
        assert_eq!(
            rejection_kind(&mut load),
            RoonBrowseErrorKind::InvalidItemKey
        );
        assert_still_waiting(&mut browse, "browse 20");
        assert!(state.pending_browses.contains_key(&20));
        assert!(!state.pending_loads.contains_key(&21));
    }

    #[test]
    fn rejection_with_a_mismatched_session_key_leaves_the_waiter_alone() {
        // `Moo` restarts its request-id counter at 0 on every reconnect, so a
        // rejection on a new connection can carry a req_id that a stale, not
        // yet timed-out waiter still occupies. Resolving it would be a
        // mis-correlation; the waiter is left to its existing timeout instead.
        let mut state = RoonState::default();
        let mut stale = pending_browse(&mut state, 7, "browse_old");

        let routing = state.route_browse_rejection(
            7,
            Some("browse_new"),
            RoonBrowseErrorKind::InvalidItemKey,
        );

        assert_eq!(routing, ErrorRouting::KeyMismatch);
        assert_still_waiting(&mut stale, "browse 7");
        assert!(
            state.pending_browses.contains_key(&7),
            "the waiter must stay in the map so its own timeout still cleans up"
        );
    }

    #[test]
    fn a_mismatched_entry_in_one_map_does_not_hide_a_match_in_the_other() {
        // Only reachable through a reconnect id collision: a stale browse from
        // the old connection holds req_id 8, and the new connection issues a
        // load that also gets req_id 8. Reporting the browse's key mismatch
        // early would cost the load its prompt answer for no safety gain.
        let mut state = RoonState::default();
        let mut stale_browse = pending_browse(&mut state, 8, "browse_old");
        let mut live_load = pending_load(&mut state, 8, "load_new");

        let routing =
            state.route_browse_rejection(8, Some("load_new"), RoonBrowseErrorKind::InvalidItemKey);

        assert_eq!(routing, ErrorRouting::Load);
        assert_eq!(
            rejection_kind(&mut live_load),
            RoonBrowseErrorKind::InvalidItemKey
        );
        assert_still_waiting(&mut stale_browse, "the stale browse");
        assert!(state.pending_browses.contains_key(&8));
        assert!(!state.pending_loads.contains_key(&8));
    }

    #[test]
    fn rejection_for_an_unknown_req_id_is_a_noop() {
        let mut state = RoonState::default();
        let mut other = pending_browse(&mut state, 30, "browse_x");

        let routing = state.route_browse_rejection(
            999,
            Some("browse_x"),
            RoonBrowseErrorKind::UnexpectedError,
        );

        assert_eq!(routing, ErrorRouting::NoWaiter);
        assert_still_waiting(&mut other, "browse 30");
        assert_eq!(state.pending_browses.len(), 1);
    }

    #[test]
    fn a_second_rejection_for_the_same_req_id_cannot_double_resolve() {
        let mut state = RoonState::default();
        let mut rx = pending_browse(&mut state, 40, "browse_y");

        let first =
            state.route_browse_rejection(40, Some("browse_y"), RoonBrowseErrorKind::InvalidItemKey);
        let second =
            state.route_browse_rejection(40, Some("browse_y"), RoonBrowseErrorKind::InvalidItemKey);

        assert_eq!(first, ErrorRouting::Browse);
        assert_eq!(second, ErrorRouting::NoWaiter);
        assert_eq!(rejection_kind(&mut rx), RoonBrowseErrorKind::InvalidItemKey);
        assert!(state.pending_browses.is_empty());
    }

    #[test]
    fn rejection_after_the_caller_timed_out_is_a_noop() {
        // `browse()` removes its own entry when BROWSE_TIMEOUT fires. A
        // rejection arriving afterwards must not panic or resurrect anything.
        let mut state = RoonState::default();
        let _rx = pending_browse(&mut state, 50, "browse_z");
        state.pending_browses.remove(&50);

        let routing =
            state.route_browse_rejection(50, Some("browse_z"), RoonBrowseErrorKind::InvalidLevels);

        assert_eq!(routing, ErrorRouting::NoWaiter);
        assert!(state.pending_browses.is_empty());
    }

    #[test]
    fn rejection_with_a_dropped_receiver_still_clears_the_entry() {
        let mut state = RoonState::default();
        let rx = pending_browse(&mut state, 60, "browse_w");
        drop(rx);

        let routing =
            state.route_browse_rejection(60, Some("browse_w"), RoonBrowseErrorKind::InvalidItemKey);

        assert_eq!(routing, ErrorRouting::ReceiverGone);
        assert!(
            state.pending_browses.is_empty(),
            "a gone receiver must not leave a stranded map entry"
        );
    }

    #[test]
    fn every_browse_rejection_kind_is_routed() {
        // Routing only InvalidItemKey would leave the other three Core
        // rejections hanging their callers for the full timeout.
        for kind in [
            RoonBrowseErrorKind::InvalidItemKey,
            RoonBrowseErrorKind::InvalidLevels,
            RoonBrowseErrorKind::ZoneNotFound,
            RoonBrowseErrorKind::UnexpectedError,
        ] {
            let mut state = RoonState::default();
            let mut rx = pending_browse(&mut state, 70, "browse_all");

            let routing = state.route_browse_rejection(70, Some("browse_all"), kind);

            assert_eq!(
                routing,
                ErrorRouting::Browse,
                "kind {kind:?} was not routed"
            );
            assert_eq!(rejection_kind(&mut rx), kind);
        }
    }

    #[tokio::test]
    async fn a_rejected_item_key_resolves_far_inside_the_browse_timeout() {
        // The point of the issue: promptness. Awaited exactly as `browse()`
        // awaits it, with the real BROWSE_TIMEOUT.
        let mut state = RoonState::default();
        let rx = pending_browse(&mut state, 80, "browse_prompt");

        let started = Instant::now();
        let routing = state.route_browse_rejection(
            80,
            Some("browse_prompt"),
            RoonBrowseErrorKind::InvalidItemKey,
        );
        let result = tokio::time::timeout(BROWSE_TIMEOUT, rx).await;
        let elapsed = started.elapsed();

        assert_eq!(routing, ErrorRouting::Browse);
        assert!(
            elapsed < Duration::from_secs(1),
            "a rejected item key must not wait out BROWSE_TIMEOUT ({BROWSE_TIMEOUT:?}); \
             took {elapsed:?}"
        );
        let Ok(Ok(Err(err))) = result else {
            unreachable!("expected a typed rejection inside the timeout");
        };
        let Some(rejection) = RoonBrowseError::from_error(&err) else {
            unreachable!("expected a RoonBrowseError, got: {err}");
        };
        assert_eq!(rejection.kind, RoonBrowseErrorKind::InvalidItemKey);
        assert_eq!(rejection.session_key.as_deref(), Some("browse_prompt"));
    }

    #[test]
    fn image_rejection_resolves_the_matching_image_request() {
        let mut state = RoonState::default();
        let mut wanted = pending_image(&mut state, 90, "image_a");
        let mut other = pending_image(&mut state, 91, "image_b");

        let routing = state.route_image_rejection(90, "image_a");

        assert_eq!(routing, ErrorRouting::Image);
        // `get_image` reads None as "Image not found".
        assert!(matches!(wanted.try_recv(), Ok(None)));
        assert!(matches!(other.try_recv(), Err(TryRecvError::Empty)));
        assert!(!state.pending_images.contains_key(&90));
        assert!(state.pending_images.contains_key(&91));
    }

    #[test]
    fn image_rejection_with_a_mismatched_key_leaves_the_waiter_alone() {
        let mut state = RoonState::default();
        let mut rx = pending_image(&mut state, 92, "image_a");

        let routing = state.route_image_rejection(92, "image_somewhere_else");

        assert_eq!(routing, ErrorRouting::KeyMismatch);
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
        assert!(state.pending_images.contains_key(&92));
    }

    #[test]
    fn a_rejection_is_distinguishable_from_a_timeout_and_a_lost_core() {
        // The three failure classes the issue insists must stay apart.
        let rejected = anyhow::Error::new(RoonBrowseError::new(
            RoonBrowseErrorKind::InvalidItemKey,
            Some("browse_1"),
        ));
        let timed_out = anyhow::anyhow!("Browse request timed out");
        let not_connected = anyhow::anyhow!("Browse service not available - not connected to Roon");

        let Some(rejection) = RoonBrowseError::from_error(&rejected) else {
            unreachable!("a rejection must be recoverable from the anyhow error");
        };
        assert_eq!(rejection.kind, RoonBrowseErrorKind::InvalidItemKey);
        assert!(rejection.kind.is_recoverable());
        assert!(RoonBrowseError::from_error(&timed_out).is_none());
        assert!(RoonBrowseError::from_error(&not_connected).is_none());

        // And the message a caller sees says what to do, without ever reading
        // like a timeout.
        let message = rejected.to_string();
        assert!(message.contains("search or browse again"), "{message}");
        assert!(!message.contains("timed out"), "{message}");
        assert!(message.contains("browse_1"), "{message}");
    }

    #[test]
    fn only_reference_rejections_are_marked_recoverable() {
        assert!(RoonBrowseErrorKind::InvalidItemKey.is_recoverable());
        assert!(RoonBrowseErrorKind::InvalidLevels.is_recoverable());
        assert!(!RoonBrowseErrorKind::ZoneNotFound.is_recoverable());
        assert!(!RoonBrowseErrorKind::UnexpectedError.is_recoverable());
        assert_eq!(
            RoonBrowseErrorKind::InvalidItemKey.as_str(),
            "invalid_item_key"
        );
    }

    // =========================================================================
    // Browse/load result correlation (issue #416)
    // =========================================================================
    //
    // These are the white-box half of #416. They could not precede the functions
    // they call, so the *red* proof for this issue is behavioural and lives in
    // `tests/roon_protocol.rs`: `concurrent_browses_in_one_session_each_receive_
    // their_own_result` and `concurrent_loads_in_one_session_each_receive_their_
    // own_page` failed in 8 of 8 trials against the previous commit, driving the
    // real event loop through #408's protocol fake. What these add is the cases a
    // fake cannot reach on demand - a reconnect req_id collision, a result after
    // the caller timed out, a message with no request id - each deterministic.
    //
    // Same coverage boundary as #405's tests above: the real routing functions,
    // the real oneshot channels, not the fork's `parse_msg`. The one claim that is
    // neither tested here nor testable here is that `moo_request_id` reads the id
    // the fork actually put there; `tests/roon_protocol.rs` covers that end to end
    // by getting three concurrent answers right through the real wire.

    fn browse_result(title: &str) -> BrowseResult {
        use roon_api::browse::{Action, List};
        BrowseResult {
            action: Action::List,
            item: None,
            list: Some(List {
                title: title.to_string(),
                ..Default::default()
            }),
            message: None,
            is_error: None,
        }
    }

    fn load_result(title: &str) -> LoadResult {
        use roon_api::browse::List;
        LoadResult {
            items: Vec::new(),
            offset: 0,
            list: List {
                title: title.to_string(),
                ..Default::default()
            },
        }
    }

    /// The MOO message shape the fork forwards alongside a browse/load result:
    /// `request_id` as a *string*, as `Moo::parse` writes it.
    fn raw_msg_with_request_id(req_id: usize) -> serde_json::Value {
        serde_json::json!({ "request_id": req_id.to_string(), "verb": "COMPLETE" })
    }

    fn delivered_title<T>(
        rx: &mut oneshot::Receiver<Result<T>>,
        title_of: fn(&T) -> String,
    ) -> String {
        match rx.try_recv() {
            Ok(Ok(value)) => title_of(&value),
            Ok(Err(err)) => unreachable!("resolved with an error: {err}"),
            Err(TryRecvError::Empty) => unreachable!("waiter was never resolved"),
            Err(TryRecvError::Closed) => unreachable!("waiter's sender was dropped"),
        }
    }

    fn browse_title(result: &BrowseResult) -> String {
        result
            .list
            .as_ref()
            .map(|list| list.title.clone())
            .unwrap_or_default()
    }

    fn load_title(result: &LoadResult) -> String {
        result.list.title.clone()
    }

    #[test]
    fn a_browse_result_resolves_only_the_request_that_asked_for_it() {
        // The defect, in one assertion: three browses share one session key, as
        // `/roon/browse` lets any two callers do and as #399's nav handle would.
        // The scan this replaced could resolve any of them.
        let mut state = RoonState::default();
        let mut first = pending_browse(&mut state, 11, "shared");
        let mut second = pending_browse(&mut state, 12, "shared");
        let mut third = pending_browse(&mut state, 13, "shared");
        let mut load_in_same_session = pending_load(&mut state, 14, "shared");

        let routing = state.route_browse_result(Some(12), Some("shared"), browse_result("TIDAL"));

        assert_eq!(routing, ResultRouting::Delivered);
        assert_eq!(delivered_title(&mut second, browse_title), "TIDAL");
        assert_still_waiting(&mut first, "browse 11");
        assert_still_waiting(&mut third, "browse 13");
        assert_still_waiting(&mut load_in_same_session, "load 14");
        assert_eq!(state.pending_browses.len(), 2);
        assert_eq!(state.pending_loads.len(), 1);
    }

    #[test]
    fn a_load_result_resolves_only_the_request_that_asked_for_it() {
        let mut state = RoonState::default();
        let mut first = pending_load(&mut state, 21, "shared");
        let mut second = pending_load(&mut state, 22, "shared");
        let mut browse_in_same_session = pending_browse(&mut state, 23, "shared");

        let routing = state.route_load_result(Some(21), Some("shared"), load_result("Artists"));

        assert_eq!(routing, ResultRouting::Delivered);
        assert_eq!(delivered_title(&mut first, load_title), "Artists");
        assert_still_waiting(&mut second, "load 22");
        assert_still_waiting(&mut browse_in_same_session, "browse 23");
        assert!(!state.pending_loads.contains_key(&21));
    }

    #[test]
    fn a_browse_result_never_reaches_a_load_waiting_on_the_same_req_id() {
        // `req_id` is unique per connection, so this needs a reconnect collision
        // to arise at all. It is pinned because the *type* is what prevents it -
        // a `BrowseResult` cannot be sent to a `LoadRequest` - and a future
        // refactor that unified the two maps would lose that protection silently.
        let mut state = RoonState::default();
        let mut load = pending_load(&mut state, 31, "shared");

        let routing = state.route_browse_result(Some(31), Some("shared"), browse_result("Library"));

        assert_eq!(routing, ResultRouting::NoWaiter);
        assert_still_waiting(&mut load, "load 31");
        assert!(state.pending_loads.contains_key(&31));
    }

    #[test]
    fn a_result_with_a_mismatched_session_key_leaves_the_waiter_alone() {
        // #405's reconnect guard, applied to the success path: `Moo` restarts its
        // request-id counter at 0 on every reconnect, so a result on a fresh
        // connection can carry a req_id a stale, not-yet-timed-out waiter still
        // occupies. Being resolved with someone else's *success* is the worst
        // outcome available, so the waiter is left to its own timeout.
        let mut state = RoonState::default();
        let mut stale = pending_browse(&mut state, 41, "browse_old");

        let routing =
            state.route_browse_result(Some(41), Some("browse_new"), browse_result("TIDAL"));

        assert_eq!(routing, ResultRouting::KeyMismatch);
        assert_still_waiting(&mut stale, "browse 41");
        assert!(
            state.pending_browses.contains_key(&41),
            "the waiter must stay in the map so its own timeout still cleans up"
        );
    }

    #[test]
    fn a_result_for_an_unknown_req_id_is_a_noop() {
        let mut state = RoonState::default();
        let mut other = pending_browse(&mut state, 51, "shared");

        let routing = state.route_browse_result(Some(999), Some("shared"), browse_result("Qobuz"));

        assert_eq!(routing, ResultRouting::NoWaiter);
        assert_still_waiting(&mut other, "browse 51");
        assert_eq!(state.pending_browses.len(), 1);
    }

    #[test]
    fn a_second_result_for_the_same_req_id_cannot_double_resolve() {
        let mut state = RoonState::default();
        let mut rx = pending_browse(&mut state, 61, "shared");

        let first = state.route_browse_result(Some(61), Some("shared"), browse_result("Library"));
        let second = state.route_browse_result(Some(61), Some("shared"), browse_result("TIDAL"));

        assert_eq!(first, ResultRouting::Delivered);
        assert_eq!(second, ResultRouting::NoWaiter);
        assert_eq!(delivered_title(&mut rx, browse_title), "Library");
        assert!(state.pending_browses.is_empty());
    }

    #[test]
    fn a_result_after_the_caller_timed_out_is_a_noop() {
        let mut state = RoonState::default();
        let _rx = pending_load(&mut state, 71, "shared");
        state.pending_loads.remove(&71);

        let routing = state.route_load_result(Some(71), Some("shared"), load_result("Albums"));

        assert_eq!(routing, ResultRouting::NoWaiter);
        assert!(state.pending_loads.is_empty());
    }

    #[test]
    fn a_result_with_a_dropped_receiver_still_clears_the_entry() {
        let mut state = RoonState::default();
        let rx = pending_browse(&mut state, 81, "shared");
        drop(rx);

        let routing = state.route_browse_result(Some(81), Some("shared"), browse_result("Library"));

        assert_eq!(routing, ResultRouting::ReceiverGone);
        assert!(
            state.pending_browses.is_empty(),
            "a gone receiver must not leave a stranded map entry"
        );
    }

    #[test]
    fn a_result_with_no_request_id_is_left_to_time_out_rather_than_guessed_at() {
        // Unreachable with the pinned fork - `Browse::parse_msg` parses
        // `msg["request_id"]` before it can produce a result, and the fork
        // forwards that same message. Pinned because the tempting fallback,
        // scanning by session key, is exactly the defect #416 removes: it would
        // resolve *a* waiter rather than *the* waiter, silently.
        let mut state = RoonState::default();
        let mut rx = pending_browse(&mut state, 91, "shared");

        let routing = state.route_browse_result(None, Some("shared"), browse_result("Library"));

        assert_eq!(routing, ResultRouting::Uncorrelated);
        assert_still_waiting(&mut rx, "browse 91");
        assert!(state.pending_browses.contains_key(&91));
    }

    #[test]
    fn the_request_id_is_read_off_the_raw_moo_message() {
        // MOO carries the id in a `Request-Id` header, which the fork copies into
        // the message as a *string* (`src/moo.rs:354-355`). A numeric one cannot
        // reach here - the fork's own `parse_msg` would have panicked on
        // `as_str().unwrap()` first - so it is treated as uncorrelatable rather
        // than quietly accepted.
        assert_eq!(moo_request_id(&raw_msg_with_request_id(7)), Some(7));
        assert_eq!(moo_request_id(&serde_json::json!({})), None);
        assert_eq!(
            moo_request_id(&serde_json::json!({ "request_id": "not-a-number" })),
            None
        );
        assert_eq!(
            moo_request_id(&serde_json::json!({ "request_id": 7 })),
            None,
            "a numeric request_id is not the shape the fork produces"
        );
        assert_eq!(moo_request_id(&serde_json::Value::Null), None);
    }

    #[test]
    fn out_of_order_results_each_reach_their_own_waiter() {
        // Three in flight under one session key, answered in reverse order. The
        // integration test in `tests/roon_protocol.rs` proves this over the real
        // wire; this states it as an invariant of the routing function itself,
        // without the 8-trial probabilistic dance.
        let mut state = RoonState::default();
        let mut first = pending_browse(&mut state, 101, "shared");
        let mut second = pending_browse(&mut state, 102, "shared");
        let mut third = pending_browse(&mut state, 103, "shared");

        for (req_id, title) in [(103, "Qobuz"), (102, "TIDAL"), (101, "Library")] {
            assert_eq!(
                state.route_browse_result(Some(req_id), Some("shared"), browse_result(title)),
                ResultRouting::Delivered
            );
        }

        assert_eq!(delivered_title(&mut first, browse_title), "Library");
        assert_eq!(delivered_title(&mut second, browse_title), "TIDAL");
        assert_eq!(delivered_title(&mut third, browse_title), "Qobuz");
        assert!(state.pending_browses.is_empty());
    }

    fn grouping_row(subtitle: &str) -> BrowseItem {
        BrowseItem {
            title: "whatever category label this source uses".to_string(),
            subtitle: Some(subtitle.to_string()),
            image_key: None,
            item_key: Some("some_key".to_string()),
            hint: Some(ItemHint::List),
            input_prompt: None,
        }
    }

    /// Issue #431: `is_category`'s title list is verified only for Library
    /// results; `looks_like_a_result_count_grouping`'s subtitle-shape check
    /// is the fallback for TIDAL/Qobuz, which may format their own grouping
    /// rows differently. Broadened to accept case and singular/plural
    /// variation, since none of these were exercised by the one live Library
    /// search this was built against -- still unverified against a real
    /// TIDAL/Qobuz result (tracked on #431), but no longer needlessly narrow
    /// to the exact casing and pluralization of the one confirmed shape.
    #[test]
    fn result_count_grouping_tolerates_case_and_pluralization_variants() {
        for subtitle in [
            "32 Results",  // the exact shape verified live against nuc14
            "1 Result",    // singular -- untested live, but plausible grammar
            "7 results",   // lowercase -- untested live, but plausible casing
            "19 RESULTS",  // shouting case, same reasoning
            " 5 Results ", // incidental whitespace
        ] {
            assert!(
                looks_like_a_result_count_grouping(&grouping_row(subtitle)),
                "subtitle {subtitle:?} should be recognised as a grouping row"
            );
        }
    }

    /// The broadened check must not start swallowing real content whose
    /// subtitle happens to end in a number-ish word, or #396's whole point
    /// (never silently misplay) fails in the other direction.
    #[test]
    fn result_count_grouping_does_not_swallow_real_subtitles() {
        for subtitle in [
            "Miles Davis",       // an artist name
            "Kind of Blue",      // an album title
            "32 Bit Adventures", // a real album title that merely starts with a number
            "Results May Vary",  // a real album title containing the word, wrong shape
            "results",           // the word alone, no count
            "32",                // a count alone, no word
        ] {
            assert!(
                !looks_like_a_result_count_grouping(&grouping_row(subtitle)),
                "subtitle {subtitle:?} should NOT be recognised as a grouping row"
            );
        }
    }
}
