# Mock backends

Test doubles for the backends UHC talks to. They exist so adapter behaviour can be
asserted without hardware, a network, or the operator's rig.

**Read the "does not cover" column before you trust one.** A mock nothing asserts
*against* is worse than no mock, because it reads as coverage: `MockHqpServer`
rejected its own adapter's wire format until #394 noticed
(`HqpAdapter::build_command` sends the XML declaration and the command on one line;
the mock discarded any line starting with `<?xml`).

| Module | Kind | Drives the real adapter? |
|---|---|---|
| `roon_core.rs` — `FakeRoonCore` | WebSocket server, MOO protocol | **yes** |
| `roon.rs` — `MockRoonCore` | in-memory state holder, no protocol | no — and no test uses it |
| `lms.rs` — `MockLmsServer` | HTTP server, JSON-RPC `slim.request` | yes |
| `hqplayer.rs` — `MockHqpServer` | TCP server, XML lines | yes |
| `openhome.rs` — `MockOpenHomeDevice` | HTTP server, SOAP | discovery only |
| `upnp.rs` — `MockUpnpRenderer` | HTTP server, SOAP | discovery only |

---

## `FakeRoonCore` (issue #408)

A real WebSocket server that speaks Roon's MOO framing and enough of
`com.roonlabs.registry:1` / `transport:2` / `browse:1` for `RoonAdapter` to run
against it end to end. Tests live in `tests/roon_protocol.rs`.

`MockRoonCore` in `roon.rs` is **not** this. It is an in-memory zone-state holder
that nothing speaks a protocol to — which is why, before #408, no test asserted a
Roon success path. It is left in place unchanged, but note what a grep shows:

```
$ grep -rn "MockRoonCore" tests/ | grep -v tests/mock_servers/roon.rs
tests/mock_servers/mod.rs:19:pub use roon::MockRoonCore;
```

**Its only callers are its own six unit tests.** No adapter test uses it. Prefer
`FakeRoonCore` for anything new; `MockRoonCore` is a removal candidate, filed
separately rather than deleted here.

### How a test uses it

```rust
let core = FakeRoonCore::start().await;              // random loopback port
let adapter = connected(&core).await;                // real event loop, no SOOD
let hits = adapter.search("kind of blue", None, Some(10), SearchSource::Library).await?;
core.assert_no_unhandled_requests().await;           // drift guard
```

`connected()` calls `RoonAdapter::run_event_loop_against_core_for_tests`, the one
production-code seam this added: `run_roon_loop` gained a `CoreConnect` parameter so
tests can connect to a known address instead of doing SOOD multicast discovery.
Everything after the connection — the event loop, the `pending_browses` /
`pending_loads` maps, the zone conversion — is the code production runs. That is
the point. A test that reimplemented the loop would assert nothing about the
adapter.

### Covered

| Area | What is asserted |
|---|---|
| Handshake | `registry:1/info` at request id 0, then `register`; `CoreEvent::Registered`; Browse becomes available |
| Zones | a `Subscribed` payload survives the fork's deserializer and reaches `get_zones()` — a missing required field would otherwise be swallowed silently |
| Search | full six-request sequence, one `search_{nanos}` session key throughout, query carried as `input`, source selection (Library / TIDAL / Qobuz), empty result sets, `hierarchy=browse` on every request. **Flat results only** — see the category-grouping entry below — and against a hierarchy taken from the adapter's own expectations, so this cannot detect the adapter expecting the wrong hierarchy |
| **title / subtitle mapping** | asymmetric values at three layers: adapter return value, `GET /roon/search`, `POST /roon/browse`. This is the hole #408 exists to close |
| Browse | two levels down and `pop_all` back to the root; per-session level stacks proven independent; `input_prompt` advertised; unkeyed rows |
| Load | offset/count paging, short final page, total count vs page size, past-the-end returns empty |
| play_item | an `item_key` resolves through the item's action list to `Play Now`; the `roon:` prefix is stripped before it reaches the Core |
| search_and_play | navigates into a search hit to find a playable action; `play` / `queue` invoke different actions; a missing action reports what *is* available |
| Errors on demand | `InvalidItemKey` and `InvalidLevels`, correlated to the right request id and session key, including with several requests in flight and responses arriving out of order; and an *unrecognised* error name, to pin what happens when the fork's four literals are wrong |
| Drift | every request name the adapter sends is a closed set; anything else is recorded as unhandled and answered `InvalidRequest` |

### Does NOT cover — do not assume otherwise

