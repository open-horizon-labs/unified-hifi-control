# Agent Guidelines

## Task Management

Use GitHub for all task tracking:

### GitHub Issues
**Purpose:** Describe problems and desired outcomes
- Create issues for bugs, features, and improvements
- Focus on the *what* and *why*, not implementation details
- Reference related issues with `#123` syntax
- Add OH endeavor ID for cross-reference: `OH: 80222d6d` (enables agent linkage to Open Horizons context)

### Branches
**Purpose:** Isolate work in progress
- Create a branch for each issue: `fix/issue-123-description` or `feat/issue-123-description`
- Base new Rust work on `v4`, not `master` or the maintained `v3` line
- Keep branches focused on a single issue

### Pull Requests
**Purpose:** Propose implementations for review
- Link to the issue being addressed: `Fixes #123`
- Describe what changed and how to test
- CodeRabbit is disabled for this repository; do not request its reviews.

### Merging PRs
**TEST BEFORE MERGING.** Do not merge PRs without testing.

1. **Wait for CI to pass** - Build must succeed
2. **Test the change** - Deploy to test environment or run locally
3. **Get user approval** - Do NOT auto-merge, even after reviews pass
4. **Only merge when explicitly told** - "merge it", "LGTM", "ship it"

**NEVER:**
- Merge a PR just because CI passed
- Merge without verifying the fix works
- Auto-merge in a "merge party" without explicit approval for each PR

### Git Workflow
- Rebasing an owned feature branch is allowed when it keeps a stacked PR current or makes its dependency history clearer.
- Publish rewritten history with `git push --force-with-lease`, never plain `--force`/`-f`, so an unseen remote update is not overwritten.
- Do not rewrite `master`, `v3`, `v4`, release branches, or another contributor's branch.
- Before rewriting a branch, check for active worktrees/collaborators and record the new exact head in affected PR reports.

---

## Code Review

### CodeRabbit
CodeRabbit automatic reviews, incremental reviews, approval workflow, and review status publishing are disabled in `.coderabbit.yaml` by owner request. Do not invoke CodeRabbit or treat its reviews or status as merge gates. Use `/review` and the repository's tests.

### Open Horizons skills
**When to use:** Apply the skill that fits the decision or phase of work.
- Use `/review` before commits and at natural pause points.
- Use `/dissent` before significant or one-way decisions.
- Use `/problem-space` when a defect or architectural concern repeats.
- Use `/salvage` when work has drifted or an approach has reversed repeatedly.

---

## Branch Strategy

- `master` = Node.js v2.x (legacy, stable)
- `v3` = Rust v3.x (maintenance)
- `v4` = Rust streaming, companion, Home Assistant, and optional HiPhi line (active development)

**Default branch for Rust development: `v4`**

## Test-Driven Development (TDD)

When fixing bugs or implementing features that affect client behavior:

1. **Write the test FIRST** - Express what the client expects in the test harness
2. **Run the test and see it FAIL** - Verify the test catches the bug
3. **ONLY THEN fix the code** - Implement the fix to make the test pass

**DO NOT:**
- Look at the Rust implementation and change the test harness to match
- Fix the code before seeing the test fail

**DO:**
- Look at what the CLIENT expects (knob C code, iOS Swift, Node.js server)
- Write a test expressing that expectation
- Verify the test fails against the current implementation
- Then fix the implementation

## Client Test Harness

The test harness (`tests/client_harness.rs`) simulates:
- ESP32 Knob client (roon-knob C firmware)
- iOS/Apple Watch client (hifi-control-ios Swift app)

Reference implementations for expected behavior:
- Node.js server: `/Users/muness1/src/unified-hifi-control/`
- Knob firmware: C code defining expected API responses
- iOS app: Swift BridgeClient defining expected API format

## API Compatibility

The Rust server must be a drop-in replacement for the Node.js server. All API responses must match the Node.js format exactly, including:
- Response structure (`{zones: [...]}` not bare arrays)
- Field names (`zone_name` not `display_name`)
- Zone ID prefixes (`roon:`, `openhome:`, `upnp:`, `lms:`)
- Error response formats

## API Stability (CRITICAL)

**DO NOT add, remove, or modify API endpoints without explicit user approval.**

This includes:
- Route paths in main.rs
- HTTP methods (GET/POST/PUT/DELETE)
- Response payload structure
- Request body schemas

### Enforcement

1. **Contract file**: `tests/fixtures/api_routes.txt` lists all routes
2. **Test**: `cargo test --test api_contract` fails if routes change
3. **CI**: PRs changing the contract require `api-change-approved` label

### If you think API needs to change

1. **ASK FIRST** - Describe the proposed change and get explicit approval
2. Only after approval: update `api_routes.txt` and implementation
3. User adds `api-change-approved` label to PR

**NEVER:**
- Add the `api-change-approved` label yourself
- Update `api_routes.txt` without explicit approval
- Assume API changes are "minor" or "safe"

## Engineering Philosophy

**Do things the right way, even if "larger" or "harder".**

### Terminology: "Refactor"

**Refactor means restructuring code WITHOUT changing behavior.**

- A refactor is a behavior-preserving transformation
- If you're adding a new type (e.g., `PrefixedZoneId`), wiring it through means updating ALL call sites to use it - not just adding the type definition
- "Refactor to use X" = find every place that should use X and change it
- Don't confuse "adding a type" with "refactoring to use a type"

- Correct architecture now prevents regressions later
- Short-term hacks create long-term maintenance burden
- When you see an architectural inconsistency, fix it - don't work around it
- If a component exists for a purpose (e.g., aggregator as "single source of truth"), USE IT

**Examples:**
- Don't bypass the aggregator to query adapters directly
- Don't cache computed values at the wrong layer
- Don't add settings-filtering at query time if it belongs at the source

---

## Architecture

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the event bus pattern and design principles.

**Key insight:** Complexity should be absorbed by the bus, not distributed across components. When in doubt:
- Adapters are dumb (discover, translate, handle commands)
- Aggregator owns state (merge, hydrate, track last-seen)
- UI talks to aggregator only (never directly to backends)

## MCP Server (AI Assistant Integration)

**Endpoint:** `http://<host>:8088/mcp` (Streamable HTTP transport)

MCP (Model Context Protocol) enables AI assistants (Claude, ChatGPT, BoltAI, etc.) to control music playback.

### Current Capabilities

**This table is generated. Do not hand-edit it.**

It is rendered from `hifi_capabilities`' own wire payload by
`tests/mcp_contract.rs::agents_md_capability_matrix_matches_the_derived_data`,
which fails on any drift. Regenerate with:

