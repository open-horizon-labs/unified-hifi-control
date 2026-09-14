# Full integration acceptance audit (active)

The user requested actual Claude CLI Fable + Sonnet implementation, repeated
prep/testing/review/dissent/gap-fix rounds, two impeccable critique-and-fix rounds,
then independent root code/alignment review and dissent, repairs, and push.
P1 import/headless bridge alone is NOT completion of this goal.

## Required evidence before completion

| Requirement | Acceptance evidence | Current gate |
|---|---|---|
| Actual Fable and Sonnet CLI work | Session/model startup evidence and substantive work logs | Both launched; supervisors retain live handles |
| Full relay reuse | Stable virtual device, opaque fresh auth, audio/feedback fixture suite | Imported corpus and managed byte-forwarding cases pass; full public-path parity pending |
| UHC-managed lifecycle | Enable/disable/reconfigure/crash/shutdown tests, no orphan service | Basic managed lifecycle and manager-drop tests pass; final ownership audit pending |
| One HQPlayer command owner | No standalone native controller in managed mode; held operation/Stop/profile tests | Select/Stop/profile tests pass; conversation-level write fencing still open |
| Exact-instance routing | Two-instance cross-target/refusal tests on public API/MCP | Pending |
| Aggregate state | Epoch/revision-fenced observations, causal command confirmation | Three epoch/retirement tests pass; final public causality gate pending |
| All proxy capabilities | Discovery, DAC cache, route CRUD, selection, Stop and meaningful error/status | Production surfaces pending |
| API/MCP parity | Production protocol tests through UHC to managed relay; contract fixtures | Baseline passes; integration pending |
| Unified Dioxus UI | Existing HQPlayer page output workflow, complete fields/states, hydrated build | Pending |
| Settings/bootstrap/packaging | API-only setup/recovery, private opt-in build and lifecycle configuration | Pending |
| Completion gap interrogation | Explicit what's-left follow-ups and resulting fixes, no narrowed scope | Pending |
| Multiple review/test/dissent cycles | Findings, fixes and tests linked to final code | Pending |
| Impeccable round 1 | Independent A/B assessments, detector/browser evidence, fixes/recheck | Pending |
| Impeccable round 2 | Fresh independent A/B assessments after round 1 fixes, fixes/recheck | Pending |
| Root code/alignment review + dissent | Actual final artifact inspection; concrete findings repaired | Pending |
| Delivery | Passing required tests/build, clean scoped commit, branch pushed, no merge | Pending |

Baseline: `cargo test --test api_contract --test mcp_contract` passes (2 API and 122 MCP checks); output at
`/tmp/uhc-naa-baseline-contract.log`. Initial pre-commit failed because generated
`public/tailwind.css` was missing in this worktree; `make css` fixed that prerequisite.
User explicitly permits per-command signing bypass (`git -c commit.gpgsign=false`);
no global signing configuration changes and no hook bypass are authorized here.

Supervision logs remain outside git. Fable initial model: claude-fable-5-1,
Claude session 4d6f8563-93ad-4776-bfb2-69bd3f284788, /tmp/uhc-naa-fable/round1.jsonl.
Sonnet launch uses --model sonnet; /tmp/uhc-naa-sonnet/stream.jsonl.
No historical Embedded or physical-DAC evidence can qualify an untested new
integrated build. Keep software fixtures, real native behavior, and listening
claims separate. Do not mark this goal complete until the actual rows are proved.

Build prerequisite verified: isolated Dioxus CLI 0.7.10 completed the matching release server/WASM build; log `/tmp/uhc-naa-web-build.log`. This baseline build predates completed integration and must be repeated after the final source changes. The global Dioxus installation was left unchanged.

## Root incremental dissent findings (implementation in progress)

- UI audio labels must require positive payload evidence for the current session and route generation. Testing a helper that only repeats the phase enum does not prove the rendered claim.
- Unknown wire phases and malformed successful projections must not silently become idle or an empty inventory.
- NAA discovery and authenticated read-through DAC enumeration are distinct. Preserve host+port identity in the DAC catalog; identical device IDs on different hosts must never select the wrong destination.
- Managed mode must enforce UHC ownership of routing mutations, not merely omit the optional native controller. A standalone `/api/select` side channel can still break UHC's cancellation ordering.
- Real UHC API/MCP to managed relay/software NAA tests remain required; self-consistent JSON fixtures or a mock HTTP response alone cannot qualify integration.

