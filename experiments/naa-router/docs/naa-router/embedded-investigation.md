# Embedded routing investigation — 2026-09-14

## Current evidence

This is a read-only investigation for the router PoC. No output was selected,
no playback started, and no Embedded configuration or service was changed.

- The user's previously identified Embedded host responds today on native TCP
  4321: `product="Signalyst HQPlayer Embedded"`, `version="6"`,
  `engine="6.0.4"`, `platform="Linux"`. `State.state` is `0` (stopped).
- `ConfigurationGet` returns `result="OK" value=""`: the base configuration is
  selected. **This operation returns the active profile name, not the output
  configuration or DAC.** It cannot identify the current physical route.
- GET `/config` on that host's port 8088 returns HTTP 401 Digest authentication.
  The local UHC configuration files inspected contain no credentials configured
  for this host. Loopback fixture credentials were not tried on the live host.
- The existing SSH configuration contains no alias for this Embedded host. No
  SSH connection was attempted using a guessed username or unrelated identity.
- Local Desktop 5.35.10 is also running and stopped, with native control on
  loopback 4321. Android `emulator-5554` is attached. Its HiPhi application and
  `libnaa_android.so` are running. This is a separate possible fixture path;
  it is not evidence about Embedded reconnection.
- The already-running discovery relay explicitly accepts only loopback and the
  local Mac address. It does not accept the known Embedded peer. Any Embedded
  experiment must explicitly add that peer to its test allowlist; broad LAN
  discovery or arbitrary physical output selection is not needed.

The private host address, initial native response and unauthenticated HTTP
response are retained outside git under `/tmp/hiphi-naa-router-private/`.
This is a baseline observation, **not a configuration backup**: the actual
output and full configuration are still unread. Before changing them, preserve
an authenticated form snapshot privately. Do not use `/backup/settings.zip`:
UHC's existing evidence ledger records that handler copying the filesystem root
on this host family.

## Control operations

The locally inspected official ControlInterface 6.0.1 source implements:

- `<Stop/>` and `<Play last="0"/>` as ordinary native control messages.
- `<ConfigurationGet/>` for the active configuration name.
- `ConfigurationLoad` through authenticated encrypted session machinery.
- `<Reset/>`, which is not a demonstrated substitute for session reconnect:
  this repository's earlier Desktop experiment could not validate its response.

No `SetDevice` or `GetDevices` operation is exposed in that inspected control
client. This does not prove the server has no additional operations; it means
we do not have an evidenced native API that changes the output directly.
The existing `tools/hqp_control.py` implements Stop/Play with bounded XML framing
and response checks, currently restricted to loopback. A router control client
can use the same shapes for an explicitly configured Embedded address.

The UHC source and evidence ledger separately demonstrate authenticated
browser-form profile selection. That is available precedent for the one-time
setup, but repeatedly loading profiles would miss the user's aim.

