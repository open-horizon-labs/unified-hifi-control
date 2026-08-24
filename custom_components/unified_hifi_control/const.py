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

# Poll interval used as a safety-net fallback in case the SSE stream to
# /events silently drops without the underlying connection erroring out.
# Real-time updates normally arrive via SSE well before this fires.
FALLBACK_POLL_INTERVAL = timedelta(seconds=60)

# How long to wait for the initial HTTP round-trip during config flow
# validation and coordinator refreshes.
REQUEST_TIMEOUT = 10

# Providers known to implement the multiroom join/leave verbs today.
# Roon and LMS grouping work landed on a sibling branch (issue #517 /
# PR #521) that had not merged into this integration's base branch at
# the time this was written -- see docs/home_assistant_integration.md.
GROUPING_CAPABLE_PROVIDERS: set[str] = {"musicassistant"}

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