These findings were sent to the owning Claude supervisors. They remain open until final code, adversarial tests, and rendered behavior demonstrate fixes.

Further incremental UI review found Stop disabled while a command is busy, unfenced concurrent projection refreshes, source/session epoch conflation, and a claim of silence inferred from an unreachable relay. Required checks: Stop remains clickable during a held switch; reordered reads cannot undo newer state; missing or mismatched evidence never confirms audio; reachability errors report uncertainty. These are open findings sent to Sonnet, not final-build conclusions.

Relay extraction review found an ownership cycle: the accept thread holds `Arc<NaaRelay>` while the relay owns its join/stop handle, preventing the documented Drop fallback from running when the external owner disappears. It also collects worker handles before joining accept, which can miss a concurrently spawned worker. Required evidence: last-owner drop releases the port and peer; concurrent connect+stop joins/tracks every worker; abnormal exit cannot self-join. Fable owns the fixes.

Lifecycle slice evidence: actual CLI log `/tmp/uhc-naa-fable/slice1.jsonl` contains the initial owner-drop failure (41 passed, 1 failed), followed by a Weak-reference patch and 42 passing tests (including shared fixture helper tests, not 42 independent lifecycle cases). Review remains open: transient Weak upgrades can make a worker the final owner and invoke self-join; final worker handles are still drained before accept completes. Passing timing-dependent drop test does not retire these races.

UI follow-up: separate busy/error mutation fencing from background reads, but order applied projections by authoritative source/revision. A later-issued GET can observe pre-commit state and must not suppress a newer POST receipt. Preserve operation identity for polling/recovery even when background telemetry is newer than the admission snapshot. Test actual held HTTP responses in both orders. Correlation IDs need independent-session entropy; a reset counter (or a formatter-only test) is insufficient proof.

MCP consumer review found inverted assertions: `hqp_outputs_mcp_consumer` initially passed when the new tools were missing while describing that as client-red. This is not red-first evidence. Sonnet was instructed to assert intended successful projection/accepted receipt now, observe a real failing run, and keep those expectations as implementation lands. The live MCP transport rig is reusable; absence-as-success assertions must not survive.

Corrected MCP consumer evidence: `/tmp/uhc-naa-sonnet/consumer-genuine-red.log` exits101 with two expected-success consumer failures naming the missing output tools (34 shared fixture checks pass). Remaining test-precondition review: configure explicit loopback discovery interface, await correlated configuration completion, prevent shared-config races, and read structured MCP envelope data. Backend must not invent defaults or treat admission as completion to satisfy premature assertions.

Coordinator/discovery review while production wiring is in progress: wildcard listener self-exclusion must not filter remote endpoints on port43210. New source epochs must replace an older output document even when its mutation revision resets. Native Play/Seek and failure-cleanup Stop need cancellation checks inside the shared native lease immediately before writes; an outer select alone does not prevent stale writes. Required regressions cover wildcard-local vs remote discovery, reconfigure epoch reset, and an old failed/cancelled switch racing a new operation. These findings are queued to Fable and not yet claimed fixed.

Shared-service and persistence review: idempotent retries must resolve their original correlation before rejecting now-stale mutation expectations; invalid supplied correlation IDs must be rejected explicitly. New manager persistence callback must hold a weak reference to the instance map to avoid retaining every adapter through an ownership cycle. Configuration write failures must reach the operation result. These are open backend findings with required retry/no-second-effect, owner-drop, and save-failure tests.

First production coordinator gate: `slice2-check2.log` confirms the feature-enabled library compiles. `slice2-run1.log` reports39 passing and4 failing tests (34 shared fixture helpers; nine coordinator cases). The failures expose correlation-retry rejection, cross-test persisted instance contamination, and an early telemetry assertion. Preserve the successful-behavior expectations; isolate fixture instances and wait for the specified observation rather than weakening assertions. This is not a passing integration gate. Root also added UHC_DATA_DIR isolation to the new test harness before the run.

Repair slice3 gate: lifecycle49pass/1fail and coordinator43pass/1fail, each including34 shared fixture checks. Original coordinator failures now pass. The lifecycle failure incorrectly expects a port0 listener to reuse its original ephemeral port after restart; it must compare UDP with the newly bound TCP port. The production manager-drop test is a real failure: a listener survives manager ownership loss. Native write-admission, publisher ownership, collision-free instance paths and request-vs-reply discovery checks remain open. These results do not qualify public API/MCP or the hydrated UI.

