"""Tests for capability mapping in media_player.py."""
from __future__ import annotations

from homeassistant.components.media_player import MediaPlayerEntityFeature

from custom_components.unified_hifi_control.coordinator import ZoneState
from custom_components.unified_hifi_control.media_player import _supported_features


def test_roon_zone_gets_full_transport_and_skip():
    zone = ZoneState(zone_id="roon:1", zone_name="Kitchen", source="roon")
    features = _supported_features(zone)
    assert features & MediaPlayerEntityFeature.NEXT_TRACK
    assert features & MediaPlayerEntityFeature.PREVIOUS_TRACK
    assert features & MediaPlayerEntityFeature.VOLUME_SET
    assert not (features & MediaPlayerEntityFeature.GROUPING)


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


def test_lms_zone_has_no_grouping_on_this_branch():
    zone = ZoneState(zone_id="lms:aa", zone_name="Bedroom", source="lms")
    features = _supported_features(zone)
    assert not (features & MediaPlayerEntityFeature.GROUPING)
    assert features & MediaPlayerEntityFeature.NEXT_TRACK
