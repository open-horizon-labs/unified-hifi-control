"""Constants for the Unified Hifi Control integration."""
from __future__ import annotations

from datetime import timedelta

DOMAIN = "unified_hifi_control"

CONF_BASE_URL = "base_url"

DEFAULT_PORT = 8088
DEFAULT_NAME = "Unified Hifi Control"

# Zeroconf service type advertised by the UHC server (src/mdns.rs).
ZEROCONF_SERVICE_TYPE = "_uhc._tcp.local."

# UHC HTTP API paths (see src/knobs/routes.rs).
PATH_ZONES = "/knob/zones"
PATH_NOW_PLAYING = "/knob/now_playing"
PATH_NOW_PLAYING_IMAGE = "/knob/now_playing/image"
PATH_CONTROL = "/knob/control"
PATH_EVENTS = "/events"
PATH_MCP = "/mcp"
# Plain-REST mirrors of the hifi_collections/hifi_play_ref MCP tools
# (src/api/browse.rs, PR #516/#531). Unlike grouping, these forward the same
# envelope shape a client would get from /mcp without paying for the
# JSON-RPC transport.
PATH_COLLECTIONS = "/api/collections"
PATH_PLAY_REF = "/api/play_ref"

# Poll interval used as a safety-net fallback in case the SSE stream to
# /events silently drops without the underlying connection erroring out.
# Real-time updates normally arrive via SSE well before this fires.
FALLBACK_POLL_INTERVAL = timedelta(seconds=60)

# How long to wait for the initial HTTP round-trip during config flow
# validation and coordinator refreshes.
REQUEST_TIMEOUT = 10

# Providers that implement the multiroom join/leave verbs (issue #517 /
# PR #521, plus Music Assistant's original wiring): `hifi_zone_group`
# routes join/leave/status to whichever of the three owns the zone prefix.
# Roon reports grouped members as roon:<output_id> (its own zone ids churn
# on merge); LMS sync groups are leaderless. Both conventions are tolerated
# verbatim in group_members rather than normalized -- see
# src/mcp/tools/groups.rs's module docs.
GROUPING_CAPABLE_PROVIDERS: set[str] = {"musicassistant", "roon", "lms"}

# Top-level hifi_collections/`/api/collections` entry points, in menu order.
# (content_id, title) pairs surfaced at the root of async_browse_media.
BROWSE_ROOT_ID = "root"
BROWSE_TOP_LEVEL: tuple[tuple[str, str], ...] = (
    ("top:browse", "Browse Library"),
    ("top:playlists", "Playlists"),
    ("top:favorites", "Favorites"),
)

# Providers whose adapters explicitly refuse next/previous (see
# src/adapters/upnp.rs::REFUSED_TRANSPORT_ACTIONS).
NO_SKIP_PROVIDERS: set[str] = {"upnp"}

# Providers where transport/volume control is not yet reliably wired
# end-to-end (see AGENTS.md capability matrix, e.g. AppleMusic #465).
NO_TRANSPORT_PROVIDERS: set[str] = {"applemusic"}

STATE_MAP = {
    "playing": "playing",
    "paused": "paused",
    "stopped": "idle",
    "loading": "buffering",
    "buffering": "buffering",
}

MANUFACTURER = "Unified Hifi Control"
