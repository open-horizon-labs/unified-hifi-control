# Router verification boundaries

## Final live qualification — 2026-09-14, renewed test authorization

This result supersedes the earlier pending/blocked qualification entries.
The frozen macOS arm64 binary is `naa-router-af3f14ca20ac`, SHA-256
`af3f14ca20ac870ec735e00b4822087e58b0f0dce7ce85fb650921d8bfa54c0c`.

- Actual Embedded 6.0.4 switched A → B → A in 5.333 and 5.376 seconds. Both
  destinations accepted start and received audio; same-track position restoration
  was confirmed by native Status. A real browser click also completed the flow.
- Stable proxy identity, fresh opaque authentication, downstream capabilities,
  audio and feedback were exercised using two existing Rust NAA engines with
  software recording sinks. Official endpoints supplied authentication only.
  No physical DAC audio or listening result is claimed.
- Paused switching now explicitly stops the native transport before changing
  routes. Both paused and stopped selections remained stopped without autoplay.
- Stop during preflight cleared routing with no late session/audio over 12 seconds.
  Native Stop timed out after about 3.016 seconds and produced a visible partial
  failure warning; final native state was stopped. Stop after route commit took
  about 0.009 seconds, with no late session/audio over 12 seconds.
- Incompatible 44.1/48 kHz capabilities remained truthful. The incompatible target
  stayed selected, received no audio, and failed visibly after 10.368 seconds.
  No fallback or DSP change occurred. Explicit selection of A recovered playback.
- Server PID 763 and its September 5 start time remained unchanged. Configuration
  hashes were unchanged throughout switching. Original runtime attributes, empty
  queue and both original on-disk XML hashes were restored after qualification.
  Configuration application was asynchronous; restoration was verified after it
  completed, including byte-for-byte original files.

Software checks on this release: 63/63 Python cases (54 router, nine fixture),
nine Rust-side tests, and Clippy with warnings denied all passed. Source and
binary hashes are in [software-gate.json](software-gate.json). Private raw evidence
remains outside git under `/tmp/hiphi-naa-router-private` and
`/tmp/hiphi-router-final-20260914`; no vendor captures or credentials are shipped.

The PoC permits a brief interruption and a beginning-of-track segment before
Seek. It does not promise seamless handover, format conversion, or instantaneous
native control cancellation. Local routing stops independently of native-control
failure. Physical DAC qualification remains a separate boundary.

Test fixtures, helper processes and the temporary ADB forward were removed;
the original discovery relay and four pre-existing ADB forwards were restored.
The implementation remains uncommitted because earlier signing attempts awaited
1Password approval; signing was not bypassed.

The executable is exercised through real TCP sockets and its HTTP API by
[`tools/naa_router_lab.py`](../../tools/naa_router_lab.py). Its controller and NAA
peers are independent Python fixtures. The Rust integration test invokes this
lab against the binary Cargo just built. No physical audio output, vendor
authentication implementation, or HQPlayer process is used by these tests.

Actual HQPlayer Embedded observations belong in
[`live-test.md`](live-test.md), with the runtime investigation in
[`embedded-investigation.md`](embedded-investigation.md). The evidence classes
must remain separate: a fixture assertion cannot establish Embedded behavior,
signed authentication validity, DAC compatibility, or physical payload equality.

## Reproduce

```sh
cargo test --manifest-path native/naa-router/Cargo.toml
python3 tools/naa_router_lab.py \
  --binary native/naa-router/target/debug/naa-router --demo
```

The demo performs A → B → A and prints stream labels, fresh challenge labels,
byte counts and SHA-256 hashes of the complete records compared at the fake NAA.
It does not open a hardware device. Test configuration and process logs use
temporary directories; all protocol listeners use explicitly chosen loopback
addresses and ephemeral ports.

## Independent software checks