Primary-source implementation reference:
[RooNAA6 main.rs](https://github.com/piercer/RooNAA6/blob/master/src/main.rs).
The currently fetched source resolves one target before entering its TCP accept
loop and opens each accepted connection to that target. It supports the basic
forwarding premise, not dynamic target selection or restart-free switching.

## Dissent

**Decision:** ADJUST the experiment, retain the transparent-session router.
Confidence is medium for the forwarding design, unproven for Embedded turnover.

The strongest case for the design is that a stable configured NAA address and
virtual DAC let HQPlayer perform a fresh end-to-end handshake with whichever
actual NAA the user chooses. DSP and downstream feedback remain unchanged.
There is no need to synthesize independent NAA authentication.

Three failure scenarios matter:

1. **Functional:** HQPlayer caches an authenticated identity or capability list
   and refuses a new destination despite an unchanged discovery name. Testing
   two DAC labels behind one auth helper would hide this failure.
2. **Adoption:** switching works technically but still needs manual Stop, Play,
   reconnect dialogs or an Embedded restart. The selector then leaves the user
   coordinating the same machinery the PoC is meant to hide.
3. **Opportunity cost:** adding fake format offers, clock emulation and silent
   conversion to paper over incompatible destinations turns a transparent
   router into another audio engine. Genuine downstream rejection and format
   feedback should remain authoritative.

| Assumption | Current evidence | Required check |
|---|---|---|
| Fresh downstream auth can pass through | Existing Rust auth relay and past live Desktop runs | Verify fresh request/reply reaches each selected peer unchanged, including distinct downstream `endpoint_id` values |
| Stop causes a fresh NAA session | Contrary evidence: existing captured Desktop sessions reuse their TCP connection after Stop | Explicitly close both old NAA sockets at selection; observe new auth and initialize after Play without restarting HQPlayer |
| Stable discovery name means stable authenticated identity | Not established; auth response contains actual downstream endpoint ID | A → B → A with distinct authenticated peers; never rewrite the signed auth envelope |
| Virtual DAC alias is enough | Native initialize carries a device ID, and capabilities follow initialization | Translate only device-bearing control fields; exercise distinct IDs and different supported rates/channels |
| Control success means routing succeeded | Native command acknowledgement is only command evidence | Require new-route stream start, payload arrival and downstream feedback before reporting active routing |
| Transport-control connection proves process continuity | A connection may drop for reasons other than restart | Prefer process PID/start-time observation through an authorized host session; absent that, retain an uninterrupted native observer and state the narrower claim |

The reconstructed story is: the transparent transport remains the simplest
credible mechanism. The weakest assumption is Embedded's recovery behavior,
followed by cached authenticated identity. Stable naming alone proves neither.
Do not claim these risks retired from synthetic tests or existing Desktop runs.

## Reversible live test plan

1. Start with two explicitly named recording-only NAA destinations. Each needs
   its own authenticated identity for the decisive identity test. Two fixture
   instances sharing one official auth helper provide useful transport evidence
   but do not retire the distinct-identity risk.
2. Run the proxy on a known test host/interface with only the exact Embedded
   peer permitted. Keep control UI loopback-only or explicitly authenticated.
3. Read and privately save the actual Embedded configuration/form, queue and
   initial transport state. Select the proxy and stable virtual DAC once. A
   setup restart, if required, is separate from measured route switching.
4. While A plays to its recording sink, choose B in the selector. The operation
   should issue Stop, confirm a stopped boundary, close A's session, commit B,
   then issue Play only if playback had been active before selection.
5. Verify fresh auth, initialize translated to B's real device ID, B's honest
   format negotiation, no late A writes, and unmodified payload bytes. Record
   elapsed selector-to-new-stream latency and control/session failures.
6. Repeat B → A without changing any Embedded profile, XML or process. Repeat
   while initially stopped and assert that selection does not start playback.
7. Deliberately choose an unreachable route and a destination incompatible with
   the current format. Fail visibly without silently falling back, processing
   audio, or reporting a selected-but-unconnected destination as active.
8. Restore the original configuration, queue and stopped/playing state only
   after those values have actually been captured and the relevant physical
   playback is authorized. Preserve all private traces outside git.

Read-only discovery has reached the configuration-access boundary. The live
Embedded route-switch result is still missing. Native reachability and a
successful `GetInfo` are useful setup evidence, not a routing completion claim.


## Follow-up evidence changed the identity assumption

The live experiment later forwarded fresh authentication from both the emulator
and the specifically identified `cm4nano` official runtime, with every later
operation handled by recording sinks. The physical peer received no initialize
or audio operations. Both providers returned the same `endpoint_id`, so this
field cannot currently be treated as a per-machine identifier. Requiring it to
change was an unsupported interpretation. Keep opaque authentication unchanged
and test handoff between actual providers; do not manufacture field variation.
The detailed routing/control results are in [live-test.md](live-test.md).
