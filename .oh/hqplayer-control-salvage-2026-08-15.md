## Salvage

### Salvage Report

**Salvaged:** 2026-08-15 — HQPlayer direct-control audit/restart

**Reason:** The work expanded from one live-mode display defect into a full HQPTuner parity audit across the native Rust client, adapter, aggregator, UI, and MCP. A live setter matrix also restarted HQPlayer into a source URL that returned 404, making the stopping point ambiguous.

**Original Aim:** Make UHC a trustworthy HQPlayer control surface: every displayed live value should describe the running engine, every control should change the native engine and verify readback, and the Rust client should be a strong reusable HQPlayer library.

### Learnings

1. HQPlayer has two distinct domains: configured settings (`State.mode`, filter indexes, rate index) and running-engine facts (`Status.active_mode`, `active_filter`, `active_shaper`, `active_rate`). They cannot be projected interchangeably.
2. On the live HQPlayer 6.0.4 engine, explicit SDM playback reported `State.mode=2` and `State.active_mode=1` while `Status.active_mode="SDM (DSD)"`. The existing UHC projection therefore showed PCM for an SDM engine.
3. The native `Status.active_filter` is the loaded filter, while configured `filter1x` and `filterNx` are separate chain settings. A UI must label these as active filter versus 1x/Nx configuration, never collapse them.
4. The aggregator already has the correct architectural role as the canonical retained snapshot. Fixes should improve the native observation/projection entering it, not create surface-specific reads.
5. MCP currently projected state/filter/shaper/rate from the canonical snapshot but had no active-mode field. Configured `options.mode.current` is not a substitute for live mode.
6. The current live instance accepted several setter requests and returned verified pipeline payloads, but a mode/control restart caused the source URL to return HTTP 404 and left HQPlayer stopped. A control test is not complete until transport state and source availability are recorded separately from DSP readback.
7. HQPTuner’s useful reference boundary is its live-chain model: active chain, configured chain, output-aware choices, settle/reconcile behavior, and explicit status/control separation. Its UI vocabulary should inform UHC, but its implementation should not be copied wholesale into the adapter.

### Frame Shifts

- “The UI has a bad mode label” → “The native client needs a typed distinction between configured pipeline and running engine state.”
- “Test each UI control independently” → “Test a control transaction: precondition snapshot, semantic write, native readback, aggregator publication, UI/MCP projection, and restoration.”
- “The filter is wrong” → “There are multiple filter facts: active loaded filter, 1x filter, Nx filter, and output-aware chain choices.”
- “HQPlayer is a set of setters” → “HQPlayer is a stateful session protocol with chain transitions, stale enumerations, ambiguous acknowledgements, restart recovery, and source lifecycle.”

### New Guardrails

1. Never use `State.active_mode` as the live display authority on the verified 6.0.4 engine; prefer native `Status.active_mode`, with a documented fallback only when the attribute is absent.
2. Keep configured selections and active output facts in separate types/fields all the way through the aggregator, UI, and MCP.
3. Every live setter test must snapshot state, mutate one semantic setting, verify native readback, verify aggregator/UI/MCP values, and restore the snapshot.
4. Do not treat an HTTP success or `result="OK"` as applied without readback; do not retry an ambiguous write blindly.
5. A stopped engine’s last active filter/rate is not fresh playback evidence. Surfaces must show lifecycle freshness explicitly or suppress stale active-chain claims.
6. Do not qualify the full matrix while the configured source is unavailable; separate source/transport failure from DSP-control failure.
7. Keep native indexes private to the Rust client/adapter boundary. HQPTuner parity should expose semantic names and typed values to all higher layers.
8. Additive MCP fields require contract tests and deliberate compatibility review; tool/resource payloads must remain generated from the same canonical snapshot.

### Missing Context

- A stable, controllable HQPlayer source was needed before running mutating PCM/SDM qualification.
- The full HQPTuner live-chain/control schema should have been inventoried before selecting individual UHC fields.
- Explicit live captures for PCM, SDM, and `[source]` were needed to settle active-mode and active-filter semantics by mode, not by one snapshot.
- The desired behavior for stale active values while stopped needs an explicit product decision.

