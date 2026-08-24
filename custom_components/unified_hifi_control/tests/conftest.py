"""Fixtures for unified_hifi_control tests."""
from __future__ import annotations

import pytest


@pytest.fixture(autouse=True)
def auto_enable_custom_integrations(enable_custom_integrations):
    """Enable custom integrations for every test (pytest-homeassistant-custom-component)."""
    yield
