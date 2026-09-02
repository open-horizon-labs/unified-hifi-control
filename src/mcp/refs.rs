//! The MCP ref table (issue #396): opaque, server-resolved content references.
//!
//! # The problem this solves
//!
//! Before this module, `hifi_search` returned `{title, subtitle}` and nothing
//! else, so the only route from "found it" to "playing it" was handing the
//! title back to `hifi_play`, which re-searches and takes the first match. A
//! client could not address the third result, could not distinguish two
//! same-titled albums, and could not tell whether what played is what it
//! picked. See `tests/mcp_contract.rs::FIELD_ROLES` for how that gap was
//! frozen (not endorsed) by #394/#395.
//!
//! # Design: a server-side table, not a self-describing token
//!
//! This was settled at #396's solution-space gate (see the issue and its gate
//! report comments; not relitigated here). A ref is a 128-bit random token
//! that names a row in this table; the row alone carries what the token
//! resolves to. The alternative -- encode the provider handle into the token
//! itself, signed or encrypted -- was rejected because:
//!
//! - Signing gives integrity, not confidentiality: `base64(item_key)` still
//!   puts the raw key in the client-visible string, just spelled differently.
//!   Real confidentiality needs authenticated encryption, a new dependency
//!   this bridge does not otherwise need.
//! - A Roon ref must carry the `multi_session_key` that minted its `item_key`
//!   alongside it (see [`RoonRefTarget`]), because portability of a Roon
//!   `item_key` across sessions is unproven and public evidence points against
//!   it (home-assistant/core#137605; Roon Labs community thread 23129, both
//!   cited in `tests/mock_servers/roon_core.rs`). A table can hold that pair
//!   without ever putting the session key in a client-visible string; an
//!   encoded token cannot do that without either leaking it or encrypting it.
//!
//! # Lifetime: one uniform TTL, not a per-target-kind one
//!
//! The solution-space gate's review argued for giving durable LMS targets
//! (`Library`, `Url`) a longer internal TTL than ephemeral ones (Roon
//! `item_key`, LMS `GlobalSearchItem`), on the theory that a model does no TTL
//! arithmetic anyway so the difference is invisible to it. The gate's own
//! dissent (D4) refuted that: a ref that survives 40 minutes on LMS and fails
//! at 40 minutes on Roon teaches the model an inconsistent rule, because
//! models generalize from what worked -- "the last ref I held that long was
//! fine" becomes an expectation the next Roon ref violates. And "never refuse
//! a ref you could have honored" plus "never document a lifetime longer than
//! the shortest kind's" cannot both hold at once; something has to give, and
//! making the *documented* contract secretly wrong in one direction is the
//! wrong thing to give up in an epic whose thesis is capability honesty. This
//! table therefore uses [`DEFAULT_TTL`] for every ref, regardless of provider
//! or target durability. A future durable content identifier (tracked by
//! #320) is a different, explicitly-durable thing; it is not modeled as a
//! quietly-longer TTL on this field.
//!
//! # No client-visible TTL field
//!
//! The lifetime lives in the tool descriptions (`hifi_search`, `hifi_play_ref`)
//! and the recovery instruction lives in the refusal
//! ([`crate::mcp::envelope::Refusal::UnknownTarget`]) -- never in a field on
//! the ref itself. A model does no TTL arithmetic across turns, so a
//! `ref_expires_in_s` field would itself be the orphaned field this issue
//! exists to end.
//!
//! # Bounded size and time, and eviction as expiry
//!
//! The table holds at most [`DEFAULT_CAPACITY`] entries. Insertion past
//! capacity evicts the oldest-inserted entry first (insertion-order, not
//! access-order LRU -- the issue leaves eviction policy as a soft constraint,
//! and insertion order is the simplest policy that satisfies "bounded" without
//! a new dependency). Because a token is 128 random bits with nothing
//! derivable from it, a client cannot distinguish "this ref was evicted for
//! space" from "this ref expired" from "this token was never valid" -- all
//! three resolve to the same `unknown_ref` outcome. That collapse is the
//! point: eviction cannot look like corruption because there is no partial or
//! wrong answer to give, only "not found".

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use tokio::sync::Mutex;

use crate::adapters::lms::LmsPlayTarget;
use crate::mcp::envelope::Provider;

