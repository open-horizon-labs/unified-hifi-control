# Live software qualification

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

`tools/naa_router_live.py` starts two independent instances of the existing
Rust NAA engine. Each uses a Unix IPC recording sink with a simulated clock,
44.1/48 kHz S32 stereo offers and bounded buffering. It opens no audio device,
changes no HQPlayer settings and advertises no discovery service. The router
can use these explicit loopback destinations for a real HQPlayer experiment.

Build and start:

```sh
cargo build --manifest-path native/naa-native/Cargo.toml --bin naa-android
python3 tools/naa_router_live.py --auth-port 49301 \
  --directory /tmp/hiphi-router-live-new --seconds 1800
```

The auth port must already lead to an authorized official NAA helper. Do not
invent auth responses or reuse captured challenges. In the 2026-09-14 run,
the existing emulator's native process environment identified its official
helper port; a new, non-rebinding ADB forward mapped that exact port to 49301.
No preexisting forwarding rule or runtime was changed. Helper port values are
runtime-specific: recheck the actual process before recreating this setup.

The ready JSON prints exact loopback ports, source WAV and child PIDs. Defaults
are A at 49311 and B at 49312. Both expose native device ID
`hw:CARD=ANDROIDNAA,DEV=0` with distinct recording descriptions. Configure only
these destinations in the router. By default both fixtures delegate fresh auth
to the same helper. `--auth-port-b` permits a separately authorized provider for
B; the actual-provider follow-up below exercised that path. The separate router
protocol lab covers distinct virtual/physical device IDs.

`source.wav` is a 30-second generated stereo PCM fixture for explicitly loading
into HQPlayer. It is media, not an automatic playback instruction. Select the
router once before starting it, and use PCM output within the offered rates.
Embedded needs the source reachable at an explicitly configured local test URL;
the harness does not start an HTTP or discovery server.

During A → B → A, preserve actual native session traces or router events,
HQPlayer control state, and sink `events.jsonl`. After graceful stops and harness
termination, `report.json` compares each nonempty accepted PCM file with its
simulated-clock WAV payload. Aborted streams legitimately may differ and are
recorded as aborts, not converted into a passing drain. This comparison checks
the recording sink; independent router wire/payload equality remains a separate
requirement. Capture files remain outside git.

A synthetic-controller smoke check exercised the harness with the compiled Rust
engine: exact grant, start, 17,640 payload bytes, real elapsed-clock feedback,
and complete drain. Accepted PCM and rendered WAV matched. That result validates
the new fixture integration only; actual HQPlayer evidence appears below.

Cleanup: terminate the harness PID from its `ready.json` with SIGTERM, wait for
its report and children to exit, and remove only the ADB forward created for
this experiment (`adb -s emulator-5554 forward --remove tcp:49301`). Restore any
preexisting discovery listener using its recorded original command. The harness
owns and terminates only its own Rust children.

## Embedded observations from this PoC

Actual Embedded 6.0.4 on the explicitly configured host streamed through the
router into both recording fixtures on 2026-09-14. The process remained PID 763
with its September 5 start time. After one-time setup, the persistent
configuration SHA-256 was
`bf6078dce3a9c7cbfc29144dffe6d2f6fabe155ae910e0de79eaaf0610ddc783`
and stayed unchanged through the measured route changes. These are software
recording observations, not physical DAC qualification.

- Real native transport state codes were measured: stopped `0`, paused `1`,
  playing `2`. A successful native command response does not prove application.
- Direct active route selection produced fresh auth/initialization after about
  2.9 seconds, but Embedded continued reporting playing with no new audio until
  its roughly ten-second failure timeout. It then stopped and reconnected again.
  One `Play last="0"` after the second initialization resumed actual audio.
- Explicit `Stop`, route selection, waiting for the new initialized connection,
  then one `Play last="0"` resumed audio faster. A trial waited five seconds;
  a separate immediate-Play trial needed a later Play after reconnect. These
  findings justify integrated transport control rather than claiming that the
  original socket-only selector already resumed playback automatically.
- Stop/Play restarts the track. The official control source's
  `Seek position="45"` uses integer seconds and was measured live: with a
  seekable source, position advanced from 45.85 to 47.56 seconds while playing.
  Seek while fully stopped returned OK but was ignored on the measured run.
  An HTTP source lacking range support explicitly rejected Seek as not seekable.
  Position preservation therefore requires both a supported source and actual
  readback; a pre-Play OK is insufficient evidence.
