# Music context HTTP and MCP proposal (#756)

Status: proposed contract; no routes, schemas or MCP tools implemented by this PR. Owner approval of the contract below is required before implementation. OH: 80222d6d.

## Solution Space

**Problem:** Paired UHC users and AI assistants cannot read source-attributed music context through UHC without a separate CLI.

**Key constraint:** Cloud enrichment is optional and must not become a dependency of playback, leak pairing credentials, or attach late results to different music.

**Working story:** An explicit context read from the aggregator is useful to both application clients and assistants; the existing signed Cloud client supplies the data.

**Success signal:** HTTP and MCP return equivalent attributed context for an explicit zone, distinguish unavailable and ambiguous results, and never issue playback commands.

**Decision criteria:** Consumer usefulness, existing client compatibility, association with music identity, one implementation for both transports, bounded optional network work, and a small public contract.

**Critical assumptions:** Cloud pairing authorizes enrichment; consumers can interpret partial/source-backed results; context describes a music identity rather than a particular queue occurrence.

### Candidates considered

| Option | Level | Approach | Main trade-off |
|---|---|---|---|
| A | Band-aid | Spawn the CLI from HTTP/MCP | Process/config coupling and duplicate transport/error parsing |
| B | Local optimum | Shared on-demand zone-context service | Explicit identity semantics; no continuous updates or UI panel |
| C | Reframe | Explicit artist/recording identity lookup only | Race-free lookup but consumers must resolve identities themselves |
| D | Redesign | Enrichment subscription attached to playback epochs | Strong occurrence binding but requires new cross-provider lifecycle/state and unsolicited Cloud traffic |

**Selected:** B. Reuse the existing Rust Cloud client in a shared service, with HTTP/MCP adapters and explicit zone selection. C remains plausible for a later lookup surface. Reject A; defer D until a UI or subscription consumer needs occurrence binding.

### Proposed public contract for approval

Add **POST `/music/details`**, controller-auth protected using the existing HTTP boundary. POST expresses an explicit Cloud operation and avoids passive browser/link prefetch triggering enrichment. No changes to existing now-playing or control responses.

Request (reject unknown fields):

```json
{"zone_id":"roon:EXPLICIT_ZONE_ID","language":"en"}
```

Both fields are required; use the existing prefixed zone ID rules and the Cloud service's supported wiki language codes. No default-zone fallback, supplied Cloud URL, credentials, raw identity override, or automatic request on heartbeat/seek/volume changes.

Successful response:

```json
{
  "version": 1,
  "zone_id": "roon:EXPLICIT_ZONE_ID",
  "identity": {"artist":"Example artist","title":"Example track","album":"Example album"},
  "language": "en",
  "details": {
    "version": 1,
    "item_token": "opaque-request-token",
    "status": "partial",
    "catalog": {},
    "sources": [],
    "entities": [],
    "genres": [],
    "unavailable": [],
    "language": "en",
    "stale": false
  }
}
```

The empty source/entity/catalog containers above are illustrative, not a definition of their nested Cloud schema. `details` preserves the documented version-1 Cloud payload and all attribution/license/provenance fields; implementation fixtures must come from its actual contract. Do not invent biography, credits or coverage claims. Optional identity fields follow `MusicIdentity`; omit absent fields.

`identity` states exactly which music the result describes. It does **not** certify the current queue occurrence. Read aggregator identity before and after enrichment; reject changed identity, removed zone, or lost usable now-playing data. A→B→A and separate queue entries with identical identity may legitimately return the same music context. Consumers must compare returned identity with their current selection before displaying; a future live UI must additionally bind its own occurrence token. Never claim a server playback epoch that the aggregator cannot supply.

HTTP outcomes use the repository's existing error envelope (freeze exact envelope/fixtures before implementation):

| HTTP | Code | Meaning |
|---|---|---|
| 400 | `INVALID_REQUEST` | Invalid/missing zone or unsupported language |
| 401/403 | Existing controller-auth codes | Local caller is not authorized |
| 404 | `ZONE_NOT_FOUND` | Explicit zone does not exist |
| 409 | `NO_MUSIC` | Zone has no usable music identity |
| 409 | `MUSIC_CHANGED` | Identity changed during the read |
| 503 | `CLOUD_NOT_PAIRED` | HiPhi pairing is absent |
| 503 | `MUSIC_DETAILS_UNAVAILABLE` | Timeout, disabled service, Cloud refusal/rate limit, unsupported Cloud authority or malformed reply |

A valid Cloud response with status `partial`, `ambiguous` or `unavailable` remains HTTP 200: that is a catalog answer, distinct from failure to obtain an answer. Do not translate upstream Cloud 401 into local controller 401, expose signed requests/installation IDs, or return arbitrary upstream error bodies. Exact upstream error categorization can be extended later only through another reviewed contract.