/// How many refs the table holds before the oldest is evicted to make room.
///
/// Sized generously above any plausible single-conversation search volume.
/// Roon `item_key` <= 500 chars (the adapter's own bound,
/// `src/adapters/roon.rs`), session keys and titles are short, so even a full
/// table is a trivial amount of memory -- boundedness, not compactness, is the
/// property this guards.
pub const DEFAULT_CAPACITY: usize = 512;

/// How long a ref remains resolvable after it is minted. Uniform across every
/// provider and target kind -- see the module docs for why that is a
/// deliberate choice, not an oversight.
///
/// # Measured against a real Roon Core (#616)
///
/// #616 asked whether this should instead track the lifetime of the Roon
/// browse session a ref depends on. Measured read-only against the
/// operator's production Core (Roon 2.x, 2026-08), by minting eleven
/// independent deep browse refs at once and probing one per interval
/// without touching the others:
///
/// | idle | result |
/// |------|--------|
/// | 30s, 90s, 180s, 300s, 480s, 660s, **840s** | resolves normally |
/// | 1020s | gone -- **this TTL**, at 900s, not the Core |
///
/// So a Roon browse session outlives this TTL rather than the other way
/// round: shortening it to "match Roon" would discard refs the Core is
/// still perfectly willing to honour, and lengthening it is what
/// [`RoonRefTarget::trail`] now makes safe rather than necessary. Left
/// uniform and unchanged; the recovery path, not the clock, is what makes a
/// stale key survivable.
pub const DEFAULT_TTL: Duration = Duration::from_secs(15 * 60);

const TOKEN_PREFIX: &str = "ref_";

/// What a Roon ref resolves through: the `item_key` **and** the
/// `multi_session_key` that minted it.
///
/// The pairing is load-bearing, not incidental. `RoonAdapter::search` mints a
/// private session per call and the item keys it returns are scoped to that
/// session (see `src/adapters/roon.rs::search_with_session`); resolution must
/// re-enter that exact session (`RoonAdapter::play_ref`), never a fresh one.
/// # Why the pair alone is not enough (#616)
///
/// An `(item_key, multi_session_key)` pair is only meaningful while the Core
/// still holds that session. When it does not -- the extension reconnected,
/// the Core restarted, or the session aged out -- the pair is unrecoverable
/// on its own: nothing in it says *what* the key pointed at, so the only
/// answer left is to tell the human to browse again. That is the error
/// reported in #616, and asking the user to redo a walk we could redo
/// ourselves is the part they feel.
///
/// [`Self::trail`] is what makes recovery possible: the titles from the
/// browse root down to this item, accumulated as the user descends. Given
/// the trail, `RoonAdapter::resolve_trail` can re-walk it in a fresh session
/// and mint an equivalent key, so a rejected ref becomes a retry rather than
/// an apology. It is best-effort: an empty trail simply means no recovery is
/// possible for that ref, exactly as before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoonRefTarget {
    pub item_key: String,
    pub multi_session_key: String,
    /// Titles from the browse root down to and including this item. Empty
    /// when the minting surface could not supply one.
    pub trail: Vec<String>,
}

/// What a ref resolves to, independent of the opaque token a client holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefTarget {
    Roon {
        target: RoonRefTarget,
        title: String,
    },
    Lms {
        target: LmsPlayTarget,
        title: String,
    },
    Spotify {
        uri: String,
        title: String,
    },
    /// A durable Music Assistant media URI. The token keeps that URI out of
    /// client-side control flow and binds it to the provider that searched it.
    MusicAssistant {
        uri: String,
        title: String,
    },
    /// A Music Assistant browse continuation. This is deliberately a separate
    /// target from playable media: the MA path remains server-side and this
    /// token is only accepted by `hifi_collections`, never `hifi_play_ref`.
    MusicAssistantBrowse {
        path: String,
        title: String,
        location: crate::mcp::collection_locations::CollectionLocation,
    },
    /// An LMS browse continuation (#531): a server-side collection path
    /// (`"albums"`, `"album:<id>"`, ...), never a raw entity id handed to a
    /// client. Only accepted by `hifi_collections`, never `hifi_play_ref` --
    /// same split as [`Self::MusicAssistantBrowse`].
    LmsBrowse {
        path: String,
        title: String,
        location: crate::mcp::collection_locations::CollectionLocation,
    },
    /// A Roon browse continuation (#531): the `item_key` **and**
    /// `multi_session_key` a collection list was loaded under, so resuming it
    /// re-enters that exact session -- same pairing requirement as
    /// [`Self::Roon`], for the same reason (see [`RoonRefTarget`]'s docs).
    /// Only accepted by `hifi_collections`, never `hifi_play_ref`.
    RoonBrowse {
        target: RoonRefTarget,
        title: String,
        location: crate::mcp::collection_locations::CollectionLocation,
    },
    /// An Apple Music catalog/library identifier resolved by the paired native
    /// companion. Clients only receive the opaque token.
    AppleMusic {
        /// The execution-owner suffix of the applemusic zone that minted it.
        /// Apple library/catalog IDs are not portable across companions.
        companion_id: String,
        /// A companion-minted content handle, never a raw Apple ID or URI.
        handle: String,
        title: String,
    },
}

