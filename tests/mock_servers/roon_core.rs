//! `FakeRoonCore` — a Roon Core that actually speaks Roon's protocol.
//!
//! Issue #408. This is **not** [`super::roon::MockRoonCore`], which is an in-memory
//! state holder with no wire protocol. This is a real WebSocket server that speaks
//! the MOO framing and the `com.roonlabs.*` request/response surface that
//! `roon_api` (the pinned fork) expects, so `RoonAdapter` can be driven end to end
//! without a Roon Core and without a network.
//!
//! # What it is for
//!
//! Before this existed, no test asserted a Roon *success* path: if the adapter had
//! swapped `title` and `subtitle` on the way out of a search, nothing would have
//! failed. See `tests/roon_protocol.rs` for the tests that close that hole.
//!
//! # Wire protocol
//!
//! Everything below is read off the pinned fork
//! (`open-horizon-labs/rust-roon-api@ohc/main`, checkout `06dd807`), not invented.
//! Where a shape is also documented by RoonLabs or recorded off a real Core, the
//! PROVENANCE section below says so and the use site cites it:
//!
//! * Transport: WebSocket at `ws://<ip>:<port>/api`, binary frames (`moo.rs::Moo::new`).
//! * Frame: `MOO/1 <VERB> <NAME>\n` then `Request-Id: <n>\n`, optional
//!   `Content-Length` / `Content-Type: application/json`, a blank line, then the
//!   body (`moo.rs::Moo::create_msg_string`, `moo.rs::MooReceiver::parse`).
//! * Handshake: client sends `REQUEST com.roonlabs.registry:1/info` as
//!   request id 0. The Core answers with a body carrying `core_id`,
//!   `display_name`, `display_version`; the client then sends
//!   `REQUEST com.roonlabs.registry:1/register` and the Core answers
//!   `COMPLETE Registered` with a `token`. Only then does `CoreEvent::Registered`
//!   fire and `state.browse` become available (`lib.rs:405-524`).
//! * Zones: `COMPLETE`/`CONTINUE Subscribed` with `{"zones": [...]}` in reply to
//!   `com.roonlabs.transport:2/subscribe_zones` (`transport.rs:479-482`).
//! * Browse/load: `COMPLETE Success` with a `BrowseResult` or `LoadResult` body
//!   (`browse.rs:159-167`).
//! * Browse errors: a body-less `COMPLETE InvalidItemKey` (or `InvalidLevels`,
//!   `UnexpectedError`, `ZoneNotFound`) — the fork keys purely off the message
//!   *name* (`browse.rs:169-183`) and emits
//!   `Parsed::Error(RoonApiError::BrowseInvalidItemKey((req_id, session_key)))`.
//! * Keepalive: the fork's `MooReceiver::receive_response` gives up after 10s of
//!   silence and the connection is then reported as a lost Core
//!   (`moo.rs:245-249`, `lib.rs:646-660`), so this fake sends a
//!   `com.roonlabs.ping:1/ping` request every 3s, which the client's built-in
//!   Ping service answers (`lib.rs:857-871`).
//!
//! # PROVENANCE — read this before trusting a shape
//!
//! **No live Roon Core was reachable when this was written** — no pairing token
//! exists in this machine's config dir, and pairing requires a human to authorize
//! the extension in Roon → Settings → Extensions. So nothing here was recorded off
//! a Core *by this work*. Rather than hand-write shapes and let the fake agree with
//! its author, the uncertain ones were chased down in published sources. Four
//! pedigrees, and they are not equally trustworthy:
//!
//! | Pedigree | What it covers | Confidence |
//! |---|---|---|
//! | **From the fork** (`06dd807`) | which fields exist, required vs optional, the handshake order, the 10s read timeout | high — a shape the fork's `serde` accepts is a shape the adapter can consume |
//! | **From RoonLabs' published API** (`RoonLabs/node-roon-api-browse`, `node-roon-api`) | `list.level` "increases from 0"; `action` ∈ `message`/`none`/`list`/`replace_item`/`remove_item`; `hint` ∈ `null`/`action`/`action_list`/`list`/`header`; the `input_prompt` shape; `InvalidRequest` as a real reply to an unknown service | high — RoonLabs' own JSDoc, quoted at the use sites below |
//! | **Recorded off real Cores by third parties** | `MOO/1 COMPLETE InvalidItemKey` as a verbatim wire frame, and the `"<int>:<int>"` shape of a real `item_key` — see the `ItemKeyScope` docs for the citations | good — real Cores, but other people's logs, not a controlled run |
//! | **From this repo's adapter** | that the browse root contains `Library` / `TIDAL` / `Qobuz`, that each contains a `Search` item, that an action list contains `Play Now` / `Queue` / `Start Radio` | high as *expectations* — `src/adapters/roon.rs` will not work against anything else, and **that is not the same as Roon being that way** |
//! | **INFERRED** | root list title (`Explore`); whether a real Core sends a *body* with an error; whether the *other three* error names are spelled as the fork matches them; whether search results are category-grouped | **unverified** — each marked `INFERRED:` at its use site |
//!
//! Two entries are load-bearing:
//!
//! * **The error names.** The fork matches four string literals; anything else
//!   becomes `Parsed::None`, which it drops — so a Core that spells one differently
//!   makes the caller time out as if unreachable, with every test here still green.
//!   #405's PR flagged this as #408's to pin. `InvalidItemKey` is now **corroborated
//!   verbatim** from real Cores (see [`ItemKeyScope`]); `InvalidLevels`,
//!   `UnexpectedError` and `ZoneNotFound` are **not** — RoonLabs' published browse
//!   API documents errors only as "an error code or false if no error" and never
//!   enumerates them, and the three names appear nowhere in `node-roon-api`. So the
//!   consequence is pinned instead: [`FakeRoonCore::reject_item_key_with_name`] and
//!   [`FakeRoonCore::FORK_ERROR_NAMES`] make it testable.
//! * **`item_key` portability across sessions.** Configurable ([`ItemKeyScope`])
//!   rather than decided — and third-party evidence now points *against* the
//!   assumption this repo makes. See that type's docs.
//!
//! # What this fake does NOT prove
//!
//! It proves the adapter is self-consistent and that it drives `roon_api` correctly.
//! It **cannot** prove the adapter matches a real Roon Core. Individual shapes are
//! now backed by RoonLabs' published API or by logs off real Cores, which is better
//! than nothing — but the *navigation model* (a `Search` item under
//! `Library`/`TIDAL`/`Qobuz`, flat search results, an action list holding
//! `Play Now`) came from `src/adapters/roon.rs` itself, so on that the fake cannot
//! disagree with the adapter. Green here means "unchanged", not "correct". See
//! `tests/mock_servers/README.md` for the full covered/not-covered table.
//!
//! # Failing loudly rather than looking like coverage
//!
//! The `MockHqpServer` lesson (#394) was that a mock nothing asserts *against* is
//! worse than no mock — it had been rejecting its own adapter's wire format for
//! months and read as coverage. Three defences here:
//!
//! 1. Every request the adapter sends is recorded ([`FakeRoonCore::requests`]), so
//!    tests assert what went *out*, not only what came back.
//! 2. Anything this fake does not understand is recorded as unhandled and answered
//!    with `InvalidRequest`; [`FakeRoonCore::assert_no_unhandled_requests`] turns
//!    adapter drift into a test failure instead of a silent hang.
//! 3. The fake is validated by the real client library, never by a hand-written
//!    client: if its framing breaks, `roon_core_completes_handshake` fails first.

#![allow(dead_code)] // a shared test module: each test binary uses a subset

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

// =============================================================================
// Library model
// =============================================================================

/// Roon item hints, spelled as the fork deserializes them (`browse.rs::ItemHint`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hint {
    List,
    Action,
    ActionList,
    Header,
}

impl Hint {
    fn wire(self) -> &'static str {
        match self {
            // FROM FORK: browse.rs::ItemHint, #[serde(rename_all = "snake_case")].
            // Corroborated by RoonLabs' published JSDoc, which documents exactly
            // these five: null "Unknown", "action", "action_list", "list", "header".
            Hint::List => "list",
            Hint::Action => "action",
            Hint::ActionList => "action_list",
            Hint::Header => "header",
        }
    }
}

/// One node of the fake Core's browse tree.
#[derive(Debug, Clone)]
pub struct FakeItem {
    pub title: String,
    pub subtitle: Option<String>,
    pub hint: Option<Hint>,
    pub image_key: Option<String>,
    /// `Some(prompt)` marks an item that accepts `BrowseOpts::input` — Roon's
    /// `Search` entries. Browsing it *with* input yields search results;
    /// browsing it without yields `children`.
    pub input_prompt: Option<String>,
    /// Items served when this node is entered.
    pub children: Vec<FakeItem>,
    /// When false the Core mints no `item_key` for this item, which is how a real
    /// Core presents non-navigable rows (headers). Lets tests cover the adapter's
    /// "has no item_key" paths.
    pub keyed: bool,
}

impl FakeItem {
    pub fn new(title: &str) -> Self {
        Self {
            title: title.to_string(),
            subtitle: None,
            hint: None,
            image_key: None,
            input_prompt: None,
            children: Vec::new(),
            keyed: true,
        }
    }

    pub fn list(title: &str) -> Self {
        Self::new(title).with_hint(Hint::List)
    }

    pub fn action(title: &str) -> Self {
        Self::new(title).with_hint(Hint::Action)
    }

