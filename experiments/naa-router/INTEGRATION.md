# NAA routing as an HQPlayer capability in UHC

## Salvage

Original aim: change NAA/DAC destination without HQPlayer Embedded profiles,
XML work, or a server restart. The standalone PoC proved the mechanism, then grew
its own picker, discovery and DAC cache. Salvage is warranted because treating
that second application as the final product would duplicate UHC's orchestration.
The useful artifact is the protocol machinery and evidence, not another control UI.

### Learnings that change the integration

1. A stable adapter name and virtual device ID can stay selected in Embedded
   while downstream NAA sessions change. Fresh auth is relayed on every session;
   recorded auth is never replayed. Identical observed endpoint IDs across hosts
   are not a safe unique-identity key.
2. Switching is a protocol/transport operation, not merely changing a TCP target.
   Actual Embedded required verified Stop, fresh initialize, one Play, accepted
   start and positive current-stream audio, then same-track Seek/readback. The
   original live artifact measured A→B→A at 5.333/5.376 seconds without restart.
3. Paused connections must be stopped before switching; they remain stopped.
   Native Stop can time out after local routing is already halted. This is a
   partial result, not proof that the downstream is still receiving audio, nor
   proof that HQPlayer transport stopped.
4. Discovery is NAA XML multicast, not mDNS. Host discovery needs no auth. DAC
   enumeration comes through an authenticated session. Last-seen DAC lists are
   observations, not authoritative current attachment or a new auth capability.
5. Native acceptance and resumed audio are different milestones. Stale start
   flags, non-audio traffic and a successful Seek ACK each gave tempting false
   positives; current-stream payload and native readback are necessary evidence.
6. Configuration restore applies asynchronously. Verify runtime and byte-exact
   disk state after it settles; an immediate state read is not final. This belongs
   to one-time setup/recovery, never ordinary routing.
7. The canonical v4 repository has moved to `src/open-horizon-labs`. Its HQPlayer
   adapter already owns operation/conversation leases, reliable command routing,
   source epochs and aggregate publication. A second native controller is a race.

Reusable fragments: all imported Rust relay/state/controller/discovery code,
HTTP protocol, browser UX behaviors, independent Python/Rust fixtures, source
attribution and historical evidence. Preserve no private vendor data. Source is
preserved verbatim under native/tools/docs with hashes in IMPORT.json.

## Solution space

Problem: make all routing capabilities natural HQPlayer capabilities in UHC,
with equal UI, HTTP and MCP access. Constraint: one exact-instance command owner
and one aggregate state projection, preserving existing API compatibility.
Success: any surface can perform the same output workflow; two instances cannot
cross-route; Stop defeats pending resume; HTTP acceptance is never called playback.

| Candidate | Frame | Decision-changing cost | Disposition |
|---|---|---|---|
| Link or iframe the PoC picker | Another app is acceptable | Duplicated settings, state and agent workflow | Rejected |
| HTTP wrapper directly from UHC UI/MCP to standalone router | Sidecar owns control | Bypasses coordinator/aggregator; native Stop/Play races | Rejected as production design; headless PoC bridge is transitional only |
| Managed relay service with UHC command/state ownership | UHC owns experience; relay owns wire sessions | Requires explicit lifecycle, control hook and ordered projection | Selected |
| Lifecycle-owned relay core inside the adapter process | Explicit ownership can replace a separate daemon | Requires owned blocking threads, bounded shutdown and cancellation fencing | Selected implementation of the managed-service boundary after extraction; qualification pending |

Interpretive variety: wrapper options preserve the standalone-controller frame;
managed relay tests it against UHC's already-established command ownership.
Failure to fence native commands or publish authoritative state means redesigning
that boundary, not adding sleeps or UI-specific retries.

## Dissent and reconstructed decision

Strongest case for a simple wrapper: HTTP already covers all operations, preserves
working Rust, and makes MCP cheap. Contrary evidence: UHC's coordinator confirms
commands only after causal projection, while the PoC select returns before resume;
the PoC also talks directly to HQPlayer. A wrapper could report success early and
race profile or pipeline changes. These are code-visible mismatches.

Pre-mortem: (1) independent controllers issue a late Play after Stop; (2) users
still configure a second app, so integration changes nothing; (3) a full rewrite
loses the byte-transparent relay behavior and spends effort re-proving auth.