impl RefTarget {
    /// The provider this ref was minted for, so `hifi_play_ref` can refuse a
    /// ref used against a zone of a different provider capability-honestly.
    pub fn provider(&self) -> Provider {
        match self {
            Self::Roon { .. } => Provider::Roon,
            Self::Lms { .. } => Provider::Lms,
            Self::Spotify { .. } => Provider::Spotify,
            Self::MusicAssistant { .. } => Provider::MusicAssistant,
            Self::MusicAssistantBrowse { .. } => Provider::MusicAssistant,
            Self::LmsBrowse { .. } => Provider::Lms,
            Self::RoonBrowse { .. } => Provider::Roon,
            Self::AppleMusic { .. } => Provider::AppleMusic,
        }
    }

    /// The title captured at mint time, for the played-confirmation message --
    /// resolution does not re-derive it from the (possibly stale) backend.
    pub fn title(&self) -> &str {
        match self {
            Self::Roon { title, .. }
            | Self::Lms { title, .. }
            | Self::Spotify { title, .. }
            | Self::MusicAssistant { title, .. }
            | Self::MusicAssistantBrowse { title, .. }
            | Self::LmsBrowse { title, .. }
            | Self::RoonBrowse { title, .. }
            | Self::AppleMusic { title, .. } => title,
        }
    }
}

struct Record {
    target: RefTarget,
    minted_at: Instant,
}

struct Inner {
    entries: HashMap<String, Record>,
    /// Insertion order, oldest first. Used only to pick an eviction candidate;
    /// never consulted on a successful read (see the module docs: this is
    /// insertion-order eviction, not access-order LRU).
    order: VecDeque<String>,
    capacity: usize,
    ttl: Duration,
}

impl Inner {
    /// Make room for one more entry if the table is at capacity, evicting the
    /// oldest-inserted token(s). A loop rather than a single pop because a
    /// token at the front of `order` may already have been removed by a prior
    /// expiry-on-read, in which case it is skipped rather than counted.
    ///
    /// Gated on `order.len()` as well as `entries.len()`: an entry expiring
    /// on read (see `RefTable::resolve`) shrinks `entries` without touching
    /// `order`, so a table that expires faster than it fills would never
    /// trip an `entries.len()`-only check and `order` would grow one token
    /// per mint forever. Popping while *either* is at capacity drains those
    /// already-expired fronts on the next mint instead of leaving them
    /// stranded.
    fn evict_to_capacity(&mut self) {
        while self.entries.len() >= self.capacity || self.order.len() >= self.capacity {
            match self.order.pop_front() {
                Some(oldest) => {
                    self.entries.remove(&oldest);
                }
                None => break,
            }
        }
    }
}

/// The server-side table every `hifi_search` ref lives in until it is
/// resolved by `hifi_play_ref`, expires, or is evicted.
///
/// Cheap to clone (an `Arc` around the lock), matching every other field on
/// [`crate::api::AppState`].
#[derive(Clone)]
pub struct RefTable {
    inner: Arc<Mutex<Inner>>,
}

impl Default for RefTable {
    fn default() -> Self {
        Self::new()
    }
}

impl RefTable {
    /// A table with [`DEFAULT_CAPACITY`] and [`DEFAULT_TTL`]. What
    /// `AppState::new` constructs; production code should not need the other
    /// constructor.
    pub fn new() -> Self {
        Self::with_capacity_and_ttl(DEFAULT_CAPACITY, DEFAULT_TTL)
    }