* **It cannot prove the adapter matches a real Roon Core.** The fake's semantics
  came from the pinned fork's deserializers and from this repo's own adapter, so
  green means *unchanged*, not *correct*. Both were derived from the same source of
  truth, and that source has never been checked against Roon.
* **Category-grouped search results — probably the most-travelled real path.** A
  real Core returns search hits grouped under `Albums` / `Tracks` / `Artists` rows;
  `is_category` and `try_category_playable` exist in `src/adapters/roon.rs` because
  someone saw that. The default library returns hits flat, so those two and the
  second half of `try_navigate_to_playable` are **untested**, which means the
  branch a real search most likely takes is the one with no coverage. A test can
  build a grouped library itself — `FakeLibrary`'s fields and the `FakeItem`
  builders are public — but nobody has verified what the real grouping looks like,
  so the fake ships no guess.
* **The hierarchy itself.** `Library` / `TIDAL` / `Qobuz` → `Search` → results is
  taken from what `src/adapters/roon.rs` requires, so a fake built from it can never
  disagree with the adapter about navigation. If Roon renamed `Search`, `search()`
  would break in production and this suite would stay green.
* **Transport control.** `play` / `pause` / `next` / volume / mute go through
  `com.roonlabs.transport:2/control` and `change_volume`; the fake does not model
  them. It answers `InvalidRequest` and records the call as unhandled — those
  adapter methods do not await a reply, so they would appear to succeed; only
  `assert_no_unhandled_requests()` catches it. Call that assertion in any test you
  add.
