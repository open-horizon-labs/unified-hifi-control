"""Data coordination for the Unified Hifi Control integration.

State updates are push-based: a background task holds a connection to
UHC's ``/events`` Server-Sent-Events stream (backed by a real
``tokio::sync::broadcast`` bus on the server, see src/api/mod.rs and
src/bus/mod.rs) and applies incremental updates to zone state as
``NowPlayingChanged`` / ``VolumeChanged`` / ``SeekPositionChanged`` /
``ZoneUpdated`` / ``ZoneDiscovered`` / ``ZoneRemoved`` events arrive.

A slow periodic poll (``FALLBACK_POLL_INTERVAL``) runs alongside the SSE
listener purely as a safety net in case a connection drops silently
without erroring (some proxies/NAT setups swallow TCP resets on idle
long-lived connections). Under normal operation the poll is a no-op
because SSE has already kept state current.
"""
from __future__ import annotations

import asyncio
import logging
from dataclasses import dataclass, field
from typing import Any

from homeassistant.core import HomeAssistant
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed

from .api import UnifiedHifiControlApiClient, UnifiedHifiControlApiError
from .const import DOMAIN, FALLBACK_POLL_INTERVAL, GROUPING_CAPABLE_PROVIDERS

_LOGGER = logging.getLogger(__name__)

# Reconnect backoff for the SSE listener.
_SSE_RECONNECT_MIN = 2
_SSE_RECONNECT_MAX = 60


@dataclass
class ZoneState:
    """In-memory state for a single UHC zone."""

    zone_id: str
    zone_name: str
    source: str
    state: str = "unknown"
    volume: float | None = None
    volume_min: float | None = None
    volume_max: float | None = None
    volume_step: float | None = None
    is_muted: bool | None = None
    title: str | None = None
    artist: str | None = None
    album: str | None = None
    image_key: str | None = None
    seek_position: int | None = None
    length: int | None = None
    is_play_allowed: bool = True
    is_pause_allowed: bool = True
    is_next_allowed: bool = True
    is_previous_allowed: bool = True
    # Server-reported: whether /api/collections (hifi_collections) implements
    # this zone's provider at all (see src/mcp/tools/collections.rs's
    # zone_supports_hifi_collections, surfaced on /knob/zones since #533).
    browse_supported: bool = False
    # This zone's multiroom group membership (this zone included), from the
    # most recent hifi_zone_group status poll. Empty when ungrouped or when
    # this zone's provider does not support grouping.
    group_members: list[str] = field(default_factory=list)
    raw: dict[str, Any] = field(default_factory=dict)


