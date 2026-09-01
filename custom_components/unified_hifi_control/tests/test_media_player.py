"""Tests for capability mapping in media_player.py."""
from __future__ import annotations

from unittest.mock import AsyncMock, MagicMock

import pytest
from homeassistant.components.media_player import (
    BrowseError,
    MediaPlayerEnqueue,
    MediaPlayerEntityFeature,
)
from homeassistant.exceptions import HomeAssistantError

from custom_components.unified_hifi_control.coordinator import ZoneState
from custom_components.unified_hifi_control.media_player import (
    UnifiedHifiControlMediaPlayer,
    _supported_features,
)


def test_roon_zone_gets_full_transport_and_skip():
    zone = ZoneState(zone_id="roon:1", zone_name="Kitchen", source="roon")
    features = _supported_features(zone)
    assert features & MediaPlayerEntityFeature.NEXT_TRACK
    assert features & MediaPlayerEntityFeature.PREVIOUS_TRACK
    assert features & MediaPlayerEntityFeature.VOLUME_SET
    # Roon grouping landed with issue #517 / PR #521.
    assert features & MediaPlayerEntityFeature.GROUPING


def test_upnp_zone_has_no_skip_per_adapter_refusal():
    zone = ZoneState(zone_id="upnp:1", zone_name="Office", source="upnp")
    features = _supported_features(zone)
    assert not (features & MediaPlayerEntityFeature.NEXT_TRACK)
    assert not (features & MediaPlayerEntityFeature.PREVIOUS_TRACK)
    # Transport/volume are still supported for UPnP.
    assert features & MediaPlayerEntityFeature.PLAY
    assert features & MediaPlayerEntityFeature.VOLUME_SET


def test_musicassistant_zone_gets_grouping():
    zone = ZoneState(zone_id="musicassistant:1", zone_name="Living Room", source="musicassistant")
    features = _supported_features(zone)
    assert features & MediaPlayerEntityFeature.GROUPING


def test_applemusic_zone_has_no_transport_features_yet():
    zone = ZoneState(zone_id="applemusic:1", zone_name="Phone", source="applemusic")
    features = _supported_features(zone)
    assert features == MediaPlayerEntityFeature(0)


def test_lms_zone_gets_grouping():
    zone = ZoneState(zone_id="lms:aa", zone_name="Bedroom", source="lms")
    features = _supported_features(zone)
    # LMS sync-group grouping landed with issue #517 / PR #521.
    assert features & MediaPlayerEntityFeature.GROUPING
    assert features & MediaPlayerEntityFeature.NEXT_TRACK


def test_browse_supported_zone_gets_browse_and_play_media():
    zone = ZoneState(
        zone_id="musicassistant:1",
        zone_name="Living Room",
        source="musicassistant",
        browse_supported=True,
    )
    features = _supported_features(zone)
    assert features & MediaPlayerEntityFeature.BROWSE_MEDIA
    assert features & MediaPlayerEntityFeature.PLAY_MEDIA
    assert features & MediaPlayerEntityFeature.MEDIA_ENQUEUE


def test_browse_unsupported_zone_has_no_browse_features():
    zone = ZoneState(
        zone_id="applemusic:1",
        zone_name="Phone",
        source="applemusic",
        browse_supported=False,
    )
    features = _supported_features(zone)
    assert not (features & MediaPlayerEntityFeature.BROWSE_MEDIA)
    assert not (features & MediaPlayerEntityFeature.PLAY_MEDIA)


class _FakeCoordinator:
    """Minimal coordinator double: just what CoordinatorEntity/media_player need."""

    def __init__(self, zones: dict[str, ZoneState]) -> None:
        self.data = zones
        self.client = AsyncMock()
        self.async_request_refresh = AsyncMock()


def _player(zone: ZoneState) -> UnifiedHifiControlMediaPlayer:
    coordinator = _FakeCoordinator({zone.zone_id: zone})
    return UnifiedHifiControlMediaPlayer(coordinator, zone.zone_id)


def _ok_envelope(data: dict) -> dict:
    return {"outcome": "ok", "data": data}


def _accepted_envelope() -> dict:
    return {"outcome": "accepted"}


def _refused_envelope(message: str) -> dict:
    return {"outcome": "unsupported", "refusal": {"message": message}}