| Behavior | What the check actually observes | Shortcut it rejects |
|---|---|---|
| Stable identity, changing physical destination | A → B → A uses different physical IDs, including XML-special characters; upstream sees `hiphi:router` and `HiPhi Router`, while each downstream initialize names its own actual ID | Dumb TCP redirection, one-DAC-only mapping, missing XML escaping |
| Authentication relay | Fresh initial requests and distinct endpoint-specific reply bytes survive unchanged, including quote/spacing/entity choices; later authentication is also relayed unchanged | Captured auth replay, fabricated reply, device-name rewriting inside opaque auth |
| Destination capabilities | Each fake NAA has a different format list; its getformats response is compared byte for byte with what the fake controller receives | Cached offers from the previous destination or invented common capabilities |
| Cached virtual device | Explicit and automatically resolved sole-output routes initialize even when the controller omits getdevices after fresh auth | Depending on the controller re-enumerating every session |
| No arbitrary DAC choice | A sole output can be resolved, multiple outputs without a selection fail visibly, and an explicit output filters the multi-output response | Automatically selecting the first available device |
| Opaque audio and side sections | 32-bit PCM and native DSD records, XML-looking sample bytes, metadata and picture sections arrive byte for byte at the selected fake NAA | DSP, sample repacking, XML search/replacement over an audio stream |
| Downstream feedback | Fragmented 16-byte startup/feedback records, including angle brackets and a leading `<` with a non-XML prefix, arrive unchanged | Reconstructing clock feedback or treating every leading `<` as XML |
| Framing and reuse | Coalesced audio records, end marker, stop and same-connection PCM-to-DSD restart preserve ordering and lengths | Treating TCP reads as messages, assuming one sample width forever, adding an end-marker acknowledgement |
| Session exclusion | Repeated switches while an old generation has partial audio, and switching during incomplete auth, close the old session and preserve the new route/session state | Redirecting an authenticated live socket, stale cleanup clearing a new session, stale partial payload entering the new route |
| Backpressure | A stalled fake NAA causes an upstream write timeout after actual forwarding progress, while route selection remains responsive and the next destination works | Unbounded queueing over the exercised send budget, dropping the session immediately and calling it backpressure, blocking the selector behind audio writes |
| Failure visibility and no fallback | Missing selection, unknown virtual ID, unreachable selected endpoint and downstream start refusal do not connect another fixture or silently change the chosen route | Automatic alternate-output selection or swallowing a format refusal |
| Configuration lifecycle | Routes/selection survive process restart; update/remove behavior is checked for selected and unselected routes | An in-memory-only selector, obsolete routes reappearing, editing an unrelated route interrupting playback |
| Stop | Active sockets close, selection clears and subsequent controller connections are refused | Updating the displayed selection while leaving audio transport alive |
| Invalid control/frame input | Oversized declared audio, overlong controls, truncation, invalid UTF-8, malformed operation structure and forbidden XML declaration constructs fail | Unbounded allocation, incomplete-message reuse, forwarding malformed rewritten control |
| HTTP control boundary | Host, Origin, fetch metadata, duplicate headers and transfer-encoding checks reject the exercised inappropriate requests; same-origin operation still works | Cross-site browser changes through the local selector or ambiguous request framing |

The separate
[`stop_save_failure.rs`](../../native/naa-router/tests/stop_save_failure.rs)
regression replaces the configuration parent directory with a regular file.
This produces a deterministic save failure without relying on file permissions.
It requires Stop to disconnect both sides and clear the route anyway, refuse a
new connection, and retain the unsaved-Stop diagnostic across that retry.
The same integration file also requires preexisting temporary files to survive
startup and a configuration save: a filename pattern or PID is not ownership.

[`deadlines.rs`](../../native/naa-router/tests/deadlines.rs) exercises elapsed
timeouts. A fake NAA with a longer idle timeout cannot mask a router that keeps
an authenticated one-byte partial record open indefinitely. Separate clients
trickle HTTP headers and bodies every 200 ms; the whole-request deadline must
still close them in approximately ten seconds, and the selector must remain
usable. A timeout that restarts on every received byte would fail these checks.

## Limits and remaining live checks

The payload fixtures exercise 32-bit PCM and native DSD byte framing. They do
not qualify PCM24, every container width, every sample rate or every vendor NAA.
The reverse-record classifier is tested against a leading angle bracket with a
non-XML prefix; opaque bytes that exactly imitate a recognized XML prefix remain
an ambiguity in the observed framing, not a proved vendor-protocol guarantee.

Fixture authentication strings are deliberately synthetic. Their distinct
endpoint fields prove routing/opacity assertions only. The separate
[`live-test.md`](live-test.md) now records fresh authentication handoff between
two actual official providers on different hosts, the emulator helper and the
explicitly identified `cm4nano` peer. Both returned the same `endpoint_id`, so
that field's uniqueness semantics remain unknown. The physical peer received
authentication only; subsequent operations and audio stayed in recording sinks.

Actual Embedded reconnect and transport-assisted A → B → A observations are
recorded separately in that live report, with unchanged Embedded PID and
persistent configuration. They do not substitute for the still-outstanding
integrated selector retest. Physical listening, USB/internal output grants and
independent payload measurements at a DAC remain separate again.

The optional native control integration has a deliberate boundary: without an
active NAA session through this router, selection does not contact or start
HQPlayer. Initial selection and retry after a failed resume therefore require
the user's normal Play action. This avoids affecting unrelated HQPlayer output.
A controlled switch with an active playing session can resume asynchronously;
HTTP 200 alone reports the route commitment, not completed playback. Its terminal
control phase, fresh stream and positive audio payload must be checked separately.

