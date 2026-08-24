"""The Unified Hifi Control integration."""
from __future__ import annotations

import logging

from homeassistant.config_entries import ConfigEntry
from homeassistant.const import Platform
from homeassistant.core import HomeAssistant
from homeassistant.helpers.aiohttp_client import async_get_clientsession

from .api import UnifiedHifiControlApiClient
from .const import CONF_BASE_URL, DOMAIN
from .coordinator import UnifiedHifiControlCoordinator

_LOGGER = logging.getLogger(__name__)

PLATFORMS: list[Platform] = [Platform.MEDIA_PLAYER]


async def async_setup_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    """Set up Unified Hifi Control from a config entry."""
    session = async_get_clientsession(hass)
    client = UnifiedHifiControlApiClient(session, entry.data[CONF_BASE_URL])
    coordinator = UnifiedHifiControlCoordinator(hass, client)

    await coordinator.async_config_entry_first_refresh()

    coordinator.start_event_listener()

    hass.data.setdefault(DOMAIN, {})[entry.entry_id] = coordinator

    entry.async_on_unload(coordinator.stop_event_listener)
    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)
    return True


async def async_unload_entry(hass: HomeAssistant, entry: ConfigEntry) -> bool:
    """Unload a config entry."""
    unload_ok = await hass.config_entries.async_unload_platforms(entry, PLATFORMS)
    if unload_ok:
        coordinator: UnifiedHifiControlCoordinator = hass.data[DOMAIN].pop(
            entry.entry_id
        )
        coordinator.stop_event_listener()
    return unload_ok