    /// A table with an explicit capacity and TTL, for tests that need to force
    /// eviction or expiry without waiting on -- or allocating -- the
    /// production defaults.
    pub fn with_capacity_and_ttl(capacity: usize, ttl: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                entries: HashMap::new(),
                order: VecDeque::new(),
                capacity,
                ttl,
            })),
        }
    }

    /// Mint an opaque token for `target`, evicting the oldest entry first if
    /// the table is already at capacity.
    ///
    /// The token is 128 random bits, base64 (URL-safe, unpadded) encoded --
    /// nothing about `target` influences it, so two refs minted for the exact
    /// same target are never equal and neither can be derived from the other.
    pub async fn mint(&self, target: RefTarget) -> String {
        let token = generate_token();
        let mut inner = self.inner.lock().await;
        inner.evict_to_capacity();
        inner.order.push_back(token.clone());
        inner.entries.insert(
            token.clone(),
            Record {
                target,
                minted_at: Instant::now(),
            },
        );
        token
    }

    /// Resolve a token to its target, or `None` if it is unknown, mangled,
    /// expired, or evicted -- all four collapse to the same answer by design;
    /// see the module docs.
    pub async fn resolve(&self, token: &str) -> Option<RefTarget> {
        let mut inner = self.inner.lock().await;
        let expired = match inner.entries.get(token) {
            Some(record) => record.minted_at.elapsed() > inner.ttl,
            None => return None,
        };
        if expired {
            inner.entries.remove(token);
            return None;
        }
        inner.entries.get(token).map(|r| r.target.clone())
    }

    /// The number of live entries, for tests asserting the table stays
    /// bounded rather than growing without limit.
    #[cfg(test)]
    async fn len(&self) -> usize {
        self.inner.lock().await.entries.len()
    }

    /// The length of the insertion-order queue, independent of `entries.len()`
    /// -- the two can diverge when entries expire on read (see `resolve`)
    /// faster than eviction runs. Exists only so a test can prove `order`
    /// itself stays bounded, not merely `entries`.
    #[cfg(test)]
    async fn order_len(&self) -> usize {
        self.inner.lock().await.order.len()
    }
}

