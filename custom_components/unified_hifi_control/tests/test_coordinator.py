"""Tests for coordinator event handling."""
from __future__ import annotations

from custom_components.unified_hifi_control.coordinator import (
    ZoneState,
    _h_now_playing_changed,
    _h_seek_changed,
    _h_volume_changed,
    _h_zone_discovered,
    _h_zone_removed,
    _h_zone_updated,
)


def _zone(zone_id="roon:1", zone_name="Kitchen", **kw):
    return ZoneState(zone_id=zone_id, zone_name=zone_name, source="roon", **kw)


def test_now_playing_changed_updates_metadata():
    data = {"roon:1": _zone()}
    changed = _h_now_playing_changed(
        data, {"zone_id": "roon:1", "title": "Song", "artist": "Artist"}
    )
    assert changed
    assert data["roon:1"].title == "Song"
    assert data["roon:1"].artist == "Artist"


def test_now_playing_changed_unknown_zone_is_noop():
    data = {"roon:1": _zone()}
    changed = _h_now_playing_changed(data, {"zone_id": "roon:999", "title": "Song"})
    assert not changed
    assert data["roon:1"].title is None


def test_volume_changed_uses_output_id():
    data = {"roon:1": _zone()}
    changed = _h_volume_changed(
        data, {"output_id": "roon:1", "value": 33, "is_muted": True}
    )
    assert changed
    assert data["roon:1"].volume == 33
    assert data["roon:1"].is_muted is True


def test_seek_changed_updates_position():
    data = {"roon:1": _zone()}
    assert _h_seek_changed(data, {"zone_id": "roon:1", "position": 42})
    assert data["roon:1"].seek_position == 42


def test_zone_updated_changes_name_and_state():
    data = {"roon:1": _zone()}
    assert _h_zone_updated(
        data, {"zone_id": "roon:1", "display_name": "Den", "state": "playing"}
    )
    assert data["roon:1"].zone_name == "Den"
    assert data["roon:1"].state == "playing"


def test_zone_removed_deletes_entry():
    data = {"roon:1": _zone()}
    assert _h_zone_removed(data, {"zone_id": "roon:1"})
    assert "roon:1" not in data


def test_zone_removed_unknown_zone_is_noop():
    data = {"roon:1": _zone()}
    assert not _h_zone_removed(data, {"zone_id": "roon:999"})


def test_zone_discovered_adds_new_zone():
    data: dict[str, ZoneState] = {}
    changed = _h_zone_discovered(
        data,
        {
            "zone": {
                "zone_id": "lms:aa",
                "zone_name": "Bedroom",
                "source": "lms",
                "state": "stopped",
            }
        },
    )
    assert changed
    assert data["lms:aa"].zone_name == "Bedroom"


def test_zone_discovered_existing_zone_is_noop():
    data = {"roon:1": _zone(zone_name="Kitchen")}
    changed = _h_zone_discovered(
        data, {"zone": {"zone_id": "roon:1", "zone_name": "Renamed"}}
    )
    assert not changed
    assert data["roon:1"].zone_name == "Kitchen"