class UnifiedHifiControlCoordinator(DataUpdateCoordinator[dict[str, ZoneState]]):
    """Owns zone state for one UHC server and keeps it current."""

    def __init__(self, hass: HomeAssistant, client: UnifiedHifiControlApiClient) -> None:
        super().__init__(
            hass,
            _LOGGER,
            name=DOMAIN,
            update_interval=FALLBACK_POLL_INTERVAL,
        )
        self.client = client
        self._sse_task: asyncio.Task | None = None
        self.sse_connected = False

    async def _async_update_data(self) -> dict[str, ZoneState]:
        """Full poll: fetch zones + now-playing for each. Fallback path."""
        try:
            zones = await self.client.async_get_zones()
        except UnifiedHifiControlApiError as err:
            raise UpdateFailed(f"Could not reach UHC server: {err}") from err

        result: dict[str, ZoneState] = {}
        for zone in zones:
            zone_id = zone.get("zone_id")
            if not zone_id:
                continue
            existing = self.data.get(zone_id) if self.data else None
            zs = ZoneState(
                zone_id=zone_id,
                zone_name=zone.get("zone_name", zone_id),
                source=zone.get("source", "unknown"),
            )
            if existing is not None:
                # Preserve push-derived fields (mute, now-playing) that
                # aren't present on the lightweight zones list.
                zs.volume = existing.volume
                zs.volume_min = existing.volume_min
                zs.volume_max = existing.volume_max
                zs.volume_step = existing.volume_step
                zs.is_muted = existing.is_muted
                zs.title = existing.title
                zs.artist = existing.artist
                zs.album = existing.album
                zs.image_key = existing.image_key
                zs.seek_position = existing.seek_position
                zs.length = existing.length
            zs.state = zone.get("state", zs.state)
            vc = zone.get("volume_control") or {}
            if vc:
                zs.volume = vc.get("value", zs.volume)
                zs.volume_min = vc.get("min", zs.volume_min)
                zs.volume_max = vc.get("max", zs.volume_max)
                zs.volume_step = vc.get("step", zs.volume_step)
            zs.raw = zone
            try:
                now_playing = await self.client.async_get_now_playing(zone_id)
            except UnifiedHifiControlApiError as err:
                _LOGGER.debug("now_playing failed for %s: %s", zone_id, err)
                now_playing = {}
            if now_playing:
                zs.title = now_playing.get("line1", zs.title)
                zs.artist = now_playing.get("line2", zs.artist)
                zs.album = now_playing.get("line3", zs.album)
                zs.image_key = now_playing.get("image_key", zs.image_key)
                zs.seek_position = now_playing.get("seek_position", zs.seek_position)
                zs.length = now_playing.get("length", zs.length)
                zs.is_play_allowed = now_playing.get("is_play_allowed", True)
                zs.is_pause_allowed = now_playing.get("is_pause_allowed", True)
                zs.is_next_allowed = now_playing.get("is_next_allowed", True)
                zs.is_previous_allowed = now_playing.get("is_previous_allowed", True)
                if now_playing.get("volume") is not None:
                    zs.volume = now_playing.get("volume")
                    zs.volume_min = now_playing.get("volume_min", zs.volume_min)
                    zs.volume_max = now_playing.get("volume_max", zs.volume_max)
                    zs.volume_step = now_playing.get("volume_step", zs.volume_step)
            zs.browse_supported = zone.get("browse_supported", False)
            result[zone_id] = zs
        await self._refresh_group_members(result)
        return result

    async def _refresh_group_members(self, zones: dict[str, ZoneState]) -> None:
        """Populate ``group_members`` for every grouping-capable zone.

        One aggregate ``hifi_zone_group`` status call covers every provider;
        a provider that cannot be reached is reported in the aggregate's
        ``errors`` rather than raising, so its zones are simply left
        ungrouped for this poll rather than failing the whole refresh.
        """
        if not any(zs.source in GROUPING_CAPABLE_PROVIDERS for zs in zones.values()):
            return
        try:
            status = await self.client.async_zone_group_status()
        except UnifiedHifiControlApiError as err:
            _LOGGER.debug("hifi_zone_group status failed, leaving groups as-is: %s", err)
            return
        for group in status.get("groups") or []:
            leader = group.get("leader_zone_id")
            members = group.get("member_zone_ids") or []
            if not isinstance(leader, str):
                continue
            all_ids = [leader, *[m for m in members if isinstance(m, str)]]
            for zone_id in all_ids:
                zs = zones.get(zone_id)
                if zs is not None:
                    zs.group_members = all_ids

    def start_event_listener(self) -> None:
        """Start the background SSE listener task."""
        if self._sse_task is None or self._sse_task.done():
            self._sse_task = self.hass.loop.create_task(self._event_listener_loop())

    def stop_event_listener(self) -> None:
        if self._sse_task is not None:
            self._sse_task.cancel()
            self._sse_task = None

    async def _event_listener_loop(self) -> None:
        backoff = _SSE_RECONNECT_MIN
        loop = self.hass.loop
        while True:
            started_at = loop.time()
            try:
                self.sse_connected = True
                await self.client.async_listen_events(self._handle_event)
            except asyncio.CancelledError:
                self.sse_connected = False
                raise
            except UnifiedHifiControlApiError as err:
                _LOGGER.debug("SSE connection error, will retry: %s", err)
            except Exception:  # keep the listener alive on unexpected errors
                _LOGGER.exception("Unexpected error in UHC event listener")
            self.sse_connected = False
            # A connection that stayed up for a while was healthy; reset
            # backoff so a brief blip doesn't leave us waiting a full
            # minute to reconnect next time.
            if loop.time() - started_at >= 5:
                backoff = _SSE_RECONNECT_MIN
            await asyncio.sleep(backoff)
            backoff = min(backoff * 2, _SSE_RECONNECT_MAX)

    async def _handle_event(self, event_type: str, payload: dict[str, Any]) -> None:
        data = dict(self.data) if self.data else {}
        handler = _EVENT_HANDLERS.get(event_type)
        if handler is None:
            return
        changed = handler(data, payload)
        if changed:
            self.async_set_updated_data(data)