@pytest.mark.asyncio
async def test_browse_media_root_lists_three_entry_points():
    zone = ZoneState(
        zone_id="lms:aa", zone_name="Bedroom", source="lms", browse_supported=True
    )
    player = _player(zone)
    result = await player.async_browse_media()
    assert result.can_expand
    ids = [child.media_content_id for child in result.children]
    assert ids == ["top:browse", "top:playlists", "top:favorites"]


@pytest.mark.asyncio
async def test_browse_media_raises_for_zone_without_browse_support():
    zone = ZoneState(
        zone_id="applemusic:1", zone_name="Phone", source="applemusic", browse_supported=False
    )
    player = _player(zone)
    with pytest.raises(BrowseError):
        await player.async_browse_media()


@pytest.mark.asyncio
async def test_browse_media_top_level_walks_into_collections_browse():
    zone = ZoneState(
        zone_id="lms:aa", zone_name="Bedroom", source="lms", browse_supported=True
    )
    player = _player(zone)
    player.coordinator.client.async_browse_collections = AsyncMock(
        return_value=_ok_envelope(
            {
                "items": [
                    {"title": "Albums", "path": "browse-token-albums"},
                    {"title": "Some Track", "ref": "play-token-1"},
                ]
            }
        )
    )
    result = await player.async_browse_media(media_content_id="top:browse")
    player.coordinator.client.async_browse_collections.assert_awaited_once_with(
        "lms:aa", "browse", path=None
    )
    assert [c.media_content_id for c in result.children] == [
        "path:browse-token-albums",
        "ref:play-token-1",
    ]
    assert result.children[0].can_expand and not result.children[0].can_play
    assert result.children[1].can_play and not result.children[1].can_expand


@pytest.mark.asyncio
async def test_browse_media_thumbnail_resolves_an_items_image_field():
    """#549: an item's `image` (a same-origin `/api/collections/image?ref=...`
    path) becomes `BrowseMedia.thumbnail`, resolved against this server the
    same way `UnifiedHifiControlApiClient.image_url` already resolves
    now-playing artwork -- a row with no `image` field gets no thumbnail,
    honestly, rather than a guessed-at one.
    """
    zone = ZoneState(
        zone_id="lms:aa", zone_name="Bedroom", source="lms", browse_supported=True
    )
    player = _player(zone)
    player.coordinator.client.async_browse_collections = AsyncMock(
        return_value=_ok_envelope(
            {
                "items": [
                    {
                        "title": "Kind of Blue",
                        "path": "browse-token-albums",
                        "image": "/api/collections/image?ref=abc123",
                    },
                    {"title": "No Art Track", "ref": "play-token-1"},
                ]
            }
        )
    )
    player.coordinator.client.resolve_url = MagicMock(
        side_effect=lambda path: f"http://uhc.test.invalid{path}"
    )
    result = await player.async_browse_media(media_content_id="top:browse")
    player.coordinator.client.resolve_url.assert_called_once_with(
        "/api/collections/image?ref=abc123"
    )
    assert (
        result.children[0].thumbnail
        == "http://uhc.test.invalid/api/collections/image?ref=abc123"
    )
    assert result.children[1].thumbnail is None


@pytest.mark.asyncio
async def test_browse_media_path_continuation_always_uses_browse_action():
    zone = ZoneState(
        zone_id="roon:1", zone_name="Kitchen", source="roon", browse_supported=True
    )
    player = _player(zone)
    player.coordinator.client.async_browse_collections = AsyncMock(
        return_value=_ok_envelope({"items": []})
    )
    await player.async_browse_media(media_content_id="path:some-token")
    player.coordinator.client.async_browse_collections.assert_awaited_once_with(
        "roon:1", "browse", path="some-token"
    )


@pytest.mark.asyncio
async def test_browse_media_refusal_raises_browse_error():
    zone = ZoneState(
        zone_id="roon:1", zone_name="Kitchen", source="roon", browse_supported=True
    )
    player = _player(zone)
    player.coordinator.client.async_browse_collections = AsyncMock(
        return_value=_refused_envelope("favorites is not available for roon zones")
    )
    with pytest.raises(BrowseError, match="favorites is not available"):
        await player.async_browse_media(media_content_id="top:favorites")


@pytest.mark.asyncio
async def test_browse_media_rejects_unknown_content_id():
    zone = ZoneState(
        zone_id="lms:aa", zone_name="Bedroom", source="lms", browse_supported=True
    )
    player = _player(zone)
    with pytest.raises(BrowseError):
        await player.async_browse_media(media_content_id="nonsense:1")