```sh
UPDATE_AGENTS_MATRIX=1 cargo test --test mcp_contract agents_md
```

Three states, not two (per epic #392). Collapsing the last two into "unsupported"
makes UHC's gaps look like provider limitations, and that is not hypothetical: a
hand-written `❌` in this very table claimed OpenHome and UPnP could not do volume,
when both adapters implement `vol_abs`/`vol_rel` and `POST /openhome/control` had
been exposing it over HTTP the whole time. Only the MCP path declined. #398 wired
it, and made the table derived so the same error cannot be typed again.

Two things this table does **not** claim:

- **It is per provider, not per device.** A fixed-volume Roon output reports
  `volume` as supported. `hifi_capabilities` reports `has_volume_control`
  separately: an explicit device flag is authoritative when available (Spotify's
  `supports_volume`, even with a null current value); other providers fall back to
  whether the aggregator has observed a numeric control.
- **Every ⛔ for OpenHome and UPnP rests on specification knowledge, not on a call
  to a device.** Each cell says so in its own footnote. LMS's cells come from a live
  Lyrion 9.1.2 inventory (#402/#403) and are the strongest rows here.

<!-- BEGIN GENERATED CAPABILITY MATRIX (#398) -->

| Capability | roon | lms | openhome | upnp | hqplayer | applemusic | spotify | musicassistant |
|---|---|---|---|---|---|---|---|---|
| `transport` | ✅ | ✅ | ✅ | ✅ | ✅ | 🚧 #465 | ✅ | ✅ |
| `transport_skip` | ✅ | ✅ | ✅ | 🚧 #392 | ✅ | 🚧 #465 | ✅ | ✅ |
| `volume` | ✅ | ✅ | ✅ | ✅ | ✅ | 🚧 #465 | ✅ | ✅ |
| `search` | ✅ | ✅ | ⛔ | ⛔ | 🚧 #209 | 🚧 #481 | ✅ | ✅ |
| `play_by_query` | ✅ | ✅ | ⛔ | ⛔ | 🚧 #209 | 🚧 #481 | ✅ | ✅ |
| `play_by_ref` | 🚧 #396 | 🚧 #396 | 🚧 #396 | 🚧 #396 | 🚧 #209 | 🚧 #481 | ✅ | ✅ |
| `browse` | ✅ | ✅ | ⛔ | ⛔ | 🚧 #209 | 🚧 #482 | 🚧 #473 | ✅ |
| `queue_read` | 🚧 #400 | 🚧 #400 | 🚧 #392 | ⛔ | 🚧 #209 | 🚧 #483 | ✅ | ✅ |
| `queue_jump` | 🚧 #400 | 🚧 #400 | 🚧 #392 | ⛔ | 🚧 #209 | 🚧 #483 | ⛔ | ✅ |
| `queue_reorder` | ⛔ | 🚧 #400 | 🚧 #392 | ⛔ | 🚧 #209 | 🚧 #483 | ⛔ | ✅ |
| `queue_remove` | ⛔ | 🚧 #400 | 🚧 #392 | ⛔ | 🚧 #209 | 🚧 #483 | ⛔ | ✅ |
| `queue_clear` | ⛔ | 🚧 #400 | 🚧 #392 | ⛔ | 🚧 #209 | 🚧 #483 | ⛔ | ✅ |
| `queue_transfer` | ⛔ | 🚧 #400 | ⛔ | ⛔ | 🚧 #209 | 🚧 #462 | ⛔ | ✅ |
| `play_next` | 🚧 #399 | 🚧 #403 | 🚧 #392 | 🚧 #396 | 🚧 #209 | 🚧 #483 | 🚧 #474 | ✅ |
| `repeat_mode` | 🚧 #360 | 🚧 #403 | 🚧 #392 | 🚧 #392 | ✅ | 🚧 #462 | ✅ | ✅ |
| `shuffle_mode` | 🚧 #360 | 🚧 #403 | 🚧 #392 | 🚧 #392 | ✅ | 🚧 #462 | ✅ | ✅ |
| `saved_playlists` | ✅ | ✅ | ⛔ | ⛔ | 🚧 #209 | 🚧 #482 | ✅ | ✅ |
| `favorites` | 🚧 #531 | ✅ | 🚧 #392 | ⛔ | 🚧 #209 | 🚧 #482 | ✅ | ✅ |
| `radio` | ✅ | ✅ | 🚧 #392 | ⛔ | 🚧 #209 | 🚧 #462 | 🚧 #462 | ✅ |
| `multiroom_sync` | ✅ | ✅ | 🚧 #392 | ⛔ | 🚧 #209 | 🚧 #462 | ⛔ | ✅ |

✅ supported · ⛔ the provider's protocol cannot do it · 🚧 the provider can, UHC has not wired it (issue that will)

Every non-supported cell states the fact it rests on, so the claim can be checked rather than trusted:

- 🚧 **applemusic / `transport`** (#465) — the native iPhone companion path is implemented, but SystemMusicPlayer transport and volume behavior remains pending signed physical-device validation (#465).
- 🚧 **upnp / `transport_skip`** (#392) — AVTransport:1 -- the service this adapter already speaks -- defines Next and Previous actions, and UHC's adapter refuses them before issuing either (src/adapters/upnp.rs, REFUSED_TRANSPORT_ACTIONS). A renderer with no playlist would reject the call, but that is the device's answer to give, not UHC's to assume.
- 🚧 **applemusic / `transport_skip`** (#465) — the native iPhone companion path is implemented, but SystemMusicPlayer transport and volume behavior remains pending signed physical-device validation (#465).
- 🚧 **applemusic / `volume`** (#465) — the native iPhone companion path is implemented, but SystemMusicPlayer transport and volume behavior remains pending signed physical-device validation (#465).
- ⛔ **openhome / `search`** — UHC discovers OpenHome zones by their av-openhome-org Product, Transport and Volume services (src/adapters/openhome.rs) and the OpenHome service set has no library: content is resolved by a control point against a separate media server. Verified from the OpenHome service definitions, not from a device.
- ⛔ **upnp / `search`** — UHC discovers UPnP zones as urn:schemas-upnp-org:device:MediaRenderer:1 and speaks only AVTransport:1 and RenderingControl:1. Searching or browsing content is a ContentDirectory:1 (MediaServer) capability, which a renderer does not have and UHC does not discover. Verified from the UPnP AV service definitions, not from a device.
- 🚧 **hqplayer / `search`** (#209) — UHC's HQPlayer adapter speaks transport, volume, seek and pipeline settings; whether HQPlayer's control protocol reaches content operations has not been verified here. Reported as not-yet-implemented rather than as a provider limit, because an unverified 'never' is the more expensive error.
- 🚧 **applemusic / `search`** (#481) — the native companion content bridge is specified but not enabled; this capability remains pending its approved owner-scoped transport and companion validation.
- ⛔ **openhome / `play_by_query`** — UHC discovers OpenHome zones by their av-openhome-org Product, Transport and Volume services (src/adapters/openhome.rs) and the OpenHome service set has no library: content is resolved by a control point against a separate media server. Verified from the OpenHome service definitions, not from a device.
- ⛔ **upnp / `play_by_query`** — UHC discovers UPnP zones as urn:schemas-upnp-org:device:MediaRenderer:1 and speaks only AVTransport:1 and RenderingControl:1. Searching or browsing content is a ContentDirectory:1 (MediaServer) capability, which a renderer does not have and UHC does not discover. Verified from the UPnP AV service definitions, not from a device.
- 🚧 **hqplayer / `play_by_query`** (#209) — UHC's HQPlayer adapter speaks transport, volume, seek and pipeline settings; whether HQPlayer's control protocol reaches content operations has not been verified here. Reported as not-yet-implemented rather than as a provider limit, because an unverified 'never' is the more expensive error.
- 🚧 **applemusic / `play_by_query`** (#481) — the native companion content bridge is specified but not enabled; this capability remains pending its approved owner-scoped transport and companion validation.
- 🚧 **roon / `play_by_ref`** (#396) — RoonAdapter::browse/load/play_item exist and are exposed over HTTP; MCP mints no reference for a search hit to act on.
- 🚧 **lms / `play_by_ref`** (#396) — the native taggedlist queries return durable entity ids (track_id/album_id/artist_id) that playlistcontrol accepts, verified live; MCP discards them and hands back a title. Note the XMLBrowser paths (globalsearch, favorites) return positional breadcrumbs instead, which is #396's safety problem, not a capability gap.
- 🚧 **openhome / `play_by_ref`** (#396) — the protocol can play a specific item -- UPnP AVTransport:1 takes SetAVTransportURI and SetNextAVTransportURI, OpenHome Playlist:1 takes Insert(AfterId, Uri, Metadata) -- so what is missing is UHC's ability to name one, not the device's ability to play it. Reported as a UHC gap rather than a provider limit, because a reference minted against a media server would work. Verified from the UPnP AV and OpenHome service definitions, not from a device.
- 🚧 **upnp / `play_by_ref`** (#396) — the protocol can play a specific item -- UPnP AVTransport:1 takes SetAVTransportURI and SetNextAVTransportURI, OpenHome Playlist:1 takes Insert(AfterId, Uri, Metadata) -- so what is missing is UHC's ability to name one, not the device's ability to play it. Reported as a UHC gap rather than a provider limit, because a reference minted against a media server would work. Verified from the UPnP AV and OpenHome service definitions, not from a device.
- 🚧 **hqplayer / `play_by_ref`** (#209) — UHC's HQPlayer adapter speaks transport, volume, seek and pipeline settings; whether HQPlayer's control protocol reaches content operations has not been verified here. Reported as not-yet-implemented rather than as a provider limit, because an unverified 'never' is the more expensive error.
- 🚧 **applemusic / `play_by_ref`** (#481) — the native companion content bridge is specified but not enabled; this capability remains pending its approved owner-scoped transport and companion validation.
- ⛔ **openhome / `browse`** — UHC discovers OpenHome zones by their av-openhome-org Product, Transport and Volume services (src/adapters/openhome.rs) and the OpenHome service set has no library: content is resolved by a control point against a separate media server. Verified from the OpenHome service definitions, not from a device.
- ⛔ **upnp / `browse`** — UHC discovers UPnP zones as urn:schemas-upnp-org:device:MediaRenderer:1 and speaks only AVTransport:1 and RenderingControl:1. Searching or browsing content is a ContentDirectory:1 (MediaServer) capability, which a renderer does not have and UHC does not discover. Verified from the UPnP AV service definitions, not from a device.
- 🚧 **hqplayer / `browse`** (#209) — UHC's HQPlayer adapter speaks transport, volume, seek and pipeline settings; whether HQPlayer's control protocol reaches content operations has not been verified here. Reported as not-yet-implemented rather than as a provider limit, because an unverified 'never' is the more expensive error.
- 🚧 **applemusic / `browse`** (#482) — the native companion content bridge is specified but not enabled; this capability remains pending its approved owner-scoped transport and companion validation.
- 🚧 **spotify / `browse`** (#473) — Spotify removed categories, category playlists, featured playlists, and new releases from the Web API surface available to new Development Mode applications in February 2026. A future browse implementation must use a currently available, quota-aware surface rather than call those retired endpoints.
- 🚧 **roon / `queue_read`** (#400) — the pinned roon-api fork exposes subscribe_queue(zone, max_items), which nothing in UHC calls.
- 🚧 **lms / `queue_read`** (#400) — status <player> <start> <n> returns the whole current playlist with playlist_cur_index; verified live.
- 🚧 **openhome / `queue_read`** (#392) — OpenHome's Playlist:1 service provides Read/ReadList/IdArray, Insert, DeleteId, DeleteAll, SeekId/SeekIndex and SetRepeat/SetShuffle. UHC discovers only Product/Transport/Volume and drives none of it, so this is a UHC gap. Verified from the OpenHome service definitions, not from a device.
- ⛔ **upnp / `queue_read`** — AVTransport:1 holds a single current transport URI plus one SetNextAVTransportURI; it has no playlist to enumerate or mutate. Verified from the UPnP AV service definitions, not from a device.
- 🚧 **hqplayer / `queue_read`** (#209) — UHC's HQPlayer adapter speaks transport, volume, seek and pipeline settings; whether HQPlayer's control protocol reaches content operations has not been verified here. Reported as not-yet-implemented rather than as a provider limit, because an unverified 'never' is the more expensive error.
- 🚧 **applemusic / `queue_read`** (#483) — the native companion content bridge is specified but not enabled; this capability remains pending its approved owner-scoped transport and companion validation.
- 🚧 **roon / `queue_jump`** (#400) — the pinned roon-api fork exposes play_from_here(zone, queue_item_id), which nothing in UHC calls.
- 🚧 **lms / `queue_jump`** (#400) — playlist index <n> jumps to a queue position; verified live.
- 🚧 **openhome / `queue_jump`** (#392) — OpenHome's Playlist:1 service provides Read/ReadList/IdArray, Insert, DeleteId, DeleteAll, SeekId/SeekIndex and SetRepeat/SetShuffle. UHC discovers only Product/Transport/Volume and drives none of it, so this is a UHC gap. Verified from the OpenHome service definitions, not from a device.
- ⛔ **upnp / `queue_jump`** — AVTransport:1 holds a single current transport URI plus one SetNextAVTransportURI; it has no playlist to enumerate or mutate. Verified from the UPnP AV service definitions, not from a device.
- 🚧 **hqplayer / `queue_jump`** (#209) — UHC's HQPlayer adapter speaks transport, volume, seek and pipeline settings; whether HQPlayer's control protocol reaches content operations has not been verified here. Reported as not-yet-implemented rather than as a provider limit, because an unverified 'never' is the more expensive error.
- 🚧 **applemusic / `queue_jump`** (#483) — the native companion content bridge is specified but not enabled; this capability remains pending its approved owner-scoped transport and companion validation.
- ⛔ **spotify / `queue_jump`** — Spotify's Web API exposes Get the User's Queue and Add Item to Playback Queue, but no endpoint to jump, reorder, remove, clear, or transfer active queue contents. Verified from the Spotify Web API Player reference, not inferred from a device.
- ⛔ **roon / `queue_reorder`** — The Roon API's transport service exposes a queue subscription and play_from_here and no mutation at all -- no move, remove or clear. The pinned roon-api fork (ohc/main) exposes subscribe_queue and play_from_here and nothing further.
- 🚧 **lms / `queue_reorder`** (#400) — playlist move <from> <to> reorders the queue; verified live. Roon cannot do this and LMS can, which is why this capability exists in the vocabulary at all.
- 🚧 **openhome / `queue_reorder`** (#392) — OpenHome's Playlist:1 has no Move action, but Insert takes an AfterId, so a reorder is DeleteId plus Insert -- composite rather than atomic, and reachable. Verified from the OpenHome service definitions, not from a device.
- ⛔ **upnp / `queue_reorder`** — AVTransport:1 holds a single current transport URI plus one SetNextAVTransportURI; it has no playlist to enumerate or mutate. Verified from the UPnP AV service definitions, not from a device.
- 🚧 **hqplayer / `queue_reorder`** (#209) — UHC's HQPlayer adapter speaks transport, volume, seek and pipeline settings; whether HQPlayer's control protocol reaches content operations has not been verified here. Reported as not-yet-implemented rather than as a provider limit, because an unverified 'never' is the more expensive error.
- 🚧 **applemusic / `queue_reorder`** (#483) — the native companion content bridge is specified but not enabled; this capability remains pending its approved owner-scoped transport and companion validation.
- ⛔ **spotify / `queue_reorder`** — Spotify's Web API exposes Get the User's Queue and Add Item to Playback Queue, but no endpoint to jump, reorder, remove, clear, or transfer active queue contents. Verified from the Spotify Web API Player reference, not inferred from a device.
- ⛔ **roon / `queue_remove`** — The Roon API's transport service exposes a queue subscription and play_from_here and no mutation at all -- no move, remove or clear. The pinned roon-api fork (ohc/main) exposes subscribe_queue and play_from_here and nothing further.
- 🚧 **lms / `queue_remove`** (#400) — playlist delete <index> removes one queued item; verified live.
- 🚧 **openhome / `queue_remove`** (#392) — OpenHome's Playlist:1 service provides Read/ReadList/IdArray, Insert, DeleteId, DeleteAll, SeekId/SeekIndex and SetRepeat/SetShuffle. UHC discovers only Product/Transport/Volume and drives none of it, so this is a UHC gap. Verified from the OpenHome service definitions, not from a device.
- ⛔ **upnp / `queue_remove`** — AVTransport:1 holds a single current transport URI plus one SetNextAVTransportURI; it has no playlist to enumerate or mutate. Verified from the UPnP AV service definitions, not from a device.
- 🚧 **hqplayer / `queue_remove`** (#209) — UHC's HQPlayer adapter speaks transport, volume, seek and pipeline settings; whether HQPlayer's control protocol reaches content operations has not been verified here. Reported as not-yet-implemented rather than as a provider limit, because an unverified 'never' is the more expensive error.
- 🚧 **applemusic / `queue_remove`** (#483) — the native companion content bridge is specified but not enabled; this capability remains pending its approved owner-scoped transport and companion validation.
- ⛔ **spotify / `queue_remove`** — Spotify's Web API exposes Get the User's Queue and Add Item to Playback Queue, but no endpoint to jump, reorder, remove, clear, or transfer active queue contents. Verified from the Spotify Web API Player reference, not inferred from a device.
- ⛔ **roon / `queue_clear`** — The Roon API's transport service exposes a queue subscription and play_from_here and no mutation at all -- no move, remove or clear. The pinned roon-api fork (ohc/main) exposes subscribe_queue and play_from_here and nothing further.
- 🚧 **lms / `queue_clear`** (#400) — playlist clear empties the queue; verified live.
- 🚧 **openhome / `queue_clear`** (#392) — OpenHome's Playlist:1 service provides Read/ReadList/IdArray, Insert, DeleteId, DeleteAll, SeekId/SeekIndex and SetRepeat/SetShuffle. UHC discovers only Product/Transport/Volume and drives none of it, so this is a UHC gap. Verified from the OpenHome service definitions, not from a device.
- ⛔ **upnp / `queue_clear`** — AVTransport:1 holds a single current transport URI plus one SetNextAVTransportURI; it has no playlist to enumerate or mutate. Verified from the UPnP AV service definitions, not from a device.
- 🚧 **hqplayer / `queue_clear`** (#209) — UHC's HQPlayer adapter speaks transport, volume, seek and pipeline settings; whether HQPlayer's control protocol reaches content operations has not been verified here. Reported as not-yet-implemented rather than as a provider limit, because an unverified 'never' is the more expensive error.
- 🚧 **applemusic / `queue_clear`** (#483) — the native companion content bridge is specified but not enabled; this capability remains pending its approved owner-scoped transport and companion validation.
- ⛔ **spotify / `queue_clear`** — Spotify's Web API exposes Get the User's Queue and Add Item to Playback Queue, but no endpoint to jump, reorder, remove, clear, or transfer active queue contents. Verified from the Spotify Web API Player reference, not inferred from a device.
- ⛔ **roon / `queue_transfer`** — The Roon API's transport service exposes a queue subscription and play_from_here and no mutation at all -- no move, remove or clear. The pinned roon-api fork (ohc/main) exposes subscribe_queue and play_from_here and nothing further.
- 🚧 **lms / `queue_transfer`** (#400) — sync <playerid> was verified live (#403) to merge a player into another's sync group by adopting the leader's queue, which destroys the source's queue rather than transferring it; the CLI reference names no dedicated queue-to-queue transfer command. A composite emulation -- read the source's playlist, replay it against the target with playlistcontrol, then playlist clear the source -- is buildable from primitives #400 already verified live, but is not wired.
- ⛔ **openhome / `queue_transfer`** — OpenHome's Playlist:1 (Read/Insert/DeleteId/DeleteAll) is scoped to one room's renderer; the service set defines no action that moves one room's playlist into another's. Songcast (Sender:1/Receiver:1) relays audio to a group, it does not merge queue state. Verified from the OpenHome service definitions, not from a device.
- ⛔ **upnp / `queue_transfer`** — AVTransport:1 holds a single current transport URI plus one SetNextAVTransportURI; it has no playlist to enumerate or mutate. Verified from the UPnP AV service definitions, not from a device.
- 🚧 **hqplayer / `queue_transfer`** (#209) — UHC's HQPlayer adapter speaks transport, volume, seek and pipeline settings; whether HQPlayer's control protocol reaches content operations has not been verified here. Reported as not-yet-implemented rather than as a provider limit, because an unverified 'never' is the more expensive error.
- 🚧 **applemusic / `queue_transfer`** (#462) — the native companion content bridge is specified but not enabled; this capability remains pending its approved owner-scoped transport and companion validation.
- ⛔ **spotify / `queue_transfer`** — Spotify's Web API exposes Get the User's Queue and Add Item to Playback Queue, but no endpoint to jump, reorder, remove, clear, or transfer active queue contents. Verified from the Spotify Web API Player reference, not inferred from a device.
- 🚧 **roon / `play_next`** (#399) — Roon's browse item actions include Play Next alongside Play Now and Queue; UHC's PlayAction models only Play, Queue and Radio, so this arrives with browse rather than with the queue.
- 🚧 **lms / `play_next`** (#403) — playlistcontrol cmd:insert places an item immediately after the current one, verified live -- and LmsPlayAction::Insert is already modelled in the adapter and simply unreachable from MCP.
- 🚧 **openhome / `play_next`** (#392) — OpenHome's Playlist:1 service provides Read/ReadList/IdArray, Insert, DeleteId, DeleteAll, SeekId/SeekIndex and SetRepeat/SetShuffle. UHC discovers only Product/Transport/Volume and drives none of it, so this is a UHC gap. Verified from the OpenHome service definitions, not from a device.
- 🚧 **upnp / `play_next`** (#396) — the protocol can play a specific item -- UPnP AVTransport:1 takes SetAVTransportURI and SetNextAVTransportURI, OpenHome Playlist:1 takes Insert(AfterId, Uri, Metadata) -- so what is missing is UHC's ability to name one, not the device's ability to play it. Reported as a UHC gap rather than a provider limit, because a reference minted against a media server would work. Verified from the UPnP AV and OpenHome service definitions, not from a device.
- 🚧 **hqplayer / `play_next`** (#209) — UHC's HQPlayer adapter speaks transport, volume, seek and pipeline settings; whether HQPlayer's control protocol reaches content operations has not been verified here. Reported as not-yet-implemented rather than as a provider limit, because an unverified 'never' is the more expensive error.
- 🚧 **applemusic / `play_next`** (#483) — the native companion content bridge is specified but not enabled; this capability remains pending its approved owner-scoped transport and companion validation.
- 🚧 **spotify / `play_next`** (#474) — Spotify exposes Add Item to Playback Queue and UHC routes it through hifi_play action=queue, but UHC does not expose a distinct play-next operation for Spotify.
- 🚧 **roon / `repeat_mode`** (#360) — the Roon API's transport service takes loop settings (disabled/loop/loop_one); UHC drives none of them from any surface.
- 🚧 **lms / `repeat_mode`** (#403) — playlist repeat <0|1|2> and playlist repeat ? read and write it; verified live. Note the mode lives on the sync master, so setting it changes every member.
- 🚧 **openhome / `repeat_mode`** (#392) — OpenHome's Playlist:1 service provides Read/ReadList/IdArray, Insert, DeleteId, DeleteAll, SeekId/SeekIndex and SetRepeat/SetShuffle. UHC discovers only Product/Transport/Volume and drives none of it, so this is a UHC gap. Verified from the OpenHome service definitions, not from a device.
- 🚧 **upnp / `repeat_mode`** (#392) — AVTransport:1's SetPlayMode takes REPEAT_ONE and REPEAT_ALL, so repeat is a protocol feature UHC does not use. Verified from the UPnP AV service definitions, not from a device.
- 🚧 **applemusic / `repeat_mode`** (#462) — the native companion content bridge is specified but not enabled; this capability remains pending its approved owner-scoped transport and companion validation.
- 🚧 **roon / `shuffle_mode`** (#360) — the Roon API's transport service takes a shuffle setting; UHC drives it from no surface.
- 🚧 **lms / `shuffle_mode`** (#403) — playlist shuffle <0|1|2> and playlist shuffle ? read and write it; verified live. Setting it reshuffles the queue, so it is also a queue mutation.
- 🚧 **openhome / `shuffle_mode`** (#392) — OpenHome's Playlist:1 service provides Read/ReadList/IdArray, Insert, DeleteId, DeleteAll, SeekId/SeekIndex and SetRepeat/SetShuffle. UHC discovers only Product/Transport/Volume and drives none of it, so this is a UHC gap. Verified from the OpenHome service definitions, not from a device.
- 🚧 **upnp / `shuffle_mode`** (#392) — AVTransport:1's SetPlayMode takes SHUFFLE and RANDOM, so shuffle is a protocol feature UHC does not use. Verified from the UPnP AV service definitions, not from a device.
- 🚧 **applemusic / `shuffle_mode`** (#462) — the native companion content bridge is specified but not enabled; this capability remains pending its approved owner-scoped transport and companion validation.
- ⛔ **openhome / `saved_playlists`** — the av-openhome-org service set has no playlist storage: Playlist:1 is the live queue (Read/Insert/DeleteId/DeleteAll) with no save or recall action, and stored playlists live on whatever media server the control point uses. Verified from the OpenHome service definitions, not from a device.
- ⛔ **upnp / `saved_playlists`** — AVTransport:1 and RenderingControl:1 store nothing; a MediaRenderer has no playlist storage. Verified from the UPnP AV service definitions, not from a device.
- 🚧 **hqplayer / `saved_playlists`** (#209) — UHC's HQPlayer adapter speaks transport, volume, seek and pipeline settings; whether HQPlayer's control protocol reaches content operations has not been verified here. Reported as not-yet-implemented rather than as a provider limit, because an unverified 'never' is the more expensive error.
- 🚧 **applemusic / `saved_playlists`** (#482) — the native companion content bridge is specified but not enabled; this capability remains pending its approved owner-scoped transport and companion validation.
- 🚧 **roon / `favorites`** (#531) — Roon exposes My Favorites and tags as browse hierarchies, so this arrives with browse.
- 🚧 **openhome / `favorites`** (#392) — OpenHome devices carry stored presets (Radio:1 presets, and a Pins service on newer firmware). UHC discovers neither. Reported as a gap rather than a limit because the per-firmware reach is unverified here.
- ⛔ **upnp / `favorites`** — AVTransport:1 and RenderingControl:1 store nothing; a MediaRenderer has no favourites. Verified from the UPnP AV service definitions, not from a device.
- 🚧 **hqplayer / `favorites`** (#209) — UHC's HQPlayer adapter speaks transport, volume, seek and pipeline settings; whether HQPlayer's control protocol reaches content operations has not been verified here. Reported as not-yet-implemented rather than as a provider limit, because an unverified 'never' is the more expensive error.
- 🚧 **applemusic / `favorites`** (#482) — the native companion content bridge is specified but not enabled; this capability remains pending its approved owner-scoped transport and companion validation.
- 🚧 **openhome / `radio`** (#392) — OpenHome's Radio:1 service is exactly this capability -- it carries station presets and a channel URI. UHC discovers neither the service nor its presets. Verified from the OpenHome service definitions, not from a device.
- ⛔ **upnp / `radio`** — A plain UPnP AV renderer stores nothing: AVTransport plays a URI a control point hands it, and no service in the AV set enumerates stations. Radio on such a device is whatever the control point remembers, which is not a provider surface UHC can browse. Verified from the UPnP AV service definitions, not from a device.
- 🚧 **hqplayer / `radio`** (#209) — UHC's HQPlayer adapter speaks transport, volume, seek and pipeline settings; whether HQPlayer's control protocol reaches content operations has not been verified here. Reported as not-yet-implemented rather than as a provider limit, because an unverified 'never' is the more expensive error.
- 🚧 **applemusic / `radio`** (#462) — the native companion content bridge is specified but not enabled; this capability remains pending its approved owner-scoped transport and companion validation.
- 🚧 **spotify / `radio`** (#462) — the Spotify adapter has no routed implementation for this capability; the provider-neutral streaming follow-up tracks it.
- 🚧 **openhome / `multiroom_sync`** (#392) — OpenHome's Sender:1 and Receiver:1 services are Songcast multiroom -- exactly this capability. UHC discovers neither. Verified from the OpenHome service definitions, not from a device.
- ⛔ **upnp / `multiroom_sync`** — UPnP AV defines no synchronised-playback service; multiroom on UPnP renderers is vendor-specific and outside the two services UHC speaks. Verified from the UPnP AV service definitions, not from a device.
- 🚧 **hqplayer / `multiroom_sync`** (#209) — UHC's HQPlayer adapter speaks transport, volume, seek and pipeline settings; whether HQPlayer's control protocol reaches content operations has not been verified here. Reported as not-yet-implemented rather than as a provider limit, because an unverified 'never' is the more expensive error.
- 🚧 **applemusic / `multiroom_sync`** (#462) — the native companion content bridge is specified but not enabled; this capability remains pending its approved owner-scoped transport and companion validation.
- ⛔ **spotify / `multiroom_sync`** — Spotify's Transfer Playback endpoint accepts a single target device and does not synchronize multiple Connect devices; transfer is device selection, not multiroom grouping. Verified from the Spotify Web API Transfer Playback reference, not inferred from a device.
<!-- END GENERATED CAPABILITY MATRIX (#398) -->

`hqplayer:` is a fifth zone prefix: `PrefixedZoneId` lists it and `HqpAdapter`
publishes `ZoneDiscovered` with it, so HQPlayer zones appear in `hifi_zones`. Until
#398 the MCP routing layer had no arm for it and sent every one of them to Roon.
#398 recognises the prefix and reports the gap; wiring it is #328.

Two rows the earlier hand-written table had are deliberately absent, rather than
lost. **Discovery** is not per-provider: `hifi_zones` reads the aggregator, which
holds every zone type identically, so there is nothing for a three-state value to
distinguish. **Queue building** was `hifi_play action='queue'` — the same call path
as `play_by_query`, so it is that row; genuine queue *mutation* is the five
`queue_*` rows, which is where LMS's advantage over Roon becomes visible.

### Structured results

Every tool returns its human-readable text **plus** a structured envelope on
`structuredContent` (issue #395), carrying outcome, scope, the resolved action and
parameters, observed state where the aggregator has it, and a classified reason
when refused. The shape and its compatibility rules are documented in
`src/mcp/envelope.rs`; downstream issues attach their payloads to `data` rather
than inventing parallel shapes.

### Tool Design Principles

**Every tool must work as documented.** AI models try the "obvious" thing first - if it fails, they give up or confuse users.

- **No broken tools** - If a tool can't reliably do what its description says, remove it
- **No orphaned fields** - Don't return data (like `item_key`) that can't be used by any tool
- **Clear queue pattern** - Use `hifi_play` with `action='queue'` to build playlists; call multiple times for multiple tracks

**Tool descriptions are documentation.** AI models read descriptions to decide which tool to use. Be explicit about capabilities:
- ❌ "Searches and immediately plays" (hides queue capability)
- ✅ "Searches and plays, queues, or starts radio. Use action='queue' to add to queue."

### Current Tools (10)

| Tool | Purpose |
|------|---------|
| `hifi_zones` | List all playback zones |
| `hifi_now_playing` | Get current track info |
| `hifi_control` | Play, pause, next, previous, volume |
| `hifi_search` | Search library/TIDAL/Qobuz (Roon, LMS) |
| `hifi_play` | Search and play/queue/radio (Roon, LMS) |
| `hifi_status` | Bridge connection status |
| `hifi_hqplayer_status` | HQPlayer pipeline status |
| `hifi_hqplayer_profiles` | List HQPlayer configs |
| `hifi_hqplayer_load_profile` | Load HQPlayer config |
| `hifi_hqplayer_set_pipeline` | Change HQPlayer setting |

### Playback devices (alpha, #774)

A **device** is the amplifier, receiver or streamer a zone plays through. Families: Denon/Marantz,
WiiM, Sonos, Yamaha MusicCast, BluOS, Cambridge StreamMagic, Trinnov, Onkyo/Integra/Pioneer
(eISCP), Arcam, Hegel, Lyngdorf, Anthem, Eversolo (Zidoo JSON API from community clients), and Home Assistant `media_player`
entities. All alpha,
enabled together by the Settings "Receivers & amplifiers" toggle; Anthem and Trinnov (no
discovery) are configured as `manual_devices` in `device-links.json`. Volume is carried in each
device's native scale (`VolumeControl.scale`: `decibel`, `percentage`, `linear`, `steps`). It is not a zone and never
appears in `hifi_zones`. A user *links* a zone to a device (`hifi_devices action=link`, or
`POST /devices/link`), optionally naming the device input the zone arrives on. From then on:

- the zone's `volume_control` is the device's (dB, the unit's own maximum lowered by the user's
  ceiling), so the knob, Watch, Home Assistant and `hifi_control volume_*` drive the amplifier
  with no client change;
- `/knob/control` and `hifi_devices action=control` accept `power_on`, `power_off`,
  `power_toggle`, `select_input`, `mute`;
- inputs never switch automatically: `select_input` is the only path.

| Tool | Purpose |
|------|---------|
| `hifi_devices` | List devices and inputs; link/unlink zones; set a volume ceiling; control power, input, volume, mute (alpha) |

Every device payload carries `"maturity": "alpha"`. Commands succeed only when the device reports
the requested state back (a write the device does not confirm is an error, never `ok`), and no
level is sent before the device has reported its range. The model, data flow and safety rules are
in `docs/device-control-weave.md`; the SR6008 captures behind the claims below are in
`tests/fixtures/devices/marantz/sr6008/`.

**This table is generated. Do not hand-edit it.** Regenerate with
`UPDATE_AGENTS_DEVICES=1 cargo test --test device_claims`.

<!-- BEGIN GENERATED DEVICE CAPABILITIES (tests/device_claims.rs) -->

| Family | Observe | Volume | Mute | Power | Input select |
|---|---|---|---|---|---|
| Marantz SR6008 | ✅ hardware-verified | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 |
| Other Denon / Marantz | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 |
| WiiM Pro with gc4a | ✅ hardware-verified | 🚧 implemented, #788 | 🚧 implemented, #788 | ⛔ | 🚧 implemented, #788 |
| WiiM Pro No MFI | ✅ hardware-verified | 🚧 implemented, #788 | 🚧 implemented, #788 | ⛔ | 🚧 implemented, #788 |
| WiiM Ultra | ✅ hardware-verified | ✅ hardware-verified | ✅ hardware-verified | ⛔ | ✅ hardware-verified |
| WiiM Amp Pro | ✅ hardware-verified | 🚧 implemented, #788 | 🚧 implemented, #788 | ⛔ | 🚧 implemented, #788 |
| WiiM Pro Plus | ✅ hardware-verified | 🚧 implemented, #788 | 🚧 implemented, #788 | ⛔ | 🚧 implemented, #788 |
| Other WiiM / LinkPlay | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | ⛔ | 🚧 implemented, #788 |
| Sonos | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | ⛔ | ⛔ |
| Yamaha MusicCast | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 |
| BluOS | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | ⛔ | 🚧 implemented, #788 |
| Cambridge StreamMagic (variable output) | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 |
| Trinnov Altitude (MAC configured) | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 |
| Onkyo / Integra / Pioneer (eISCP) | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 |
| Arcam | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 |
| Hegel | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 |
| Lyngdorf | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 |
| Anthem | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 |
| Eversolo DMP-A6 | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 |
| Home Assistant media_player | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 | 🚧 implemented, #788 |

✅ hardware-verified: exercised on real units with captured fixtures · 🚧 implemented from the vendor protocol or reference client, not exercised on hardware (#788) · ⛔ not supported by this family's protocol path

- **Marantz SR6008** — volume: MV over Telnet, or formiPhoneAppVolume over HTTP; bounded by the unit's MVMAX (Telnet) or the protocol's +18 dB full scale, and by any user ceiling; pending on-device verification (#788); power: ZMON/ZMOFF over Telnet, or formiPhoneAppPower over HTTP (the SR6008 refuses Telnet in standby); pending on-device verification (#788)
- **Other Denon / Marantz** — volume: MV over Telnet, or formiPhoneAppVolume over HTTP; bounded by the unit's MVMAX (Telnet) or the protocol's +18 dB full scale, and by any user ceiling; pending on-device verification (#788); power: ZMON/ZMOFF over Telnet, or formiPhoneAppPower over HTTP (the SR6008 refuses Telnet in standby); pending on-device verification (#788)
- **WiiM Pro with gc4a** — volume: setPlayerCmd:vol:N, N in 0-100 (percentage). The device's max_volume setting (else 100) is the maximum; a user ceiling lowers it; fixed-output mode is not detectable over this API; power: WiiM/LinkPlay has no power command; pause is transport, not power, and is never reported as standby
- **WiiM Pro No MFI** — volume: setPlayerCmd:vol:N, N in 0-100 (percentage). The device's max_volume setting (else 100) is the maximum; a user ceiling lowers it; fixed-output mode is not detectable over this API; power: WiiM/LinkPlay has no power command; pause is transport, not power, and is never reported as standby
- **WiiM Ultra** — volume: setPlayerCmd:vol:N, N in 0-100 (percentage). The device's max_volume setting (else 100) is the maximum; a user ceiling lowers it; fixed-output mode is not detectable over this API; power: WiiM/LinkPlay has no power command; pause is transport, not power, and is never reported as standby
- **WiiM Amp Pro** — volume: setPlayerCmd:vol:N, N in 0-100 (percentage). The device's max_volume setting (else 100) is the maximum; a user ceiling lowers it; fixed-output mode is not detectable over this API; power: WiiM/LinkPlay has no power command; pause is transport, not power, and is never reported as standby
- **WiiM Pro Plus** — volume: setPlayerCmd:vol:N, N in 0-100 (percentage). The device's max_volume setting (else 100) is the maximum; a user ceiling lowers it; fixed-output mode is not detectable over this API; power: WiiM/LinkPlay has no power command; pause is transport, not power, and is never reported as standby
- **Other WiiM / LinkPlay** — volume: setPlayerCmd:vol:N, N in 0-100 (percentage). The device's max_volume setting (else 100) is the maximum; a user ceiling lowers it; fixed-output mode is not detectable over this API; power: WiiM/LinkPlay has no power command; pause is transport, not power, and is never reported as standby
- **Sonos** — volume: RenderingControl SetVolume, Channel Master, 0-100 (percentage), per player not per group. 100 is the maximum; a user ceiling lowers it; power: Sonos has no generic power on/off; players sleep and wake by themselves
- **Yamaha MusicCast** — volume: main/setVolume, native units 0..161 step 1 (not a percentage and not dB; the model's own scale), limited by the unit's max_volume; implemented from the YXC specification and aiomusiccast; no MusicCast hardware exercised yet; power: main/setPower on|standby over HTTP. Waking a unit in standby needs network standby enabled on the device; otherwise it does not answer; implemented from the YXC specification and aiomusiccast; no MusicCast hardware exercised yet
- **BluOS** — volume: /Volume?level= (0-100 percent); implemented from pyblu and the BluOS API; no BluOS hardware exercised yet; power: the BluOS API has no universal power command (a streaming module has no amplifier to switch, and power is a host-device matter); refused with an error; implemented from pyblu and the BluOS API; no BluOS hardware exercised yet
- **Cambridge StreamMagic (variable output)** — volume: /zone/state volume_percent (Percentage 0-100), bounded by the unit's volume_limit_percent and the user ceiling; implementation-derived from aiostreammagic, not yet exercised on a unit; power: /system/power ON, and NETWORK standby (ECO standby disables the network and cannot be woken from here); implementation-derived from aiostreammagic, not yet exercised on a unit
- **Trinnov Altitude (MAC configured)** — volume: volume <dB> (-120..+20, the protocol's full scale), lowered by the user ceiling; implementation-derived from py-trinnov-altitude, not yet exercised on a unit; power: off: power_off_SECURED_FHZMCH48FE, complete on OK. on: Wake-on-LAN magic packet to the configured MAC (UDP broadcast, port 9); a sent packet is not confirmation, the unit reads on only when its session reconnects; implementation-derived from py-trinnov-altitude, not yet exercised on a unit
- **Onkyo / Integra / Pioneer (eISCP)** — volume: MVL in the receiver's native hexadecimal units; maximum is the spec's 100 (0x64), and models that stop at 80 clamp a higher write themselves, which the readback reports; implemented from the Onkyo ISCP spec v1.15, not yet exercised on hardware; power: PWR01/PWR00 over eISCP; a receiver in standby accepts network control only when Network Standby is enabled in its setup menu, otherwise power-on is unreachable; implemented from the Onkyo ISCP spec v1.15, not yet exercised on hardware
- **Arcam** — volume: VOLUME (0x0D) in the unit's native 0-99 steps; 99 is the protocol's full scale; implemented from arcam_fmj and Arcam's control documents, not yet exercised on hardware; power: POWER (0x00) direct write while the unit answers; IP wake is unsupported: older Arcam models turn network control off in standby, so switch the unit on at the unit itself (newer models keep LAN control when "HDMI & IP On" is enabled); implemented from arcam_fmj and Arcam's control documents, not yet exercised on hardware
- **Hegel** — volume: -v.N, 0-100 percent (the protocol's full scale is the maximum; a user ceiling lowers it); power: -p.1 / -p.0 over the control session
- **Lyngdorf** — volume: !VOL(n) in tenths of a dB, -99.9 to +24.0 dB; the unit's own !MAXVOL (read-only over the protocol) bounds the range, lowered by the user ceiling; refused while the unit is not on, because standby discards volume writes silently; implementation-derived from the lyngdorf library and vendor manual, not exercised on hardware; power: !POWERONMAIN / !POWEROFFMAIN (main zone only); implementation-derived from the lyngdorf library and vendor manual, not exercised on hardware
- **Anthem** — volume: Z1PVOL in percent, 0 to 100; the protocol's full scale is the maximum, lowered by the user ceiling; refused while the zone is off; implementation-derived from anthemav, not exercised on hardware; Zone 1 only; power: Z1POW1 / Z1POW0 (Zone 1, not the whole unit); waking from standby needs standby IP control enabled on the unit; implementation-derived from anthemav, not exercised on hardware; Zone 1 only
- **Eversolo DMP-A6** — volume: absolute GET setDevicesVolume?volume=N in native steps 0..maxVolume (200 on every captured unit), read back from getState volumeData.currenttVolume; DMP-A6: HDMI output and 6 inputs seen on firmware v1.5.46 (mase1981/uc-intg-eversolo DISCOVERY_ANALYSIS.md); the UPnP descriptor in andrewdunndev/narjo-eversolo-upnp is a DMP-A6; power: off: GET setPowerOption?tag=poweroff (fallback Key.Poweroff); standby is inferred only when the unit stops answering after UHC's own accepted power-off. On: Wake-on-LAN (UDP 9 and 9517) with the unit's MAC, currently available (ableRemoteBoot true); a magic packet is not confirmation; DMP-A6: HDMI output and 6 inputs seen on firmware v1.5.46 (mase1981/uc-intg-eversolo DISCOVERY_ANALYSIS.md); the UPnP descriptor in andrewdunndev/narjo-eversolo-upnp is a DMP-A6
- **Home Assistant media_player** — volume: media_player.volume_set (0–1), bounded by any user ceiling; confirmed by state readback; power: media_player.turn_on/turn_off via Home Assistant; confirmed by state readback

<!-- END GENERATED DEVICE CAPABILITIES -->

---

## UI Stack

**Web UI:** Dioxus + Tailwind CSS + DioxusLabs components

- **Dioxus** - Fullstack Rust framework (SSR + WASM hydration)
- **Tailwind CSS v4** - Utility-first styling via `src/input.css` (standalone CLI, no Node.js)
- **DioxusLabs/components** - Accessible primitives (Navbar, Button, Collapsible, etc.)

**DO NOT use:**
- Bootstrap, Pico CSS, or other CSS frameworks
- Hand-rolled navigation with onclick handlers
- Inline styles for common patterns (use Tailwind utilities)

**Build CSS:** `make css` or `make css-watch` (auto-downloads standalone CLI)

### Building & Running

**CRITICAL: Dioxus fullstack requires `dx build`, not `cargo build`**

The web UI uses SSR + WASM hydration. This means:
1. Server renders HTML (SSR)
2. Client loads WASM bundle
3. WASM "hydrates" the DOM (attaches event handlers, starts futures)

Without the WASM bundle, components render but don't work (no navigation, no button clicks, no data loading).

**Correct workflow:**
```bash
# Build both server + WASM client
dx build --release --platform web

# Run from the dx output directory (contains public/wasm/ assets)
./target/dx/unified-hifi-control/release/web/server
```

**Why `cargo run` doesn't work:**
- Only builds the server, not the WASM client
- Server panics looking for `public/wasm/` which doesn't exist
- Even if it doesn't panic, no hydration = no interactivity

**For development:** Use `dx serve` which handles both builds and hot reload.

**Verify build:** Use `dx build --release --platform web --features web` to verify both server and WASM compile. Do NOT use `cargo check --target wasm32-unknown-unknown` - it lacks correct feature flags. See [README.md Development section](README.md#verify-build-wasm--server) for details.
- If using XcodeBuildMCP, use the installed XcodeBuildMCP skill before calling XcodeBuildMCP tools.
