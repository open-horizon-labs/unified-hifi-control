# ADR 003: HQPlayer conformance boundary — corpus + scriptable fake as protocol truth

## Status

Accepted (2026-07-29, issue #322, program #310 / epic #311)

## Context

HQPlayer's native control protocol had no executable specification in this repository. What it had
instead was `docs/hqplayer-protocol-reference.md` — internally consistent, derived from `hqp-control`
v5.2.30 sources, and silent on the single most consequential thing the client got wrong: the
`result` attribute that tells you whether a command was accepted.

The test double could not have caught that. `tests/mock_servers/hqplayer.rs` answered `<Ok/>` to
every setter — a shape the daemon never sends — never mutated state, and returned an empty string
for the exact single-line request shape `build_request` emits, so no test could drive `HqpAdapter`
through it at all. `tests/adapter_integration.rs:603-604` says so in a comment: *"Use reqwest to test
the mock directly (not through adapter) because HQP adapter uses a complex TCP protocol."*

The consequences were not hypothetical. `State.volume` is a double on the wire; parsed as `i32`,
`"-23.5"` failed to parse and `unwrap_or(0)` yielded **0 dB — maximum output**. A `Status` document's
self-closing `<metadata …/>` child ended framing one document too early, leaving `</Status>` in the
socket for the next command to consume as its own reply; the repo had already noticed the symptom and
deleted a feature rather than fix it, per the comment in `connect()` about *"response desync bugs."*

Something had to become the protocol authority. The choice was which thing.

## Decision

Protocol truth lives in a **versioned document corpus** under `tests/fixtures/hqplayer/<version>/`,
exercised by a **scriptable fake daemon** that drives the real `HqpAdapter` over a real TCP socket.
Three concerns stay separated, because fusing them is what made the old mock incapable of failing:

| Layer | Owns | File |
|---|---|---|
| Corpus | *what* the daemon says, with provenance | `tests/mock_servers/hqplayer/corpus.rs` |
| Wire | *how and when* it says it — chunk boundaries, drops, silence, coalescing | `tests/mock_servers/hqplayer/wire.rs` |
| Model | how its *state* changes, and how it misbehaves on purpose | `tests/mock_servers/hqplayer/model.rs` |

Consequences of that split, all load-bearing:

- **Every fixture carries a provenance header** — source, daemon version, `verified` /
  `derived-excerpt` / `UNVERIFIED`, date, and caveats — and tests enforce it
  (`every_corpus_fixture_records_its_provenance`,
  `the_legacy_profile_is_marked_unverified_so_it_cannot_pass_as_protocol_truth`,
  `the_verified_profile_marks_excerpts_honestly`). A corpus that cannot distinguish a live
  observation from a transcription would re-create the problem this ADR exists to end, one layer out.
- **Assertions go through the adapter's public surface**, never a private helper, so the sans-io
  extraction that #162 is likely to perform does not invalidate the suite.
- **No test asserts elapsed wall-clock time.** Timeout and reconnect behaviour is driven through an
  injectable `HqpTimeouts` seam and asserted on outcomes and attempt counts
  (`WireStats::element_count`). This follows HQPTuner's testing policy, whose suite went from 84 s to
  7 s by removing real sleeps.
- **The default suite is hermetic.** The opt-in real-daemon mode is `UHC_HQP_CONFORMANCE_HOST`,
  read-only, and skips with a printed note rather than being permanently `#[ignore]`d. It is a
  **connectivity/identity smoke check** and nothing more — see the stage-3 section below for what it
  deliberately does not do.
- **`MockHqpServer` survives untouched** as the facade for `tests/adapter_integration.rs` and
  `tests/zones_sha_integration.rs`.

## Options considered

Five candidates across four escalation levels were evaluated in
`.oh/issue-322-hqplayer-protocol-conformance.md`:

| Option | Level | Why not |
|---|---|---|
| A — extend the existing mock in place | Band-Aid | Its `read_line` → one-response loop cannot express fragmentation or coalescing; reaching #322's hard framing constraints turns it into option C anyway |
| B — fixture corpus + pure framer unit tests, no sockets | Local Optimum | Cannot produce a red result first: the framer and decoder were private, so extracting one is a production change, inverting the TDD constraint. No time dimension, so no verification, reconnect or idle-close coverage |
| **C — corpus + scriptable fake, asserted through the adapter** | **Reframe** | **Selected** |
| D — record/replay golden transcripts | Reframe | Ordering-brittle against the client refactoring #322 exists to enable; cannot synthesize `result="Error"`, accept-but-ignore, or arbitrary chunk boundaries on demand; CI can never regenerate a stale transcript without hardware |
| E — sans-io protocol state machine | Redesign | The right destination, the wrong starting point: nothing can go red until the rewrite exists, which inverts TDD on the program's first PR and consumes #162's scope |

The binding constraint decided it. "Tests precede fixes" means the boundary must fail against the
adapter *as it is*, and the adapter's only seam is a TCP socket. C was the only candidate that could
go red on unmodified production code, and the only one covering verification, timing and framing in
one boundary. B is not rejected so much as absorbed — the corpus *is* B's document layer.

## Consequences

Accepted costs, with the measured numbers rather than the estimates:

- **We own real test infrastructure**: 1,434 lines across `corpus.rs`, `wire.rs` and `model.rs`, plus
  a 1,316-line suite. The stage-1 dissent estimated 700–900 for the fake and was close (1,434 with
  the model's fault injection); the growth is in the assertion suite, which stage 1 costed separately
  and loosely. Amortised across #162, #208, #328, #329 and #330, all of which need a peer that can
  misbehave on demand.
- **Every verified setter now costs an extra round trip.** `verify_applied` reads `State` back after
  `set_mode`, `set_filter_1x`, `set_filter_nx`, `set_shaper` and `set_rate`. This is deliberate: epic
  #311 forbids reporting success that was never confirmed, and the daemon demonstrably answers `OK`
  without applying. The latency envelope, with the shipped defaults of two attempts and a one-second
  reconnect delay:

  | Case | Added cost |
  |---|---|
  | The setting applied (the normal case) | one `State` round trip, **no sleep** — the first readback is immediate |
  | The setting landed a poll later | two `State` round trips plus one `reconnect_delay` |
  | The setting never applied | two `State` round trips plus one `reconnect_delay`, then an error |

  So a successful UI action pays a single local round trip, not a second of delay; only the failing
  and delayed paths pay the sleep. If it ever proves too expensive on a loaded daemon, the lever is
  the retry budget, not removing the readback.
- **Callers were already shaped for the new failure.** These setters always returned `Result`, so
  every call site already had an `Err` arm; what changed is that the arm now fires in a case it
  previously could not. Audited: `src/api/mod.rs:853` maps it to `400` with the error string,
  `src/api/mod.rs:926` maps it to `500`, and `src/mcp/mod.rs:624` returns an MCP error result. No
  caller silently discards it, and no call site needed changing.
- **Volume is the documented asymmetry.** `set_volume_db` is result-checked but *not*
  readback-verified, because a fixed-volume daemon answers an explicit `result="Error"` (already
  surfaced) and an adaptive-volume daemon moves the level itself, so a readback comparison would
  report spurious failures. This is an intentional exception, stated in the method's doc comment.
- **A setter the daemon silently ignores now returns an error** where it previously returned success,
  so `POST /hqplayer/pipeline` setter routes can surface a failure they could not before. No route,
  request schema or response schema changed: the new decimal-dB and `filter_junk` fields are
  `#[serde(skip)]`, and `HqpVolumeRequest.value` stays an integer.
- **`HqpTimeouts::set_timeouts` must be `pub`** for an integration-test crate to reach it, which the
  type system cannot restrict further. `no_production_code_retunes_the_timeout_seam` lints `src/` for
  callers instead, in the same spirit as the repo's existing `architecture_lint` and
  `arbitrary_find_lint` tests. Defaults remain the shipped constants. That lint is a text scan and is
  a first line of defence, **not** the API boundary: a differently-named wrapper would defeat it. It
  is worth what it costs precisely because the defaults are unchanged, so the worst case if it is
  bypassed is a caller opting into different retry behaviour deliberately.
- **Only byte-for-byte captures may claim `verified`.** A second superego pass found that
  `Provenance::is_verified()` accepts any `verified*` status while the honesty guard only checked the
  exact string `verified`, so a `verified-shape` fixture whose own notes admitted it was an excerpt
  was read as verified everywhere and caught nowhere. The guard now covers everything `is_verified()`
  accepts, case-insensitively, across the whole family of construction admissions; and four fixtures
  were relabelled to `derived-*` accordingly. Three fixtures now claim verification — `getinfo`,
  `modes`, `rates_sdm` — and those three are the ones whose content is byte-for-byte from the
  reference.
- **One command, one deadline.** `HqpTimeouts::response` is a *whole-command* budget, not a per-read
  one, and that distinction is load-bearing. A per-read timeout resets on every document, so a daemon
  streaming unsolicited `Status` frames — a verified 1–2 Hz during playback — can keep a reply-less
  command alive for as long as it keeps pushing. An intermediate revision tried to fix this by
  raising a skip *count* to a derived 64 and exposing it as `HqpTimeouts::max_unsolicited`; that made
  the worst case **worse**, roughly 32–64 s against the ~4–8 s it replaced, because counts were never
  the thing at risk. The public seam therefore stays at four fields, the skip ceiling is a private
  `MAX_UNSOLICITED_BACKLOG` that exists only as CPU protection, and
  `continuous_unsolicited_traffic_cannot_extend_the_command_deadline` pins the property by asserting
  on frames consumed rather than on elapsed time.
- **The corpus is transcribed, not captured.** Enumeration excerpts preserve the verified
  name/enum-ID pairs and the verified `Set*` anchors, but their list *positions* are excerpt-local and
  say so in their provenance. Closing that gap needs the opt-in real-daemon run, which is recorded as
  pending for stage 3 rather than claimed.
- **A setter overridden by another controller fails, and says what it saw.** `verify_applied`
  compares a readback against the value *we* requested, so it cannot distinguish "the daemon dropped
  our change" from "the daemon took it and another controller then moved it". Both are reported as
  failures, because in both cases the client cannot confirm the state it was asked to produce — and
  the error names the value the daemon actually reports, so an operator can tell the two apart. This
  came out of stage-2 dissent rather than the tests, and is pinned by
  `a_setter_overridden_by_another_controller_fails_and_names_the_observed_value`. If #329 later wants
  divergence *reported* rather than *failed* — the model HQPTuner uses — that is a product decision
  for the settings UX, and this primitive does not block it.
- **`docs/hqplayer-protocol-reference.md` is demoted** from authority to reader's guide, and corrected
  where the corpus contradicts it.

## Notes

Protocol evidence is HQPTuner's audit of `hqp-control` 6.0.1 sources with findings verified against a
live `hqplayerd` 6.0.4 (Opal), pinned at commit `6755793`:

- <https://github.com/ohshitgorillas/hqptuner/blob/67557939ae04b157b47cb67bd651b72c3140bcdd/docs/protocol.md>
- <https://github.com/ohshitgorillas/hqptuner/blob/67557939ae04b157b47cb67bd651b72c3140bcdd/docs/testing.md>

Written in stage 2 rather than stage 1, deliberately: the stage-1 dissent deferred it until the
timing seam was settled and the cost was measured, so that its two most consequential sections would
not have to hedge.

## Stage 3: what the real-daemon merge gate actually requires

The corpus is transcribed rather than captured, so a live comparison is the only thing that can
settle it. This section exists because that comparison was previously described as "run the opt-in
suite", which overstated what the opt-in suite does.

### What exists today (stage 2)

`real_daemon_smoke_check_when_opted_in` connects to `UHC_HQP_CONFORMANCE_HOST`, calls `GetInfo` and
`GetFilters`, and asserts the daemon identifies itself and returns a multi-entry container the framer
parses whole. That is a **smoke check**: it proves the client can talk to real hardware at all.

It satisfies AC8, which asks that the harness *"can run in CI without HQPlayer and has a documented
opt-in real-server conformance mode"* — an existence-and-documentation requirement. It does **not**
read `State`, `Status`, `VolumeRange`, `GetModes`, `GetShapers`, `GetRates`, `GetJunkFilters` or the
matrix family; it compares nothing against the corpus; and it settles none of the `derived-*`
fixtures' list positions. It must not be cited as doing any of that.

### Tier 1 — read-only live verification (the merge gate)

Safe against any daemon, including someone's listening room. Extend the opt-in mode to **capture**
and **diff against the corpus**:

| Family | What the diff must compare |
|---|---|
| `GetInfo` | attribute set and value shapes |
| `GetModes` | names, enum IDs, and **list positions** |
| `GetFilters`, `GetShapers` | names, enum IDs, list positions, `arg` flags, `description` presence — **per mode**, since the lists are mode-relative |
| `GetRates` | index-to-Hz mapping, and that index 0 is rate 0 |
| `GetJunkFilters` | names, enum IDs, positions |
| `State` | attribute set, numeric-vs-decimal types, `filter_junk` as an int, whether `filter1x`/`filterNx` are present |
| `Status` | attribute set, the `metadata` child's presence and self-closing shape, active-\* fields as strings |
| `VolumeRange` | whether `step` is sent at all, and that min/max are doubles |
| `MatrixListProfiles` / `MatrixGetProfile` | container and child shape |
| `GET /config` (8088) | the `profile` / `profile_name` field names and the `[default]`-versus-named distinction |

Every mismatch either re-provenances a fixture from the capture or ships as a stated gap. A
`derived-excerpt` or `UNVERIFIED` label surviving this pass unexamined is a finding, not a footnote.

**This tier is what the real-daemon merge gate means.** It is implemented as of stage 3 — see the
runbook below — and awaits a reachable daemon.

### Running tier 1 (implemented, stage 3)

```bash
UHC_HQP_CONFORMANCE_HOST=<daemon-ip> \
  cargo test --test hqplayer_conformance -- --nocapture tier1_live
```

| Variable | Required | Default | Purpose |
|---|---|---|---|
| `UHC_HQP_CONFORMANCE_HOST` | **yes** | — | Daemon address. Absent ⇒ the gate prints that it skipped and passes, keeping CI hermetic |
| `UHC_HQP_CONFORMANCE_PORT` | no | `4321` | Native control port |
| `UHC_HQP_CONFORMANCE_PROFILE` | no | `hqpd-6.0.4-opal` | Corpus profile to diff against |
| `UHC_HQP_CONFORMANCE_WEB_PORT` | no | `8088` | HTTP port for the `/config` read side |
| `UHC_HQP_CONFORMANCE_WEB_USER` | no | — | Digest user. Supply with `_PASS` to include the persistent read lane |
| `UHC_HQP_CONFORMANCE_WEB_PASS` | no | — | Digest password |

The gate **fails on divergence** and prints the full report either way. A divergence is resolved by
re-provenancing the fixture from the capture, or by shipping it as a stated gap — never by loosening
the differ.

What the report carries: daemon identity (product/version/engine/platform/name), the active mode and
an explicit note that the enumerations are *that mode's* lists only, whether `Status` carried its
`metadata` child, the count of unsolicited documents the client skipped, per-family delivery times as
evidence for setting `HqpTimeouts::response`, every divergence with family/kind/both numbers, and a
`NOT captured` list naming what a read-only run structurally cannot reach.

**Read-only by construction.** Every call on the capture path is a query. Verified by
`tier1_finds_no_divergence_when_the_daemon_serves_the_corpus_it_is_diffed_against`,
`tier1_reports_index_divergence_when_the_daemon_orders_a_list_differently`,
`tier1_records_the_inactive_mode_lists_as_not_captured`,
`tier1_records_container_delivery_time_per_family` and
`tier1_records_how_many_unsolicited_documents_the_client_skipped`, all of which run hermetically
against the fake daemon. What those cannot cover is the live gate's own env-var handling and its
connect-to-real-hardware path; those are exercised only by an actual run.

### Tier 2 — mutating verification (never a merge gate, never unattended)

Some corpus claims cannot be confirmed read-only, because the evidence *is* a state change. Chiefly
the `Set*` anchors: that `<SetFilter value="6"/>` selects the entry at list index 6 rather than the
entry whose enum ID is 6, and the equivalent for `SetShaping` and `SetRate`. Confirming those means
moving a real daemon's settings.

Requirements, all of them:

- A **dedicated or expendable** daemon. Never a user's playback system, never a shared one.
- A **separate** opt-in variable from tier 1, so nobody enables mutation by reusing a host name.
- Capture-and-restore of every setting touched, with the restore verified.
- Never in CI, never unattended.

Because tier 2 needs hardware nobody should volunteer casually, the honest position is that the
`Set*` anchors stay **derived** until someone runs it deliberately — and the fixtures say so.

### Also outstanding for stage 3

- ~~Make the unsolicited-document skip count observable.~~ **Done in stage 3**:
  `HqpAdapter::unsolicited_skipped()` counts them and the tier-1 report carries the figure. Zero is
  expected against a well-behaved daemon, so a non-zero count on real hardware is the signal that the
  reply-element invariant is narrower than the reference implies.
- **Track tier 1 as a real follow-up, not a paragraph.** Superego's standing objection is that
  `derived-*` fixtures will accumulate dependents (#162, #328, #329) while the live verification stays
  narrated. Opening that issue belongs to the program owner, not to this PR — the issue graph under
  #310 is maintained deliberately — so it is recorded here as a pre-merge action rather than filed
  unilaterally.
- Confirm on GitHub runners, not this sandbox, the three known full-run failures: the two
  deterministic `/hqp/discover` multicast 500s, and the pre-existing ~1-in-10
  `error_handling::lms_fails_gracefully_when_unconfigured` concurrency flake in `adapter_integration`
  (green 4/4 under `--test-threads=1`; the file is byte-identical to `v3`).

