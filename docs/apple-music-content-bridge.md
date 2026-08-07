# Apple Music content bridge proposal (#463)

This is the approved UHC companion content protocol. It extends the existing
paired-companion command/acknowledgement envelopes; it is not Apple Music's
API and it does not expose Apple credentials or raw Apple identifiers. The
iPhone package uses this contract for catalog, library, recommendation,
playlist retrieval, exact playback, and the supported playlist mutations.

## Companion readiness state (future wire extension)

The current bridge does **not** publish a readiness field. Its stable,
owner-scoped internal status is limited to `unpaired`, `awaiting_snapshot`,
`reachable`, and `stale`; the legacy HTTP status remains the boolean
compatibility projection. Do not infer authorization, subscription, account,
or playback state from those values. The signed companion's authorization and
native playback validation are still an external #465 gate.

If a later, separately approved bridge extension needs to explain why a paired
owner cannot play, it may carry a bounded, non-secret readiness value alongside
each published snapshot (and in the status projection):

```text
unpaired | awaiting_snapshot | authorization_needed | subscription_required |
restricted | reachable | inactive | stale | offline
```

`reachable` means the companion has published a valid snapshot and is able to
accept commands; `inactive` means the player is reachable but has no current
item or is stopped. `authorization_needed`, `subscription_required`, and
`restricted` are provider/account outcomes, not transport failures. `stale`
means the paired lease expired, while `offline` means the companion could not
publish or report its current state. The companion maps native MusicKit
authorization and playback errors to these values; UHC never infers them from
an absent track, a timeout, or a generic HTTP error.

That value is deliberately separate from `playback_state`, `route`, and
`liveness` so a reachable but paused player cannot be mistaken for an
unauthorized account. Until that extension is implemented and validated, the
values after `reachable` in the list above are design vocabulary only; they are
not current owner-status responses. No Apple account email, token, raw
provider error, or subscription metadata crosses the bridge.

## Request envelope

Every content request uses the existing bridge command delivery path and adds:

```json
{
  "kind": "content",
  "request_id": "opaque-uhc-correlation-id",
  "owner_id": "stable-companion-id",
  "operation": "catalog_search",
  "params": {
    "query": "artist or track",
    "limit": 25,
    "offset": 0
  },
  "idempotency_key": "opaque-retry-key",
  "precondition": null,
  "confirm": false,
  "expires_at": 0
}
```

`owner_id` must equal the paired execution owner selected by the
`applemusic:<owner_id>` zone. The server never accepts an Apple catalog or
library ID as a client-visible reference. `idempotency_key` is required for
mutations and optional for reads. Mutations additionally carry an explicit
`confirm: true` and, where applicable, a read-before-write precondition.

Bounds are part of the contract: page size at most 50, queue/listening-plan
items at most 200, operation and error text at most 128/512 bytes, and a
bounded response body. The server rejects oversized requests before delivery.

## Response/acknowledgement envelope

```json
{
  "request_id": "opaque-uhc-correlation-id",
  "owner_id": "stable-companion-id",
  "operation": "catalog_search",
  "outcome": "success",
  "data": {
    "items": [],
    "source_kind": "catalog",
    "has_more": false,
    "next_offset": null
  },
  "error": null,
  "observed_at": 0
}
```

`outcome` is one of `success`, `unsupported`, `unauthorized`,
`subscription_required`, `restricted`, `not_found`, `offline`,
`rate_limited`, `stale_owner`, `conflict`, `invalid`, or `failed`. Failures
carry only a bounded, redacted `{code, message, retryable}` object. Apple
tokens, developer tokens, raw authorization material, audio, and unnecessary
account identifiers never appear in either envelope.

Mutation retries return the original bounded result for the same
`idempotency_key`; they do not execute a second time. A stale or mismatched
`precondition` is a `conflict`, never an implicit overwrite. No `force` escape
hatch is defined.

## Normalized content data

Search and retrieval results carry a short-lived UHC ref minted in the server
table. The internal row binds the ref to `owner_id`, provider, source kind
(`catalog`, `library`, `playlist`, `recent`, or `recommendation`), and the
companion-local Apple handle. Clients receive only the opaque ref plus
permitted title/artist/album/artwork/link attribution. A ref from one iPhone
cannot be used against another iPhone or a Mac companion.

Recommendation sections are not tracks. They expose a bounded section title,
reason, refresh hint, and separately scoped playable contents when the
companion can resolve them. Empty, deleted, inaccessible, or restricted items
are represented as unavailable results rather than silently dropped into a
different item.

## Operations

The initial operation names are `catalog_search`, `library`, `playlists`,
`playlist_tracks`, `recent`, `recommendations`, `play_ref`, `queue_plan`,
`playlist_create`, `playlist_add`, `playlist_update`, `favorite_add`,
`rating_set`, and `context`. Each operation has its own capability result.

`playlist_remove`, arbitrary reorder/delete, `favorite_remove` (unfavorite),
and broad library removal remain refused until Apple documents a safe operation
and UHC has an ownership/precondition model. A listening plan is UHC's durable
intent; it is never presented as full visibility into the iPhone system queue.

## Approval boundary

The repository owner explicitly approved this additive UHC content protocol.
The contract is intentionally separate from Apple's MusicKit API: the native
companion translates these operations into documented MusicKit calls, while
UHC owns routing, opaque references, bounded delivery, retries, and truthful
outcomes.
