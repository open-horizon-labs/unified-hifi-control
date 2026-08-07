# Apple Music content bridge proposal (#463)

This is a contract proposal, not an enabled API change. It extends the
existing paired-companion command/acknowledgement envelopes only after #463 is
approved and the repository's `api-change-approved` gate is applied. Until
then, the iPhone package's catalog, library, recommendation, and playlist
methods remain companion-local.

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

`playlist_remove`, arbitrary reorder/delete, unfavorite, and broad library
removal remain refused until Apple documents a safe operation and UHC has an
ownership/precondition model. A listening plan is UHC's durable intent; it is
never presented as full visibility into the iPhone system queue.

## Approval boundary

This document records the proposed wire contract so #463 can review it. No
route, HTTP method, or existing transport payload is changed by this file.
Implementation begins only after explicit contract approval and the required
repository label.