Decision: ADJUST. Preserve the relay protocol core, replace standalone native
transport control with a UHC-owned hook, and project observations through the
existing bus. The implementation now uses an in-process lifecycle owner rather
than a second daemon: the extracted worker core has no native-control client or
HTTP listener, and an external owner guard stops and joins its blocking workers.
This revises the initial process-boundary choice without changing the required
command, state, cancellation or API ownership. Lifecycle and concurrency tests
must qualify that boundary before delivery. The weakest assumption is that this control hook can preserve
cancellation without a lease deadlock. A held-response concurrency test must
falsify it before production integration is considered complete.

## Ownership and all-feature migration map

| PoC capability | UHC home / mechanism | Required preserved behavior |
|---|---|---|
| Stable NAA identity, TCP relay, auth, payload/feedback | Optional lifecycle-owned relay core; extracted protocol.rs | No DSP, auth replay, format rewrite or silent fallback |
| Native Stop/Play/Seek handoff | HqpAdapter exact-instance operation lane and existing command gateway | One controller; confirmed milestones; cancellation fencing |
| Instance configuration and proxy lifecycle | HqpInstanceManager + AdapterCoordinator | Disabled means no listeners; one relay per explicit instance; restart/reconfigure fences old work |
| Route CRUD and import | Instance-owned settings, stable route UUIDs | Read old PoC JSON; preview mapping; preserve exact host/port/device; never import active playback implicitly |
| Discovery and refresh | Relay observation producer | Explicit interfaces; bounded scan; exclude self; no auth/playback side effects |
| Read-through physical DAC lists | Aggregate output observation cache | Per endpoint, observation time/provenance, whole-list replacement, unknown differs from empty |
| Selection/Stop/retry | Shared output command service | Same semantics for every surface; no hidden Play from UI/MCP; Stop cancellation has priority |
| Session/traffic/errors | Ordered output projection alongside HqpSnapshot | Bytes vs actual audio distinguished; preserve error/partial/indeterminate outcomes |
| Output/DAC picker | Existing Dioxus HQPlayer page and controls | Read aggregate only; preserve DSP controls/profile functions; no iframe |
| HTTP | Additive shared-service handlers and route contract tests | No existing route/schema removal; controller auth applies to mutations |
| MCP | Existing toolbox, handler and structured envelope | Exact zone targeting; truthful capability; no raw bridge calls around gateway |
| Live setup/rollback | Existing HQPlayer credential/config machinery plus verified transaction | One-time configuration only; no credentials in tools, logs or git |
| Packaging | Private opt-in feature/service bundle | Default off; explicit bind/allow-list; Docker multicast topology verified before deployment |
| Regression corpus and original native engine | Isolated crates/tools now imported | Run original adversarial suite; do not substitute mock ACKs for real audio evidence |

Relevant existing seams: src/adapters/hqplayer.rs and lifecycle.rs;
src/bus/runtime.rs::HqpRuntimeCommand; src/knobs/routes.rs dispatch helpers;
src/aggregator.rs::HqpSnapshot; src/producers/hqplayer_command_service.rs;
src/api/controller_auth.rs; src/mcp/tools/hqplayer.rs, handler.rs, envelope.rs;
src/app/pages/hqplayer.rs and components/hqp_controls.rs.

## Shared API/MCP contract for production integration

This is a proposed additive contract, not endpoints already installed in UHC.
The user's request authorizes designing API/MCP access; retain the repository's
explicit API contract review and required release label before publishing a
production contract. Do not silently change frozen legacy endpoints.

Every command addresses `hqplayer:<instance>` explicitly, plus expected source
epoch/output revision and a caller correlation ID. Browser handlers, HTTP and MCP
all construct the same typed command. Mutation URLs, arbitrary proxy addresses,
credentials and native XML are never tool parameters.

Proposed HTTP surface:
- GET /hqplayer/outputs?zone_id=... — aggregate routes, discovery, DAC cache,
  selected route/DAC, proxy/session state and current operation.
- POST /hqplayer/outputs/command — typed action: discover, route_add,
  route_update, route_remove, select, stop, import_preview or import_apply.
- GET /hqplayer/outputs/operation?zone_id=...&operation_id=... — correlated status.