* **Images.** `com.roonlabs.image:1/get_image` is not modelled.
* **Queue.** `subscribe_queue` / `play_from_here` (which #400 needs) are not
  modelled.
* **Zone updates over time.** Zones are sent once at subscribe. No `Changed`,
  `zones_added`, `zones_removed` or `zones_seek_changed` events.
* **Reconnection and Core loss.** Dropping the connection is not exercised.
* **Roon's real error bodies.** The fake sends error names with no body.
* **Authorization.** A real extension must be authorized in Roon → Settings →
  Extensions before `register` succeeds. The fake always registers, so the
  unauthorized state is untested.

### Which shapes are inferred rather than recorded

**No live Roon Core was reachable when this was written** — this machine's config
directory has no `roon_state.json`, so there is no pairing token, and obtaining one
requires a human to authorize the extension in Roon's UI. So nothing here is
recorded from a Core. Two pedigrees, unequal confidence:

*From the pinned fork* (`~/.cargo/git/checkouts/rust-roon-api-*/06dd807`) — which
fields exist, which are required, enum spellings, the four browse error names, the
handshake order. High confidence: a shape the fork accepts is a shape the adapter
can consume.

*From this repo's adapter* — that the root contains `Library` / `TIDAL` / `Qobuz`,
that each contains a `Search` item, that an action list contains
`Play Now` / `Queue` / `Start Radio`. High confidence as *expectations*:
`src/adapters/roon.rs` will not work against anything else.

*Inferred, unverified* — each marked `INFERRED:` at its use site in `roon_core.rs`:

| Shape | Why it is a guess | Consequence if wrong |
|---|---|---|
| root list title `"Explore"` | never read by this repo | none — cosmetic |
| `list.level` numbered from 0 at the root | plausible, unchecked | #399's "report your position" would be off by one |
| `action: "none"` in reply to invoking an action item | the adapter discards this BrowseResult | none today; would matter if a caller started reading it |
| `InvalidItemKey` carries no body | the fork keys only off the name | none for the fork; a real body that parsed as a `BrowseResult` would be read as success |
| `item_key` format | opacity is all this repo needs | none |
| **the four browse error *names*** | only evidence is the fork's own literals | **a Core that spells one differently is dropped inside the dependency, and the caller times out as if unreachable — with every test here still green** |
| **`item_key` portability across `multi_session_key`s** | **this is the epic's open question** | if keys are *not* portable, `/roon/play_item` is broken and #396's ref design changes |

The error-name row is the one #405's PR (#412) explicitly handed to this issue: the
fork matches `"InvalidItemKey"`, `"InvalidLevels"`, `"UnexpectedError"` and
`"ZoneNotFound"` against `msg["name"]`, and anything else becomes `Parsed::None`,
which it drops. This fake sends the fork's own literals, so it can never catch a
mismatch. What it *can* do — and does — is pin the consequence:
`an_unrecognised_error_name_degrades_to_an_indistinguishable_timeout` makes the Core
answer instantly with an unrecognised name and asserts the caller still times out.
`FakeRoonCore::FORK_ERROR_NAMES` pins the four literals so a fork bump that renames
one fails here rather than as a mysterious timeout. **Verifying the names needs a
real Core.**

That last row is the one that matters. `RoonAdapter::play_item` mints a fresh,
unrelated session key and browses the caller's key inside it, so the repo already
assumes keys are global. The fake refuses to decide: `ItemKeyScope::Global` is the
default (matching the repo's assumption) and `ItemKeyScope::PerSession` makes the
Core reject a foreign key, so a test can pin either answer. #405 must settle it
against the operator's rig — see
`a_foreign_item_key_is_rejected_when_keys_are_session_scoped`.

### Rejection tests work on both sides of #405, by design

Four tests drive a Core rejection: a bad item key, a foreign key under
`ItemKeyScope::PerSession`, a load with no browse, and one rejection among three
concurrent browses. None of them asserts "it hangs".

`classify_rejection()` in `tests/roon_protocol.rs` enforces what must hold whether
or not #405 (PR #412) has landed — a rejected request **never** resolves as success,
and if it resolves at all it resolves promptly, with a message that neither reads as
a timeout nor omits the browse session — and reports which of the two it observed.
Verified both ways:

```
$ # on v3 (c47d36a), #405 not present
load without a browse: DroppedByAdapter
rejected browse: DroppedByAdapter
rejected browse among three in flight: DroppedByAdapter
play_item against a foreign key: DroppedByAdapter
ok. 49 passed

$ # same file, merged with fix/issue-405-roon-browse-errors
load without a browse: RoutedToCaller
rejected browse: RoutedToCaller
rejected browse among three in flight: RoutedToCaller
play_item against a foreign key: RoutedToCaller
ok. 49 passed
```

So this is the end-to-end proof #412 asked for — its own correlation tests are
unit-scoped precisely because nothing could drive the wire path — and it does not
have to be inverted on merge. What is pinned unconditionally is the wire half: the
Core answered, named the error, and correlated it to the rejected request's own
`req_id` **and** session key, which is exactly the `Parsed::Error` payload.

### One test does pin a defect: same-session cross-delivery

`concurrent_browses_in_one_session_cross_deliver_their_results` asserts a bug is
**present**, and it survives #405.

Two browses in flight under the *same* `multi_session_key` cross-deliver their
results. `pending_browses` is keyed by `req_id` but the `Parsed::BrowseResult` arm
scans by session key — `Parsed::BrowseResult` carries no `req_id`, so it cannot say
which request in a session answered — and `.find()` returns an arbitrary match.
Measured: seven trials in eight hand the caller someone else's list, with no error.

`search`, `search_and_play` and `play_item` each mint a private session key and run
sequentially, so they are safe. `POST /roon/browse` takes a **caller-supplied**
session key and has no in-repo callers today — but any external client can trigger
it, and #399's navigation handle will give clients a reusable session identity,
which is exactly the shape that triggers it. #412's own comment says closing the gap
needs a change to the pinned fork.

When that assertion starts failing, the bug is fixed: invert it to compare each
trial against `expected`.

### Two scheduled edits — do these, do not skip them

1. **When #405 (PR #412) is on `v3`:** tighten `classify_rejection` so
   `RejectionOutcome::DroppedByAdapter` is a hard failure, and replace the string
   checks with `RoonBrowseError::from_error(&e)` asserting
   `kind == RoonBrowseErrorKind::InvalidItemKey` and the session key. As written the
   helper accepts both outcomes, which means it cannot catch a regression *back* to
   dropping the error — the exact bug #405 fixed.
2. **`concurrent_browses_in_one_session_cross_deliver_their_results` will rot.** It
   asserts a defect is present, so it turns red when the defect is fixed, and the
   person who sees that red will not know it is good news. The instruction is at the
   assertion site: invert it, compare each trial against `expected`. It is also the
   only probabilistic assertion in this repo's suite (eight trials, roughly 1 in
   10^7 for a false failure).

### Adding to the fake

* Do not encode a Roon semantic this repo does not already depend on. If a shape
  is uncertain, make it configurable and mark it `INFERRED:`, the way
  `ItemKeyScope` is.
* Library *shape* is data a test supplies (`FakeLibrary`, `FakeItem`); protocol
  behaviour is the fake's. Keep that line.
* If you teach it a new request, `the_fake_models_everything_the_adapter_sends`
  pins the closed set of request names — update it deliberately, and never by
  deleting the assertion.