Slice3 rerun: `slice3-lifecycle2.log`50/0, `slice3-integration2.log`44/0 (each includes34 shared fixtures), and `slice3-agg.log`3/0. The earlier manager-drop failure is repaired through worker shutdown cleanup, and the responder restart test now follows the actual newly bound TCP port. Remaining findings21–27 require write-admission-level cancellation and accurate delivery classification, deadline checks, collision-free persisted identity, final owner-release proof, response-packet rejection, loopback responder isolation and honest multicast/nondefault-port qualification.

Slice4 rerun: `slice4-lifecycle2.log`52/0 and `slice4-integration2.log`46/0
(each includes34 shared fixture checks). The added multicast case exercises the
configured discovery port and self-exclusion. This does not imply that Embedded
queries nonstandard ports. Root found another native admission gap: the outer
fence still precedes awaited timeout/connection locks in the actual write helper.
Finding28 requires cancellation after those waits and a regression that holds the
inner lock, cancels, releases it, and observes no Play/Seek bytes.

Ownership was split to unblock public implementation: Fable retains native,
relay, coordinator, shared-service and setup internals; a separate actual Sonnet
CLI owns additive HTTP/MCP bindings, authentication registration and public
transport tests. The UI Sonnet remains isolated to frontend work. Public expected-
success tests have a real missing-tool failure; their later stream assertions
have not yet executed and are not passing evidence.

The actual UI receipt callback now gates on mutation ownership before comparing
authoritative revisions, addressing a later-issued stale GET suppressing a newer
POST receipt. `round8-tests.log`102/0 does not retire that callback regression:
its new tests still duplicate the guard formula. The next bounded repair uses a
shared production application path with deferred responses and completes the
`discovery_port` action DTO. Hydrated browser qualification remains pending.

Root added `docs/naa-proxy.md` as the operator/API workflow, covering exact-instance
targeting, operation follow-up, discovery versus read-through DAC names, setup,
import, and current versus historical forwarding evidence. Reconcile this guide
with the final public contract and setup results before delivery.

Packaging review found that Docker's second Cargo build would drop a feature
enabled only on the preceding Dioxus server build. Root added a default-`server`
`UHC_SERVER_FEATURES` build argument used by both steps; opt-in instructions name
`server,naa-proxy`. `git diff --check` passes. Docker CLI is installed but its
configured OrbStack socket is absent, so no image build or container-network
qualification has run. The matching local Dioxus build remains required; do not
turn source inspection into a container multicast claim.

Setup invalidation found during slice5: the draft invented
`<output backend="networkaudio" device="...">` and used `/restore` plus disk
readback as application proof. Existing private captures instead show
`<output type="network">` and a separate `<network address="advertised name"
device="hiphi:router" ...>` element. The successful configuration form uses
`backend=network` and `net_device=advertised name/hiphi:router`. Earlier restoration
evidence explicitly proves `/restore` alone does not reload running settings.
Fable must use the existing credential owner and evidenced form application,
preserving other successful controls, and distinguish disk from running readback.
Raw captures remain private. New tests must use sanitized evidenced shapes,
not confirm the invented schema. Root also found a self-deadlock in the draft
upload-error completion callback (nested ledger locking) and absent setup
generation fencing; both were sent to Fable before qualification.

Public implementation now has a real `api::hqp_outputs_http::routes()` attachment,
merged by main and reusable by public tests, plus controller-auth protection for
its command POST. Root inspected those paths. No public success is claimed until
the new tests run through them; MCP bindings and the final contract checks remain
pending.

The HTTP binding feature check passes (`check-http.log`). Initial MCP handlers
now call the same service, but review found missing `discovery_port`, absent
resolved scope, and feature-not-compiled mislabeled as a provider protocol
limitation. Shared machine-readable error codes and projection recovery must
also survive the envelope mapping. These are binding-worker repairs, not reasons
to widen or change the legacy envelope contract.

Root added an opt-in browser-peer launcher in
`tests/hqp_outputs_browser_fixture.rs`: existing native/NAA software peers, two
exact instance names, multi-DAC catalog, a private manifest, bounded lifetime and
stop-file cleanup. It does not provide substitute UHC HTTP responses. Its first
compile attempt was blocked by the in-progress backend setup type migration
(missing disk/runtime result fields and related signature changes); no launcher
run or browser proof exists yet. Retry after the backend's coherent library
check, then connect the real matching Dioxus server and hydrate the actual page.
