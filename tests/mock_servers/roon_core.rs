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
//! (`open-horizon-labs/rust-roon-api@ohc/main`, checkout `06dd807`), not invented:
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
//! **No live Roon Core was reachable when this was written** (no pairing token
//! exists in this machine's config dir, and pairing requires a human to authorize
//! the extension in Roon → Settings → Extensions). So the shapes here have two
//! different pedigrees, and they are not equally trustworthy:
//!
//! | Pedigree | What it covers | Confidence |
//! |---|---|---|
//! | **From the fork** | which fields exist, which are required vs optional, enum spellings (`"action_list"`, `"list"`, …), the four browse error names, the handshake order | high — a shape the fork's `serde` accepts is a shape the adapter can consume |
//! | **From this repo's adapter** | that the browse root contains `Library` / `TIDAL` / `Qobuz`, that each contains a `Search` item, that an action list contains `Play Now` / `Queue` / `Start Radio` | high as *expectations* — `src/adapters/roon.rs` will not work against anything else |
//! | **INFERRED** | root list title (`Explore`), `level` numbering from 0, `action: "none"` as the reply to invoking an action item, whether a real Core sends a body with `InvalidItemKey`, whether an `item_key` minted under one `multi_session_key` resolves under another | **unverified** — every one is marked `INFERRED:` at its use site |
//!
//! The last row is the dangerous one. In particular the `item_key` portability
//! question is the empirical unknown #405 must answer against the operator's rig;
//! this fake makes it *configurable* ([`ItemKeyScope`]) rather than pretending to
//! know, so a test can pin either answer and be re-pointed once it is known.
//!
//! # What this fake does NOT prove
//!
//! It proves the adapter is self-consistent and that it drives `roon_api` correctly.
//! It **cannot** prove the adapter matches a real Roon Core, because the fake's
//! semantics were derived from the same repo the adapter lives in. Green here means
//! "unchanged", not "correct". See `tests/mock_servers/README.md` for the full
//! covered/not-covered table.
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
            // FROM FORK: browse.rs::ItemHint, #[serde(rename_all = "snake_case")]
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
                album("Blue Train", "John Coltrane", &["Blue Train", "Moment's Notice"]),
            ],
        );
        search_results.insert(
            "TIDAL".to_string(),
            vec![album("Blue Note Reimagined", "Various Artists", &["Footprints"])],
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKeyScope {
    /// Any session may use any key. Matches what this repo assumes today. Default.
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
        // INFERRED: real Roon item keys look like short opaque colon-separated
        // strings. Only their opacity matters to this repo.
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
}

struct CoreState {
    arena: Arena,
    sessions: HashMap<String, Session>,
    /// item_key -> sessions that have been served that key
    minted: HashMap<String, HashSet<String>>,
    item_key_scope: ItemKeyScope,
    /// item_keys the Core will reject with `InvalidItemKey`
    rejected_keys: HashSet<String>,
    /// One-shot: the next browse, whatever it is, is rejected.
    reject_next_browse: bool,
    /// Applied before every response.
    delay: Duration,
    /// Per-item_key delay override, so a test can force responses to arrive out
    /// of order and prove correlation is by request, not by arrival.
    key_delays: HashMap<String, Duration>,
    zones: Vec<Value>,
    log: Vec<RecordedRequest>,
    /// What this Core answered, in send order: (request id, message name).
    /// Without this, "the adapter hung" and "the Core never replied" are
    /// indistinguishable — and that ambiguity is exactly what #405 is about.
    sent: Vec<(usize, String)>,
    core_id: String,
    display_name: String,
    display_version: String,
}

// =============================================================================
// The fake
// =============================================================================

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
            reject_next_browse: false,
            delay: Duration::ZERO,
            key_delays: HashMap::new(),
            zones: vec![default_zone("zone_fake_1", "Fake Living Room")],
            log: Vec::new(),
            sent: Vec::new(),
            core_id: "fake-core-408".to_string(),
            display_name: "Fake Roon Core".to_string(),
            display_version: "2.0.408".to_string(),
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

    pub async fn set_item_key_scope(&self, scope: ItemKeyScope) {
        self.state.write().await.item_key_scope = scope;
    }

    pub async fn set_zones(&self, zones: Vec<Value>) {
        self.state.write().await.zones = zones;
    }

    pub async fn core_name(&self) -> String {
        self.state.read().await.display_name.clone()
    }

    // ---- observation -------------------------------------------------------

    /// The `item_key` this Core mints for the (first) item with this title.
    /// Tests use it to name a key for [`Self::reject_item_key`]; they must not
    /// construct keys themselves.
    pub async fn key_for_title(&self, title: &str) -> Option<String> {
        let state = self.state.read().await;
        state.arena.find_by_title(title).map(|i| state.arena.key_of(i))
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
            let body = json!({ "token": "fake-token-408" });
            respond(&core, &writer, "COMPLETE", "Registered", req_id, Some(&body)).await;
        }
        RequestKind::SubscribeZones => {
            let body = json!({ "zones": core.read().await.zones.clone() });
            // FROM FORK: transport.rs:479 accepts name "Subscribed"; a
            // subscription stays open, hence CONTINUE.
            respond(&core, &writer, "CONTINUE", "Subscribed", req_id, Some(&body)).await;
        }
        RequestKind::UnsubscribeZones | RequestKind::Ping => {
            respond(&core, &writer, "COMPLETE", "Success", req_id, None).await;
        }
        RequestKind::Browse => handle_browse(req_id, &body, &core, &writer, &root_title).await,
        RequestKind::Load => handle_load(req_id, &body, &core, &writer).await,
        RequestKind::Unknown => {
            // Mirrors the fork's own reply to an unknown service (lib.rs:588-592)
            // so an unmodelled call fails fast instead of hanging for 10s.
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
        || item_key.is_some_and(|k| state.arena.index_of(k).is_none());
    if rejected {
        state.reject_next_browse = false;
        drop(state);
        // FROM FORK: browse.rs:170-172 keys purely off the message name, and
        // yields Parsed::Error(RoonApiError::BrowseInvalidItemKey((req_id, key))).
        // INFERRED: whether a real Core attaches a body. None is sent, because a
        // body that parsed as BrowseResult/LoadResult would be taken for success.
        respond(core, writer, "COMPLETE", "InvalidItemKey", req_id, None).await;
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
    }
    if let Some(n) = pop_levels {
        let keep = session.levels.len().saturating_sub(n as usize).max(1);
        session.levels.truncate(keep);
    }

    if let Some(key) = item_key {
        let Some(index) = state.arena.index_of(key) else {
            drop(state);
            respond(core, writer, "COMPLETE", "InvalidItemKey", req_id, None).await;
            return;
        };
        let node_hint = state.arena.nodes[index].hint;
        let accepts_input = state.arena.nodes[index].input_prompt.is_some();
        let node_title = state.arena.nodes[index].title.clone();

        // Invoking an action does not produce a list.
        if node_hint == Some(Hint::Action) {
            drop(state);
            // INFERRED: a real Core answers an invoked action with action "none"
            // or a "message". Nothing in this repo reads it — `execute_play_action`
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
    let offset = body
        .get("offset")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
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
        session.levels.get(index).cloned().map(|level| (index, level))
    });
    let Some((level_index, level)) = selected else {
        drop(state);
        // FROM FORK: browse.rs:173-175 — loading a session with no browse level.
        respond(core, writer, "COMPLETE", "InvalidLevels", req_id, None).await;
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
            // is_password }.
            item["input_prompt"] = json!({ "prompt": prompt, "action": "Search" });
        }
        let keyed = node.keyed;
        if keyed {
            item["item_key"] = json!(key);
            state
                .minted
                .entry(key)
                .or_default()
                .insert(session_key.clone());
        }
        items.push(item);
    }

    let list = list_json(&level.title, total, level_index as u32);
    drop(state);

    // FROM FORK: browse.rs:93-98 LoadResult { items, offset, list } — all required.
    let body = json!({ "items": items, "offset": offset, "list": list });
    respond(core, writer, "COMPLETE", "Success", req_id, Some(&body)).await;
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
    // INFERRED: levels numbered from 0 at the browse root.
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