fn generate_token() -> String {
    let mut bytes = [0u8; 16]; // 128 bits
    rand::thread_rng().fill_bytes(&mut bytes);
    format!("{TOKEN_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// What an image ref resolves to (#549): the zone whose provider serves the
/// artwork, and the provider-native image handle to fetch through that
/// provider's `AppState::get_image` dispatch -- the same handle a Roon
/// `image_key`, an LMS coverid/URL, or a Music Assistant `image_url` already
/// is, just minted here instead of handed to a client.
///
/// A deliberately separate table from [`RefTable`], not a new [`RefTarget`]
/// variant: an image ref is resolved only by the collections image-proxy
/// route (`src/api/mod.rs::collections_image_handler`), never by
/// `hifi_play_ref`. Folding it into `RefTarget` would drag it through every
/// `(LibraryRoute, RefTarget)` pairing in
/// `src/mcp/tools/library.rs::handle_play_ref`'s exhaustive dispatch for a
/// variant that dispatch can never validly reach -- a lot of churn for a
/// combination that should simply never arise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub zone_id: String,
    pub image_key: String,
}

struct ImageRecord {
    target: ImageRef,
    minted_at: Instant,
}

struct ImageInner {
    entries: HashMap<String, ImageRecord>,
    order: VecDeque<String>,
    capacity: usize,
    ttl: Duration,
}

impl ImageInner {
    /// Same eviction policy as [`Inner::evict_to_capacity`]: insertion-order,
    /// draining already-expired fronts opportunistically. See that method's
    /// docs for why the loop checks both `entries` and `order`.
    fn evict_to_capacity(&mut self) {
        while self.entries.len() >= self.capacity || self.order.len() >= self.capacity {
            match self.order.pop_front() {
                Some(oldest) => {
                    self.entries.remove(&oldest);
                }
                None => break,
            }
        }
    }
}

/// The server-side table an `hifi_collections`/`/api/collections` item's
/// `image` field resolves through. Same opaque-token, bounded-size,
/// uniform-TTL design as [`RefTable`] -- see its module docs -- kept as its
/// own type rather than a case of that one (see [`ImageRef`]'s docs).
#[derive(Clone)]
pub struct ImageRefTable {
    inner: Arc<Mutex<ImageInner>>,
}

impl Default for ImageRefTable {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageRefTable {
    /// A table with [`DEFAULT_CAPACITY`] and [`DEFAULT_TTL`].
    pub fn new() -> Self {
        Self::with_capacity_and_ttl(DEFAULT_CAPACITY, DEFAULT_TTL)
    }

    /// A table with an explicit capacity and TTL, for tests that need to
    /// force eviction or expiry without waiting on the production defaults.
    pub fn with_capacity_and_ttl(capacity: usize, ttl: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ImageInner {
                entries: HashMap::new(),
                order: VecDeque::new(),
                capacity,
                ttl,
            })),
        }
    }

    /// Mint an opaque token for `target`, evicting the oldest entry first if
    /// the table is already at capacity.
    pub async fn mint(&self, target: ImageRef) -> String {
        let token = generate_token();
        let mut inner = self.inner.lock().await;
        inner.evict_to_capacity();
        inner.order.push_back(token.clone());
        inner.entries.insert(
            token.clone(),
            ImageRecord {
                target,
                minted_at: Instant::now(),
            },
        );
        token
    }

    /// Resolve a token to its target, or `None` if it is unknown, mangled,
    /// expired, or evicted -- all four collapse to the same answer, exactly
    /// as [`RefTable::resolve`] does.
    pub async fn resolve(&self, token: &str) -> Option<ImageRef> {
        let mut inner = self.inner.lock().await;
        let expired = match inner.entries.get(token) {
            Some(record) => record.minted_at.elapsed() > inner.ttl,
            None => return None,
        };
        if expired {
            inner.entries.remove(token);
            return None;
        }
        inner.entries.get(token).map(|r| r.target.clone())
    }

    #[cfg(test)]
    async fn len(&self) -> usize {
        self.inner.lock().await.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::lms::LmsSearchResultType;

    fn roon_target(item_key: &str, session: &str, title: &str) -> RefTarget {
        RefTarget::Roon {
            target: RoonRefTarget {
                item_key: item_key.to_string(),
                multi_session_key: session.to_string(),
                trail: vec![title.to_string()],
            },
            title: title.to_string(),
        }
    }

    fn lms_target(id: i64, title: &str) -> RefTarget {
        RefTarget::Lms {
            target: LmsPlayTarget::Library {
                kind: LmsSearchResultType::Album,
                id,
            },
            title: title.to_string(),
        }
    }

    fn apple_target(companion_id: &str, handle: &str, title: &str) -> RefTarget {
        RefTarget::AppleMusic {
            companion_id: companion_id.to_string(),
            handle: handle.to_string(),
            title: title.to_string(),
        }
    }

    // =========================================================================
    // Opacity: nothing about the target is derivable from the token
    // =========================================================================

    /// The core opacity claim, made concrete: neither a Roon `item_key`/session
    /// key nor an LMS entity id nor either target's title appears verbatim in
    /// the minted token. A base64-of-the-key design would fail this the moment
    /// the key were long enough to survive encoding, which is exactly the
    /// design #396's gate rejected in favor of a table.
    ///
    /// Needles are chosen to make a chance collision statistically
    /// impossible rather than merely unlikely: each is either long enough
    /// that a random 128-bit token matching it by coincidence would need
    /// astronomically bad luck, or contains a character (space, `:`) that
    /// never appears in the token's base64 URL-safe alphabet at all. A
    /// single-character or tiny needle would make this test genuinely flaky
    /// against a random token -- that is a property of a bad assertion, not
    /// of the opacity claim, so this test deliberately avoids them.
    #[tokio::test]
    async fn minted_tokens_carry_no_verbatim_target_data() {
        let table = RefTable::new();

        let roon_token = table
            .mint(roon_target(
                "109:7",
                "search_1730000000000000000",
                "Kind of Blue",
            ))
            .await;
        for needle in [
            "109:7",                      // contains ':' -- impossible in base64url
            "search_1730000000000000000", // 27 chars -- astronomically unlikely
            "Kind of Blue",               // contains ' ' -- impossible in base64url
        ] {
            assert!(
                !roon_token.contains(needle),
                "token {roon_token:?} must not contain {needle:?} verbatim"
            );
        }

        let lms_token = table.mint(lms_target(424_242, "Blue Train Album")).await;
        for needle in [
            "424242",           // 6 digits -- ~1 in 10^10 chance in a 22-char token
            "Blue Train Album", // contains ' ' -- impossible in base64url
        ] {
            assert!(
                !lms_token.contains(needle),
                "token {lms_token:?} must not contain {needle:?} verbatim"
            );
        }
    }

    /// A hand-mangled ref is refused, never misresolved to a *different* real
    /// target. Because the token space is 128 random bits, flipping a
    /// character almost certainly lands on a token nothing ever minted --
    /// which resolves to `None`, not to some other client's item.
    #[tokio::test]
    async fn a_hand_mangled_ref_is_refused_not_misresolved() {
        let table = RefTable::new();
        let token = table.mint(lms_target(1, "Some Album")).await;

        let mut mangled = token.clone();
        let last = mangled.pop().expect("token is non-empty");
        let flipped = if last == 'A' { 'B' } else { 'A' };
        mangled.push(flipped);

        assert_ne!(mangled, token);
        assert!(table.resolve(&mangled).await.is_none());
        // The real token still resolves -- mangling one ref does not corrupt
        // the table.
        assert!(table.resolve(&token).await.is_some());
    }

    /// Garbage that never came from `mint` at all -- not just a mutation of a
    /// real token -- is refused the same way.
    #[tokio::test]
    async fn an_unknown_token_is_refused() {
        let table = RefTable::new();
        assert!(table.resolve("ref_not-a-real-token").await.is_none());
        assert!(table.resolve("").await.is_none());
    }

    // =========================================================================
    // Unguessability and non-correlation
    // =========================================================================

    /// Two refs minted for the *same* item are never equal, and neither is a
    /// prefix, suffix, or otherwise-derivable transform of the other. This is
    /// the assertion the gate's own dissent (F9) said was necessary: a
    /// verbatim check alone would pass a design that base64-encodes the key,
    /// which leaks completely despite matching no verbatim substring test.
    #[tokio::test]
    async fn two_refs_for_the_same_item_are_unequal_and_uncorrelated() {
        let table = RefTable::new();

        let mut tokens = Vec::new();
        for _ in 0..20 {
            tokens.push(
                table
                    .mint(roon_target("42:1", "search_same", "Same Album"))
                    .await,
            );
        }

        // All distinct.
        let unique: std::collections::HashSet<&String> = tokens.iter().collect();
        assert_eq!(
            unique.len(),
            tokens.len(),
            "every mint must be unique: {tokens:?}"
        );

        // No pair shares a prefix or suffix longer than the constant "ref_"
        // tag -- a weak proxy for "not derivable from each other", cheap
        // enough to assert without a statistical test.
        for i in 0..tokens.len() {
            for j in (i + 1)..tokens.len() {
                let a = tokens[i].strip_prefix(TOKEN_PREFIX).unwrap();
                let b = tokens[j].strip_prefix(TOKEN_PREFIX).unwrap();
                let common_prefix = a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count();
                assert!(
                    common_prefix < 4,
                    "tokens {a:?} and {b:?} share a suspiciously long prefix ({common_prefix})"
                );
            }
        }
    }

    /// Holding one ref does not let a client derive another: minting a second,
    /// unrelated item does not produce a token related to the first by any
    /// simple transform (equality, containment, or a shared byte at a fixed
    /// offset across every pair -- the sort of pattern a broken RNG or a
    /// counter-based scheme would leak).
    #[tokio::test]
    async fn holding_one_ref_does_not_predict_another() {
        let table = RefTable::new();
        let a = table.mint(lms_target(1, "Album A")).await;
        let b = table.mint(lms_target(2, "Album B")).await;
        let c = table.mint(roon_target("1:1", "s", "Album C")).await;

        assert_ne!(a, b);
        assert_ne!(b, c);
        assert!(!b.contains(a.strip_prefix(TOKEN_PREFIX).unwrap()));
        assert!(!c.contains(a.strip_prefix(TOKEN_PREFIX).unwrap()));
    }

    // =========================================================================
    // Bounded size and time; eviction is expiry, not corruption
    // =========================================================================

    /// Minting past capacity evicts the oldest entry, and the evicted ref
    /// resolves exactly like a token that never existed -- `None`, not a
    /// wrong-but-plausible answer. That is "eviction observable as expiry, not
    /// corruption": there is no partial-resolution failure mode to have.
    #[tokio::test]
    async fn minting_past_capacity_evicts_the_oldest_as_a_clean_miss() {
        let table = RefTable::with_capacity_and_ttl(3, Duration::from_secs(60));

        let first = table.mint(lms_target(1, "Oldest")).await;
        let _second = table.mint(lms_target(2, "Middle")).await;
        let _third = table.mint(lms_target(3, "Newer")).await;
        assert_eq!(table.len().await, 3);

        // A fourth mint must evict the first without exceeding capacity.
        let fourth = table.mint(lms_target(4, "Newest")).await;
        assert_eq!(table.len().await, 3, "table must stay bounded at capacity");

        assert!(
            table.resolve(&first).await.is_none(),
            "the oldest ref must be evicted, not merely displaced"
        );
        assert!(table.resolve(&fourth).await.is_some());
    }

    /// The table never exceeds capacity even under many mints, and each
    /// eviction is a clean miss rather than a corrupted read of a *different*
    /// entry.
    #[tokio::test]
    async fn the_table_stays_bounded_under_sustained_minting() {
        let table = RefTable::with_capacity_and_ttl(5, Duration::from_secs(60));
        let mut tokens = Vec::new();
        for i in 0..50 {
            tokens.push(table.mint(lms_target(i, "Item")).await);
            assert!(
                table.len().await <= 5,
                "table exceeded capacity at mint {i}"
            );
        }

        // Only the most recent 5 remain resolvable; every earlier one is a
        // clean miss.
        for (i, token) in tokens.iter().enumerate() {
            let resolved = table.resolve(token).await;
            if i < 45 {
                assert!(resolved.is_none(), "mint {i} should have been evicted");
            } else {
                assert!(resolved.is_some(), "mint {i} should still be live");
            }
        }
    }

    /// `order` stays bounded even when it is expiry, not eviction, doing the
    /// shrinking -- a table whose entries always expire before the next mint
    /// never trips an `entries.len()`-only capacity check (`entries` is
    /// already near-empty every time `mint` runs), so without also gating on
    /// `order.len()` this would grow by one token per mint forever. CodeRabbit
    /// review on PR #427.
    #[tokio::test]
    async fn order_does_not_leak_when_expiry_outpaces_eviction() {
        let table = RefTable::with_capacity_and_ttl(5, Duration::from_millis(5));
        for i in 0..50 {
            let token = table.mint(lms_target(i, "Fleeting")).await;
            tokio::time::sleep(Duration::from_millis(10)).await;
            // Resolving a stale ref is what actually triggers `entries`'
            // lazy expiry removal (see `resolve`) -- exactly what happens
            // in production whenever a client looks up an old ref. Nothing
            // sweeps `entries` on its own, so without this the test would
            // only be proving mint() alone never shrinks anything.
            assert!(
                table.resolve(&token).await.is_none(),
                "token {i} should already have expired"
            );
        }

        assert!(
            table.len().await <= 1,
            "entries should stay near-empty since every token was resolved past its TTL before the next mint"
        );
        assert!(
            table.order_len().await <= 5,
            "order leaked to {} entries despite entries.len() never approaching capacity",
            table.order_len().await
        );
    }

    /// A ref past its TTL resolves to `None`, indistinguishable from one that
    /// was evicted for space or never existed -- proving expiry is real and
    /// not merely documented.
    #[tokio::test]
    async fn an_expired_ref_resolves_to_none() {
        let table = RefTable::with_capacity_and_ttl(DEFAULT_CAPACITY, Duration::from_millis(20));
        let token = table.mint(lms_target(1, "Fleeting")).await;
        assert!(
            table.resolve(&token).await.is_some(),
            "must resolve before it expires"
        );

        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            table.resolve(&token).await.is_none(),
            "must expire after its TTL"
        );
    }

    /// A ref well within its TTL keeps resolving -- the counterpart to the
    /// expiry test, so "expires eventually" and "does not expire early" are
    /// both pinned.
    #[tokio::test]
    async fn a_fresh_ref_resolves_repeatedly_within_its_ttl() {
        let table = RefTable::with_capacity_and_ttl(DEFAULT_CAPACITY, Duration::from_secs(60));
        let token = table.mint(lms_target(1, "Durable For Now")).await;
        for _ in 0..3 {
            assert!(table.resolve(&token).await.is_some());
        }
    }

    // =========================================================================
    // Provider identification, for cross-provider refusal
    // =========================================================================

    #[tokio::test]
    async fn resolved_target_reports_its_own_provider() {
        let table = RefTable::new();
        let roon_token = table.mint(roon_target("1:1", "s", "R")).await;
        let lms_token = table.mint(lms_target(1, "L")).await;
        let apple_token = table
            .mint(apple_target("iphone", "companion-handle-123", "A"))
            .await;

        assert_eq!(
            table.resolve(&roon_token).await.unwrap().provider(),
            Provider::Roon
        );
        assert_eq!(
            table.resolve(&lms_token).await.unwrap().provider(),
            Provider::Lms
        );
        let apple = table.resolve(&apple_token).await.unwrap();
        assert_eq!(apple.provider(), Provider::AppleMusic);
        assert_eq!(apple.title(), "A");
        assert!(matches!(
            apple,
            RefTarget::AppleMusic { companion_id, handle, .. }
                if companion_id == "iphone" && handle == "companion-handle-123"
        ));
    }

    #[tokio::test]
    async fn apple_refs_are_opaque_and_companion_scoped_in_the_table() {
        let table = RefTable::new();
        let token = table
            .mint(apple_target(
                "iphone",
                "apple-catalog-id-should-not-leak",
                "Song",
            ))
            .await;
        assert!(!token.contains("apple-catalog-id-should-not-leak"));
        let target = table.resolve(&token).await.unwrap();
        assert!(matches!(
            target,
            RefTarget::AppleMusic { companion_id, .. } if companion_id == "iphone"
        ));
    }

    // =========================================================================
    // ImageRefTable (#549): opaque, same as RefTable, but a separate table
    // so hifi_play_ref's dispatch never has to consider an image ref.
    // =========================================================================

    #[tokio::test]
    async fn image_ref_mints_and_resolves_to_its_zone_and_key() {
        let table = ImageRefTable::new();
        let token = table
            .mint(ImageRef {
                zone_id: "roon:zone1".to_string(),
                image_key: "abc123".to_string(),
            })
            .await;
        assert!(token.starts_with(TOKEN_PREFIX));
        let resolved = table.resolve(&token).await.unwrap();
        assert_eq!(resolved.zone_id, "roon:zone1");
        assert_eq!(resolved.image_key, "abc123");
    }

    #[tokio::test]
    async fn image_ref_token_carries_no_verbatim_image_key() {
        let table = ImageRefTable::new();
        let token = table
            .mint(ImageRef {
                zone_id: "lms:player1".to_string(),
                image_key: "a-very-long-and-distinctive-coverid-value-123456".to_string(),
            })
            .await;
        assert!(!token.contains("a-very-long-and-distinctive-coverid-value-123456"));
    }

    #[tokio::test]
    async fn unknown_image_token_resolves_to_none() {
        let table = ImageRefTable::new();
        assert!(table.resolve("ref_does-not-exist").await.is_none());
    }

    #[tokio::test]
    async fn image_ref_expires_after_its_ttl() {
        let table =
            ImageRefTable::with_capacity_and_ttl(DEFAULT_CAPACITY, Duration::from_millis(20));
        let token = table
            .mint(ImageRef {
                zone_id: "musicassistant:zone1".to_string(),
                image_key: "https://example.invalid/art.jpg".to_string(),
            })
            .await;
        assert!(table.resolve(&token).await.is_some());
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(table.resolve(&token).await.is_none());
    }

    #[tokio::test]
    async fn image_ref_table_stays_bounded_by_evicting_the_oldest() {
        let table = ImageRefTable::with_capacity_and_ttl(2, DEFAULT_TTL);
        let first = table
            .mint(ImageRef {
                zone_id: "roon:z".to_string(),
                image_key: "one".to_string(),
            })
            .await;
        table
            .mint(ImageRef {
                zone_id: "roon:z".to_string(),
                image_key: "two".to_string(),
            })
            .await;
        table
            .mint(ImageRef {
                zone_id: "roon:z".to_string(),
                image_key: "three".to_string(),
            })
            .await;
        assert!(table.resolve(&first).await.is_none(), "oldest is evicted");
        assert_eq!(table.len().await, 2);
    }
}