## Earlier pre-cutoff gate (superseded)

Work is paused, not complete. The coordinating UI tool call stalled across the
02:00 ET deadline; once it returned, the coordinator stopped implementation and
new tests and allowed cleanup/documentation only. The Sonnet worker had already
finished at approximately 01:16 ET. Its process was subsequently confirmed
terminal, and no worker or fixture process owned by that verification session
remained running.

The last recorded gate was:

```sh
cargo test --manifest-path native/naa-router/Cargo.toml --no-fail-fast
```

It started at **2026-09-14 05:15:23 UTC (01:15:23 ET)** and failed the black-box
wrapper. The Python suite contained **58 cases: 56 passed, two failed**. Its
inventory is 41 routing/protocol cases, eight native-controller integration cases
and nine checks of the fake native-control server against the reference client.
The fixture-server checks establish the test harness's behavior, not product
interoperability.

These two cases failed at that point; their assertions remain and now pass:

| Case | Required behavior | Observed failure |
|---|---|---|
| `test_unknown_native_state_refuses_selection_before_mutation` | With an established router NAA session, an unknown native State must leave the route unchanged and report an error | A present non-`2` State such as `bogus` was treated as stopped and allowed selection |
| `test_seek_ok_without_confirmed_position_is_not_reported_restored` | Seek OK followed by an unchanged source position must not claim restoration | `position_restored` became true from the command acknowledgement without Status confirmation |

The passing native-controller cases include stopped/paused no-autoplay,
Stop → switch → automatic fresh NAA initialization → Play with actual new-route
PCM, floored integer-second Seek on the positive path, unreachable-controller
preflight, Play refusal without fallback, and rejection of Play OK plus State 2
when no actual audio payload arrived. Fake native replies are XML documents
without a trailing newline. These checks exercise the compiled router, with
independent fake controller and NAA peers.

**Nine Rust-side cases passed:** four unit tests, three elapsed-deadline
regressions, and two configuration/Stop regressions. The final recorded durations
were 32.82 seconds for the failing Python wrapper, 10.19 seconds for the deadline
group and 0.18 seconds for the configuration/Stop group.

The retained private evidence is `/tmp/hiphi-sonnet-control.jsonl` (tool requests,
test outputs and the worker result), with the earlier protocol-only pass in
`/tmp/hiphi-sonnet-review.jsonl`. The current test definitions are
[`naa_router_lab.py`](../../tools/naa_router_lab.py),
[`black_box.rs`](../../native/naa-router/tests/black_box.rs),
[`deadlines.rs`](../../native/naa-router/tests/deadlines.rs) and
[`stop_save_failure.rs`](../../native/naa-router/tests/stop_save_failure.rs).

A frozen-source rerun and matching final build hash were **not obtained**. The
two red cases, integrated selector retest and final frozen gate remain pending;
earlier green runs do not qualify later edits as complete.


## Resumed software gate — 2026-09-14 07:20 ET

The current release passes **63/63 Python cases**: 54 router cases and nine
reference-client/fixture cases. **Nine Rust-side cases pass**, and Clippy with
`--all-targets -- -D warnings` passes. [software-gate.json](software-gate.json)
records the source files, hashes, exact release artifact, commands and local logs.
The earlier red run above is historical, not the current result.

The resumed checks close the two failures and the additional review risks:

- Unknown native State refuses selection before a route mutation.
- Seek success requires bounded same-track Status position confirmation. The
  fixture now models the observed Embedded Stop reset to zero; the negative
  check explicitly proves an ignored Seek leaves zero and a visible warning.
- Missing, zero, unknown and non-success NAA result values cannot qualify
  initialize/start. Even subsequent audio cannot excuse an unaccepted start.
- New starts reset accepted-start and stream-audio evidence, including a rejected
  start on a connection which previously streamed successfully.
- Stop cancels a held native preflight request without a late route commit or
  Play. The fixture proves socket closure; expected cancellation is not confused
  with a fixture BrokenPipe failure.
- Stop acknowledgement alone cannot claim native State 0. Local routing still
  stops if native Stop is ignored, and the transport discrepancy stays visible.

Browser verification against an isolated backend exercised pending selection
cancellation, a fresh selection after cancellation, background resume status and
completion, background refusal with Retry, and Stop during background resume.
At 390px viewport width, document scroll width also measured 390px. The displayed
stream state uses actual stream-audio counters rather than auth/control bytes.
Temporary browser state and fixture services were cleaned up.

**Still outstanding:** the frozen controller's own one-click sequence against
actual Embedded, including A → B → A, position, paused/stopped behavior,
cancellation and incompatible-format refusal. Embedded remains restored; a new
live-test window has been requested. The overall goal is not complete.