- For different capabilities, A advertised only 44.1 kHz and B only 48 kHz.
  Embedded's `GetRates` correctly changed from `[Auto,44100]` to `[Auto,48000]`.
  This rules out a stale rate enumeration in that trial. With the unchanged
  44.1 kHz source/default output, B did not stream; A streamed again when
  reselected. One evidenced same-mode reload did not change that result. The
  proxy must report incompatible effective output clearly and must not silently
  choose DSP settings, invent formats or convert samples to conceal it.
- Two separate official helper processes on the same emulator returned the same
  authenticated endpoint ID. Different ports, names and working directories are
  not evidence of distinct identities. Both helpers only receive the fresh auth
  exchange; recording fixtures implement every later protocol/audio operation.

Private measurements are under `/tmp/hiphi-naa-router-private/`, with recording
runs in `/tmp/hiphi-router-live-20260914-v3/` and
`/tmp/hiphi-router-live-caps/`. Complete drained streams had exact accepted/WAV
payload equality; deliberate socket-abort cases retained their unequal tails as
abort evidence. No timeout or abort was relabeled a complete drain.

The follow-up test used two actual official authentication providers on different
hosts: the emulator helper and the exact `cm4nano` peer identified by Embedded's
own current discovery log. A bounded auth-only bridge sent one fresh
`authenticate` request, received one reply, and closed that physical socket
before returning the reply. Its manifests record zero initialize and audio bytes
sent to the physical peer. All later operations and audio remained in recording
fixture B. A → B → A streamed successfully with both actual providers, unchanged
Embedded PID and unchanged persistent configuration.

Contrary to the initial risk framing, both actual official providers returned
the same `endpoint_id` field. Its uniqueness semantics are therefore unknown;
the field must not be asserted to identify a machine. This result establishes
fresh handoff between actual providers, not a claim that the field varied.
Across the two actual providers, 20 fresh exchanges produced 20 distinct full
response and public-key hashes, while the `endpoint_id` field stayed the same.
The private evidence is `different-actual-auth-peers-live.json` and
`/tmp/hiphi-router-cm4-auth-only/`. The completed recording report at
`/tmp/hiphi-router-live-distinct-auth/report.json` has three streams, totaling
3,838,464 accepted bytes; every accepted payload exactly matched its simulated
rendered WAV payload. This checks the recording sink, not physical playback.

## Final restoration and qualification boundary

The final integrated one-click controller was **not live-qualified**. Tool dispatch
stalled across the authorized 02:00 ET cutoff and recovered around 07:04 ET;
work then stopped at restoring the environment. Manual real Embedded A → B → A,
actual-provider auth forwarding, seek semantics and capability observations above
remain the measured evidence. They do not establish the final controller's
one-click, cancellation or position-restoration behavior.

At 2026-09-14 11:08:17 UTC, restoration checks passed:

- `/etc/hqplayer/hqplayerd.xml` exactly matched its original SHA-256,
  `7a8c3130cc533230bc883745f0672a19f4c70d2ecec0c7b91959c7166c7f371a`.
- `/var/lib/hqplayer/hqplayerd.xml` exactly matched its original SHA-256,
  `5dc488db18e40f58889fec0c549986dc076153182fb05adf487052df659c9c94`.
- Every native `State` attribute matched the saved initial response, including
  stopped `0`, SDM mode `2`, filters `48`/`51` and shaper `24`. The original
  queue was empty and was restored empty. PID 763 and its September 5 start
  time remained unchanged.
- The original authenticated form was applied with the original XML's literal
  ALSA `null` device to reload running settings, followed by raw XML restoration.
  This second step matters: web `/restore` alone restores disk bytes but does
  not reload the currently running DSP settings. No playback was started during
  restoration.
- Owned fixture engines, auth traces, HTTP source, auth-only bridge and the two
  dedicated emulator auth helpers were stopped. Only the two test-created ADB
  forwards were removed; the four preexisting forwards remained. The parent
  restored the original discovery relay. Private captures and reports remain
  outside git.

`/tmp/hiphi-naa-router-private/final-restoration.json` records the exact-file,
native-state, queue and process assertions. No physical audio was played.
