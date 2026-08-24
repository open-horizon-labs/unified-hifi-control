"""Media player platform for Unified Hifi Control zones."""
from __future__ import annotations

import logging
from typing import Any

from homeassistant.components.media_player import (
    MediaPlayerEntity,
    MediaPlayerEntityFeature,
    MediaPlayerState,
)
from homeassistant.config_entries import ConfigEntry
from homeassistant.core import HomeAssistant
from homeassistant.exceptions import HomeAssistantError
from homeassistant.helpers.device_registry import DeviceInfo
from homeassistant.helpers.entity_platform import AddEntitiesCallback
from homeassistant.helpers.update_coordinator import CoordinatorEntity

from .api import UnifiedHifiControlApiError
from .const import (
    DOMAIN,
    GROUPING_CAPABLE_PROVIDERS,
    MANUFACTURER,
    NO_SKIP_PROVIDERS,
    NO_TRANSPORT_PROVIDERS,
    STATE_MAP,
)
from .coordinator import UnifiedHifiControlCoordinator, ZoneState

_LOGGER = logging.getLogger(__name__)


async def async_setup_entry(
    hass: HomeAssistant,
    entry: ConfigEntry,
    async_add_entities: AddEntitiesCallback,
) -> None:
    """Set up media_player entities for each known UHC zone."""
    coordinator: UnifiedHifiControlCoordinator = hass.data[DOMAIN][entry.entry_id]
    known_zone_ids: set[str] = set()

    def _add_new_entities() -> None:
        new_entities = []
        for zone_id in coordinator.data:
            if zone_id in known_zone_ids:
                continue
            known_zone_ids.add(zone_id)
            new_entities.append(UnifiedHifiControlMediaPlayer(coordinator, zone_id))
        if new_entities:
            async_add_entities(new_entities)

    _add_new_entities()
    entry.async_on_unload(coordinator.async_add_listener(_add_new_entities))


def _supported_features(zone: ZoneState) -> MediaPlayerEntityFeature:
    if zone.source in NO_TRANSPORT_PROVIDERS:
        return MediaPlayerEntityFeature(0)

    features = (
        MediaPlayerEntityFeature.PLAY
        | MediaPlayerEntityFeature.PAUSE
        | MediaPlayerEntityFeature.STOP
        | MediaPlayerEntityFeature.VOLUME_SET
        | MediaPlayerEntityFeature.VOLUME_STEP
        | MediaPlayerEntityFeature.VOLUME_MUTE
        | MediaPlayerEntityFeature.SEEK
    )
    if zone.source not in NO_SKIP_PROVIDERS:
        features |= (
            MediaPlayerEntityFeature.NEXT_TRACK
            | MediaPlayerEntityFeature.PREVIOUS_TRACK
        )
    if zone.source in GROUPING_CAPABLE_PROVIDERS:
        features |= (
            MediaPlayerEntityFeature.GROUPING
        )
    return features