Add MCP tool **`hifi_music_details`** with required `zone_id` and `language`, using the same service and identity/details payload in the existing MCP envelope. Preserve machine-readable refusal codes above through established MCP error handling. Declare read-only, non-destructive and open-world annotations: the read makes an external Cloud request. Do not auto-call enrichment from `hifi_now_playing`, resources or playback tools. Keep MCP capability discovery derived from implementation; do not conflate Cloud enrichment with provider-native search/browse support.

### Interpretive variety

B assumes consumers want context for the selected zone. C tests the alternate frame that consumers really want an identity catalog lookup; D tests whether live occurrence semantics are the actual requirement. If consumer validation needs continuous updates or strong queue epochs, pivot from B before expanding the contract with heuristics.

### Risk retirement plan

“Retired by evidence” below describes the required execution disposition; these checks are planned, not already passed.

| Risk | Planned disposition | Tempting patch the check must fail | Required evidence | Stop/pivot if |
|---|---|---|---|---|
| Late A displayed as B | Retired by evidence | Proxy the CLI and return whatever finishes | Delayed mock reply while aggregator switches A→B: both transports refuse with MUSIC_CHANGED; unchanged A preserves identity | No safe before/after aggregator read |
| Identical metadata hides occurrence change | Accepted with rationale | Invent timestamps as playback epochs | Contract scopes context to music identity; explicitly document A→B→A and identical entries | Consumer needs occurrence-specific facts |
| HTTP/MCP drift | Retired by evidence | Separate implementations | Same service fixture through both transports: equivalent identity, attribution, status and errors | Transport cannot preserve envelope semantics |
| Optional Cloud breaks playback | Retired by evidence | Invoke enrichment on every now-playing read | Cloud down/rate-limited plus simultaneous transport/now-playing checks: no enrichment calls from unrelated routes/tools; playback unaffected | Shared locks or failure paths stall controls |
| Pairing proof leaks or arbitrary recipient | Retired by evidence | Accept caller-provided endpoint/credentials | Auth boundary tests, unknown-field rejection, production-authority restriction, sanitized refusal payload; inspect logs for proof/keys | Existing auth is unavailable at new boundary |
| Fabricated completeness or lost attribution | Retired by evidence | Flatten sources into prose or force complete | Realistic partial/ambiguous/stale fixture preserves all source/license fields and unknown permitted Cloud sections | Cloud schema incompatible with proposed projection |
| Request flood | Retired by evidence | Unbounded per-request Cloud clients | Bounded shared concurrency, fixed deadline and response cap tested with slow/oversized replies; overload returns sanitized unavailable | Capacity needs a durable job system |
| No deployed catalog | Accepted with rationale | Claim feature works because mocks pass | Signed live paired smoke required before advertising live coverage; mocks prove transport only | Service does not implement version-1 contract |

## Dissent

**Steel-man:** One zone-context service makes existing Cloud capability usable without subprocesses or duplicated policy.

**Contrary evidence:** The existing CLI only compares metadata before/after and cannot supply playback epochs. Its response uses flattened extensible Cloud sections, not a stable typed biography schema. Pairing alone does not prove live catalog deployment.

**Pre-mortem:** Functional failure: a late result is misattributed. Adoption failure: assistants receive JSON but cannot distinguish partial evidence. Opportunity cost: a broad subscription system is built before any consumer needs it.

**Decision: ADJUST.** Scope the response to explicit music identity, preserve uncertainty and attribution, share the service, and keep subscriptions/lookup/UI out of the initial contract. Confidence: medium until consumer fixtures and live Cloud validation pass. The weakest assumption is that identity-level context meets consumer needs; occurrence-specific requests trigger reconsideration.

## Execution handoff

Preserve all existing API responses, local HTTP availability and playback behavior. After owner approval, write failing HTTP/MCP consumer tests first, implement one shared service, update the route fixture and derived MCP contracts, and run targeted compatibility/auth/race tests plus required CI. Add bounded concurrency without cross-request playback locks. Use source-backed Cloud fixtures; distinguish transport correctness from live data coverage. No firmware or existing music endpoints change.

Selected work: shared context service, controller-auth HTTP adapter, MCP adapter, consumer/contract tests and documentation. Deferred: identity-only lookup, UI panel, subscriptions, caching, new Cloud payload schema and playback epoch redesign. These selected pieces are sufficient for explicit on-demand reads; they do not promise a live UI.

Human verification: owner approves this exact API/MCP contract; a paired installation verifies live response usefulness and attribution. Stop on incompatible Cloud schema/auth or consumer requirement for occurrence-specific context. The agent must not add `api-change-approved` itself.
