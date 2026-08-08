# Session: streaming-adapters

## Aim

**Aim:** Listeners control major music services through UHC regardless of whether playback is local, bridged, or cloud-hosted.

**Guardrails:** Provider capabilities remain truthful and adapter-scoped; Music Assistant is optional and never a prerequisite; API contract additions require explicit approval; credentials remain at their provider-native authority boundary.

## Problem Space

Apple Music is device-native; Spotify is account- and Connect-device-scoped; Amazon Music is externally access-gated. UHC's event bus and aggregator remain the sole client-facing state path. Search, playlists, queues, and transport are separate per-provider capabilities rather than a lowest-common-denominator surface.

## Problem Weave

**Recommended working frame:** A direct provider-specific adapter family with shared lifecycle and truthful capability projection.

- Apple Music is a direct adapter backed by a native MusicKit companion.
- Spotify is a direct controller for existing Spotify Connect devices.
- Amazon Music is discovery/access-gated until approved API access and compatible terms are confirmed.
- Music Assistant is an optional peer adapter.

**Selected decisions:**

1. Apple uses native MusicKit; macOS uses `ApplicationMusicPlayer` because the macOS SDK marks `SystemMusicPlayer` unavailable.
2. Apple supports inline macOS and paired-Mac bridge modes behind one adapter-facing identity.
3. Zones remain adapter-scoped: `spotify:`, `applemusic:`, and `musicassistant:` may each identify the same physical endpoint independently.
4. Spotify is controller-only; UHC does not become a Spotify Connect receiver in this initiative.
5. Browse/play saved playlists precedes create/edit provider playlists.
6. OAuth and bridge endpoints are part of the reviewed #463 contract and are implemented in the initiative. The user applies `api-change-approved` to the PR because these routes intentionally extend the API contract.

**Open evidence:** Amazon production access/terms; native companion entitlement/runtime validation; whether current public capability vocabulary can represent all playlist actions without an approved expansion.

## Dissent

**Decision under review:** Direct Apple and Spotify adapters with a native Apple MusicKit companion, plus an access-gated Amazon investigation.

**Steel-man:** This preserves UHC as the direct control plane and avoids coupling every streaming service to Music Assistant. MusicKit keeps the Apple session owned by UHC's companion; Spotify's supported remote-device API covers existing Connect devices.

**Contrary evidence:** Apple native playback creates a distributed companion and entitlement burden; Spotify availability varies by Premium status, account, and device; Amazon's published playback API is not generally available.

**Pre-mortem:**

1. The Apple companion cannot sustain the intended queue/playback workflow. Warning: native MusicKit integration tests cannot publish a stable now-playing state. Mitigation: verify the companion contract before adapter implementation.
2. Spotify appears supported but users cannot control their account/device. Warning: OAuth, Premium, restricted-device, or API eligibility failures dominate smoke tests. Mitigation: expose classified availability reasons before claiming transport support.
3. The shared authorization layer becomes an unreviewed public API. Warning: a bridge needs an ad-hoc route or request shape. Mitigation: make contract review and `api-change-approved` a hard implementation gate.

**Weakest assumption:** Apple MusicKit's macOS `ApplicationMusicPlayer` can deliver the intended playback behavior through both inline and paired bridge modes.

**Recommendation:** ADJUST — proceed with contract and native-companion validation before provider adapter implementation; keep Amazon discovery separate from delivery.

## GitHub Tracking

- Epic: #462
- Authorization and bridge contract: #463
- Apple Music adapter: #465
- Spotify controller adapter: #466
- Music Assistant peer adapter: #467
- Amazon access discovery: #464

## Dissent: Approved MA parity contract expansion

**Decision:** add a provider-neutral MCP browse/collection surface and MA
outbound setup/status path.

- **Steel-man:** one zone-scoped browse contract gives MA, Roon, LMS, Spotify,
  and Apple a common discovery shape while leaving item identity and mutation
  semantics provider-owned. MA configuration belongs beside provider setup but
  is an outbound bearer-authenticated peer connection, not an Apple-style
  pairing bridge.
- **Contrary evidence:** a generic browse model can erase provider-specific
  paging/item semantics; reusing the Apple bridge would invert connection
  ownership; a settings save can leave a stopped or half-replaced adapter.
- **Decision:** proceed with owner-scoped opaque refs, explicit capability
  refusals, provider-native payload fields behind a common envelope, encrypted
  MA credentials, transactional replacement, and aggregator-only client state.
- **Invalidation / stop:** stop if the model requires cross-provider refs,
  exposes a secret, bypasses the registry/aggregator, or cannot roll back a
  failed MA replacement.

## Execute: Approved MA parity expansion

**Aim:** complete advertised MA library, queue, and setup capabilities without
turning UHC into an MA host, proxy, or source of fabricated state.

**Scope:** provider-neutral MCP browse/collections; MA browse/playlists/
favorites; MA queue actions; secure outbound configuration/status and Settings
workflow. **Non-goals:** audio relay, MA OAuth, Apple pairing reuse, playlist
mutation. **Success:** deterministic wire/MCP/API/UI tests cover provider scope,
pagination, refs, failures, secret redaction, and lifecycle rollback.

## Review: Approved MA parity expansion

**Status:** continue. The implementation keeps UHC an outbound MA client and
uses the existing adapter registry and aggregator rather than a direct UI
connection. MA collection paths and playable references are opaque,
provider-scoped, and cannot cross into Spotify; MA configuration preserves the
old encrypted endpoint and runtime on a failed replacement.

**Evidence:** focused adapter wire tests cover authenticated commands, grouped
queue resolution and mode changes; MCP tests cover catalog refs, collections,
queue mutation/play-next, pagination and cross-provider ref rejection; API
tests cover the approved route and MA secret redaction/rollback.

**Remaining initiative work:** MA multiroom membership is deliberately still
separate because it needs an approved user-facing grouping contract. The new
collection vocabulary is MA-first: other adapters remain truthfully unavailable
until each maps its native library semantics into the shared opaque-ref shape.
