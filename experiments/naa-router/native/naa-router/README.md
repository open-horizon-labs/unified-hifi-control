# HiPhi NAA Router — private proof of concept

**Live one-click switching passed against HQPlayer Embedded 6.0.4.**
The qualified release switched A → B → A in 5.333 and 5.376 seconds,
restored playback position, and passed an actual browser selection. HQPlayer's
process and configuration stayed unchanged during switching. Tests used real
Rust NAA engines with software recording sinks and fresh official auth relays;
physical DAC playback and listening are not qualified. The software gate passes
63 Python checks and nine Rust-side checks, with Clippy clean.
See [verification](../../docs/naa-router/verification.md), the
[recorded build](../../docs/naa-router/software-gate.json), and the
[execution handoff](../../docs/naa-router/execution.md).

Select a destination NAA in a small local browser UI. HQPlayer sees one stable
adapter/device name (`HiPhi Router`) and device ID (`hiphi:router`). A route change
closes both ends of the existing session. The next HQPlayer connection goes to
the selected NAA with a fresh authentication exchange.

The proxy forwards the real NAA's authentication, capabilities, audio payloads
and clock feedback. It does not implement authentication, DSP or a DAC backend.
Authentication is never copied from a recording. Vendor runtime bytes are not
included. This is a standalone host experiment, independent of the Android build
and its default-off `enableHqplayer` integration.

## Discover destinations

With `--discovery-interface` enabled, opening the picker lists available NAA hosts
by name. Refresh runs a new two-second scan. Select saves the destination and
uses the existing routing flow; attached DAC choices follow the authenticated
connection. Results expire after 30 seconds without removing saved destinations.
Manual entry remains available if multicast cannot reach an endpoint.

Discovery is native NAA UDP multicast, not mDNS. It uses a separate socket and
never authenticates, starts playback, or switches routes by itself. See
[discovery verification](../../docs/naa-router/discovery.md). The live audio
qualification above applies to the earlier frozen build; the discovery extension
has separate software, LAN discovery and browser evidence.

## Read-through DAC lists

The picker retains the physical DAC IDs and names from successful `getdevices`
replies before rewriting them for HQPlayer. Lists appear under saved and discovered
endpoints, labelled with their last-observed time. Selecting a DAC saves or reuses
an explicit route for that device ID.

This is an in-memory observation cache, bounded to 256 endpoints. It survives
route changes and Stop, but not router restart. Every new successful reply replaces
the endpoint's whole list, including removed outputs. Unseen endpoints need their
first relayed connection; browsing cached lists never authenticates or polls a DAC.
Names/address aliases are not merged, and a last-seen entry is not proof that the
DAC is still attached. A cached choice uses the normal connection/error flow.

## Build and launch

Requires a Rust toolchain. Run from the repository root:

```sh
cargo build --release --manifest-path native/naa-router/Cargo.toml
native/naa-router/target/release/naa-router --config "$HOME/.config/hiphi-router/routes.json"
```