class UnifiedHifiControlMediaPlayer(
    CoordinatorEntity[UnifiedHifiControlCoordinator], MediaPlayerEntity
):
    """A media_player entity backed by a single UHC zone."""

    _attr_has_entity_name = True
    _attr_name = None

    def __init__(
        self, coordinator: UnifiedHifiControlCoordinator, zone_id: str
    ) -> None:
        super().__init__(coordinator)
        self._zone_id = zone_id
        # UHC's zone ids are provider-prefixed (e.g. "roon:<zone_id>",
        # "lms:<mac>") and stable for the lifetime of that zone on the
        # server. One documented exception: Roon's own zone_id can churn
        # when outputs are grouped/ungrouped in Roon itself (see PR
        # #515's discussion of the roon:<output_id> convention) -- that
        # churn is inherent to Roon's zone model and outside UHC/this
        # integration's control.
        self._attr_unique_id = zone_id
        zone = coordinator.data.get(zone_id)
        self._attr_device_info = DeviceInfo(
            identifiers={(DOMAIN, zone_id)},
            name=zone.zone_name if zone else zone_id,
            manufacturer=MANUFACTURER,
            model=zone.source if zone else None,
        )
        self._attr_supported_features = (
            _supported_features(zone) if zone else MediaPlayerEntityFeature(0)
        )

    @property
    def _zone(self) -> ZoneState | None:
        return self.coordinator.data.get(self._zone_id)

    @property
    def available(self) -> bool:
        return super().available and self._zone is not None

    @property
    def name(self) -> str | None:
        zone = self._zone
        return zone.zone_name if zone else self._zone_id

    @property
    def state(self) -> MediaPlayerState | None:
        zone = self._zone
        if zone is None:
            return None
        return MediaPlayerState(STATE_MAP.get(zone.state, "idle"))

    @property
    def media_title(self) -> str | None:
        zone = self._zone
        return zone.title if zone else None

    @property
    def media_artist(self) -> str | None:
        zone = self._zone
        return zone.artist if zone else None

    @property
    def media_album_name(self) -> str | None:
        zone = self._zone
        return zone.album if zone else None

    @property
    def media_duration(self) -> int | None:
        zone = self._zone
        return zone.length if zone else None

    @property
    def media_position(self) -> int | None:
        zone = self._zone
        return zone.seek_position if zone else None

    @property
    def media_image_url(self) -> str | None:
        zone = self._zone
        if zone is None or not zone.image_key:
            return None
        return self.coordinator.client.image_url(self._zone_id)

    @property
    def media_image_hash(self) -> str | None:
        zone = self._zone
        return zone.image_key if zone else None

    @property
    def volume_level(self) -> float | None:
        zone = self._zone
        if zone is None or zone.volume is None:
            return None
        vmin = zone.volume_min if zone.volume_min is not None else 0.0
        vmax = zone.volume_max if zone.volume_max is not None else 100.0
        if vmax <= vmin:
            return None
        return max(0.0, min(1.0, (zone.volume - vmin) / (vmax - vmin)))

    @property
    def is_volume_muted(self) -> bool | None:
        zone = self._zone
        # UHC's now_playing payload does not report mute state; this is
        # only known once at least one VolumeChanged SSE event with
        # is_muted has been observed for this zone.
        return zone.is_muted if zone else None

    async def async_media_play(self) -> None:
        await self._async_control("play")

    async def async_media_pause(self) -> None:
        await self._async_control("pause")

    async def async_media_stop(self) -> None:
        await self._async_control("stop")

    async def async_media_next_track(self) -> None:
        await self._async_control("next")

    async def async_media_previous_track(self) -> None:
        await self._async_control("previous")

    async def async_media_seek(self, position: float) -> None:
        await self._async_control("seek", value=position)

    async def async_set_volume_level(self, volume: float) -> None:
        zone = self._zone
        if zone is None:
            raise HomeAssistantError(f"Zone {self._zone_id} is unavailable")
        vmin = zone.volume_min if zone.volume_min is not None else 0.0
        vmax = zone.volume_max if zone.volume_max is not None else 100.0
        absolute = vmin + volume * (vmax - vmin)
        await self._async_control("vol_abs", value=absolute)

    async def async_volume_up(self) -> None:
        await self._async_control("vol_up")

    async def async_volume_down(self) -> None:
        await self._async_control("vol_down")

    async def async_mute_volume(self, mute: bool) -> None:
        # Best-effort: not every provider has confirmed mute/unmute
        # wiring through /knob/control (see docs/home_assistant_integration.md).
        await self._async_control("mute", value=mute)

    async def _async_control(self, action: str, value: Any | None = None) -> None:
        try:
            await self.coordinator.client.async_control(
                self._zone_id, action, value
            )
        except UnifiedHifiControlApiError as err:
            raise HomeAssistantError(
                f"Failed to send '{action}' to zone {self._zone_id}: {err}"
            ) from err
        await self.coordinator.async_request_refresh()

    async def async_join_players(self, group_members: list[str]) -> None:
        zone = self._zone
        if zone is None or zone.source not in GROUPING_CAPABLE_PROVIDERS:
            source = zone.source if zone else "unknown"
            raise HomeAssistantError(
                f"Grouping is not supported for provider '{source}' yet "
                "(tracked in unified-hifi-control issue #517/#521)."
            )
        try:
            await self.coordinator.client.async_zone_group(
                "join", self._zone_id, member_zone_ids=group_members
            )
        except UnifiedHifiControlApiError as err:
            raise HomeAssistantError(f"Failed to join players: {err}") from err
        await self.coordinator.async_request_refresh()

    async def async_unjoin_player(self) -> None:
        zone = self._zone
        if zone is None or zone.source not in GROUPING_CAPABLE_PROVIDERS:
            source = zone.source if zone else "unknown"
            raise HomeAssistantError(
                f"Grouping is not supported for provider '{source}' yet "
                "(tracked in unified-hifi-control issue #517/#521)."
            )
        try:
            await self.coordinator.client.async_zone_group(
                "leave", self._zone_id, member_zone_ids=[self._zone_id]
            )
        except UnifiedHifiControlApiError as err:
            raise HomeAssistantError(f"Failed to unjoin player: {err}") from err
        await self.coordinator.async_request_refresh()
