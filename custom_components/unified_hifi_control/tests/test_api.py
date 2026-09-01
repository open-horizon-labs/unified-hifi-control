"""Tests for the UHC API client, including the SSE frame parser.

Uses Home Assistant's ``aioclient_mock`` fixture (an aiohttp session
mock) rather than a real TestServer/socket, since the pytest-socket
plugin bundled with pytest-homeassistant-custom-component blocks real
network sockets by default.
"""
from __future__ import annotations

import pytest

from custom_components.unified_hifi_control.api import (
    UnifiedHifiControlApiClient,
    UnifiedHifiControlApiError,
    UnifiedHifiControlAuthError,
    UnifiedHifiControlConnectionError,
    _parse_sse,
)

BASE_URL = "http://uhc.local:8088"


async def _fake_content_lines(lines: list[bytes]):
    for line in lines:
        yield line


@pytest.mark.asyncio
async def test_parse_sse_single_event():
    lines = [
        b'data: {"type":"VolumeChanged","payload":{"output_id":"roon:1","value":42,"is_muted":false}}\n',
        b"\n",
    ]
    events = [e async for e in _parse_sse(_fake_content_lines(lines))]
    assert events == [
        ("VolumeChanged", {"output_id": "roon:1", "value": 42, "is_muted": False})
    ]


@pytest.mark.asyncio
async def test_parse_sse_ignores_comments_and_multiple_frames():
    lines = [
        b": ping\n",
        b'data: {"type":"NowPlayingChanged","payload":{"zone_id":"lms:aa","title":"Song"}}\n',
        b"\n",
        b'data: {"type":"SeekPositionChanged","payload":{"zone_id":"lms:aa","position":10}}\n',
        b"\n",
    ]
    events = [e async for e in _parse_sse(_fake_content_lines(lines))]
    assert events[0][0] == "NowPlayingChanged"
    assert events[1][0] == "SeekPositionChanged"


@pytest.mark.asyncio
async def test_parse_sse_skips_invalid_json():
    lines = [b"data: {not valid json\n", b"\n"]
    events = [e async for e in _parse_sse(_fake_content_lines(lines))]
    assert events == []


def _client(hass) -> UnifiedHifiControlApiClient:
    """Build a client using HA's managed, test-harness-owned session.

    Using homeassistant.helpers.aiohttp_client.async_get_clientsession
    (routed through aioclient_mock by the test harness) rather than a
    raw aiohttp.ClientSession avoids leaking background resolver
    threads that the harness's cleanup fixture would otherwise flag.
    """
    from homeassistant.helpers.aiohttp_client import async_get_clientsession

    return UnifiedHifiControlApiClient(async_get_clientsession(hass), BASE_URL)


@pytest.mark.asyncio
async def test_get_zones(hass, aioclient_mock):
    aioclient_mock.get(
        f"{BASE_URL}/knob/zones",
        json={"zones": [{"zone_id": "roon:1", "zone_name": "Kitchen"}]},
    )
    zones = await _client(hass).async_get_zones()
    assert zones == [{"zone_id": "roon:1", "zone_name": "Kitchen"}]


@pytest.mark.asyncio
async def test_get_zones_bad_shape_raises(hass, aioclient_mock):
    aioclient_mock.get(f"{BASE_URL}/knob/zones", json={"unexpected": True})
    with pytest.raises(UnifiedHifiControlApiError):
        await _client(hass).async_get_zones()


@pytest.mark.asyncio
async def test_control_posts_expected_body(hass, aioclient_mock):
    aioclient_mock.post(f"{BASE_URL}/knob/control", json={"ok": True})
    await _client(hass).async_control("roon:1", "vol_abs", value=50)
    assert aioclient_mock.mock_calls[0][2] == {
        "zone_id": "roon:1",
        "action": "vol_abs",
        "value": 50,
    }


@pytest.mark.asyncio
async def test_auth_error_raised_on_401(hass, aioclient_mock):
    aioclient_mock.get(f"{BASE_URL}/knob/zones", status=401, json={"error": "no"})
    with pytest.raises(UnifiedHifiControlAuthError):
        await _client(hass).async_get_zones()