Open the printed control URL, normally [http://127.0.0.1:8787](http://127.0.0.1:8787).
Add your NAA's name and host, then select the route. The default NAA port is 43210.
No endpoint is selected implicitly when adding a route. A route with one output
can leave Device ID blank; its sole device is resolved after fresh authentication.
If it has multiple outputs, the UI reports those devices and requires your choice.
The optional JSON config persists routes and selection; omitting `--config` keeps
those settings only for this process.

For HQPlayer on another machine, bind to this machine's specific LAN IPv4 address,
allow that HQPlayer IP, and opt in to discovery on the same local interface.
The following addresses are documentation placeholders; replace both:

```sh
native/naa-router/target/release/naa-router \
  --naa-bind 192.0.2.10:43210 \
  --hqp-allow 192.0.2.20 \
  --discovery-interface 192.0.2.10 \
  --config "$HOME/.config/hiphi-router/routes.json"
```

Choose **HiPhi Router** as HQPlayer's NAA and output device once. Then use the
router's selector for destination changes. Discovery is off by default. With it
enabled, UDP discovery and TCP must use the same port; standard NAA discovery uses
43210. Another NAA service already using that port must be stopped or moved by its
owner before launching this router. `--hqp-allow` is repeatable, and is mandatory
for LAN NAA exposure. The browser/API listener remains loopback by default.

**Stop routing** clears the selected route and disconnects both sockets, so an
HQPlayer reconnect cannot resume forwarding until you explicitly select a route.
Editing a selected route also disconnects the current session. There is no
mid-stream handover or queued audio replay to the next destination.

## One-click switching with HQPlayer native control (optional)

Without control, a route change closes the NAA pair and HQPlayer stops. Pass the
explicit native control address (HQPlayer's control server, usually port 4321) to
enable the experimental controller (the failing gates above remain open):

```sh
native/naa-router/target/release/naa-router --hqp-control 192.0.2.20:4321 ...
```

Nothing is discovered or guessed; omit the flag to keep pure proxy behavior. The
router issues only `State`, `Status`, `Stop`, `Play` and `Seek`. It never loads
configuration or profiles, restarts, resets, changes rates or selects tracks.

Selecting a route while HQPlayer reports state 2 (playing) implements the
sequence exercised manually against Embedded 6.0.4; the integrated implementation
was live-qualified with this sequence: capture State and Status (track and
position), native `Stop` verified to state 0, commit the route and close the old
NAA pair, wait for a fresh successful downstream `initialize` on the new route,
a single `Play`, then require an accepted downstream `start`, positive
audio-section payload bytes on the new route and native state 2. Only then does the
router attempt an integer-second `Seek`, and only if the current track still
matches. It sets `position_restored: true` only after a bounded Status readback
confirms the same track and target position, allowing elapsed playback time.
An ignored or refused Seek leaves the new route playing and reports a visible
position warning. A paused session is explicitly stopped before changing routes and remains stopped.
An already stopped session also never auto-starts. An unreachable controller or unknown State value fails the
request before the route changes.

`/api/select` remains `{route_id}`. The controller is consulted only while an
NAA session exists (otherwise nothing can be playing through the router and the
route simply changes). For a playing source the request returns as soon as
HQPlayer is verified stopped and the route is committed, with
`hqp_control.phase` at `waiting_for_naa`; the resume continues in the background
inside the same 10 second budget and finishes at `idle` or `error`. A failed
resume (for example a destination that refuses the current format) keeps the
chosen destination selected and visible, reports the failure, tears down the
local pair, sends a bounded native `Stop`, and sets `routing_enabled: false` so
nothing can keep playing unexpectedly until the next selection. There is no
fallback to the previous route and no synthesized capabilities.

`state.hqp_control` is null without the flag, otherwise
`{address, enabled: true, phase, transport_state, track, position, position_restored, last_error}`
with phases `idle`, `checking`, `stopping`, `waiting_for_naa`, `resuming` and
`error`. `GET /api/state` reads cached values only. `POST /api/stop` halts local
routing first, cancels any in-flight selection and its control sockets, and then
sends a bounded native `Stop` and checks State 0; a controller failure there is reported without
delaying the local halt. A newer selection, Stop, or editing/removing the selected
route supersedes a pending operation, which then never issues a late `Play`.

## Control API

All responses are JSON except the browser page. Mutations require
`Content-Type: application/json`; errors are `{ "error": "reason" }`.
The loopback API rejects mismatched Host, Origin and cross-site browser requests.

| Method | Path | Request / response |
|---|---|---|
| GET | `/api/state` | Name, virtual ID, selected route, session, generation, last error, discovered devices, bound addresses |
| GET | `/api/routes` | Array of saved routes |
| POST | `/api/routes` | `{name,host,port?,device_id?}` → route with generated `id` |
| POST | `/api/routes/update` | `{route_id,name,host,port?,device_id?}` → updated route |
| POST | `/api/routes/remove` | `{route_id}` → state |
| POST | `/api/select` | `{route_id}` → state |
| POST | `/api/stop` | `{}` → state |

`session` is null or contains `state`, `route_id`, `peer`, `connected_at` (Unix
seconds), `bytes_to_naa`, `bytes_from_naa`, `initialized`, `started` and
`audio_bytes`. The byte counters measure transport, including protocol overhead;
`audio_bytes` counts only current-stream audio payload and resets on each start.
Initialization/start milestones require an explicit `result="1"`; a new start
cannot inherit accepted status or audio from the previous stream. “Forwarding” is not
confirmation of DAC playback.
`discovered_devices` contains `{id,description}` observations from the selected
endpoint; it does not scan other machines. Persistence errors fail route edits
and selection without changing the running route. Stop always clears the running
selection and disconnects first, even if saving fails; the unsaved Stop is reported
visibly and survives rejected reconnect attempts.

## Verification and limits

The current gate passes 63 Python cases (54 router cases and nine fixture/reference
client cases), plus nine Rust-side checks. The exact source hashes, release binary
hash and commands are recorded in the software gate linked above. Browser checks
cover cancellation, asynchronous success/refusal, Retry, and a 390px viewport.
These remain software evidence. The final live one-click controller qualification
is still required before calling the complete PoC verified.
```sh
cargo test --manifest-path native/naa-router/Cargo.toml
python3 tools/naa_router_lab.py --binary native/naa-router/target/debug/naa-router
```

The independent loopback fixture exercises distinct NAA identities and formats,
A → B → A changes, fresh auth, virtual device translation, exact PCM/DSD and side
payload forwarding, clock records, coalescing/fragmentation, refused formats,
partial-frame cancellation, stop, malformed input and persistent configuration.
These are software fixture results, not listening or independent DAC payload
measurements. See `docs/naa-router/execution.md` for current live observations.

This PoC relies on the observed protocol-6 framing already implemented in
`native/naa-native`: newline XML control; 32-byte upstream binary header with
sample and side-section lengths; 16-byte downstream feedback. It reuses the native
crate's discovery validation/response and header size, with attributed framing
logic adapted for opaque forwarding. Supported framing is PCM 8/16/24/32/64 bits
and native DSD (`stream="dsd" bits="1"`); unsupported framing fails explicitly.
The actual downstream NAA still decides whether the requested format can open.
Capability replies are never synthesized or expanded.

Restart-free routing still depends on HQPlayer initiating a fresh NAA session
after route change and accepting refreshed downstream capabilities. Any confirmed
Embedded limitation belongs in the current execution record; a passing loopback
test does not establish that behavior. The native-control sequence is modeled on
live Embedded 6.0.4 observations, but this crate's own tests only exercise it
against loopback fixtures. Seamless mid-track switching and independently
authenticated sessions are outside this PoC's implemented mechanism; a brief
restart from the beginning before the after-Play seek is an accepted trade-off.