def _zone_id_from(payload: dict[str, Any], key: str = "zone_id") -> str | None:
    value = payload.get(key)
    return value if isinstance(value, str) else None


def _h_now_playing_changed(data: dict[str, ZoneState], payload: dict[str, Any]) -> bool:
    zone_id = _zone_id_from(payload)
    zs = data.get(zone_id) if zone_id else None
    if zs is None:
        return False
    zs.title = payload.get("title", zs.title)
    zs.artist = payload.get("artist", zs.artist)
    zs.album = payload.get("album", zs.album)
    zs.image_key = payload.get("image_key", zs.image_key)
    return True


def _h_volume_changed(data: dict[str, ZoneState], payload: dict[str, Any]) -> bool:
    zone_id = _zone_id_from(payload, "output_id") or _zone_id_from(payload)
    zs = data.get(zone_id) if zone_id else None
    if zs is None:
        return False
    if "value" in payload:
        zs.volume = payload["value"]
    if "is_muted" in payload:
        zs.is_muted = payload["is_muted"]
    return True


def _h_seek_changed(data: dict[str, ZoneState], payload: dict[str, Any]) -> bool:
    zone_id = _zone_id_from(payload)
    zs = data.get(zone_id) if zone_id else None
    if zs is None:
        return False
    zs.seek_position = payload.get("position", zs.seek_position)
    return True


def _h_zone_updated(data: dict[str, ZoneState], payload: dict[str, Any]) -> bool:
    zone_id = _zone_id_from(payload)
    zs = data.get(zone_id) if zone_id else None
    if zs is None:
        return False
    zs.zone_name = payload.get("display_name", zs.zone_name)
    zs.state = payload.get("state", zs.state)
    return True


def _h_zone_removed(data: dict[str, ZoneState], payload: dict[str, Any]) -> bool:
    zone_id = _zone_id_from(payload)
    if zone_id and zone_id in data:
        del data[zone_id]
        return True
    return False


def _h_zone_discovered(data: dict[str, ZoneState], payload: dict[str, Any]) -> bool:
    zone = payload.get("zone") if isinstance(payload.get("zone"), dict) else payload
    zone_id = _zone_id_from(zone)
    if not zone_id or zone_id in data:
        return False
    data[zone_id] = ZoneState(
        zone_id=zone_id,
        zone_name=zone.get("zone_name", zone_id),
        source=zone.get("source", "unknown"),
        state=zone.get("state", "unknown"),
        browse_supported=zone.get("browse_supported", False),
    )
    return True


_EVENT_HANDLERS = {
    "NowPlayingChanged": _h_now_playing_changed,
    "VolumeChanged": _h_volume_changed,
    "SeekPositionChanged": _h_seek_changed,
    "ZoneUpdated": _h_zone_updated,
    "ZoneRemoved": _h_zone_removed,
    "ZoneDiscovered": _h_zone_discovered,
}