Proposed MCP surface: `hifi_hqplayer_outputs` for aggregate reads and operation
inspection; `hifi_hqplayer_output_control` for the same typed command actions.
Use UHC's existing Envelope/Scope/refusal classes and append tools without
reordering existing ones. Add capability/contract fixtures deliberately.
Existing hifi_hqplayer_status can link to output observations without inventing
another instance resolver. Keep discovery, route and DAC IDs usable by commands.

Projection includes instance/source epoch, aggregate revision, route generation,
operation ID, phase, selected route/DAC, device observations and freshness,
initialized/started/current-stream audio bytes, transport state, position restoration
and classified errors. Desired destination and observed forwarding destination
are separate fields. An unreachable proxy is unavailable, not an empty inventory.

Command phases: admitted → stopping → connecting → initialized → resuming →
forwarding/complete, with cancelled, rejected, failed, partial and indeterminate
outcomes. Return a correlated accepted operation first; confirm only after the
matching projection commits. The relay must expose ordered observations or be
polled under explicit epochs with reconciliation on gaps. Do not reconstruct
canonical state independently in UI, HTTP or MCP.

Normal Stop must immediately cancel the relay generation/close its session,
then request native Stop through UHC. It must not wait behind the operation it
is cancelling. Cancellation closes held network operations and prevents any
late Play/Seek. A rejected or stale request never selects another instance.
A client disconnect is not a rollback; operation lookup tells the client what
happened. Deduplicate retries by correlation AND request fingerprint.

One-time setup should have preview/apply/readback/rollback API operations through
UHC's existing credential owner. It must not require browser automation. Output
selection never loads a profile or restarts Embedded. User-requested profile and
pipeline operations still work, but share the lease and invalidate incompatible
pending output work. Cached DAC selection must be verified on a fresh connection;
no present-DAC claim from cache and no automatic format conversion.

## Execution checklist and risk retirement

| Step | Required outcome | Wrong shortcut the check must fail | Status |
|---|---|---|---|
| P1 | Preserve complete source/evidence and headless PoC operations | Copy UI only; leave uncallable DAC/discovery features | Implemented here; import hashes + copied Rust and MCP wire tests |
| P2 | Managed relay lifecycle and UHC native-control hook | Two independent HQPlayer controllers | Pending: gated concurrent route/Stop/profile/reconfigure test, no late native commands |
| P3 | Aggregate projection and shared command service | UI reads proxy cache directly; HTTP 200 means playing | Pending: delayed/out-of-order observations, wrong epochs, ACK-without-audio, indeterminate native write tests |
| P4 | Route settings migration, discovery and DAC freshness | Auto-select imported route; merge same-name hosts; retain removed DAC | Pending UHC tests; PoC adversarial evidence preserved |
| P5 | HTTP/MCP/Dioxus parity and exact-instance routing | Default-instance fallback or per-surface orchestration | Pending: identical command receipts and projection on all surfaces; cross-instance and controller-auth tests |
| P6 | Private packaging, bootstrap/rollback and actual live qualification | Rename PoC evidence as UHC proof; browser-only setup | Pending: dx server+WASM build, container interface test, API-only A→B→A + cancellation + restore |

Named risk dispositions: existing PoC protocol, discovery, cache and cancellation
risks retain their original tested evidence. MCP coverage and concurrent Stop are
retired by new headless tests. Production coordination, managed lifecycle,
projection ordering, API compatibility, settings migration and UI parity remain
P2–P6 acceptance gates, not risks declared solved by this import. Vendor terms,
physical listening and cross-network topology are accepted external verification
boundaries. A missing model-checkable gate blocks declaring integration complete.

No Problem Weave was present. P1–P6 are local implementation stages, not fabricated
OH lineage. Selected stages collectively cover the full integration; only P1 is
implemented in this change. The ordinary UHC adapter feature set is unchanged.

## Execution result / handoff

Completed P1: a self-contained source snapshot plus an API/MCP bridge, with the
copied relay suite and real-router MCP wire tests. The original standalone source
and UHC working checkouts were preserved; work is isolated on
codex/naa-proxy-integration. Do not release the experimental bridge as if it were
UHC's aggregate-backed MCP implementation. Continue with P2's red concurrency
and lifecycle tests before extracting the controller hook.

Human verification needed before shipping integrated behavior: actual target
network/packaging and listening behavior, and the final user-facing workflow.
No full UHC integration or hardware qualification is claimed by P1.