    pub fn action_list(title: &str) -> Self {
        Self::new(title).with_hint(Hint::ActionList)
    }

    pub fn with_hint(mut self, hint: Hint) -> Self {
        self.hint = Some(hint);
        self
    }

    pub fn with_subtitle(mut self, subtitle: &str) -> Self {
        self.subtitle = Some(subtitle.to_string());
        self
    }

    pub fn with_image_key(mut self, key: &str) -> Self {
        self.image_key = Some(key.to_string());
        self
    }

    pub fn searchable(mut self, prompt: &str) -> Self {
        self.input_prompt = Some(prompt.to_string());
        self
    }

    pub fn unkeyed(mut self) -> Self {
        self.keyed = false;
        self
    }

    pub fn with_children(mut self, children: Vec<FakeItem>) -> Self {
        self.children = children;
        self
    }
}

/// The three standard play actions, as an [`FakeItem`] list.
///
/// Titles come from `RoonAdapter::PlayAction::action_title()`
/// (`src/adapters/roon.rs:82-88`) — this repo's own expectation, not a guess.
pub fn play_actions() -> Vec<FakeItem> {
    vec![
        FakeItem::action("Play Now"),
        FakeItem::action("Queue"),
        FakeItem::action("Start Radio"),
    ]
}

/// An album whose page carries an action list, the way the adapter expects to
/// find playable content one level below a search hit.
pub fn album(title: &str, artist: &str, tracks: &[&str]) -> FakeItem {
    let mut children = vec![FakeItem::action_list("Play Album").with_children(play_actions())];
    for track in tracks {
        children.push(
            FakeItem::list(track)
                .with_subtitle(artist)
                .with_children(vec![
                    FakeItem::action_list("Play Track").with_children(play_actions())
                ]),
        );
    }
    FakeItem::list(title)
        .with_subtitle(artist)
        .with_children(children)
}

/// A playlist whose page puts an immediately-invokable `"Play Playlist"`
/// action directly alongside its tracks -- the exact shape issue #545's live
/// repro captured against a real Core (`Available: ["Play Playlist",
/// "Laundromat (Remastered 2017)", ...]`).
///
/// Deliberately different from [`album`]'s shape: an album's first child is
/// `action_list("Play Album")`, a *wrapper* that itself must be entered to
/// reach `play_actions()` (double-nested). A playlist's `"Play Playlist"` is
/// `action` hinted directly -- browsing its own `item_key` invokes it on the
/// spot (see `handle_browse`'s "Invoking an action does not produce a list"
/// case below), with no further menu to open. Getting this distinction
/// wrong is exactly what #545's play-matcher bug was: the adapter treated
/// every action-hinted row as a submenu wrapper and searched a level too
/// deep for a literal `"Play Now"`.
pub fn playlist(title: &str, tracks: &[&str]) -> FakeItem {
    let mut children = vec![FakeItem::action("Play Playlist")];
    for track in tracks {
        children.push(
            FakeItem::list(track)
                .with_subtitle(title)
                .with_children(vec![
                    FakeItem::action_list("Play Track").with_children(play_actions())
                ]),
        );
    }
    FakeItem::list(title).with_children(children)
}

/// An album level shaped like the #573 live crawl captured it: the "Play
/// Album" verb is an `action_list` wrapper, and -- crucially -- **the track
/// rows themselves are `action_list` hinted too** (entering a track opens
/// its Play Now/Queue/Start Radio menu), each carrying an artist subtitle
/// and artwork. The pre-#573 fixtures modeled tracks as `hint: list`, which
/// is why #545's "drop every action-hinted row" filter passed every test
/// while emptying every real album ("`{\"items\":[]}` for a 95-track
/// playlist").
pub fn album_live(title: &str, artist: &str, tracks: &[&str]) -> FakeItem {
    let mut children = vec![FakeItem::action_list("Play Album").with_children(play_actions())];
    for track in tracks {
        children.push(
            FakeItem::action_list(track)
                .with_subtitle(artist)
                .with_image_key(&format!("img_{}", track.to_lowercase().replace(' ', "_")))
                .with_children(play_actions()),
        );
    }
    FakeItem::list(title)
        .with_subtitle(artist)
        .with_image_key(&format!("img_{}", title.to_lowercase().replace(' ', "_")))
        .with_children(children)
}

/// A playlist level shaped like the #573 live crawl: an immediately-invokable
/// `"Play Playlist"` action alongside `action_list`-hinted track rows with
/// artist subtitles (the same live shape #545's repro captured, now with the
/// track hint modeled correctly -- see [`album_live`]).
pub fn playlist_live(title: &str, tracks: &[(&str, &str)]) -> FakeItem {
    let mut children = vec![FakeItem::action("Play Playlist")];
    for (track, artist) in tracks {
        children.push(
            FakeItem::action_list(track)
                .with_subtitle(artist)
                .with_children(play_actions()),
        );
    }
    FakeItem::list(title).with_children(children)
}

/// A live-radio station row, shaped exactly as the operator's real Core
/// served one under "My Live Radio" (issue #587, recorded 2026-08 via the
/// raw `/roon/browse` endpoint against the production install):
///
/// ```json
/// {"title":"WOSU-HD2 WOSU Public Media: Classical 101",
///  "subtitle":"Columbus, Ohio, USA FM 89.7 HD2 English",
///  "item_key":"1646:0","hint":"action","image_key":"afd6..."}
/// ```
///
/// The load-bearing part is `hint: action` -- browsing a station **plays it
/// immediately** (no Play Now menu below it), which is why it has no
/// children here. #587 was exactly this row being mistaken for a play-verb
/// row and filtered out of `hifi_collections` listings.
pub fn radio_station(title: &str, subtitle: &str) -> FakeItem {
    FakeItem::action(title)
        .with_subtitle(subtitle)
        .with_image_key(&format!("img_{}", title.to_lowercase().replace(' ', "_")))
}

/// The fake Core's library: a browse root plus a flat set of searchable items.
#[derive(Debug, Clone)]
pub struct FakeLibrary {
    /// INFERRED: a real Core's browse root list is titled "Explore". Cosmetic —
    /// nothing in this repo reads it.
    pub root_title: String,
    /// Top-level rows. `RoonAdapter::search` requires one titled `Library`,
    /// `TIDAL` or `Qobuz` (`src/adapters/roon.rs:686-700`).
    pub root_items: Vec<FakeItem>,
    /// Candidate results, matched by case-insensitive substring against title and
    /// subtitle. Keyed by the source name they are reachable through.
    pub search_results: HashMap<String, Vec<FakeItem>>,
}

impl FakeLibrary {
    /// A library big enough for search, two-level browse, paging and play_item.
    pub fn standard() -> Self {
        let mut search_results = HashMap::new();
        search_results.insert(
            "Library".to_string(),
            vec![
                album(
                    "Kind of Blue",
                    "Miles Davis",
                    &["So What", "Blue in Green", "Flamenco Sketches"],
                ),
                album(
                    "Blue Train",
                    "John Coltrane",
                    &["Blue Train", "Moment's Notice"],
                ),
            ],
        );
        search_results.insert(
            "TIDAL".to_string(),
            vec![album(
                "Blue Note Reimagined",
                "Various Artists",
                &["Footprints"],
            )],
        );

        Self {
            root_title: "Explore".to_string(),
            root_items: vec![
                FakeItem::list("Library").with_children(vec![
                    FakeItem::list("Search").searchable("Search"),
                    FakeItem::list("Artists").with_children(
                        (1..=25)
                            .map(|n| FakeItem::list(&format!("Artist {n:02}")))
                            .collect(),
                    ),
                    // Deliberately not one of the search results, so that every
                    // title in this library is unambiguous.
                    FakeItem::list("Albums").with_children(vec![album(
                        "Sketches of Spain",
                        "Miles Davis",
                        &["Solea"],
                    )]),
                ]),
                FakeItem::list("TIDAL")
                    .with_children(vec![FakeItem::list("Search").searchable("Search TIDAL")]),
                FakeItem::list("Qobuz")
                    .with_children(vec![FakeItem::list("Search").searchable("Search Qobuz")]),
                FakeItem::new("Settings").unkeyed(),
            ],
            search_results,
        }
    }
}

// =============================================================================
// Behaviour knobs
// =============================================================================

