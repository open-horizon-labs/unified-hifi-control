"""Config flow for Unified Hifi Control."""
from __future__ import annotations

import logging
from typing import TYPE_CHECKING, Any

import voluptuous as vol
from homeassistant.config_entries import ConfigFlow, ConfigFlowResult
from homeassistant.helpers import config_validation as cv
from homeassistant.helpers.aiohttp_client import async_get_clientsession

if TYPE_CHECKING:
    # Only needed for type hints. The actual module path has moved
    # across Home Assistant versions (helpers.service_info.zeroconf in
    # 2025.x+, components.zeroconf before that, and the latter requires
    # the optional `zeroconf` PyPI package to even import). Since this
    # file uses `from __future__ import annotations`, annotations are
    # never evaluated at runtime, so we can avoid importing either at
    # import time and only pay the cost when a real zeroconf discovery
    # actually happens.
    from homeassistant.helpers.service_info.zeroconf import ZeroconfServiceInfo

from .api import (
    UnifiedHifiControlApiClient,
    UnifiedHifiControlApiError,
    UnifiedHifiControlAuthError,
    UnifiedHifiControlConnectionError,
)
from .const import CONF_BASE_URL, DEFAULT_NAME, DEFAULT_PORT, DOMAIN

_LOGGER = logging.getLogger(__name__)

STEP_USER_DATA_SCHEMA = vol.Schema({vol.Required(CONF_BASE_URL): cv.string})


def _normalize_base_url(value: str) -> str:
    value = value.strip().rstrip("/")
    if not value.startswith(("http://", "https://")):
        value = f"http://{value}"
    return value


class UnifiedHifiControlConfigFlow(ConfigFlow, domain=DOMAIN):
    """Handle a config flow for Unified Hifi Control."""

    VERSION = 1

    def __init__(self) -> None:
        self._discovered_base_url: str | None = None

    async def _async_validate(self, base_url: str) -> dict[str, str]:
        """Return a dict of errors (empty on success)."""
        session = async_get_clientsession(self.hass)
        client = UnifiedHifiControlApiClient(session, base_url)
        try:
            await client.async_ping()
        except UnifiedHifiControlAuthError:
            return {"base": "auth_required"}
        except UnifiedHifiControlConnectionError:
            return {"base": "cannot_connect"}
        except UnifiedHifiControlApiError:
            return {"base": "unknown"}
        return {}

    async def async_step_user(
        self, user_input: dict[str, Any] | None = None
    ) -> ConfigFlowResult:
        """Handle the initial step."""
        errors: dict[str, str] = {}
        if user_input is not None:
            base_url = _normalize_base_url(user_input[CONF_BASE_URL])
            errors = await self._async_validate(base_url)
            if not errors:
                await self.async_set_unique_id(base_url)
                self._abort_if_unique_id_configured()
                return self.async_create_entry(
                    title=DEFAULT_NAME, data={CONF_BASE_URL: base_url}
                )

        schema = STEP_USER_DATA_SCHEMA
        if self._discovered_base_url:
            schema = vol.Schema(
                {
                    vol.Required(
                        CONF_BASE_URL, default=self._discovered_base_url
                    ): cv.string
                }
            )
        return self.async_show_form(
            step_id="user", data_schema=schema, errors=errors
        )

    async def async_step_zeroconf(
        self, discovery_info: ZeroconfServiceInfo
    ) -> ConfigFlowResult:
        """Handle zeroconf discovery of a UHC server (_uhc._tcp.local.)."""
        base_url = discovery_info.properties.get("base")
        if not base_url:
            host = discovery_info.host
            port = discovery_info.port or DEFAULT_PORT
            base_url = f"http://{host}:{port}"
        base_url = _normalize_base_url(base_url)

        await self.async_set_unique_id(base_url)
        self._abort_if_unique_id_configured()

        errors = await self._async_validate(base_url)
        if errors:
            return self.async_abort(reason="cannot_connect")

        self._discovered_base_url = base_url
        self.context["title_placeholders"] = {"base_url": base_url}
        return await self.async_step_user()
