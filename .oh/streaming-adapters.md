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

1. Apple uses a dedicated `ApplicationMusicPlayer` playback session, not the user's Music.app session and not Music.app automation.
2. Apple supports inline macOS and paired-Mac bridge modes behind one adapter-facing identity.
3. Zones remain adapter-scoped: `spotify:`, `applemusic:`, and `musicassistant:` may each identify the same physical endpoint independently.
4. Spotify is controller-only; UHC does not become a Spotify Connect receiver in this initiative.
5. Browse/play saved playlists precedes create/edit provider playlists.
6. OAuth and bridge endpoints are part of the reviewed #463 contract and are implemented in the initiative. The user applies `api-change-approved` to the PR because these routes intentionally extend the API contract.

**Open evidence:** Amazon production access/terms; native companion entitlement/runtime validation; whether current public capability vocabulary can represent all playlist actions without an approved expansion.

## Dissent

**Decision under review:** Direct Apple and Spotify adapters with a native Apple MusicKit companion, plus an access-gated Amazon investigation.

**Steel-man:** This preserves UHC as the direct control plane and avoids coupling every streaming service to Music Assistant. ApplicationMusicPlayer keeps the Apple session owned by UHC's companion; Spotify's supported remote-device API covers existing Connect devices.

**Contrary evidence:** Apple native playback creates a distributed companion and entitlement burden; Spotify availability varies by Premium status, account, and device; Amazon's published playback API is not generally available.

**Pre-mortem:**

1. The Apple companion cannot sustain the intended queue/playback workflow. Warning: native MusicKit integration tests cannot publish a stable now-playing state. Mitigation: verify the companion contract before adapter implementation.
2. Spotify appears supported but users cannot control their account/device. Warning: OAuth, Premium, restricted-device, or API eligibility failures dominate smoke tests. Mitigation: expose classified availability reasons before claiming transport support.
3. The shared authorization layer becomes an unreviewed public API. Warning: a bridge needs an ad-hoc route or request shape. Mitigation: make contract review and `api-change-approved` a hard implementation gate.

**Weakest assumption:** Apple MusicKit ApplicationMusicPlayer can deliver the intended dedicated playback behavior through both inline and paired bridge modes.

**Recommendation:** ADJUST — proceed with contract and native-companion validation before provider adapter implementation; keep Amazon discovery separate from delivery.

## GitHub Tracking

- Epic: #462
- Authorization and bridge contract: #463
- Apple Music adapter: #465
- Spotify controller adapter: #466
- Music Assistant peer adapter: #467
- Amazon access discovery: #464
