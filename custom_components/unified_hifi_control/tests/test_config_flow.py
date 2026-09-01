"""Tests for the config flow."""
from __future__ import annotations

import pytest
from homeassistant import config_entries
from homeassistant.core import HomeAssistant
from homeassistant.data_entry_flow import FlowResultType

from custom_components.unified_hifi_control.const import CONF_BASE_URL, DOMAIN


@pytest.mark.asyncio
async def test_user_flow_success(hass: HomeAssistant, aioclient_mock) -> None:
    aioclient_mock.get(
        "http://192.168.1.50:8088/knob/zones",
        json={"zones": [{"zone_id": "roon:1", "zone_name": "Kitchen"}]},
    )

    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": config_entries.SOURCE_USER}
    )
    assert result["type"] == FlowResultType.FORM

    result2 = await hass.config_entries.flow.async_configure(
        result["flow_id"], {CONF_BASE_URL: "192.168.1.50:8088"}
    )
    assert result2["type"] == FlowResultType.CREATE_ENTRY
    assert result2["data"][CONF_BASE_URL] == "http://192.168.1.50:8088"


@pytest.mark.asyncio
async def test_user_flow_cannot_connect(hass: HomeAssistant, aioclient_mock) -> None:
    import aiohttp

    aioclient_mock.get(
        "http://192.168.1.50:8088/knob/zones",
        exc=aiohttp.ClientConnectionError("boom"),
    )

    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": config_entries.SOURCE_USER}
    )
    result2 = await hass.config_entries.flow.async_configure(
        result["flow_id"], {CONF_BASE_URL: "http://192.168.1.50:8088"}
    )
    assert result2["type"] == FlowResultType.FORM
    assert result2["errors"] == {"base": "cannot_connect"}


@pytest.mark.asyncio
async def test_user_flow_auth_required(hass: HomeAssistant, aioclient_mock) -> None:
    aioclient_mock.get("http://192.168.1.50:8088/knob/zones", status=401)

    result = await hass.config_entries.flow.async_init(
        DOMAIN, context={"source": config_entries.SOURCE_USER}
    )
    result2 = await hass.config_entries.flow.async_configure(
        result["flow_id"], {CONF_BASE_URL: "http://192.168.1.50:8088"}
    )
    assert result2["type"] == FlowResultType.FORM
    assert result2["errors"] == {"base": "auth_required"}
