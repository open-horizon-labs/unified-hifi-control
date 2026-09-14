# HQPlayer network output routing

The `naa-proxy` feature gives HQPlayer one stable NAA device while UHC chooses
the physical NAA and DAC behind it. HQPlayer does not need a new profile or a
restart when you switch outputs.

The normal path is:

```text
HQPlayer
  └─ selects HiPhi Router (`hiphi:router`) once
      └─ UHC relay forwards authentication, control, and audio
          └─ selected NAA endpoint and DAC
```

This is a transparent relay in the RooNAA6 sense. UHC does not resample,
mix, or otherwise process PCM or DSD. It changes only the length-delimited NAA6
metadata sections when metadata is available, so the downstream device can show
the current title, artist, and album. The frame writer also accepts artwork
payloads; the current HQPlayer adapter does not yet fetch its cover bytes for
injection. The audio payload passes through unchanged.

UHC takes metadata from the already-bound HQPlayer zone projection. An optional
source-zone fallback is only relevant when HQPlayer supplies no usable track
metadata; it is not required for ordinary HQPlayer playback.

Server builds include this feature by default. A relay-free server remains
available explicitly with `--no-default-features --features server`. The imported
experiment's historical hardware results do not qualify a new integrated build.
Current qualification is tracked in [the integration audit](../experiments/naa-router/GOAL-AUDIT.md).

## One control service, three clients

The HQPlayer page, HTTP and MCP use the same exact-instance command service.
An instance is addressed as `hqplayer:<instance>`; a missing or unknown target
never selects another instance.

| HTTP | MCP | Purpose |
|---|---|---|
| `GET /hqplayer/outputs?zone_id=...` | `hifi_hqplayer_outputs` | Read committed routes, relay state, discovery, DAC observations and operations |
| `POST /hqplayer/outputs/command` | `hifi_hqplayer_output_control` | Submit a typed action |
| `GET /hqplayer/outputs/operation?zone_id=...&operation_id=...` | `hifi_hqplayer_outputs` with `operation_id` | Follow an accepted operation |

MCP returns these payloads in its existing structured envelope's `data` field.
Use the configured UHC controller authentication for protected mutations.
Credentials for HQPlayer remain in UHC's existing credential configuration;
output commands do not accept passwords, upload URLs or arbitrary XML.

## Straight pass-through setup

Configure the relay once, select `HiPhi Router` in HQPlayer, and leave that
device selected. After that, output changes happen through UHC's route command;
HQPlayer stays connected to the same virtual device.

The relay forwards the NAA authentication handshake and control messages to the
currently selected endpoint. It enumerates that endpoint's DACs through the
forwarded `getdevices` exchange, then forwards the audio stream. If the endpoint
goes away, UHC reports the connection error; it does not silently choose another
DAC.

## Setup and switching sequence

1. Configure the HQPlayer instance through UHC's existing instance settings.
   Read its output projection to obtain `source_epoch` and `output_revision`.
2. Submit `relay_configure` with `enabled: true`, an explicit TCP `bind`,
   optionally supply `hqp_allow` to restrict source IPs, and set the local IPv4
   `discovery_interface`. An empty allow-list accepts any reachable NAA peer. The default discovery port is 43210. For standard
   HQPlayer discovery, use TCP and UDP port 43210 on the selected address.
   A custom discovery port requires a peer configured to query that port;
   it does not make an unmodified Embedded scanner discover arbitrary ports.
3. Run `discover`. This is NAA's XML multicast discovery, not mDNS. It discovers
   NAA hosts without initiating authentication or playback. Add chosen hosts
   with `route_add`, including a physical `device_id` when known.
4. If HQPlayer is not already configured to use the virtual device, use
   `setup_preview` and, after inspecting it, `setup_apply` once. This is initial
   setup only. Route switches do not edit a profile or restart HQPlayer.
   Inspect `setup_readback` when you need proof that the initial configuration
   reached HQPlayer's running engine; an upload acknowledgement alone is not that
   proof.
5. Submit `select` with the route ID. Poll its operation until `outcome` is
   non-null. Initial selection without an existing relay session can complete
   before audio starts. A switch from active playback requires fresh session
   and stream evidence; inspect the operation's evidence and current projection.
6. `stop` cancels a pending selection and stops the current forwarding pair.
   It does not require a fresh revision and remains available during a switch.