### Ownership / Coordination Breakdowns

- The adapter, aggregator, and UI each had a plausible value, but no single typed contract named configured versus active DSP state.
- Live qualification authority was blurred: UHC controlled HQPlayer while Roon supplied the stream, so a source 404 looked like a DSP-control failure.
- The MCP schema gap was discovered after the UI issue because display and assistant projections were audited separately.

### Reusable Fragments

- `HqpAdapter::active_mode_name` now prefers the native live `Status.active_mode` and has regression coverage for a stale `State.active_mode` index.
- `PipelineStatus` remains the shared projection feeding HTTP, aggregator, UI, and MCP.
- MCP `McpPipelineStatus.mode` now carries active mode separately from configured options.
- Existing coherent snapshot fencing, semantic name-to-index resolution, verified setter receipts, and aggregator publication are the foundation for the restart.
- HQPTuner’s live-chain/status/control separation and output-aware filter narrowing are the reference behaviors to port as Rust concepts.

### Fresh Start Recommendation

Restart as one scoped HQPlayer client-library effort:

1. Define typed native observations for configured pipeline, active engine chain, transport lifecycle, capabilities, and setter outcomes.
2. Build a parity matrix from HQPTuner and native wire behavior for PCM, SDM, and source-following modes.
3. Add hermetic failing tests for each projection and transaction before implementation.
4. Route one coherent observation through the adapter and aggregator, then derive HTTP/UI/MCP from it.
5. Run the complete live matrix against a stable source, including restoration and stopped/reconnect cases.
6. Review the diff and package a beta only after the live matrix and source lifecycle both pass.

## Dissent

### Dissent Report

**Decision under review:** Treat the existing coherent pipeline snapshot plus active-mode correction as the foundation of a complete HQPlayer surface.

**Stakes:** This sets the public Rust library, aggregator state, UI vocabulary, and MCP contract. Calling the current slice complete would freeze a live-control subset while omitting the persistent settings and preset lifecycle the user explicitly expects.

**Confidence before dissent:** MEDIUM

### Steel-Man Position

The existing adapter already has a strong native protocol implementation, coherent generation-fenced snapshots, semantic name-to-index conversion, verified immediate setters, native profile listing/loading, matrix profile switching, aggregator ownership, UI selectors, and MCP tools. Extending that path avoids duplicating HQPTuner and preserves UHC's architecture.

### Contrary Evidence

1. UHC exposes only immediate mode, rate, active-chain 1x/Nx filters, shaper/modulator, junk filter, matrix profile, convolution, adaptive volume, repeat, random, and volume. HQPTuner exposes persistent Output, DSP, Volume, Matrix, and System settings in addition to its LIVE lane.
2. UHC can list and load daemon profiles, but the active configuration name is private to profile-load verification. The aggregator, UI, and MCP cannot report which profile is active. There is no profile preview, save, delete, rename, autosave, or description lifecycle.
3. HQPTuner found the daemon's native profile subsystem unreliable and owns full-config presets itself. UHC currently assumes daemon profile list/load is the desired product model.
4. UHC models one currently enumerated filter/shaper/rate set. HQPTuner preserves separate dormant PCM and SDM chain selections, re-enumerates after mode changes, reapplies held chain/rate values, and sequences mode before dependent settings.
5. UHC exposes opaque filter names. HQPTuner joins live enumerations with filter/modulator metadata, device capability and rate constraints, favorites, and narrowing facets.
6. UHC has no persistent-config transaction: no staged changes, restart-impact preview, backup, surgical XML edit, restore, reconnect/settle, or readback of the resulting active configuration.
7. UHC lacks persistent selectors for backend, output devices, PCM/SDM rate ceilings, DAC bits, DoP/DSD transport, buffers, high-frequency filter, DAC correction, Direct SDM, integrator, SDM conversion, fixed/startup/min/max volume, gain compensation, CUDA/multicore/E-core/blocks-per-cycle, pre-metering, and logging.
8. UHC lacks full matrix pipeline editing, filter import, speaker/crossfeed/EQ operations, and live response models.
9. UHC also has capabilities HQPTuner intentionally omits: transport, queue-related flags, repeat/random, and convolution. The target is the union of useful capabilities, not a clone.

