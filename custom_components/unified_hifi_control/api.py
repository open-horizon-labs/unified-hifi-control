"""Thin async HTTP client for the Unified Hifi Control (UHC) server.

Wraps the routes documented in ``src/knobs/routes.rs`` (unified,
cross-provider zone/transport surface) plus the MCP JSON-RPC endpoint for
functionality (zone grouping) that has no plain-REST route yet.

UHC's own docs (``docs/protocol.md``) are explicit that this HTTP API is
internal and may change without notice. This client is intentionally
narrow and defensive so a route-shape change fails loudly (an
``UnifiedHifiControlApiError``) rather than corrupting entity state.
"""
from __future__ import annotations

import asyncio
import logging
from collections.abc import AsyncIterator, Callable, Coroutine
from typing import Any

import aiohttp

from .const import (
    PATH_CONTROL,
    PATH_EVENTS,
    PATH_MCP,
    PATH_NOW_PLAYING,
    PATH_NOW_PLAYING_IMAGE,
    PATH_ZONES,
    REQUEST_TIMEOUT,
)

_LOGGER = logging.getLogger(__name__)


class UnifiedHifiControlApiError(Exception):
    """Base error for UHC API failures."""


class UnifiedHifiControlConnectionError(UnifiedHifiControlApiError):
    """Raised when the UHC server cannot be reached at all."""


class UnifiedHifiControlAuthError(UnifiedHifiControlApiError):
    """Raised when the UHC server rejects the request as unauthenticated.

    Only relevant when the server is run with
    ``UHC_REQUIRE_CONTROLLER_AUTH=1`` -- see docs/controller-auth.md. This
    integration does not implement the bootstrap-cookie handshake; users
    running with that flag set must front UHC with a reverse proxy that
    supplies the required cookie/CSRF pair, or disable the flag for the
    interface this integration talks to.
    """


class UnifiedHifiControlApiClient:
    """Client for a single UHC server instance."""

    def __init__(self, session: aiohttp.ClientSession, base_url: str) -> None:
        self._session = session
        self._base_url = base_url.rstrip("/")

    @property
    def base_url(self) -> str:
        return self._base_url

    async def async_get_zones(self) -> list[dict[str, Any]]:
        """Return the unified zone list from GET /knob/zones."""
        data = await self._request("GET", PATH_ZONES)
        zones = data.get("zones")
        if not isinstance(zones, list):
            raise UnifiedHifiControlApiError(
                f"Unexpected /knob/zones response shape: {data!r}"
            )
        return zones

    async def async_get_now_playing(self, zone_id: str) -> dict[str, Any]:
        """Return now-playing state for a single zone."""
        return await self._request(
            "GET", PATH_NOW_PLAYING, params={"zone_id": zone_id}
        )

    async def async_control(
        self, zone_id: str, action: str, value: Any | None = None
    ) -> None:
        """POST a transport/volume action to /knob/control."""
        body: dict[str, Any] = {"zone_id": zone_id, "action": action}
        if value is not None:
            body["value"] = value
        await self._request("POST", PATH_CONTROL, json=body)

    def image_url(self, zone_id: str) -> str:
        """Build the direct URL for a zone's now-playing artwork."""
        return f"{self._base_url}{PATH_NOW_PLAYING_IMAGE}?zone_id={zone_id}"

    async def async_zone_group(
        self,
        action: str,
        leader_zone_id: str,
        member_zone_ids: list[str] | None = None,
    ) -> None:
        """Call the ``hifi_zone_group`` MCP tool (join/leave).

        Grouping has no plain-REST route as of this writing; it is only
        reachable through the MCP JSON-RPC transport at /mcp. See
        docs/home_assistant_integration.md for the tracking note.
        """
        params: dict[str, Any] = {
            "name": "hifi_zone_group",
            "arguments": {
                "action": action,
                "leader_zone_id": leader_zone_id,
                "confirm": True,
            },
        }
        if member_zone_ids is not None:
            params["arguments"]["member_zone_ids"] = member_zone_ids
        payload = {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": params,
        }
        result = await self._request("POST", PATH_MCP, json=payload)
        if isinstance(result, dict) and result.get("error"):
            raise UnifiedHifiControlApiError(
                f"hifi_zone_group failed: {result['error']!r}"
            )

    async def async_ping(self) -> None:
        """Raise if the server is unreachable. Used by the config flow."""
        await self.async_get_zones()

    async def _request(
        self,
        method: str,
        path: str,
        *,
        params: dict[str, Any] | None = None,
        json: dict[str, Any] | None = None,
    ) -> dict[str, Any]:
        url = f"{self._base_url}{path}"
        try:
            async with asyncio.timeout(REQUEST_TIMEOUT):
                async with self._session.request(
                    method, url, params=params, json=json
                ) as resp:
                    if resp.status == 401 or resp.status == 403:
                        raise UnifiedHifiControlAuthError(
                            f"{method} {path} -> {resp.status}"
                        )
                    if resp.status >= 400:
                        text = await resp.text()
                        raise UnifiedHifiControlApiError(
                            f"{method} {path} -> {resp.status}: {text}"
                        )
                    return await resp.json(content_type=None)
        except UnifiedHifiControlApiError:
            raise
        except TimeoutError as err:
            raise UnifiedHifiControlConnectionError(
                f"Timed out calling {method} {path}"
            ) from err
        except aiohttp.ClientError as err:
            raise UnifiedHifiControlConnectionError(
                f"Error calling {method} {path}: {err}"
            ) from err

    async def async_listen_events(
        self,
        on_event: Callable[[str, dict[str, Any]], Coroutine[Any, Any, None]],
    ) -> None:
        """Hold a connection to GET /events and dispatch SSE frames.

        Runs until cancelled. Callers are expected to wrap this in a
        reconnect loop (see coordinator.py) since any single connection
        may drop.
        """
        url = f"{self._base_url}{PATH_EVENTS}"
        async with self._session.get(
            url, headers={"Accept": "text/event-stream"}
        ) as resp:
            if resp.status >= 400:
                raise UnifiedHifiControlApiError(f"GET /events -> {resp.status}")
            async for event_type, data in _parse_sse(resp.content):
                await on_event(event_type, data)


async def _parse_sse(
    content: aiohttp.StreamReader,
) -> AsyncIterator[tuple[str, dict[str, Any]]]:
    """Minimal Server-Sent-Events parser.

    UHC's /events handler (src/api/mod.rs::events_handler) emits plain
    ``data: <json>`` frames (one JSON object per BusEvent, tagged with a
    "type" field) separated by blank lines, plus periodic ``: ping``
    keep-alive comments. We only care about ``data:`` lines.
    """
    import json as json_module

    buffer: list[str] = []
    async for raw_line in content:
        line = raw_line.decode("utf-8", errors="replace").rstrip("\n").rstrip("\r")
        if line == "":
            if buffer:
                payload = "\n".join(buffer)
                buffer = []
                try:
                    parsed = json_module.loads(payload)
                except ValueError:
                    _LOGGER.debug("Ignoring non-JSON SSE frame: %s", payload)
                    continue
                event_type = parsed.get("type", "")
                data = parsed.get("payload", parsed)
                yield event_type, data
            continue
        if line.startswith(":"):
            continue  # comment / keep-alive
        if line.startswith("data:"):
            buffer.append(line[len("data:") :].lstrip())
