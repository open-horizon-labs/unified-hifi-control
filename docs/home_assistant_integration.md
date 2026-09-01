# Home Assistant integration (`custom_components/unified_hifi_control`)

**Status: Alpha.** This integration and the MQTT/Home Assistant discovery
publisher it complements (below) are new; expect rough edges and breaking
changes between releases while the entity model and event contract settle.
Report issues on the [tracker](https://github.com/open-horizon-labs/unified-hifi-control/issues).
The pre-1.0 `version` in `manifest.json` reflects this: bump it to `1.0.0`
only once the entity/service contract is considered stable.

A HACS-installable custom integration that exposes every UHC zone as a
`media_player` entity, giving Home Assistant native voice control
("pause the kitchen"), media-player dashboard cards, and `media_player.*`
service calls — beyond what the MQTT device-composition path (PR #518,
issue #508) can offer, since HA's built-in MQTT integration has no
`media_player` platform.

## Repo location

Lives at the repository root (`custom_components/unified_hifi_control/`
plus a root-level `hacs.json`), not in a sibling repo or a nested
subdirectory. Rationale:

- HACS's "integration" category expects `custom_components/<domain>/`
  and `hacs.json` at the **root** of the repository it's pointed at.
  Nesting the integration under e.g. `ha_integration/` would require
  either a second HACS-visible repo or an unsupported layout; keeping it
  at repo root lets users add
  `https://github.com/open-horizon-labs/unified-hifi-control` directly
  as a HACS custom repository today, with zero extra hosting.
- This mirrors how several other server+integration monorepos ship (the
  Rust crate and the Python integration are independent build/test
  units that happen to share a repo and an issue tracker).
- A sibling repo was considered but rejected for this PR: it would
  require creating and maintaining a second GitHub repository, which
  isn't something this change can do unattended, and would separate the
  integration's issue history from the UHC server changes it depends
  on (capability matrix, zone id conventions, event bus shape).

## Architecture

- **Transport**: plain HTTP against UHC's unified `/knob/*` routes
  (`src/knobs/routes.rs`) — `GET /knob/zones`, `GET /knob/now_playing`,
  `POST /knob/control`, `GET /knob/now_playing/image`. These work across
  every adapter (Roon, LMS, HQPlayer, OpenHome, UPnP, Music Assistant)
  without per-provider client code.
- **State updates**: push-based via Server-Sent Events. UHC's
  `GET /events` (`src/api/mod.rs::events_handler`) streams every
  `BusEvent` off a real `tokio::sync::broadcast` channel
  (`src/bus/mod.rs`). The coordinator (`coordinator.py`) holds a
  long-lived connection and applies `NowPlayingChanged`,
  `VolumeChanged`, `SeekPositionChanged`, `ZoneUpdated`,
  `ZoneDiscovered`, and `ZoneRemoved` events directly to in-memory zone
  state, then pushes updates to entities via
  `DataUpdateCoordinator.async_set_updated_data` — no polling delay for
  ordinary playback changes.
  - **Documented polling fallback**: a 60-second periodic poll
    (`FALLBACK_POLL_INTERVAL`) runs alongside the SSE listener as a
    safety net for connections that go idle-dead without erroring
    (some NAT/proxy setups swallow resets on long-lived idle TCP
    connections). Under normal operation this poll observes no changes
    because SSE already applied them.
  - The SSE listener reconnects with exponential backoff (2s-60s) on
    any disconnect and resets to the minimum backoff once a connection
    has stayed up for 5+ seconds.
- **Unavailability**: entities report unavailable when the coordinator's
  last poll failed (`UpdateFailed`) or when their zone_id has been
  removed from the coordinator's zone map (e.g. after a `ZoneRemoved`
  event, or the zone no longer appears in a poll).
- **Capability mapping**: `media_player.py::_supported_features` derives
  `MediaPlayerEntityFeature` flags from the zone's provider using the
  same facts as `src/mcp/capabilities.rs` / AGENTS.md's capability
  matrix on this branch:
  - Transport (play/pause/stop) and volume: all providers except
    AppleMusic, which is gated behind #465 pending physical-device
    validation on the UHC side.
  - Next/previous: all providers except UPnP, whose adapter explicitly
    refuses skip actions (`src/adapters/upnp.rs::REFUSED_TRANSPORT_ACTIONS`).
  - Grouping (`MediaPlayerEntityFeature.GROUPING`, wired to
    `async_join_players`/`async_unjoin_player`): **Music Assistant,
    Roon and LMS**, matching `Capability::MultiroomSync` in
    `src/mcp/capabilities.rs` (issue #517 / PR #521). Calling join/unjoin
    on any other provider's zone raises a `HomeAssistantError`, as does
    passing `group_members` whose zone ids belong to a different
    provider than the target zone — UHC has no protocol that groups
    zones across providers, so cross-provider joins are refused
    client-side before any adapter call. `group_members` is populated
    from a periodic aggregate `hifi_zone_group` status poll
    (`UnifiedHifiControlCoordinator._refresh_group_members`), run
    alongside the fallback poll; a provider that cannot be reached for
    that poll is simply skipped rather than failing the whole refresh.
    Two provider-specific conventions are tolerated verbatim in
    `group_members`, per `src/mcp/tools/groups.rs`: Roon reports grouped
    members as `roon:<output_id>` (its own zone ids churn when outputs
    are grouped/ungrouped in Roon itself), and LMS sync groups are
    leaderless (the reported "leader" is UHC's stable-but-arbitrary pick
    of the first member, not an LMS fact).
  - Grouping itself is implemented over the `/mcp` JSON-RPC endpoint
    (`hifi_zone_group` tool) because grouping has no plain-REST route
    yet; browsing (`/api/collections`, `/api/play_ref`) and everything
    else in this integration is plain REST.
  - Browsing (`MediaPlayerEntityFeature.BROWSE_MEDIA` /
    `PLAY_MEDIA` / `MEDIA_ENQUEUE`, wired to `async_browse_media`/
    `async_play_media`): gated per-zone on the server-reported
    `browse_supported` flag (`/knob/zones`, PR #533's
    `zone_supports_hifi_collections`) rather than a hardcoded provider
    set, so it tracks whichever providers `hifi_collections` implements
    (Music Assistant, Roon, LMS as of #531) without a client-side
    update. `async_browse_media` presents three root entries mirroring
    `hifi_collections`' actions (Browse Library, Playlists, Favorites);
    navigating into a subfolder always re-enters with `action=browse`
    and the previous page's opaque `path` token, since a browse
    continuation is provider-generic regardless of which entry point
    minted it (see `src/mcp/tools/collections.rs`'s path-resolution
    comments). `async_play_media` only accepts a `media_id` that
    `async_browse_media` itself returned (an opaque `ref:` token) and
    forwards it to `/api/play_ref`; an `enqueue` value HA passes maps to
    `hifi_play_ref`'s `queue`/`next`/`play` actions where a distinct verb
    exists, and to `queue` otherwise (UHC has no separate "replace the
    queue" verb). **Artwork** (#549): an item that has provider art
    carries an `image` field, a same-origin
    `/api/collections/image?ref=...` path over an opaque token UHC mints
    server-side — never a raw provider image key/URL. `async_browse_media`
    resolves it against the server (`UnifiedHifiControlApiClient
    .resolve_url`, the same pattern `image_url` already uses for
    now-playing artwork) and sets it as `BrowseMedia.thumbnail`; a row
    with no provider art simply carries no `image` field, and
    `thumbnail` is left unset rather than guessing at one.
- **Mute caveat**: UHC's `/knob/now_playing` response does not include
  mute state, and not every provider has confirmed `mute`/`unmute`
  wiring through `/knob/control` end-to-end (only HQPlayer's handler was
  found to set `Command::Mute` explicitly during research for this
  integration). `media_player.is_volume_muted` is therefore `None`
  (unknown) until a `VolumeChanged` SSE event with `is_muted` has been
  observed for that zone, and `async_mute_volume` is a best-effort call
  that surfaces a `HomeAssistantError` if UHC rejects it rather than
  failing silently.
- **Zone id stability**: unique_id is UHC's provider-prefixed zone id
  (`roon:<id>`, `lms:<mac>`, `hqplayer:<instance>`,
  `musicassistant:<id>`, etc. — see `PrefixedZoneId` in
  `src/bus/events.rs`). These are stable for the lifetime of a zone on
  the UHC server, with one known exception: Roon's own zone_id can
  change when a user regroups outputs inside Roon itself, since Roon's
  zone model — not UHC — owns that identity. That churn is inherent to
  Roon and out of this integration's control; it shows up in Home
  Assistant as the old entity going unavailable and a new one appearing
  after a Roon-side regroup.
- **Auth**: this integration talks to UHC assuming the default,
  unauthenticated-on-LAN posture. If a server is run with
  `UHC_REQUIRE_CONTROLLER_AUTH=1` (see `docs/controller-auth.md`), most
  `/knob/*` and provider routes become protected by a cookie+CSRF
  session this integration does not implement; the config flow surfaces
  a clear "auth_required" error in that case rather than silently
  failing. Front such a server with a reverse proxy that supplies the
  session, or leave the flag unset for the interface this integration
  is pointed at.

## Delivery: the add-on installs this (#613)

The image this integration ships in is the one the Home Assistant add-on
runs. `Dockerfile.release` (and `Dockerfile`/`Dockerfile.ci`, kept
identical) copy `custom_components/unified_hifi_control` to
`/app/custom_components/unified_hifi_control`, minus `tests/` — Home
Assistant loads that directory as a Python package and the test suite is
not part of it.

Baking it into the base image rather than having the add-on's own
Dockerfile fetch it from GitHub is deliberate: the add-on pins a UHC image
tag, so the integration it installs is exactly the one built alongside
that UHC release. A build-time fetch would introduce a second version to
keep in sync, and a ref that can drift from the server it talks to.

The add-on's `run.sh` copies it into `/homeassistant/custom_components` on
start (install when absent, update when the bundled `manifest.json`
`version` is newer, never over a newer copy, a copy the add-on did not
install, or one the user has edited), then exports what it did as
`UHC_HA_INTEGRATION_STATUS`/`_VERSION`/`_DETAIL` plus `UHC_ADDON=1`.
`src/api/ha_integration.rs` reads those and asks Home Assistant's core API
whether our domain is in its `components` list, which is how Settings can
say "restart Home Assistant once" — the one step that is otherwise
invisible, since HA only loads custom integrations at startup.

Bumping `manifest.json`'s `version` is what makes an upgrade land on an
existing add-on install. Without a bump, `run.sh` sees equal versions and
does nothing.

HACS installs are unaffected: a copy the add-on did not place carries no
`.installed_by_uhc_addon` stamp, and the add-on leaves it alone.

## Choosing MQTT vs. this integration

- **Under the add-on, this integration is the default and MQTT is off**
  (#613). The add-on installs this for you and fills in the Supervisor's
  broker details without switching publishing on, so MQTT is a one-click
  opt-in for people who want it rather than something that happens to
  them.
- Use the MQTT path (PR #518 / issue #508) for lightweight dashboards
  and automations without installing a custom component, or when HA and
  UHC can't reach each other over plain HTTP but already share an MQTT
  broker. For a **standalone** (non-add-on) UHC this remains the primary
  route and its behaviour is unchanged.
- Use this integration for real `media_player` entities: Assist voice
  control, media-player dashboard cards, and `media_player.*` service
  calls (play/pause/volume/seek/grouping) — none of which HA's MQTT
  integration can provide, since it has no `media_player` platform.
  Both can be installed side by side; they don't conflict.

## Testing

`custom_components/unified_hifi_control/tests/` uses
`pytest-homeassistant-custom-component`. Run locally with:

```
pip install -r custom_components/unified_hifi_control/requirements_test.txt
pip uninstall -y aiodns pycares  # see note below
pytest custom_components/unified_hifi_control/tests -v
```

The `aiodns`/`pycares` uninstall works around a false-positive thread-leak
failure: `homeassistant` depends on `aiodns`, whose `AsyncResolver` spins
up a background c-ares thread the first time any test constructs a real
aiohttp connector, and pytest-homeassistant-custom-component's per-test
thread-leak check then flags that thread against whichever test happens
to run first — even though every HTTP call in this suite goes through
`aioclient_mock` and never builds a real connector. Since the suite never
needs real DNS resolution, removing `pycares` avoids the false positive
without weakening the check for genuine leaks.

CI: `.github/workflows/ha-integration.yml` runs the test suite plus
`hacs/action` (HACS repository validation) and
`home-assistant/actions/hassfest` (manifest/schema validation) on pushes
and PRs to `v3` that touch the integration. Per this repo's stacked-PR
convention, feature-branch-based PRs (including the one that introduced
this integration) do not run CI — the workflow was verified locally and
becomes active once this lands on `v3`.

## Known follow-ups

- No plain-REST route exists for zone grouping itself (`hifi_zone_group`)
  — it's MCP-only today, unlike `/api/collections`/`/api/play_ref`
  (PR #516/#531), which already got plain-REST mirrors. A UHC-side
  plain-REST equivalent for grouping (mirroring `src/api/browse.rs`'s
  pattern) would let this integration drop its one remaining MCP
  JSON-RPC call.
- Queue management (`hifi_queue`: read/reorder/remove/transfer) is not
  exposed through this integration yet — only browse-and-play. A future
  version could surface it via `async_get_media_source`/a
  queue-specific service, once there's a concrete HA UX for it.