@pytest.mark.asyncio
async def test_connection_error_wraps_client_exceptions(hass, aioclient_mock):
    import aiohttp

    aioclient_mock.get(
        f"{BASE_URL}/knob/zones",
        exc=aiohttp.ClientConnectionError("boom"),
    )
    with pytest.raises(UnifiedHifiControlConnectionError):
        await _client(hass).async_get_zones()


def test_image_url_builds_expected_path():
    class _FakeSession:
        pass

    api = UnifiedHifiControlApiClient(_FakeSession(), "http://uhc.local:8088/")
    assert (
        api.image_url("roon:1")
        == "http://uhc.local:8088/knob/now_playing/image?zone_id=roon:1"
    )


@pytest.mark.asyncio
async def test_browse_collections_posts_expected_body(hass, aioclient_mock):
    aioclient_mock.post(
        f"{BASE_URL}/api/collections",
        json={"outcome": "ok", "data": {"items": []}},
    )
    result = await _client(hass).async_browse_collections(
        "lms:aa", "browse", path="tok", limit=10, offset=5
    )
    assert result == {"outcome": "ok", "data": {"items": []}}
    assert aioclient_mock.mock_calls[0][2] == {
        "zone_id": "lms:aa",
        "action": "browse",
        "path": "tok",
        "limit": 10,
        "offset": 5,
    }


@pytest.mark.asyncio
async def test_browse_collections_omits_absent_optional_fields(hass, aioclient_mock):
    aioclient_mock.post(
        f"{BASE_URL}/api/collections", json={"outcome": "ok", "data": {}}
    )
    await _client(hass).async_browse_collections("lms:aa", "playlists")
    assert aioclient_mock.mock_calls[0][2] == {
        "zone_id": "lms:aa",
        "action": "playlists",
    }


@pytest.mark.asyncio
async def test_play_ref_posts_expected_body(hass, aioclient_mock):
    aioclient_mock.post(
        f"{BASE_URL}/api/play_ref", json={"outcome": "accepted"}
    )
    result = await _client(hass).async_play_ref("tok", "lms:aa", action="queue")
    assert result == {"outcome": "accepted"}
    assert aioclient_mock.mock_calls[0][2] == {
        "ref": "tok",
        "zone_id": "lms:aa",
        "action": "queue",
    }


@pytest.mark.asyncio
async def test_zone_group_status_scoped_to_one_provider(hass, aioclient_mock):
    aioclient_mock.post(
        f"{BASE_URL}/mcp",
        json={
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "structuredContent": {
                    "data": {
                        "groups": [
                            {
                                "leader_zone_id": "roon:1",
                                "member_zone_ids": ["roon:2"],
                            }
                        ]
                    }
                }
            },
        },
    )
    result = await _client(hass).async_zone_group_status("roon:1")
    assert result == {
        "groups": [{"leader_zone_id": "roon:1", "member_zone_ids": ["roon:2"]}]
    }
    body = aioclient_mock.mock_calls[0][2]
    assert body["params"]["arguments"] == {"action": "status", "zone_id": "roon:1"}


@pytest.mark.asyncio
async def test_zone_group_status_aggregate_omits_zone_id(hass, aioclient_mock):
    aioclient_mock.post(
        f"{BASE_URL}/mcp",
        json={
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"structuredContent": {"data": {"groups": [], "errors": []}}},
        },
    )
    result = await _client(hass).async_zone_group_status()
    assert result == {"groups": [], "errors": []}
    body = aioclient_mock.mock_calls[0][2]
    assert body["params"]["arguments"] == {"action": "status"}


@pytest.mark.asyncio
async def test_zone_group_status_raises_on_transport_error(hass, aioclient_mock):
    aioclient_mock.post(
        f"{BASE_URL}/mcp",
        json={"jsonrpc": "2.0", "id": 1, "error": {"message": "boom"}},
    )
    with pytest.raises(UnifiedHifiControlApiError):
        await _client(hass).async_zone_group_status()


@pytest.mark.asyncio
async def test_zone_group_status_raises_on_refusal(hass, aioclient_mock):
    aioclient_mock.post(
        f"{BASE_URL}/mcp",
        json={
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "structuredContent": {
                    "refusal": {"message": "zone_id must be from a provider..."}
                }
            },
        },
    )
    with pytest.raises(UnifiedHifiControlApiError):
        await _client(hass).async_zone_group_status("upnp:1")
