# MCP resource-list invalidation (#833)

OH: 80222d6d

Aim: ordinary playback projections must produce zero resource discovery hints when
`resources/list` is unchanged, while real advertisement changes remain discoverable.

Decision: compare the complete serialized advertisement with each session's last
served snapshot. A separate pending-invalidation bit coalesces changes until the
next full list response. Notifications never replace the served snapshot. A serve
revision protects a newly served snapshot from a delayed send failure; generation
identity protects replacement entries from obsolete task cleanup. The store lock
serializes claims and served-baseline updates. Snapshot construction and transport
sends happen outside it; revision rechecks discard comparisons made during a re-list.

The existing lifecycle events prompt comparisons. A one-second reconciliation
also covers aggregate scheduling, lagged events, and direct settings-file edits.
Both listing paths use the same builder and one settings snapshot per comparison.
No API payloads, routes, or capability declarations change.

Lifecycle: capacity is 256 entries, evicting least recently served first. Entries
expire 30 minutes after their last list response. Notifications do not extend that
lifetime. Old tasks stop on their next reconciliation (or within the two-second
send bound); shutdown cancels a pending send. Re-listing after eviction returns the
full current list and starts a replacement notifier. Opening SSE alone does not
re-list or replay missed hints. These bounds apply to UHC's added baseline state;
they do not repair the SDK's independent session-store retention. Capacity is a
hard bound on stored entries, not on transient tasks: bursts can leave evicted
notifiers alive until their next tick/send deadline. No hard task-admission limit
is claimed. Failed sends are rate-limited to at most one attempt per second.

Accepted trade-offs: a healthy client that never re-lists for 30 minutes also
expires and must re-list to resume hints. One pending hint coalesces intermediate
list changes. A send already in flight may arrive after a concurrent list response;
this is a bounded extra refresh, never permission to overwrite its newer baseline.
Per-session periodic work follows the existing notifier structure; a shared worker
can be considered if measured session load warrants it.

Review / dissent: Sol requested atomic claims, revision checks, bounded sends and
entry/task lifetimes, coherent settings reads, and transport-level regression tests.
These are execution criteria. The reported Gemini SSE disconnect remains unproven
and needs the reporter's client versions and timestamped transport logs.

Risk checks:
- False invalidations: the unchanged-projection wire test fails on unmodified v4.
- Independent clients and refresh rearming: real two-session/SSE contract coverage.
- Membership, rename, visibility and provider settings: full-list wire assertions.
- Lost/early bus wakeups and settings edits: periodic comparison, not event payloads,
  determines the advertisement; the wire test also edits settings without events.
- Retry storm, blocked sends, shutdown, expiry and eviction/replacement races:
  controlled-runtime tests exercise the production notifier loop with paused time.
- Delayed failure after a newer serve: revision-state regression test.
- False-negative notification tests: persistent SSE parser retains duplicates and
  incomplete frames, with its own regression test.

Sol's final review/dissent verdict: Continue / Proceed, no remaining blocking
findings. Keep this scoped to invalidation correctness; a shared task scheduler or
SDK session-store redesign is not needed to resolve #833.
