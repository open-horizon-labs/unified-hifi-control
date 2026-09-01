# Adaptive Interaction Plane

**Updated:** 2026-07-30

## Problem Space

### Objective

We are optimizing for controllers to become general-purpose listening interfaces: each client receives controls appropriate to its hardware and current context, can originate voice commands when microphone-capable, and can browse or manage content without embedding Roon, LMS, HQPlayer, Home Assistant, speech-provider, or playlist-provider logic in firmware.

Success means the same semantic action is available through adaptive UI, voice, web, HTTP, and MCP when the selected producer supports it; unsupported operations are never advertised; and adding an integration or control does not require another firmware-specific UI.

### Constraints

| Constraint | Type | Reason | Question? |
|------------|------|--------|-----------|
| Aggregator owns authoritative state | hard | UHC architecture | No |
| Devices never call adapters directly | hard | Safety, consistency, and recovery | No |
| Existing API changes require explicit approval | hard | Compatibility policy | New protocols need approved additive routes/contracts |
| Device resources and peripherals vary | hard | Microphone, speaker, PSRAM, input, and refresh capabilities differ | Negotiate capabilities; do not infer from board names |
| Audio, UI, and content payloads are bounded | hard | ESP memory, network, latency, and denial-of-service risk | No |
| Provider capabilities differ | hard | Roon, LMS, HQPlayer, and Home Assistant expose different operations | Do not manufacture parity |
| Voice/provider credentials stay server-side | hard | Secrets cannot ship to clients | No |
| Voice is optional | hard | Accessibility, privacy, hardware, and recovery | No |
| v4 manifest code must be directly ported | assumed | It demonstrates useful behavior | Salvage concepts, fixtures, and tests; do not assume wholesale cherry-pick |
| Playlist management is one capability | assumed | Browse, queue mutation, saved playlists, and programs differ | Split the resource model |
| Home Assistant must own voice | soft | It has a complete pipeline | Use as first provider, not the only provider |
| LLM-generated layouts are required initially | soft | v4 explored this | Deterministic schemas and validation come first |

### Terrain

- **Systems:** UHC bus/aggregator, adaptive producer documents, v4 manifest/matcher work, ESP firmware, device negotiation, Roon Browse/Transport, LMS CLI, HQPlayer, Home Assistant WebSocket/Assist, Wyoming services, MCP, and persistence.
- **Stakeholders:** Dial/frame/touch users, voice users, playlist builders, privacy-sensitive installations, firmware maintainers, and operators recovering disconnected providers.
- **Blast radius:** Wrong-zone commands, unsafe volume/home actions, microphone privacy failures, leaked credentials, unbounded manifests/catalogs, stale content references, invalid queue edits, and firmware exhaustion.
- **Precedents:** UHC #121, #248, #283, #288-291, #309-326, #331, #333-336; roon-knob #106, #187, #190, #193-195.

### Assumptions Made Explicit

1. Clients can render semantic controls without receiving exact pixels — if false, each display class needs a bounded presentation dialect.
2. One semantic command layer can serve UI, voice, MCP, HTTP, and automation — if false, outcomes and errors will diverge.
3. Microphone-capable firmware can stream bounded audio while maintaining UI/control duties — if false, voice needs separate hardware or richer clients.
4. Home Assistant can remain optional — if false, non-HA users lose voice.
5. Content references can be persisted safely — if false, bindings need provider epoch/expiry and re-resolution.
6. Users want more than playlist selection — if false, saved-playlist mutation can remain deferred.

### X-Y Check

- **Stated need (Y):** Port adaptive UI back, accept voice from clients, and add playlist management.
- **Underlying need (X):** Establish an adaptive interaction plane where producers declare semantics, clients declare capabilities and input, and every modality invokes the same validated action layer.
- **Confidence:** Medium-high. The features are real, but independent protocols would recreate the coupling adaptive control is intended to eliminate.

### Ready for Solution Space?

Yes. Exact microphone hardware and saved-playlist mutation breadth can be qualified inside bounded issues.

## Problem Statement

**Current framing:** Restore adaptive UI, add voice endpoints, and add playlist screens and MCP tools.

**Reframed as:** Listeners need heterogeneous controllers to discover, present, and invoke trustworthy media and home capabilities through physical input, voice, web, HTTP, and MCP, but current state, actions, content navigation, and input modalities are split across backend-specific code and independently evolving client contracts.

