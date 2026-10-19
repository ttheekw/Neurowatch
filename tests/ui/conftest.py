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
def _no_event_image_network():
    """New preview widgets must never query a user's real event database.

    A subclass keeps its harmless run method even after fixture teardown,
    unlike temporarily patching a worker method while threads may be queued.
    Worker-contract tests can explicitly use the returned original class.
    """
    from neurowatch.widgets import event_list

    original = event_list._ImageWorker

    class OfflineImageWorker(original):
        def run(self):
            self.done.emit(self._event_id, self._generation, None)

    # Do not request pytest's monkeypatch fixture here: doing so makes tests'
    # own patches outlive unrelated teardown fixtures (e.g. build-info caches).
    event_list._ImageWorker = OfflineImageWorker
    try:
        yield original
    finally:
        event_list._ImageWorker = original


@pytest.fixture(autouse=True)
def _no_camera_settings_network():
    """Shown camera dialogs must not poll or control the user's recorder.

    A subclass stays offline even if a queued thread starts after teardown.
    Tests that exercise actions install their own fake worker explicitly.
    """
    from neurowatch.dialogs import camera_dialog

    original = camera_dialog._RpcWorker

    class OfflineSettingsWorker(original):
        def run(self):
            self.done.emit(RuntimeError("Offline camera settings test"))

    camera_dialog._RpcWorker = OfflineSettingsWorker
    try:
        yield original
    finally:
        camera_dialog._RpcWorker = original


@pytest.fixture(autouse=True)
def _no_alert_network():
    """Desktop alert feeds (main window, tray) never ask a user's real recorder."""
    from neurowatch.ipc import alert_feed

    original = alert_feed._ask
    alert_feed._ask = lambda method, params, endpoint: None
    yield
    alert_feed._ask = original


@pytest.fixture(autouse=True)
def _no_cloud_poll():
    """The main window's cloud light never asks a recorder for ``cloud.status``
    (tests/ui/cloud tests its poll on its own).

    Not through ``monkeypatch``: that would make every test's monkeypatch
    outlive its other fixtures' teardown."""
    from neurowatch.cloud import desktop

    original = desktop._CloudPoll.start
    desktop._CloudPoll.start = lambda self, *args: None
    yield
    desktop._CloudPoll.start = original


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