Each mutation uses the latest `source_epoch` and `output_revision` as
`expected_source_epoch` and `expected_output_revision`. `stop`, `discover`,
`import_preview`, `setup_preview` and `setup_readback` are exempt. Use mutation
revision, not `aggregate_revision`: telemetry can advance the latter while
audio is playing without changing routing configuration.

Example selection body (replace the example identifiers and revisions):

```json
{
  "zone_id": "hqplayer:living",
  "correlation_id": "client-unique-command-id",
  "expected_source_epoch": 3,
  "expected_output_revision": 12,
  "action": "select",
  "route_id": "chosen-route-id"
}
```

`accepted: true` is command admission. Follow `operation.operation_id`; terminal
outcomes include `complete`, `cancelled`, `rejected`, `failed`, `partial` and
`indeterminate`. Reuse the same correlation ID and payload when retrying an
uncertain request; changing the payload under that ID is a conflict. Read current
state before making a new decision. Operation history is bounded to 32 records,
so correlation history is not a permanent transaction ledger.

## DAC names and evidence

HQPlayer sees the stable virtual device and configured adapter name. UHC's
physical DAC picker reads `dac_observations`, populated by forwarded `getdevices`
responses during a fresh authenticated connection. It does not independently
authenticate to every discovered host.

Observations are keyed by host and port, including device IDs, names, provenance
and observation time. A new reply replaces the endpoint's previous list. Missing
observations mean unknown; an observed empty list means that reply contained no
devices. Cached names do not prove current attachment. A route with no device ID
can resolve an endpoint's sole output; ambiguous multiple outputs require a
choice instead of silently selecting one.

Total network bytes include protocol traffic. Positive current-stream audio
payload after an accepted start is forwarding evidence; an old operation's
evidence is historical and is not a claim that audio is flowing now. Software
payload comparisons and a physical listening report are separate evidence.

## Import and lifecycle

`import_preview` accepts the original routes JSON and reports valid routes and
conflicts. `import_apply` requires the same content and matching preview ID.
Stable route IDs are preserved; the file's selected route is reported but never
activated implicitly. Route CRUD and relay settings belong to their exact UHC
instance and persist through its configuration owner.

Disabling or removing the instance stops its owned relay. Disabled relay settings
open no listener. A bind or worker failure is unavailable state, not an empty DAC
inventory. There is no second proxy web service to run or automate.

Build the matching Dioxus client and server; the server build includes
`naa-proxy` by default. A server-only Cargo build does not produce a working
hydrated UI.
See the repository's normal Dioxus build instructions for its supported CLI
version and deployment layout.

For a source-built Docker image, the default
`UHC_SERVER_FEATURES=server,naa-proxy` enables the relay in both the Dioxus server
build and the final binary embedding its assets; the client remains `web` only.
Override the argument with `server` when a relay-free image is specifically needed.
Runtime relay enablement is still explicit per instance.

NAA multicast needs access to the intended LAN interface. The repository's Linux
host-network deployment pattern avoids Docker bridge multicast isolation; verify
actual discovery and TCP reachability in the deployment topology. A loopback
software fixture does not qualify a container's LAN multicast behavior.

## Local browser qualification

`tests/hqp_outputs_browser_fixture.rs` is an ignored, bounded launcher for two
software HQPlayer peers and two NAA destinations. Run it with a fresh
`UHC_BROWSER_FIXTURE_DIR` and the `naa-proxy` feature:

```sh
cargo test --features naa-proxy --test hqp_outputs_browser_fixture \
  browser_software_peers -- --ignored --nocapture
```

The launcher writes `peers.json` containing loopback addresses and a stop-file
path. Configure a separately running, isolated, fullstack UHC server through its
public API using those addresses. Both peers begin stopped; the fixture mirrors
native Play/Stop commands into software NAA traffic. Create the indicated stop
file to shut it down, or let its bounded lifetime expire (30 minutes by default,
controlled by `UHC_BROWSER_FIXTURE_SECONDS`, maximum two hours).

The final fixture report records received payload comparisons. Starting this
launcher, or its test exiting successfully, is not a UI acceptance test: record
the actual browser interactions, public operations and forwarding evidence
separately. It does not supply an Embedded configuration web server or claim
hardware/authentication qualification.