/// Whether an `item_key` minted inside one `multi_session_key` resolves inside
/// another.
///
/// **This is the epic's open empirical question** (#392, #405):
/// `RoonAdapter::play_item` mints a fresh, unrelated session key and browses the
/// caller's key inside it (`src/adapters/roon.rs:1105-1122`), so the repo already
/// assumes [`ItemKeyScope::Global`] — but `/roon/play_item` has no in-repo callers
/// and no test, so it may never have worked. The fake refuses to decide.
///
/// # Third-party evidence points against the repo's assumption
///
/// Not a controlled experiment, and not this repo's code path — but recorded off
/// real Roon Cores, which is more than anything else here has:
///
/// * Home Assistant issue `home-assistant/core#137605`: playing a Roon playlist
///   from a script "works successfully on the first attempt but fails on the second
///   attempt", logging `Could not play id:122:4, result: MOO/1 COMPLETE
///   InvalidItemKey` (and the same for `130:12`, `133:13`, `109:7`, `135:5`). Same
///   shape as UHC's `play_item`: hold a key from an earlier lookup, browse it later.
/// * Roon Labs community thread 23129: `item_key`s "change with each refresh" —
///   `"115:0"` becoming `"116:0"`.
///
/// Read together: a key from an earlier lookup does **not** reliably resolve later.
/// That is [`ItemKeyScope::PerSession`] behaviour, and if it is right then
/// `/roon/play_item` is broken as #405 feared and #396's ref design changes.
///
/// The default here stays `Global` because that is what the adapter's code assumes,
/// and these tests describe the adapter. **Do not read the default as a claim about
/// Roon.** `a_foreign_item_key_is_rejected_when_keys_are_session_scoped` exercises
/// the other setting; flip the default once the operator's rig settles it.
///
/// Two caveats against over-reading the citations: Home Assistant's integration is
/// not UHC and may reuse keys differently, and neither source states the *rule* —
/// only that reuse fails in practice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKeyScope {
    /// Any session may use any key. Matches what this repo assumes today. Default —
    /// see the type docs: that is the adapter's assumption, not a fact about Roon.
    Global,
    /// A key only resolves in the session that minted it; elsewhere the Core
    /// answers `InvalidItemKey`.
    PerSession,
}

/// One MOO request as it arrived, for assertions about what the adapter sent.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub req_id: usize,
    /// e.g. `com.roonlabs.browse:1/browse`
    pub name: String,
    pub body: Value,
    /// True when this fake had no handler for `name`.
    pub unhandled: bool,
}

impl RecordedRequest {
    pub fn session_key(&self) -> Option<&str> {
        self.body.get("multi_session_key")?.as_str()
    }

    pub fn item_key(&self) -> Option<&str> {
        self.body.get("item_key")?.as_str()
    }

    pub fn input(&self) -> Option<&str> {
        self.body.get("input")?.as_str()
    }

    pub fn pop_all(&self) -> bool {
        self.body
            .get("pop_all")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}

// =============================================================================
// Internals
// =============================================================================

/// Flattened library. `item_key` is an index into `nodes`, wrapped in a
/// per-Core-instance nonce so tests cannot hardcode keys — they must read them
/// out of a load result, exactly as a client does.
struct Arena {
    nodes: Vec<ArenaNode>,
    root_children: Vec<usize>,
    /// source name -> candidate result indices
    search_results: HashMap<String, Vec<usize>>,
    nonce: u32,
}

struct ArenaNode {
    title: String,
    subtitle: Option<String>,
    hint: Option<Hint>,
    image_key: Option<String>,
    input_prompt: Option<String>,
    children: Vec<usize>,
    keyed: bool,
}

impl Arena {
    fn build(library: &FakeLibrary, nonce: u32) -> Self {
        let mut nodes = Vec::new();
        let root_children = library
            .root_items
            .iter()
            .map(|item| arena_insert(&mut nodes, item))
            .collect();
        let mut search_results = HashMap::new();
        for (source, items) in &library.search_results {
            let indices = items
                .iter()
                .map(|item| arena_insert(&mut nodes, item))
                .collect();
            search_results.insert(source.clone(), indices);
        }
        Self {
            nodes,
            root_children,
            search_results,
            nonce,
        }
    }

    fn key_of(&self, index: usize) -> String {
        // RECORDED (third-party): real keys are two colon-separated integers —
        // `122:4`, `130:12`, `115:0` — from home-assistant/core#137605 logs and
        // Roon Labs community thread 23129. Same shape as this. Only their opacity
        // matters to the adapter; the nonce keeps tests from hardcoding them.
        format!("{}:{}", self.nonce, index)
    }

    fn index_of(&self, key: &str) -> Option<usize> {
        let (nonce, index) = key.split_once(':')?;
        if nonce.parse::<u32>().ok()? != self.nonce {
            return None;
        }
        let index = index.parse::<usize>().ok()?;
        if index < self.nodes.len() {
            Some(index)
        } else {
            None
        }
    }

    fn find_by_title(&self, title: &str) -> Option<usize> {
        self.nodes.iter().position(|n| n.title == title)
    }
}

fn arena_insert(nodes: &mut Vec<ArenaNode>, item: &FakeItem) -> usize {
    let children = item
        .children
        .iter()
        .map(|child| arena_insert(nodes, child))
        .collect();
    nodes.push(ArenaNode {
        title: item.title.clone(),
        subtitle: item.subtitle.clone(),
        hint: item.hint,
        image_key: item.image_key.clone(),
        input_prompt: item.input_prompt.clone(),
        children,
        keyed: item.keyed,
    });
    nodes.len() - 1
}

/// One browse level: the list a subsequent `load` returns.
#[derive(Debug, Clone)]
struct Level {
    title: String,
    items: Vec<usize>,
}

#[derive(Default)]
struct Session {
    levels: Vec<Level>,
    /// Keys *this session itself* has minted (via `handle_load`), each
    /// tagged with the epoch (see [`Self::epoch`]) it was minted in.
    ///
    /// Deliberately **not** a simple "is this key currently valid" set: a key
    /// this session has never minted at all (reused from a different
    /// session) must fall through to the existing cross-session checks
    /// (`CoreState::minted` / [`ItemKeyScope`]) unaffected by anything here —
    /// `play_item` legitimately mints a fresh, unrelated session and browses
    /// a foreign key inside it under [`ItemKeyScope::Global`], and that must
    /// keep working. This map only ever *adds* a restriction for keys this
    /// exact session minted and then invalidated by popping past them.
    minted_at_epoch: HashMap<String, u64>,
    /// Incremented on every `pop_all`. Verified live against a real Core
    /// (nuc14, Roon 2.70, #396's ship-gate re-review): `pop_all` invalidates
    /// every key minted at the levels it discards, not merely the client's
    /// conceptual position -- a key this session minted in an earlier epoch
    /// is `InvalidItemKey` now, even though the global arena still contains
    /// it and even under [`ItemKeyScope::Global`].
    epoch: u64,
}

struct CoreState {
    arena: Arena,
    sessions: HashMap<String, Session>,
    /// item_key -> sessions that have been served that key
    minted: HashMap<String, HashSet<String>>,
    item_key_scope: ItemKeyScope,
    /// item_keys the Core will reject with `InvalidItemKey`
    rejected_keys: HashSet<String>,
    /// Per-key override of the rejection's message *name*.
    rejection_names: HashMap<String, String>,
    /// One-shot: the next browse, whatever it is, is rejected.
    reject_next_browse: bool,
    /// Applied before every response.
    delay: Duration,
    /// Per-item_key delay override, so a test can force responses to arrive out
    /// of order and prove correlation is by request, not by arrival.
    key_delays: HashMap<String, Duration>,
    /// Per-`level` delay override for `load` requests, which carry no item_key
    /// (issue #416). Same purpose as `key_delays`, for the load side.
    level_delays: HashMap<u64, Duration>,
    zones: Vec<Value>,
    log: Vec<RecordedRequest>,
    /// What this Core answered, in send order: (request id, message name).
    /// Without this, "the adapter hung" and "the Core never replied" are
    /// indistinguishable — and that ambiguity is exactly what #405 is about.
    sent: Vec<(usize, String)>,
    /// Responses that were browse/load errors, as (request id, name, session key).
    /// The fork's `Parsed::Error` payload carries exactly this pair, so a test can
    /// assert the Core correlated its refusal to the right request and session even
    /// though today's adapter throws the payload away.
    errors: Vec<(usize, String, String)>,
    /// Session keys of every browse request that combined `pop_all: true`
    /// with a present `item_key` -- see the `handle_browse` comment at that
    /// check. Empty is the passing state for any test asserting this
    /// combination was never sent.
    illegal_pop_all_with_item_key: Vec<String>,
    core_id: String,
    display_name: String,
    display_version: String,
    /// The zone subscription's `(req_id, writer)`, captured when
    /// `subscribe_zones` completes (#509). `group_outputs`/`ungroup_outputs`
    /// push their effect on this same subscription, mirroring a real Core --
    /// see the `SubscribeZones` handler for why it must be this req_id.
    zone_push: Option<(usize, Writer)>,
}

// =============================================================================
// The fake
// =============================================================================

/// Token handed out by `com.roonlabs.registry:1/register`.
const REGISTRATION_TOKEN: &str = "fake-token-408";

pub struct FakeRoonCore {
    addr: SocketAddr,
    state: Arc<RwLock<CoreState>>,
    handle: JoinHandle<()>,
}

impl FakeRoonCore {
    /// Start a fake Core on a random loopback port with the standard library.
    pub async fn start() -> Self {
        Self::start_with(FakeLibrary::standard()).await
    }

