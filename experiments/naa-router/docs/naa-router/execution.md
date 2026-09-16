# NAA router PoC execution — 2026-09-14

## Final live qualification — 2026-09-14, renewed test authorization

This result supersedes the earlier pending/blocked qualification entries, including
the pre-renewal restoration boundary and blocked audit retained below. See
[qualification evidence](qualification-evidence.md) for the implementation and
saved observations checked on 2026-09-16. This qualifies the named experimental
binary only, not subsequent UHC builds.
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

## Aim

Choose the destination DAC from a small selector while HQPlayer Embedded keeps
its DSP configuration and running process. The user should not edit XML, save
profiles, or restart Embedded to change NAA destination.

Current behavior: destination changes require the user's profile/configuration
workflow. Desired behavior: choose A, B, then A in the router and resume playback.
One-time selection of the router in HQPlayer is acceptable. Brief interruption
for a fresh NAA session is acceptable; seamless mid-track transfer is not the
initial hypothesis.

Mechanism hypothesis: a stable NAA adapter and virtual output identity route
fresh HQPlayer sessions to the selected physical NAA. Relay opaque authentication
end to end, translate only device identity control fields, and forward audio and
downstream feedback unchanged. Reuse native/naa-native Rust code where applicable.

Misunderstanding signal: a prettier profile editor, hidden Embedded restart,
single-target proxy, or mock-only success presented as actual Embedded support.

## Execution contract

User authorized Claude CLI Fable and Sonnet workers, a complete working PoC, and
parallel implementation through 02:00 America/New_York on 2026-09-14. Fable owns
the Rust core; Sonnet owns independent integration fixtures/tests. Coordinator
owns selector and integration. An independent worker investigates the actual
Embedded runtime and challenges reconnection assumptions. Private CLI logs and
runtime captures stay outside git.

This is an isolated host-side PoC, not an implicit change to Android production
scope or a public release. Rootless Android adapters remain unchanged. No vendor
bytes, credentials or signing material may be committed.

### Declared success criteria

- Buildable standalone Rust executable with a usable local selector and CLI.
- Stable advertised adapter identity and virtual device identity across routes.
- Explicit destination selection; persisted choices without hand-written XML.
- Fresh opaque authentication relayed to the selected real NAA.
- Correct device-ID mapping and truthful selected-destination capabilities.
- Dynamic A → B → A routing without restarting the router or Embedded.
- Old session teardown before new route audio, bounded buffering and responsive
  Stop, with no stale clock feedback carried into the new session.
- Audio and feedback byte preservation, no DSP, no inserted samples and no
  silent fallback to another output.
- Complete failure paths for no selection, busy output, refusal, unreachable
  destination, malformed/truncated frames and disconnection.
- Independent fixture evidence and actual HQPlayer interoperability evidence
  reported separately. Actual Embedded restart-free switching requires actual
  Embedded observation; Desktop and software fixtures cannot prove it.

### Risk retirement checklist

| Risk | Check that can reject the tempting shortcut | Initial status |
|---|---|---|
| Embedded will not reconnect | Observe A → B → A with unchanged Embedded PID and configuration; distinguish controller Stop/Play from process restart | Triggered pending actual runtime evidence |
| Cached DAC ID or capabilities | Two distinct IDs and format lists; assert virtual identity upstream, actual identity downstream, selected formats only | Triggered pending checks |
| Auth session incorrectly reused | Fresh unique handshake on every new connection; assert destination-specific opaque response, no replay or fabricated auth | Triggered pending checks |
| Byte rewriting corrupts audio | Fragmented frames containing XML-like audio bytes, PCM/DSD and side sections; equality in both directions | Triggered pending checks |
| Switch races leak audio or feedback | Switch during streaming/backpressure; old paired sockets shut down, next session isolated | Triggered pending checks |
| Refusal becomes implicit fallback | Explicit unreachable/refusing/missing target, verify no other NAA receives connection/audio | Triggered pending checks |
| UI saves configuration but cannot route | Browser selects and switches real fixture destinations through the running binary API | Triggered pending checks |
| Persistent-session redesign needed | If Embedded needs restart or cannot refresh selected capabilities, investigate explicit control/reconnect before claiming completion | Triggered on failed actual-runtime check |

Hardware listening and independent physical payload measurement require a named
output and appropriate test setup. Do not substitute inferred compatibility.
Never select arbitrary household players or launch network-wide test servers.
Do not reclassify an inconvenient model-checkable risk as accepted.

## Progress and verification

Initial checkout was clean. Existing native NAA sources implement framing,
discovery, post-auth streaming and lifecycle. Existing records cover HQPlayer
Desktop; they do not establish this router or Embedded switching.

Execution is in progress. This document is a contract, not a completion claim.

### Actual Embedded observation and adjustment

The actual Embedded 6.0.4 server has now authenticated through the router and
played A → B → A into the two Rust recording endpoints. Its process remained the
same, and no profile or DSP configuration changed between route selections.
These fixtures use real protocol implementation and fresh official authentication,
with simulated output clocks and no physical audio output. The initial run used one official authentication provider. A later A → B → A
run used separate official providers on an emulator and a physical NAA host;
the physical host handled authentication only, with zero initialize/audio sent
to it. All audio still went to recording fixtures. Across 20 actual exchanges,
all complete replies and public keys differed while endpoint_id remained the
same on both hosts. The semantics of that field are unknown; per-host uniqueness
is not a justified acceptance criterion.

Direct selection while playing closes the old paired sockets. In the observed
case Embedded reconnects and initializes the newly selected endpoint after about
2.5 seconds, but then reports stopped. One ordinary native Play command resumes
audio. Therefore the next implementation increment is an explicitly configured
HQPlayer control connection that resumes only when the user was already playing.
No configuration/profile operation or server restart belongs in that switch.
Exact command ordering and timing remain under repeat testing; do not bake in
blind retries or use fixture success to assume server behavior.

