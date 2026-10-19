"""Cloud UI tests: the app's test setup (tests/ui/conftest.py), with the cloud
light's status poll left on (the rest of tests/ui switches it off)."""

from __future__ import annotations

import pytest


@pytest.fixture(autouse=True)
def _cloud_poll(_no_cloud_poll):
    """The real ``_CloudPoll.start`` (after ``_no_cloud_poll`` stubbed it)."""
    from PySide6.QtCore import QThread

    from neurowatch.cloud import desktop

    desktop._CloudPoll.start = QThread.start
    yield