**The shift:** From three features and a device manifest to one adaptive interaction plane: producer state/actions plus content resources flow through the aggregator; client capabilities select a bounded projection; physical, voice, web, HTTP, and MCP inputs enter the same semantic command service.

### Constraints

- **Hard:** Aggregator ownership; server-side credentials; negotiated bounded payloads; provider capability honesty; verified outcomes; optional voice; compatibility and explicit API approval.
- **Soft:** Exact v4 implementation reuse, Home Assistant as first voice provider, initial playlist mutation breadth, and LLM-assisted configuration timing.

### What this framing enables

Adaptive UI without backend firmware, client-originated voice with interchangeable processing providers, paged content browsing, truthful queue and saved-playlist operations, and MCP parity over the same semantics.

### What this framing excludes

Server-authored pixels as the domain model, credentials in firmware, direct adapter access, provider-specific voice phrases or screens, fake cross-provider playlist parity, and LLM-generated configuration without deterministic validation.

## Solution Space

### Candidates Considered

| Option | Level | Approach | Trade-off |
|--------|-------|----------|-----------|
| A | Band-Aid | Cherry-pick PR #291 and add audio/playlist screen types | Fast demo; deep coupling and unrelated diff |
| B | Local Optimum | Restore manifest v2 and bolt on separate voice/playlist APIs | Three contracts and divergent behavior |
| C | Reframe | Make Home Assistant the controller platform | Strong voice stack; HA becomes mandatory |
| D | Redesign | Unified adaptive interaction plane over one semantic action layer | More contract work up front |
| E | Redesign | Put adaptation and speech intelligence on each client | Offline potential; unsuitable across ESP targets |

### Evaluation

**Option A: Direct v4 restoration**
- Solves stated problem: Partially
- Implementation cost: Medium initially, high integration cost
- Maintenance burden: High
- Second-order effects: Retains IP-derived device identity, hardcoded matcher fields, server-authored screens, live-state/template merging, and unrelated branch changes.

**Option B: Three feature-specific protocols**
- Solves stated problem: Initially
- Implementation cost: Medium
- Maintenance burden: High
- Second-order effects: UI, voice, and MCP acquire different routing, validation, availability, and outcome semantics.

**Option C: Home Assistant-centric**
- Solves stated problem: Mostly
- Implementation cost: Medium
- Maintenance burden: Medium
- Second-order effects: Reuses Assist but subordinates UHC and excludes non-HA voice installations.

**Option D: Unified adaptive interaction plane**
- Solves stated problem: Yes
- Implementation cost: High
- Maintenance burden: Medium-low after adoption
- Second-order effects: Requires deliberate schemas/versioning but makes every producer, renderer, voice provider, and content source an extension of stable seams.

**Option E: Client-owned intelligence**
- Solves stated problem: Inconsistently
- Implementation cost: High
- Maintenance burden: Very high
- Second-order effects: Firmware variants multiply and resource-constrained clients are excluded.

### Recommendation

**Selected:** Option D — Unified adaptive interaction plane
**Level:** Redesign

**Rationale:** Adaptive UI, voice, playlists, HTTP, and MCP are projections and inputs around the same state/action system. The server publishes and validates semantics; clients declare what they can present or capture.

**Accepted trade-offs:**
- More schema and compatibility work before broad UI functionality.
- Push-to-talk precedes always-listening wake words.
- Playlist functionality honestly differs by provider.

### Implementation Notes

1. Salvage versioning, semantic element/action bindings, pure-data ordered matchers, fast/slow revisions, validation, and fallback from v4. Do not port pixels as the domain contract, IP identity, fixed Zone-field matchers, style-based live-state merging, or LLM-first configuration.
2. Add provider-neutral voice sessions with client/device/zone context, cancellation, correlation, bounded PCM or negotiated formats, and visual/audio feedback. Start with push-to-talk and Home Assistant Assist; add text-only clients, local Wyoming, and optional hosted providers later.
3. Split content discovery, playback-session observation, queue mutation, and saved-collection mutation. Each producer advertises supported operations and stable-reference/expiry behavior.
4. Project all advertised capabilities into web, HTTP, device UI, and MCP through one semantic execution path; surface-specific subsets are allowed, contradictory semantics are not.
5. Sequence: contract/device negotiation; v4 salvage/projector; HQPlayer proving producer; normalized content/session; client voice transport; HA Assist; local/hosted providers; saved playlists/programs; beta qualification.