Additional acceptance checks for this adjustment:

- Observe transport state before switching; stopped/paused must not auto-play.
- Confirm the fresh selected NAA session, with no reuse of the old session clock.
- Bound controller calls and expose their failures while preserving explicit
  destination selection and no automatic fallback.
- Keep local Stop immediate even during controller I/O; cancel any pending resume
  so a late network response cannot restart playback after Stop.
- Verify one-click switching against the actual Embedded process after the final
  build, not just through a manually scripted sequence external to the product.

The first live discovery failure was traced to the previously shared emulator
auth helper exiting, not rejection of proxy authentication. A dedicated official
auth-only fixture process replaced that dependency. Private raw configuration
backups and an authenticated web restore path are available for final cleanup.


## Paused handoff — 07:04 ET

A browser viewport tool call made around 01:03 ET returned roughly six hours
later. The user-authorized 02:00 ET work window had passed. The coordinator
stopped new implementation and qualification work on recovery and initiated
restoration/cleanup. This is an incomplete PoC, not a completed one-click product.
No final frozen-binary live controller test was performed.

### Evidence at the pause

- Actual Embedded 6.0.4 authenticated and streamed A → B → A through the router
  into two native Rust recording fixtures. PID 763 and configuration hash stayed
  unchanged between route switches. No profile changes or server restarts were
  needed for those switches.
- Native Stop → route change → fresh initialize → one Play worked in the
  separate live orchestration, usually within roughly 3–5 seconds. Play then
  Seek restored position on a seekable source. Seek before Play returned OK but
  was ignored. A brief beginning-of-track segment before the Seek is a PoC
  tradeoff, not seamless switching.
- Different destination capabilities were refreshed correctly. An incompatible
  current output tuple refused playback and never fell back to another output.
- Independent final software run reported 58 Python cases: 56 passed, two failed.
  Nine Rust-side tests passed. The two failures are actionable controller gaps:
  unknown native State values do not refuse before route mutation, and Seek OK
  is treated as position restoration without confirming Status afterward.
  Final core inspection also found an untested review risk: NAA initialize/start
  milestones currently accept missing or unknown result values rather than
  requiring explicit observed success. This needs an adversarial check and fix.
- The selector was exercised in the browser against the actual router for
  add/select/persistence and against an isolated UI fixture for pending-select
  Stop, a subsequent selection, truthful no-audio status, and visible refusal
  with Retry. The later asynchronous-resume feedback changes were not browser
  verified; the responsive viewport check timed out and did not produce evidence.

### Remaining work before claiming completion

1. Fix the two independently failing controller checks, require explicit NAA
   success for milestones with adversarial missing/unknown-result cases, and rerun the frozen
   software gate. Review the final asynchronous controller and UI together.
2. Test the frozen executable with its own --hqp-control integration against
   actual Embedded: one-click A → B → A, position confirmation, paused/stopped
   behavior, Stop cancellation, unsupported-format refusal, unchanged PID/config.
3. Verify the final UI in the browser, including asynchronous failure and retry.
4. Rebuild the deliverable and record its exact validation. No physical DAC
   listening or independent physical payload measurement has been performed.

### Cleanup

The coordinator stopped the owned live router and isolated UI fixture server,
closed both temporary browser tabs, reset the viewport override, and restored
the preexisting discovery relay with its original command. The evidence worker
restored the raw Embedded configuration and verified both original hashes,
State 0 and an empty queue. Because raw file restoration alone did not reload
the running DSP, the original authenticated form was applied with the original
ALSA null device, then the exact raw files were restored again. Every native
State attribute now matches the original snapshot (including SDM, filters and
shaper); Embedded remains PID 763 with its September 5 start. Owned recording
fixtures, auth helpers/bridge, source server and task-specific ADB forwards are
stopped/removed. Detailed verification belongs in live-test.md. Private backups and raw captures remain outside git.


## Resumed isolated work — 07:20 ET

On the goal continuation, the coordinator completed local correctness work
without restarting Claude workers or touching restored Embedded. The prior turn
was progress: it added evidence and restored authoritative external state.

The two failing controller checks and explicit-success review gap are now fixed.
Additional cases cover stale stream milestones, Stop state readback and cancelling
a stalled preflight. The release passes 63 Python checks; nine Rust checks and
Clippy pass. The current source and release hashes are in software-gate.json.
Phone-width and asynchronous UI checks passed against an isolated backend.

The software portion has progressed; the requested complete outcome has not been
redefined. Final live integrated controller qualification remains required.
A new live-test window was requested because the original 02:00 cutoff passed.
Until that window is supplied, keep Embedded restored and do not restart its live
fixtures. The remaining physical-listening boundary is unchanged.

The scoped commit attempt waited for 1Password signing approval. It was cancelled
without bypassing signing, and only this task's files were unstaged to leave the
shared index available for concurrent metadata work. Source and the qualified
release artifact are preserved; the NAA changes remain uncommitted.

## Historical blocked audit — 07:23 ET, before renewed authorization

The release artifact still exists and every recorded source and binary hash
matches the current worktree. No new qualification run is needed to re-establish
the unchanged software result. At that checkpoint the manifest recorded that the integrated controller was not
yet live-qualified. The renewed qualification above supersedes this status.

The expired live-test window has remained the completion blocker through the
cutoff/restoration turn, the local software-repair continuation, and this current
revalidation. Local software work was completed during the second turn; no
remaining local substitute can prove actual Embedded one-click behavior. No new
live-test window has been supplied. The goal is blocked pending that user input,
not complete. Preserve Embedded's restored state and the recorded release.