    pub async fn start_with(library: FakeLibrary) -> Self {
        // Nonce derived from the port keeps keys distinct between concurrently
        // running fakes without pulling in a RNG.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let nonce = u32::from(addr.port());

        let state = Arc::new(RwLock::new(CoreState {
            arena: Arena::build(&library, nonce),
            sessions: HashMap::new(),
            minted: HashMap::new(),
            item_key_scope: ItemKeyScope::Global,
            rejected_keys: HashSet::new(),
            rejection_names: HashMap::new(),
            reject_next_browse: false,
            delay: Duration::ZERO,
            key_delays: HashMap::new(),
            level_delays: HashMap::new(),
            zones: vec![default_zone("zone_fake_1", "Fake Living Room")],
            log: Vec::new(),
            sent: Vec::new(),
            errors: Vec::new(),
            illegal_pop_all_with_item_key: Vec::new(),
            // Per-instance, so two fakes running concurrently are distinct Cores.
            // Tests read it back via `core_id()` rather than hardcoding it.
            core_id: format!("fake-core-408-{}", addr.port()),
            display_name: "Fake Roon Core".to_string(),
            display_version: "2.0.408".to_string(),
            zone_push: None,
        }));
        let root_title = library.root_title.clone();

        let state_for_task = state.clone();
        let handle = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let state = state_for_task.clone();
                let root_title = root_title.clone();
                tokio::spawn(async move {
                    serve_connection(stream, state, root_title).await;
                });
            }
        });

        Self {
            addr,
            state,
            handle,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn ip(&self) -> IpAddr {
        self.addr.ip()
    }

    pub fn port(&self) -> String {
        self.addr.port().to_string()
    }

    // ---- behaviour knobs ---------------------------------------------------

    /// Delay every response by `delay`, so several adapter requests are genuinely
    /// in flight at once.
    pub async fn set_delay(&self, delay: Duration) {
        self.state.write().await.delay = delay;
    }

    /// Delay only browses carrying `item_key`, so responses can be forced to
    /// arrive out of order.
    pub async fn set_delay_for_item_key(&self, item_key: &str, delay: Duration) {
        self.state
            .write()
            .await
            .key_delays
            .insert(item_key.to_string(), delay);
    }

    /// Delay only loads carrying `level`, so *load* responses can be forced to
    /// arrive out of order (issue #416).
    ///
    /// The `item_key` hook above cannot do this: a `load` request carries no item
    /// key. `LoadOpts::level` is the field that selects which level of the session's
    /// stack is paged, so it is the load-side analogue - it names the content the
    /// response will carry, which is exactly what a correlation test needs to hold
    /// fixed while it varies arrival order.
    pub async fn set_delay_for_load_level(&self, level: u32, delay: Duration) {
        self.state
            .write()
            .await
            .level_delays
            .insert(level as u64, delay);
    }

    /// Answer `InvalidItemKey` for every browse carrying this key.
    pub async fn reject_item_key(&self, item_key: &str) {
        self.state
            .write()
            .await
            .rejected_keys
            .insert(item_key.to_string());
    }

    /// Answer `InvalidItemKey` for the next browse, whatever it carries.
    pub async fn reject_next_browse(&self) {
        self.state.write().await.reject_next_browse = true;
    }

    /// Answer browses carrying `item_key` with an arbitrary error *name*.
    ///
    /// The four names the fork recognises are `InvalidItemKey`, `InvalidLevels`,
    /// `UnexpectedError` and `ZoneNotFound` (`browse.rs:169-183`). **Whether a real
    /// Roon Core spells them that way is unverified** — the fork's own literals are
    /// the only evidence, and #405's PR flags the same gap. Any other name yields
    /// `Parsed::None`, which the fork drops, so the caller times out exactly as it
    /// did before #405 — with every test still green. This knob exists so that
    /// consequence can be pinned rather than merely described; see
    /// `an_unrecognised_error_name_degrades_to_an_indistinguishable_timeout`.
    pub async fn reject_item_key_with_name(&self, item_key: &str, error_name: &str) {
        let mut state = self.state.write().await;
        state.rejected_keys.insert(item_key.to_string());
        state
            .rejection_names
            .insert(item_key.to_string(), error_name.to_string());
    }

    /// The four browse error names the pinned fork recognises, in the order they
    /// appear in `browse.rs`. Pinned by a test so a fork bump that renames one is
    /// visible here rather than as a mysterious timeout.
    ///
    /// Corroboration status, because they are not equal:
    /// * `InvalidItemKey` — **recorded verbatim** off real Cores as
    ///   `MOO/1 COMPLETE InvalidItemKey` (home-assistant/core#137605).
    /// * `InvalidLevels`, `UnexpectedError`, `ZoneNotFound` — **unverified.**
    ///   RoonLabs' published browse API documents errors only as "an error code or
    ///   false if no error" and never enumerates them; none of the three appears in
    ///   `node-roon-api`. A Roon Labs community thread shows the prose "Zone not
    ///   found", which is not evidence of the wire spelling.
    pub const FORK_ERROR_NAMES: [&'static str; 4] = [
        "InvalidItemKey",
        "InvalidLevels",
        "UnexpectedError",
        "ZoneNotFound",
    ];

    pub async fn set_item_key_scope(&self, scope: ItemKeyScope) {
        self.state.write().await.item_key_scope = scope;
    }

    /// Replace the children of the node titled `parent_title`, mid-run.
    ///
    /// This is how a test models the library changing under a live Core --
    /// e.g. the operator adding a radio station in Roon's own app while UHC
    /// is connected (#587). A real Core serves whatever the level contains
    /// *at browse time* (this fake likewise snapshots a level's children
    /// when the level is entered, in `handle_browse`), so the change is
    /// visible to any browse that happens after the mutation and invisible
    /// to loads of a level that was entered before it.
    ///
    /// Panics when no node carries that title, so a typo fails the test
    /// loudly instead of mutating nothing.
    pub async fn set_children_by_title(&self, parent_title: &str, children: Vec<FakeItem>) {
        let mut state = self.state.write().await;
        let parent = state
            .arena
            .find_by_title(parent_title)
            .unwrap_or_else(|| panic!("no node titled {parent_title:?} in the fake library"));
        let indices: Vec<usize> = children
            .iter()
            .map(|child| arena_insert(&mut state.arena.nodes, child))
            .collect();
        state.arena.nodes[parent].children = indices;
    }

    pub async fn set_zones(&self, zones: Vec<Value>) {
        self.state.write().await.zones = zones;
    }

    pub async fn core_name(&self) -> String {
        self.state.read().await.display_name.clone()
    }

    /// The `core_id` this Core reports, unique per instance.
    pub async fn core_id(&self) -> String {
        self.state.read().await.core_id.clone()
    }

    /// The token this Core hands out on `register`.
    pub fn token(&self) -> &'static str {
        REGISTRATION_TOKEN
    }

    // ---- observation -------------------------------------------------------

    /// The `item_key` this Core mints for the (first) item with this title.
    /// Tests use it to name a key for [`Self::reject_item_key`]; they must not
    /// construct keys themselves.
    pub async fn key_for_title(&self, title: &str) -> Option<String> {
        let state = self.state.read().await;
        state
            .arena
            .find_by_title(title)
            .map(|i| state.arena.key_of(i))
    }

    /// The title of the item a given `item_key` points at.
    ///
    /// Prefer this over [`Self::key_for_title`] when a title is not unique — a
    /// library has one `Play Now` per action list, so asserting on the *key* the
    /// adapter sent is ambiguous while asserting on the title it resolves to is not.
    pub async fn title_for_key(&self, item_key: &str) -> Option<String> {
        let state = self.state.read().await;
        let index = state.arena.index_of(item_key)?;
        Some(state.arena.nodes[index].title.clone())
    }

    /// Titles of the items the adapter browsed into, in order. Browses that carry
    /// no `item_key` (a `pop_all` to the root) are omitted.
    pub async fn browsed_titles(&self) -> Vec<String> {
        let state = self.state.read().await;
        state
            .log
            .iter()
            .filter(|r| r.name == "com.roonlabs.browse:1/browse")
            .filter_map(|r| r.item_key())
            .filter_map(|key| {
                let index = state.arena.index_of(key)?;
                Some(state.arena.nodes[index].title.clone())
            })
            .collect()
    }

    /// Every MOO request the adapter sent, in arrival order.
    pub async fn requests(&self) -> Vec<RecordedRequest> {
        self.state.read().await.log.clone()
    }

    /// Requests to `com.roonlabs.browse:1/browse`, in arrival order.
    pub async fn browse_requests(&self) -> Vec<RecordedRequest> {
        self.requests_named("com.roonlabs.browse:1/browse").await
    }

    /// Requests to `com.roonlabs.browse:1/load`, in arrival order.
    pub async fn load_requests(&self) -> Vec<RecordedRequest> {
        self.requests_named("com.roonlabs.browse:1/load").await
    }

    pub async fn requests_named(&self, name: &str) -> Vec<RecordedRequest> {
        self.state
            .read()
            .await
            .log
            .iter()
            .filter(|r| r.name == name)
            .cloned()
            .collect()
    }

    /// Every response this Core sent, as `(request id, message name)`.
    ///
    /// This is what distinguishes "the Core never answered" from "the Core
    /// answered and the adapter dropped it" — the exact ambiguity #405 exists to
    /// remove. Without it, a hang looks the same either way.
    pub async fn responses(&self) -> Vec<(usize, String)> {
        self.state.read().await.sent.clone()
    }

    /// Browse/load refusals this Core sent, as `(request id, name, session key)`.
    pub async fn errors_sent(&self) -> Vec<(usize, String, String)> {
        self.state.read().await.errors.clone()
    }

    /// Session keys of every browse request that illegally combined
    /// `pop_all: true` with a present `item_key` -- see `handle_browse`'s
    /// comment on that check, and #396's ship-gate re-review for the live
    /// evidence this hangs against a real Core. Empty is the passing state.
    pub async fn illegal_pop_all_with_item_key_attempts(&self) -> Vec<String> {
        self.state
            .read()
            .await
            .illegal_pop_all_with_item_key
            .clone()
    }

    /// Names of the responses sent, in order.
    pub async fn response_names(&self) -> Vec<String> {
        self.state
            .read()
            .await
            .sent
            .iter()
            .map(|(_, name)| name.clone())
            .collect()
    }

    /// Requests this fake did not understand. Non-empty means the adapter has
    /// grown a call this fake does not model — drift, and a test failure, rather
    /// than a silent hang.
    pub async fn unhandled_requests(&self) -> Vec<RecordedRequest> {
        self.state
            .read()
            .await
            .log
            .iter()
            .filter(|r| r.unhandled)
            .cloned()
            .collect()
    }

    /// Fail if the adapter sent anything this fake does not model.
    pub async fn assert_no_unhandled_requests(&self) {
        let unhandled = self.unhandled_requests().await;
        assert!(
            unhandled.is_empty(),
            "FakeRoonCore received request(s) it does not model: {:?}\n\
             The adapter's protocol surface changed. Teach this fake the new call \
             rather than deleting the assertion.",
            unhandled
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>()
        );
    }

    /// Session keys the adapter has opened, in first-seen order.
    pub async fn session_keys(&self) -> Vec<String> {
        let mut seen = Vec::new();
        for req in &self.state.read().await.log {
            if let Some(key) = req.session_key() {
                if !seen.iter().any(|k| k == key) {
                    seen.push(key.to_string());
                }
            }
        }
        seen
    }

    /// Titles of the levels currently stacked in a session, root first. Lets a
    /// test assert that `pop_all` really reset the stack.
    pub async fn session_levels(&self, session_key: &str) -> Vec<String> {
        self.state
            .read()
            .await
            .sessions
            .get(session_key)
            .map(|s| s.levels.iter().map(|l| l.title.clone()).collect())
            .unwrap_or_default()
    }

    pub async fn stop(self) {
        self.handle.abort();
    }
}