@pytest.mark.asyncio
async def test_play_media_plays_a_browsed_ref():
    zone = ZoneState(
        zone_id="lms:aa", zone_name="Bedroom", source="lms", browse_supported=True
    )
    player = _player(zone)
    player.coordinator.client.async_play_ref = AsyncMock(
        return_value=_accepted_envelope()
    )
    await player.async_play_media("music", "ref:play-token-1")
    player.coordinator.client.async_play_ref.assert_awaited_once_with(
        "play-token-1", "lms:aa", action="play"
    )
    player.coordinator.async_request_refresh.assert_awaited_once()


@pytest.mark.asyncio
async def test_play_media_honors_enqueue():
    zone = ZoneState(
        zone_id="lms:aa", zone_name="Bedroom", source="lms", browse_supported=True
    )
    player = _player(zone)
    player.coordinator.client.async_play_ref = AsyncMock(
        return_value=_accepted_envelope()
    )
    await player.async_play_media(
        "music", "ref:play-token-1", enqueue=MediaPlayerEnqueue.ADD
    )
    player.coordinator.client.async_play_ref.assert_awaited_once_with(
        "play-token-1", "lms:aa", action="queue"
    )


@pytest.mark.asyncio
async def test_play_media_rejects_a_non_ref_media_id():
    zone = ZoneState(
        zone_id="lms:aa", zone_name="Bedroom", source="lms", browse_supported=True
    )
    player = _player(zone)
    with pytest.raises(HomeAssistantError):
        await player.async_play_media("music", "some-raw-id")


@pytest.mark.asyncio
async def test_play_media_refusal_raises_home_assistant_error():
    zone = ZoneState(
        zone_id="lms:aa", zone_name="Bedroom", source="lms", browse_supported=True
    )
    player = _player(zone)
    player.coordinator.client.async_play_ref = AsyncMock(
        return_value=_refused_envelope("ref expired")
    )
    with pytest.raises(HomeAssistantError, match="ref expired"):
        await player.async_play_media("music", "ref:stale-token")


@pytest.mark.asyncio
async def test_join_players_refuses_cross_provider_members():
    zone = ZoneState(zone_id="roon:1", zone_name="Kitchen", source="roon")
    player = _player(zone)
    with pytest.raises(HomeAssistantError, match="across providers"):
        await player.async_join_players(["lms:aa"])
    player.coordinator.client.async_zone_group.assert_not_called()


@pytest.mark.asyncio
async def test_join_players_refuses_ungrouping_capable_provider():
    zone = ZoneState(zone_id="upnp:1", zone_name="Office", source="upnp")
    player = _player(zone)
    with pytest.raises(HomeAssistantError, match="not supported"):
        await player.async_join_players(["upnp:2"])


@pytest.mark.asyncio
async def test_join_players_calls_client_for_same_provider_members():
    zone = ZoneState(zone_id="roon:1", zone_name="Kitchen", source="roon")
    player = _player(zone)
    player.coordinator.client.async_zone_group = AsyncMock()
    await player.async_join_players(["roon:2", "roon:3"])
    player.coordinator.client.async_zone_group.assert_awaited_once_with(
        "join", "roon:1", member_zone_ids=["roon:2", "roon:3"]
    )
    player.coordinator.async_request_refresh.assert_awaited_once()


@pytest.mark.asyncio
async def test_unjoin_player_calls_client_leave():
    zone = ZoneState(zone_id="lms:aa", zone_name="Bedroom", source="lms")
    player = _player(zone)
    player.coordinator.client.async_zone_group = AsyncMock()
    await player.async_unjoin_player()
    player.coordinator.client.async_zone_group.assert_awaited_once_with(
        "leave", "lms:aa", member_zone_ids=["lms:aa"]
    )


def test_group_members_reflects_zone_state():
    zone = ZoneState(
        zone_id="roon:1",
        zone_name="Kitchen",
        source="roon",
        group_members=["roon:1", "roon:2"],
    )
    player = _player(zone)
    assert player.group_members == ["roon:1", "roon:2"]


def test_group_members_empty_for_unavailable_zone():
    coordinator = _FakeCoordinator({})
    player = UnifiedHifiControlMediaPlayer(coordinator, "roon:missing")
    assert player.group_members == []
