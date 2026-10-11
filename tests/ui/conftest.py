"""Pytest configuration and shared fixtures."""
from __future__ import annotations

import os

# Force the offscreen Qt platform for the entire test session BEFORE any Qt
# module is imported (pytest-qt reads QT_QPA_PLATFORM at plugin load time).
# This only affects the pytest process — production runtime is untouched.
os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")
# Tests (and the subprocess probes that run the real main()) must never start,
# register or stop the user's real background service, or reach the window of
# a NeuroWatch the user has open.
os.environ["NW_SERVICE"] = "external"
os.environ["NW_UI_INSTANCE"] = f"neurowatch-ui-test-{os.getpid()}"

import pytest


@pytest.fixture(autouse=True)
def _no_extensions():
    """Tests run without extensions, whatever is installed; a test can set its own.

    Not through ``monkeypatch``: that would make every test's monkeypatch
    outlive its other fixtures' teardown."""
    from neurowatch import extensions

    original = extensions.load
    extensions.load = lambda: ()
    yield
    extensions.load = original


@pytest.fixture
def app(qtbot):
    """Provide a QApplication instance for widget tests."""
    from PySide6.QtWidgets import QApplication
    instance = QApplication.instance()
    if instance is None:
        instance = QApplication([])
    return instance