/// A zone with every field `roon_api`'s `transport::Zone` requires.
///
/// FROM FORK: the required-field list is `transport.rs:94-107` (`Zone`),
/// `:112-120` (`Output`), `:127-131` (`Settings`), `:36-47` (`Volume`). If a
/// required field is missing the fork's deserializer errors and the client library
/// swallows the zone, so this is asserted by `roon_core_publishes_zones`.
pub fn default_zone(zone_id: &str, display_name: &str) -> Value {
    json!({
        "zone_id": zone_id,
        "display_name": display_name,
        "state": "stopped",
        "is_next_allowed": true,
        "is_previous_allowed": true,
        "is_pause_allowed": false,
        "is_play_allowed": true,
        "is_seek_allowed": false,
        "queue_items_remaining": 0,
        "queue_time_remaining": 0,
        "now_playing": null,
        "settings": { "loop": "disabled", "shuffle": false, "auto_radio": false },
        "outputs": [{
            "output_id": format!("{zone_id}_output"),
            "zone_id": zone_id,
            "can_group_with_output_ids": [],
            "display_name": display_name,
            "source_controls": null,
            "volume": {
                "type": "number",
                "min": 0.0,
                "max": 100.0,
                "value": 50.0,
                "step": 1.0,
                "is_muted": false
            }
        }]
    })
}

/// A single-output zone with an explicit output id and grouping
/// compatibility list (issue #509), for tests that need several distinct
/// zones whose outputs can (or deliberately cannot) be grouped together.
/// `default_zone` above always derives `"{zone_id}_output"` and leaves
/// `can_group_with_output_ids` empty, which cannot express either.
pub fn zone_with_grouping(
    zone_id: &str,
    display_name: &str,
    output_id: &str,
    can_group_with_output_ids: &[&str],
) -> Value {
    json!({
        "zone_id": zone_id,
        "display_name": display_name,
        "state": "stopped",
        "is_next_allowed": true,
        "is_previous_allowed": true,
        "is_pause_allowed": false,
        "is_play_allowed": true,
        "is_seek_allowed": false,
        "queue_items_remaining": 0,
        "queue_time_remaining": 0,
        "now_playing": null,
        "settings": { "loop": "disabled", "shuffle": false, "auto_radio": false },
        "outputs": [{
            "output_id": output_id,
            "zone_id": zone_id,
            "can_group_with_output_ids": can_group_with_output_ids,
            "display_name": display_name,
            "source_controls": null,
            "volume": {
                "type": "number",
                "min": 0.0,
                "max": 100.0,
                "value": 50.0,
                "step": 1.0,
                "is_muted": false
            }
        }]
    })
}

// =============================================================================
// MOO framing
// =============================================================================

/// A parsed inbound MOO message.
struct MooRequest {
    req_id: usize,
    /// `service/name`, e.g. `com.roonlabs.browse:1/browse`
    name: String,
    body: Value,
}

/// Mirror of `moo.rs::MooReceiver::parse` for the server side.
fn parse_moo(data: &[u8]) -> Option<MooRequest> {
    let split = data.windows(2).position(|w| w == b"\n\n")?;
    let header = std::str::from_utf8(&data[..split]).ok()?;
    let body_bytes = &data[split + 2..];

    let mut lines = header.split('\n');
    let first = lines.next()?;
    let rest = first.strip_prefix("MOO/1 ")?;
    let (verb, name) = rest.split_once(' ')?;
    if verb != "REQUEST" {
        return None;
    }

    let mut req_id = None;
    let mut content_type = None;
    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            match key.trim() {
                "Request-Id" => req_id = value.trim().parse::<usize>().ok(),
                "Content-Type" => content_type = Some(value.trim().to_string()),
                _ => {}
            }
        }
    }

    let body = if content_type.as_deref() == Some("application/json") {
        serde_json::from_slice(body_bytes).unwrap_or(Value::Null)
    } else {
        Value::Null
    };

    Some(MooRequest {
        req_id: req_id?,
        name: name.to_string(),
        body,
    })
}

/// Mirror of `moo.rs::Moo::create_msg_string`.
fn encode_moo(verb: &str, name: &str, req_id: usize, body: Option<&Value>) -> Vec<u8> {
    let mut out = format!("MOO/1 {verb} {name}\nRequest-Id: {req_id}\n");
    match body {
        Some(body) => {
            let body = body.to_string();
            out.push_str(&format!(
                "Content-Length: {}\nContent-Type: application/json\n\n",
                body.len()
            ));
            out.push_str(&body);
        }
        None => out.push('\n'),
    }
    out.into_bytes()
}

type Writer = Arc<Mutex<futures::stream::SplitSink<WebSocketStream<TcpStream>, Message>>>;

async fn send(writer: &Writer, verb: &str, name: &str, req_id: usize, body: Option<&Value>) {
    let frame = encode_moo(verb, name, req_id, body);
    let _ = writer.lock().await.send(Message::Binary(frame)).await;
}

/// Send a response *and record it*, so a test can tell "the Core answered
/// `InvalidItemKey`" apart from "the Core never answered".
async fn respond(
    state: &Arc<RwLock<CoreState>>,
    writer: &Writer,
    verb: &str,
    name: &str,
    req_id: usize,
    body: Option<&Value>,
) {
    state.write().await.sent.push((req_id, name.to_string()));
    send(writer, verb, name, req_id, body).await;
}

/// Refuse a browse/load, recording the `(req_id, name, session_key)` triple the
/// fork's `Parsed::Error` payload carries.
async fn refuse(
    state: &Arc<RwLock<CoreState>>,
    writer: &Writer,
    name: &str,
    req_id: usize,
    session_key: &str,
) {
    {
        let mut st = state.write().await;
        st.sent.push((req_id, name.to_string()));
        st.errors
            .push((req_id, name.to_string(), session_key.to_string()));
    }
    // FROM FORK: browse.rs:169-183 keys purely off the message name.
    // RECORDED (third-party): home-assistant/core#137605 logs the whole reply as
    // `MOO/1 COMPLETE InvalidItemKey` — verb, name, and nothing else — which is
    // exactly this frame.
    // INFERRED: that a real Core attaches no body at all. None is sent here, since
    // a body that parsed as BrowseResult/LoadResult would be taken for success.
    send(writer, "COMPLETE", name, req_id, None).await;
}

// =============================================================================
// Connection handling
// =============================================================================

async fn serve_connection(stream: TcpStream, state: Arc<RwLock<CoreState>>, root_title: String) {
    let ws = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(_) => return,
    };
    let (write, mut read) = ws.split();
    let writer: Writer = Arc::new(Mutex::new(write));

    // Keepalive: the fork's reader times out after 10s of silence and the client
    // then reports a lost Core (moo.rs:245-249). The client answers ping with its
    // built-in Ping service (lib.rs:857-871).
    let ping_writer = writer.clone();
    let ping = tokio::spawn(async move {
        let mut req_id = 1_000_000;
        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;
            req_id += 1;
            send(
                &ping_writer,
                "REQUEST",
                "com.roonlabs.ping:1/ping",
                req_id,
                None,
            )
            .await;
        }
    });

    while let Some(Ok(msg)) = read.next().await {
        let data = match msg {
            Message::Binary(data) => data,
            Message::Text(text) => text.into_bytes(),
            Message::Close(_) => break,
            _ => continue,
        };
        let Some(request) = parse_moo(&data) else {
            continue;
        };

        // One task per request: a real Core pipelines, and serialising here would
        // make it impossible to have several adapter requests in flight, which is
        // exactly what #405's correlation tests need.
        let state = state.clone();
        let writer = writer.clone();
        let root_title = root_title.clone();
        tokio::spawn(async move {
            handle_request(request, state, writer, root_title).await;
        });
    }

    ping.abort();
}

