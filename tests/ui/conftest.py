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


class UnexpectedMessage(AssertionError):
    """A themed message or question opened that the test did not stub."""


@pytest.fixture(autouse=True)
def _no_unexpected_messages():
    """A modal message waits for a click, so one a test did not expect would
    hang the run: it fails at once with its title and text instead. Tests
    that expect one stub ``show_message``/``ask``/``confirm_action`` where
    they are used (``monkeypatch.setattr(module, "ask", ...)``)."""
    from neurowatch.dialogs.message import MessageDialog

    def refuse(dialog: MessageDialog) -> int:
        raise UnexpectedMessage(
            f"unexpected modal message {dialog.windowTitle()!r}: {dialog.text_label.text()!r}"
        )

    original = MessageDialog.exec
    MessageDialog.exec = refuse  # type: ignore[method-assign]
    yield
    MessageDialog.exec = original  # type: ignore[method-assign]