### Pre-Mortem Scenarios

1. **Functional failure:** A mode switch succeeds but dependent filter/rate values resolve against old enumerations or reset, so the UI reports a coherent-looking chain the engine did not keep.
2. **Adoption failure:** Users still open HQPTuner for profiles, hardware/output settings, filter guidance, or matrix work; UHC remains a secondary remote rather than the trusted control surface.
3. **Opportunity cost:** Surface-specific endpoints accumulate around the legacy `PipelineStatus`, making a later complete Rust client harder because configured, active, persistent, and staged state have already leaked into incompatible payloads.

### Hidden Assumptions

| Assumption | Evidence | Risk if Wrong | Test |
|---|---|---|---|
| Daemon native profiles are sufficient | UHC list/load works and verifies `ConfigurationGet` | Cannot save/preview/delete reliably; profile identity remains hidden | Compare native profile round trips with HQPTuner-owned snapshot behavior on 6.0.4 |
| One active enumeration set is enough | Native API only serves the loaded chain | Dormant PCM/SDM choices disappear or reset across mode changes | Set distinct chains, switch twice, verify held values and rate pins |
| Immediate controls constitute “all settings” | Existing UI/MCP expose twelve names | Most output/DSP/volume/system configuration remains unreachable | Derive a generated capability matrix from HQPTuner settings metadata and UHC typed operations |
| `PipelineStatus` can remain the primary model | It feeds all existing surfaces | Persistent/staged/profile state gets bolted on or duplicated | Define a typed client snapshot first and prove every surface is a projection |
| HQPTuner parity means copying its surface | It is the strongest live reference | UHC loses transport/convolution or imports HQPTuner-specific storage assumptions | Build a union matrix with provenance and explicit exclusions |

### Reconstructed Story

- **Still true:** The coherent native adapter and aggregator ownership are the correct foundation.
- **Weakest assumption:** Existing profile list/load and twelve immediate selectors are close to a complete HQPlayer client.
- **Changed situation model:** HQPlayer needs four distinct lanes: observed runtime, immediate live control, persistent restart-backed configuration, and user-owned presets/profiles.
- **Changed beliefs:** Confidence in the current architecture remains high; confidence in the current model and scope drops to low.
- **Next action:** Define a complete, generated capability/settings inventory and typed Rust contracts for the four lanes before adding more public fields.

### Decision

**Recommendation:** RECONSIDER

**Reasoning:** Keep the adapter/aggregator architecture, but reject the narrow feature boundary and the legacy pipeline payload as the model of completeness. The public design must include active profile identity, profile/preset CRUD, both PCM and SDM chains, all native live selectors, persistent configuration selectors, matrix capabilities, and consistent UI/MCP projections.

**Confidence after dissent:** HIGH

**Follow-up artifact:** This report; a formal ADR should follow once the four-lane Rust contract and API compatibility strategy are concrete.

### User Scope Correction — 2026-08-15

The user explicitly rejected the persistent-configuration, custom preset, hardware/system tuning,
and matrix-editing scope introduced by the dissent. Those are not requirements for this effort.

The approved target is:

1. Native HQPlayer profile inventory, current/active profile identity, and verified profile loading in
   the Rust client, aggregator, UI, and MCP. Native profile save/delete/rename is not requested.
2. Complete immediate/live DSP selectors and readback: configured mode, active mode, rate, separate
   1x/Nx filters, shaper/modulator, junk filter, matrix profile selection, convolution, adaptive
   volume, repeat, random, volume, and any additional setting the native control protocol exposes as
   an immediate verified setter.
3. Correct PCM/SDM behavior: re-enumerate after mode changes, preserve/restore dependent selections
   where the native protocol permits it, and never resolve a setting against the previous chain's
   indexes.
4. One coherent native observation retained by the aggregator and projected consistently to the web
   UI and MCP.

Explicitly out of scope:

- restart-backed persistent configuration editing;
- user-owned live or full-configuration presets;
- CUDA, multicore DSP, E-core, blocks/cycle, logging, license, and device-capability configuration;
- matrix profile CRUD and full matrix pipeline editing.