async fn handle_request(
    request: MooRequest,
    core: Arc<RwLock<CoreState>>,
    writer: Writer,
    root_title: String,
) {
    let MooRequest { req_id, name, body } = request;

    let (delay, kind) = {
        let mut st = core.write().await;
        let kind = RequestKind::of(&name);
        st.log.push(RecordedRequest {
            req_id,
            name: name.clone(),
            body: body.clone(),
            unhandled: kind == RequestKind::Unknown,
        });
        let delay = body
            .get("item_key")
            .and_then(Value::as_str)
            .and_then(|k| st.key_delays.get(k).copied())
            .or_else(|| {
                body.get("level")
                    .and_then(Value::as_u64)
                    .and_then(|level| st.level_delays.get(&level).copied())
            })
            .unwrap_or(st.delay);
        (delay, kind)
    };

    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }

    match kind {
        RequestKind::RegistryInfo => {
            // FROM FORK: lib.rs:405 keys registration off request id 0 plus a
            // string `core_id`; display_name / display_version are read at :460.
            let body = {
                let st = core.read().await;
                json!({
                    "core_id": st.core_id,
                    "display_name": st.display_name,
                    "display_version": st.display_version,
                })
            };
            respond(&core, &writer, "COMPLETE", "Success", req_id, Some(&body)).await;
        }
        RequestKind::RegistryRegister => {
            // FROM FORK: lib.rs:479 requires the message name "Registered" and a
            // string `token`. A real Core returns more; the fork reads nothing
            // else, so nothing else is invented here.
            let body = json!({ "token": REGISTRATION_TOKEN });
            respond(
                &core,
                &writer,
                "COMPLETE",
                "Registered",
                req_id,
                Some(&body),
            )
            .await;
        }
        RequestKind::SubscribeZones => {
            let body = json!({ "zones": core.read().await.zones.clone() });
            // Remember this request id and connection so group_outputs /
            // ungroup_outputs (#509) can push a later `CONTINUE Changed` on
            // the same subscription -- exactly how a real Core reports the
            // effect of grouping, per the fork's own `parse_msg`
            // (`transport.rs:457-484`): it only recognises `zones_changed` /
            // `zones_added` / `zones_removed` arriving on the *subscription's*
            // `req_id`, not a fresh one.
            core.write().await.zone_push = Some((req_id, writer.clone()));
            // FROM FORK: transport.rs:479 accepts name "Subscribed"; a
            // subscription stays open, hence CONTINUE.
            respond(
                &core,
                &writer,
                "CONTINUE",
                "Subscribed",
                req_id,
                Some(&body),
            )
            .await;
        }
        RequestKind::UnsubscribeZones | RequestKind::Ping => {
            respond(&core, &writer, "COMPLETE", "Success", req_id, None).await;
        }
        RequestKind::Browse => handle_browse(req_id, &body, &core, &writer, &root_title).await,
        RequestKind::Load => handle_load(req_id, &body, &core, &writer).await,
        RequestKind::ImageGet => {
            // A minimal but real 1x1 PNG, framed the way `moo.rs::parse`
            // reads binary bodies (`Content-Type: image/png` -> the fork's
            // `ContentType::Png(body)`), so `RoonAdapter::get_image` -- and
            // through it `/api/collections/image` (#549/#573) -- can be
            // exercised end to end against this fake.
            const PNG_1X1: &[u8] = &[
                0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
                0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
                0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78,
                0x9C, 0x63, 0xFC, 0xCF, 0xC0, 0x50, 0x0F, 0x00, 0x04, 0x85, 0x01, 0x80, 0x84, 0xA9,
                0x8C, 0x21, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
            ];
            core.write()
                .await
                .sent
                .push((req_id, "Success".to_string()));
            let mut frame = format!(
                "MOO/1 COMPLETE Success\nRequest-Id: {req_id}\nContent-Length: {}\nContent-Type: image/png\n\n",
                PNG_1X1.len()
            )
            .into_bytes();
            frame.extend_from_slice(PNG_1X1);
            let _ = writer.lock().await.send(Message::Binary(frame)).await;
        }
        RequestKind::GroupOutputs => handle_group_outputs(req_id, &body, &core, &writer).await,
        RequestKind::UngroupOutputs => handle_ungroup_outputs(req_id, &body, &core, &writer).await,
        RequestKind::Unknown => {
            // Mirrors the fork's own reply to an unknown service (lib.rs:588-592).
            // FROM ROONLABS' PUBLISHED API: `InvalidRequest` with an `error` string
            // is what node-roon-api itself sends for "unknown service" and "unknown
            // request name", so this is the real shape, not an invention. It also
            // means an unmodelled call fails fast instead of hanging for 10s.
            let body = json!({ "error": format!("FakeRoonCore does not model {name}") });
            respond(
                &core,
                &writer,
                "COMPLETE",
                "InvalidRequest",
                req_id,
                Some(&body),
            )
            .await;
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum RequestKind {
    RegistryInfo,
    RegistryRegister,
    SubscribeZones,
    UnsubscribeZones,
    Browse,
    Load,
    Ping,
    GroupOutputs,
    UngroupOutputs,
    ImageGet,
    Unknown,
}

impl RequestKind {
    fn of(name: &str) -> Self {
        match name {
            "com.roonlabs.registry:1/info" => Self::RegistryInfo,
            "com.roonlabs.registry:1/register" => Self::RegistryRegister,
            "com.roonlabs.transport:2/subscribe_zones" => Self::SubscribeZones,
            "com.roonlabs.transport:2/unsubscribe_zones" => Self::UnsubscribeZones,
            "com.roonlabs.browse:1/browse" => Self::Browse,
            "com.roonlabs.browse:1/load" => Self::Load,
            "com.roonlabs.ping:1/ping" => Self::Ping,
            // FROM FORK: transport.rs:334,343 -- both send only `output_ids`.
            "com.roonlabs.transport:2/group_outputs" => Self::GroupOutputs,
            "com.roonlabs.transport:2/ungroup_outputs" => Self::UngroupOutputs,
            // FROM FORK: image.rs SVCNAME + get_image.
            "com.roonlabs.image:1/get_image" => Self::ImageGet,
            _ => Self::Unknown,
        }
    }
}

// =============================================================================
// Browse / load semantics
// =============================================================================

fn session_key_of(body: &Value) -> String {
    // The adapter always sets multi_session_key; a real Core tolerates its
    // absence by using a single default session.
    body.get("multi_session_key")
        .and_then(Value::as_str)
        .unwrap_or("__default__")
        .to_string()
}

async fn handle_browse(
    req_id: usize,
    body: &Value,
    core: &Arc<RwLock<CoreState>>,
    writer: &Writer,
    root_title: &str,
) {
    let session_key = session_key_of(body);
    let item_key = body.get("item_key").and_then(Value::as_str);
    let input = body.get("input").and_then(Value::as_str);
    let pop_all = body
        .get("pop_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let pop_levels = body.get("pop_levels").and_then(Value::as_u64);

    let mut state = core.write().await;

    // A request combining `pop_all: true` with a present `item_key` hangs
    // against a real Core (verified live, nuc14/Roon 2.70, #396's ship-gate
    // re-review) -- it never answers at all, which this fake cannot
    // faithfully reproduce as a *hang* without making every test that
    // regresses to this pattern pay a real wall-clock `BROWSE_TIMEOUT`. The
    // fork's `Parsed::Error` path only recognises
    // `FakeRoonCore::FORK_ERROR_NAMES` (an invented name degrades to
    // `Parsed::None`, i.e. exactly the timeout this refuses to reproduce —
    // see `an_unrecognised_error_name_degrades_to_an_indistinguishable_timeout`),
    // so this answers `InvalidItemKey`, a real recognised name, for a fast
    // and loud adapter-level test failure. The attempt is *also* recorded
    // separately (`FakeRoonCore::illegal_pop_all_with_item_key_attempts`) so
    // a test can assert the combination was never even sent, independent of
    // whatever generic error text resulted.
    if pop_all && item_key.is_some() {
        state
            .illegal_pop_all_with_item_key
            .push(session_key.clone());
        state.reject_next_browse = false;
        drop(state);
        refuse(core, writer, "InvalidItemKey", req_id, &session_key).await;
        return;
    }

    // --- error injection ---------------------------------------------------
    let rejected = state.reject_next_browse
        || item_key.is_some_and(|k| state.rejected_keys.contains(k))
        || match (state.item_key_scope, item_key) {
            (ItemKeyScope::PerSession, Some(key)) => !state
                .minted
                .get(key)
                .is_some_and(|sessions| sessions.contains(&session_key)),
            _ => false,
        }
        || item_key.is_some_and(|k| state.arena.index_of(k).is_none())
        // Same-session invalidation: THIS session minted this key in an
        // earlier epoch (before its most recent `pop_all`) and hasn't seen it
        // since. A key this session has never minted at all is untouched by
        // this check -- see `Session::minted_at_epoch`'s own docs for why
        // that distinction matters.
        || item_key.is_some_and(|k| {
            state.sessions.get(&session_key).is_some_and(|s| {
                s.minted_at_epoch
                    .get(k)
                    .is_some_and(|minted_epoch| *minted_epoch != s.epoch)
            })
        });
    if rejected {
        state.reject_next_browse = false;
        let name = item_key
            .and_then(|k| state.rejection_names.get(k).cloned())
            .unwrap_or_else(|| "InvalidItemKey".to_string());
        drop(state);
        refuse(core, writer, &name, req_id, &session_key).await;
        return;
    }

    // --- level stack -------------------------------------------------------
    let root_level = Level {
        title: root_title.to_string(),
        items: state.arena.root_children.clone(),
    };

    let session = state.sessions.entry(session_key.clone()).or_default();
    if pop_all || session.levels.is_empty() {
        session.levels = vec![root_level];
        // pop_all starts a new epoch: everything this session minted before
        // it is gone, including the root-level keys this very response is
        // about to hand out again (freshly, under the new epoch) -- matching
        // what was observed live: browsing the "same" node twice mints a
        // disjoint set of keys each time.
        session.epoch += 1;
    }
    if let Some(n) = pop_levels {
        let keep = session.levels.len().saturating_sub(n as usize).max(1);
        session.levels.truncate(keep);
    }

    if let Some(key) = item_key {
        let Some(index) = state.arena.index_of(key) else {
            drop(state);
            refuse(core, writer, "InvalidItemKey", req_id, &session_key).await;
            return;
        };
        let node_hint = state.arena.nodes[index].hint;
        let accepts_input = state.arena.nodes[index].input_prompt.is_some();
        let node_title = state.arena.nodes[index].title.clone();

        // Invoking an action does not produce a list.
        if node_hint == Some(Hint::Action) {
            drop(state);
            // FROM ROONLABS' PUBLISHED API: `action: "none"` is documented as "No
            // action is required" (node-roon-api-browse JSDoc), so this is a legal
            // reply rather than a guess. INFERRED: that it is the one a real Core
            // picks for an invoked action, rather than "message".
            // Nothing in this repo reads it — `execute_play_action`
            // discards the BrowseResult (src/adapters/roon.rs:1276-1284) — so the
            // load-bearing assertion is that the request arrived, which the
            // request log carries.
            let body = json!({ "action": "none" });
            respond(core, writer, "COMPLETE", "Success", req_id, Some(&body)).await;
            return;
        }

        let (title, items) = if accepts_input {
            match input {
                Some(query) => {
                    let source = search_source_for(&state, index);
                    let matches = search(&state, &source, query);
                    (format!("Search results for \"{query}\""), matches)
                }
                // Browsing a Search item without input: a real Core returns the
                // item's own list so the client can read `input_prompt`.
                None => (node_title, state.arena.nodes[index].children.clone()),
            }
        } else {
            (node_title, state.arena.nodes[index].children.clone())
        };

        let session = state.sessions.entry(session_key.clone()).or_default();
        session.levels.push(Level { title, items });
    }

    let list = current_list(&state, &session_key);
    drop(state);

    // FROM FORK: browse.rs:85-91 BrowseResult { action, item, list, message,
    // is_error }; only `action` is required. Action::List spells as "list"
    // (browse.rs:10-17).
    let body = json!({ "action": "list", "list": list });
    respond(core, writer, "COMPLETE", "Success", req_id, Some(&body)).await;
}

async fn handle_load(req_id: usize, body: &Value, core: &Arc<RwLock<CoreState>>, writer: &Writer) {
    let session_key = session_key_of(body);
    let offset = body.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
    let count = body
        .get("count")
        .and_then(Value::as_u64)
        .map(|c| c as usize);
    let requested_level = body.get("level").and_then(Value::as_u64);

    let mut state = core.write().await;

    // `LoadOpts::level` selects a level in the stack; the adapter never sets it,
    // so the default is the top of the stack.
    let selected = state.sessions.get(&session_key).and_then(|session| {
        let index = match requested_level {
            Some(level) => level as usize,
            None => session.levels.len().saturating_sub(1),
        };
        session
            .levels
            .get(index)
            .cloned()
            .map(|level| (index, level))
    });
    let Some((level_index, level)) = selected else {
        drop(state);
        // FROM FORK: browse.rs:173-175 — loading a session with no browse level.
        refuse(core, writer, "InvalidLevels", req_id, &session_key).await;
        return;
    };

    let total = level.items.len();
    let start = offset.min(total);
    let end = count.map_or(total, |c| (start + c).min(total));

    let mut items = Vec::with_capacity(end - start);
    for &index in &level.items[start..end] {
        let key = state.arena.key_of(index);
        let node = &state.arena.nodes[index];
        let mut item = json!({ "title": node.title });
        if let Some(subtitle) = &node.subtitle {
            item["subtitle"] = json!(subtitle);
        }
        if let Some(image_key) = &node.image_key {
            item["image_key"] = json!(image_key);
        }
        if let Some(hint) = node.hint {
            item["hint"] = json!(hint.wire());
        }
        if let Some(prompt) = &node.input_prompt {
            // FROM FORK: browse.rs:70-75 InputPrompt { prompt, action, value,
            // is_password } — matching RoonLabs' published JSDoc, where `action` is
            // "The verb that goes with this action".
            item["input_prompt"] = json!({ "prompt": prompt, "action": "Search" });
        }
        let keyed = node.keyed;
        if keyed {
            item["item_key"] = json!(key.clone());
            state
                .minted
                .entry(key.clone())
                .or_default()
                .insert(session_key.clone());
            // Same-session epoch tracking (`Session::minted_at_epoch`),
            // orthogonal to the cross-session `minted` map above: record
            // which epoch of *this* session minted this key, so a later
            // `pop_all` (which bumps the epoch) can tell a still-live key
            // apart from one it left behind.
            if let Some(session) = state.sessions.get_mut(&session_key) {
                let epoch = session.epoch;
                session.minted_at_epoch.insert(key, epoch);
            }
        }
        items.push(item);
    }

    let list = list_json(&level.title, total, level_index as u32);
    drop(state);

    // FROM FORK: browse.rs:93-98 LoadResult { items, offset, list } — all required.
    let body = json!({ "items": items, "offset": offset, "list": list });
    respond(core, writer, "COMPLETE", "Success", req_id, Some(&body)).await;
}

// =============================================================================
// Grouping: group_outputs / ungroup_outputs (issue #509)
// =============================================================================
//
// FROM FORK: `transport.rs:334-350` sends only `{"output_ids": [...]}` and
// reads no reply body at all -- `Transport::group_outputs`/`ungroup_outputs`
// return the raw `Option<usize>` request id, not a parsed result. So the
// client observes the *effect* of grouping only through the zone
// subscription's `Changed` push, exactly like every other Roon transport
// write in this fake (`control`, `mute`, `change_volume` get the same
// body-less `COMPLETE Success`). This fake's own zone-merge/split semantics
// below are a simplified model, not a documented Core behavior: real Roon's
// exact index/ordering rules for a merged zone's `outputs` list are
// unpublished. What is load-bearing is that a merge (a) keeps the leader's
// zone_id, (b) unions the outputs, and (c) retires any source zone left with
// none -- which is exactly what issue #509's acceptance criteria describe.

async fn handle_group_outputs(
    req_id: usize,
    body: &Value,
    core: &Arc<RwLock<CoreState>>,
    writer: &Writer,
) {
    let output_ids = string_array(body, "output_ids");
    respond(core, writer, "COMPLETE", "Success", req_id, None).await;

    let (merge, push) = {
        let mut state = core.write().await;
        let merge = merge_zone_outputs(&mut state.zones, &output_ids);
        (merge, state.zone_push.clone())
    };
    let Some((merged_zone, removed_zone_ids)) = merge else {
        return;
    };
    if let Some((sub_req_id, sub_writer)) = push {
        let mut change = json!({ "zones_changed": [merged_zone] });
        if !removed_zone_ids.is_empty() {
            change["zones_removed"] = json!(removed_zone_ids);
        }
        send(
            &sub_writer,
            "CONTINUE",
            "Changed",
            sub_req_id,
            Some(&change),
        )
        .await;
    }
}

async fn handle_ungroup_outputs(
    req_id: usize,
    body: &Value,
    core: &Arc<RwLock<CoreState>>,
    writer: &Writer,
) {
    let output_ids = string_array(body, "output_ids");
    respond(core, writer, "COMPLETE", "Success", req_id, None).await;

    let (changed, added, removed, push) = {
        let mut state = core.write().await;
        let (changed, added, removed) = split_zone_outputs(&mut state.zones, &output_ids);
        (changed, added, removed, state.zone_push.clone())
    };
    if changed.is_empty() && added.is_empty() && removed.is_empty() {
        return;
    }
    if let Some((sub_req_id, sub_writer)) = push {
        let mut zones_changed = changed;
        zones_changed.extend(added);
        let mut change = json!({});
        if !zones_changed.is_empty() {
            change["zones_changed"] = json!(zones_changed);
        }
        if !removed.is_empty() {
            change["zones_removed"] = json!(removed);
        }
        send(
            &sub_writer,
            "CONTINUE",
            "Changed",
            sub_req_id,
            Some(&change),
        )
        .await;
    }
}

fn string_array(body: &Value, field: &str) -> Vec<String> {
    body.get(field)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The `(zone index, output index)` of the zone currently holding `output_id`.
fn zone_output_index(zones: &[Value], output_id: &str) -> Option<(usize, usize)> {
    zones.iter().enumerate().find_map(|(zi, zone)| {
        let outs = zone.get("outputs")?.as_array()?;
        let oi = outs
            .iter()
            .position(|o| o.get("output_id").and_then(Value::as_str) == Some(output_id))?;
        Some((zi, oi))
    })
}

/// Merge every output in `output_ids` into the zone holding the first id.
/// Returns the merged zone's full JSON and the zone_ids of any source zones
/// that lost their last output and were retired as a result. `None` when
/// there was nothing to merge (the leader output was not found, or every
/// other requested output already belongs to the leader's zone).
fn merge_zone_outputs(
    zones: &mut Vec<Value>,
    output_ids: &[String],
) -> Option<(Value, Vec<String>)> {
    let leader_output_id = output_ids.first()?;
    let (leader_zi, _) = zone_output_index(zones, leader_output_id)?;
    let leader_zone_id = zones[leader_zi]["zone_id"].as_str()?.to_string();

    let mut moved_from = Vec::new();
    let mut moved_outputs = Vec::new();
    for output_id in &output_ids[1..] {
        let Some((zi, _)) = zone_output_index(zones, output_id) else {
            continue;
        };
        if zi == leader_zi {
            continue; // already part of the leader's zone
        }
        let outs = zones[zi]["outputs"].as_array_mut()?;
        let Some(pos) = outs
            .iter()
            .position(|o| o["output_id"].as_str() == Some(output_id.as_str()))
        else {
            continue;
        };
        let mut out = outs.remove(pos);
        out["zone_id"] = json!(leader_zone_id);
        moved_from.push(zi);
        moved_outputs.push(out);
    }
    if moved_outputs.is_empty() {
        return None;
    }

    // Retire any source zone that lost its last output. Removing in
    // descending index order keeps the remaining indices (including
    // `leader_zi`, adjusted below) valid.
    let mut emptied: Vec<usize> = moved_from;
    emptied.sort_unstable();
    emptied.dedup();
    let mut removed_zone_ids = Vec::new();
    let mut leader_zi = leader_zi;
    for zi in emptied.into_iter().rev() {
        if zones[zi]["outputs"]
            .as_array()
            .is_some_and(|outs| outs.is_empty())
        {
            removed_zone_ids.push(
                zones[zi]["zone_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            );
            zones.remove(zi);
            if zi < leader_zi {
                leader_zi -= 1;
            }
        }
    }

    let leader_outputs = zones[leader_zi]["outputs"].as_array_mut()?;
    leader_outputs.extend(moved_outputs);

    Some((zones[leader_zi].clone(), removed_zone_ids))
}

/// Pull every output in `output_ids` out of its current zone into its own
/// standalone zone. Returns `(zones_changed, zones_added, zones_removed)`:
/// a source zone that keeps at least one output after losing this one is
/// `zones_changed`; a source zone left empty is retired into `zones_removed`.
/// An output already alone in its zone is left untouched (not returned in
/// any of the three).
fn split_zone_outputs(
    zones: &mut Vec<Value>,
    output_ids: &[String],
) -> (Vec<Value>, Vec<Value>, Vec<String>) {
    let mut zones_changed = Vec::new();
    let mut zones_added = Vec::new();
    let mut zones_removed = Vec::new();

    for output_id in output_ids {
        let Some((zi, _)) = zone_output_index(zones, output_id) else {
            continue;
        };
        let output_count = zones[zi]["outputs"].as_array().map_or(0, Vec::len);
        if output_count <= 1 {
            continue; // already standalone
        }
        let outs = zones[zi]["outputs"].as_array_mut().unwrap();
        let pos = outs
            .iter()
            .position(|o| o["output_id"].as_str() == Some(output_id.as_str()))
            .unwrap();
        let mut out = outs.remove(pos);

        let new_zone_id = format!("ungrouped-{output_id}");
        out["zone_id"] = json!(new_zone_id);
        let new_zone = standalone_zone_from_output(&new_zone_id, out);
        zones.push(new_zone.clone());
        zones_added.push(new_zone);

        if zones[zi]["outputs"].as_array().unwrap().is_empty() {
            zones_removed.push(
                zones[zi]["zone_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            );
        } else {
            zones_changed.push(zones[zi].clone());
        }
    }
    if !zones_removed.is_empty() {
        zones.retain(|z| {
            !zones_removed.contains(&z["zone_id"].as_str().unwrap_or_default().to_string())
        });
    }
    (zones_changed, zones_added, zones_removed)
}

/// Build a minimal, fully-populated standalone zone (every field
/// `roon_api`'s `transport::Zone` requires, same as [`default_zone`]) around
/// one output that just left a group.
fn standalone_zone_from_output(zone_id: &str, output: Value) -> Value {
    json!({
        "zone_id": zone_id,
        "display_name": output["display_name"],
        "state": "stopped",
        "is_next_allowed": true,
        "is_previous_allowed": true,
        "is_pause_allowed": false,
        "is_play_allowed": true,
        "is_seek_allowed": false,
        "queue_items_remaining": 0,
        "queue_time_remaining": 0,
        "now_playing": null,
        "settings": { "loop": "disabled", "shuffle": false, "auto_radio": false },
        "outputs": [output],
    })
}

fn current_list(state: &CoreState, session_key: &str) -> Value {
    let Some(session) = state.sessions.get(session_key) else {
        return list_json("", 0, 0);
    };
    let level_index = session.levels.len().saturating_sub(1);
    match session.levels.last() {
        Some(level) => list_json(&level.title, level.items.len(), level_index as u32),
        None => list_json("", 0, 0),
    }
}

fn list_json(title: &str, count: usize, level: u32) -> Value {
    // FROM FORK: browse.rs:57-65 List { title, count, level, subtitle?,
    // image_key?, display_offset?, hint? }.
    // FROM ROONLABS' PUBLISHED API: `level` is documented as "increases from 0"
    // (node-roon-api-browse JSDoc), so root = 0 is documented, not inferred.
    json!({ "title": title, "count": count, "level": level })
}

/// Which search source an input-accepting node sits under, so `Library`, `TIDAL`
/// and `Qobuz` can return different results.
fn search_source_for(state: &CoreState, search_index: usize) -> String {
    for &root in &state.arena.root_children {
        if contains(state, root, search_index) {
            return state.arena.nodes[root].title.clone();
        }
    }
    "Library".to_string()
}

fn contains(state: &CoreState, parent: usize, needle: usize) -> bool {
    if parent == needle {
        return true;
    }
    state.arena.nodes[parent]
        .children
        .iter()
        .any(|&child| contains(state, child, needle))
}

fn search(state: &CoreState, source: &str, query: &str) -> Vec<usize> {
    let query = query.to_lowercase();
    state
        .arena
        .search_results
        .get(source)
        .map(|candidates| {
            candidates
                .iter()
                .copied()
                .filter(|&index| {
                    let node = &state.arena.nodes[index];
                    node.title.to_lowercase().contains(&query)
                        || node
                            .subtitle
                            .as_deref()
                            .is_some_and(|s| s.to_lowercase().contains(&query))
                })
                .collect()
        })
        .unwrap_or_default()
}

// =============================================================================
// Self-tests: the framing codec, independent of the adapter
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_parses_a_body_less_request() {
        let frame = encode_moo("REQUEST", "com.roonlabs.registry:1/info", 0, None);
        let parsed = parse_moo(&frame).expect("body-less frame should parse");
        assert_eq!(parsed.req_id, 0);
        assert_eq!(parsed.name, "com.roonlabs.registry:1/info");
        assert_eq!(parsed.body, Value::Null);
    }

    #[test]
    fn encodes_and_parses_a_json_body() {
        let body = json!({ "multi_session_key": "s1", "pop_all": true });
        let frame = encode_moo("REQUEST", "com.roonlabs.browse:1/browse", 7, Some(&body));
        let parsed = parse_moo(&frame).expect("json frame should parse");
        assert_eq!(parsed.req_id, 7);
        assert_eq!(parsed.body, body);
    }

    #[test]
    fn content_length_counts_bytes_not_chars() {
        // moo.rs uses body.as_bytes().len(); a multibyte title must not desync.
        let body = json!({ "title": "Björk – Homogénic" });
        let frame = encode_moo("COMPLETE", "Success", 1, Some(&body));
        let text = String::from_utf8(frame.clone()).unwrap();
        let declared: usize = text
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .unwrap()
            .parse()
            .unwrap();
        // String::len() is a byte count, which is what moo.rs declares
        // (body.as_bytes().len()); a char count would be 17 here, not 20.
        assert_eq!(declared, body.to_string().len());
        assert!(declared > body.to_string().chars().count());
        assert!(parse_moo(&frame).is_none()); // COMPLETE is a response, not a request
    }

    #[test]
    fn rejects_non_request_verbs() {
        let frame = encode_moo("COMPLETE", "Success", 1, None);
        assert!(parse_moo(&frame).is_none());
    }

    #[test]
    fn item_keys_are_scoped_to_the_core_instance() {
        let arena = Arena::build(&FakeLibrary::standard(), 4242);
        let key = arena.key_of(0);
        assert_eq!(arena.index_of(&key), Some(0));
        // A key minted by a different Core instance must not resolve here.
        let other = Arena::build(&FakeLibrary::standard(), 99);
        assert_eq!(arena.index_of(&other.key_of(0)), None);
        assert_eq!(arena.index_of("garbage"), None);
        assert_eq!(arena.index_of("4242:999999"), None);
    }
}
