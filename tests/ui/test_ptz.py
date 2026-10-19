"""PTZ input, lifecycle and command ordering; every transport is fake."""

from __future__ import annotations

import threading

import pytest
from PySide6.QtCore import QEvent, QObject, Qt, Signal
from PySide6.QtWidgets import QApplication

from neurowatch.dialogs import ptz_dialog
from neurowatch.ipc import ptz_worker


class Worker(QObject):
    result = Signal(str, object)
    finished = Signal()

    def __init__(self, endpoint, camera_id, client_id):
        super().__init__()
        self.calls = []
        self.client_id = client_id

    def command(self, method, params=None):
        self.calls.append((method, params or {}))
        return True

    def start(self):
        pass

    def isRunning(self):
        return False

    def move(self, *vector):
        self.calls.append(("move", vector))

    def halt(self, *, force=False):
        self.calls.append(("stop", {"force": True} if force else {}))

    def shutdown(self):
        self.calls.append(("shutdown", {}))


@pytest.fixture
def dialog(qtbot, monkeypatch):
    monkeypatch.setattr(ptz_dialog, "PtzWorker", Worker)
    dialog = ptz_dialog.PtzDialog("unix:/offline", "camera", "Gate")
    dialog.setAttribute(Qt.WidgetAttribute.WA_DeleteOnClose, False)
    qtbot.addWidget(dialog)
    dialog.show()
    yield dialog
    from shiboken6 import isValid

    if isValid(dialog):
        dialog.close()


def ready(dialog):
    dialog._worker.result.emit(
        "ptz.capabilities", {"pan_tilt": True, "zoom": True, "presets": True, "home": True}
    )


def test_controls_wait_for_capabilities_and_hold_release_or_focus_loss_stop(dialog, qtbot):
    assert not dialog.directions[0].isEnabled()
    assert not dialog.home.isEnabled()
    ready(dialog)
    assert dialog.directions[0].isEnabled()
    button = dialog.directions[1]  # up
    qtbot.mousePress(button, Qt.MouseButton.LeftButton)
    assert dialog._worker.calls[-1] == ("move", (0.0, 0.5, 0.0))
    qtbot.mouseRelease(button, Qt.MouseButton.LeftButton)
    assert dialog._worker.calls[-1][0] == "stop"
    qtbot.mousePress(dialog.zoom_buttons[0], Qt.MouseButton.LeftButton)
    assert dialog._worker.calls[-1] == ("move", (0.0, 0.0, 0.5))
    QApplication.sendEvent(dialog, QEvent(QEvent.Type.WindowDeactivate))
    assert dialog._worker.calls[-1][0] == "stop"
    qtbot.mouseRelease(dialog.zoom_buttons[0], Qt.MouseButton.LeftButton)


def test_keyboard_movement_and_escape_terminate_the_worker(dialog, qtbot):
    ready(dialog)
    dialog.stop.setFocus()
    qtbot.keyPress(dialog.stop, Qt.Key.Key_Right)
    assert dialog._worker.calls[-1] == ("move", (0.5, 0.0, 0.0))
    qtbot.keyPress(dialog.stop, Qt.Key.Key_Up)
    assert dialog._worker.calls[-1] == ("move", (0.5, 0.5, 0.0))
    qtbot.keyRelease(dialog.stop, Qt.Key.Key_Right)
    assert dialog._worker.calls[-1] == ("move", (0.0, 0.5, 0.0))
    qtbot.keyRelease(dialog.stop, Qt.Key.Key_Up)
    assert dialog._worker.calls[-1][0] == "stop"
    qtbot.keyClick(dialog, Qt.Key.Key_Escape)
    assert dialog._worker.calls[-1][0] == "shutdown"


def test_preset_actions_use_opaque_camera_tokens_and_delete_confirmation(dialog, monkeypatch):
    ready(dialog)
    dialog._worker.result.emit("ptz.presets", [{"name": "Gate", "token": "opaque&1"}])
    assert dialog.name.text() == "Gate"
    dialog.go.click()
    assert dialog._worker.calls[-1] == ("ptz.goto_preset", {"token": "opaque&1"})
    dialog._worker.result.emit("ptz.goto_preset", {"ok": True})
    dialog.name.setText("Front yard")
    dialog.update_preset.click()
    assert dialog._worker.calls[-1] == (
        "ptz.save_preset",
        {"token": "opaque&1", "name": "Front yard"},
    )
    dialog._worker.result.emit("ptz.save_preset", {"ok": True})
    assert dialog._worker.calls[-1][0] == "ptz.presets"
    dialog.save_new.click()
    assert dialog._worker.calls[-1] == ("ptz.save_preset", {"name": "Front yard"})
    dialog._worker.result.emit("ptz.save_preset", {"ok": True})
    monkeypatch.setattr(ptz_dialog, "ask", lambda *args, **kwargs: False)
    before = list(dialog._worker.calls)
    dialog.delete.click()
    assert dialog._worker.calls == before
    monkeypatch.setattr(ptz_dialog, "ask", lambda *args, **kwargs: True)
    dialog.delete.click()
    assert dialog._worker.calls[-1] == ("ptz.delete_preset", {"token": "opaque&1"})


def test_move_failure_stays_visible_after_stop_and_can_refresh_capabilities(dialog):
    ready(dialog)
    dialog._worker.result.emit("ptz.move", RuntimeError("Camera did not answer"))
    dialog._worker.result.emit("ptz.stop", {"ok": True})
    assert "did not answer" in dialog.status.text()
    dialog.refresh.click()
    assert dialog._worker.calls[-1] == ("ptz.capabilities", {"refresh": True})
    dialog._worker.result.emit("ptz.capabilities", RuntimeError("No PTZ service"))
    assert not dialog.directions[0].isEnabled()
    assert "No PTZ" in dialog.status.text()


def test_worker_release_discards_queued_motion_and_stop_precedes_shutdown(qtbot, monkeypatch):
    entered, release = threading.Event(), threading.Event()
    calls = []

    class Client:
        def __init__(self, endpoint):
            assert endpoint == "unix:/offline"

        def call(self, method, params, **kwargs):
            calls.append((method, params))
            if method == "ptz.move":
                entered.set()
                assert release.wait(3)
            return {"ok": True}

    monkeypatch.setattr(ptz_worker, "SyncIpcClient", Client)
    worker = ptz_worker.PtzWorker("unix:/offline", "cam", "client")
    worker.move(0.5, 0, 0)
    worker.start()
    try:
        qtbot.waitUntil(entered.is_set)
        for _ in range(1000):
            worker.move(-0.5, 0, 0)
        worker.command("ptz.goto_preset", {"token": "must-not-run"})
        worker.shutdown()
        release.set()
        qtbot.waitUntil(lambda: not worker.isRunning())
        assert [method for method, _ in calls] == ["ptz.move", "ptz.stop"]
        assert calls[-1][1] == {"camera_id": "cam", "client_id": "client"}
    finally:
        release.set()
        worker.shutdown()
        worker.wait(3000)


def test_worker_does_not_keep_renewing_after_network_failure(qtbot, monkeypatch):
    calls = []

    class Client:
        def __init__(self, endpoint):
            pass

        def call(self, method, params, **kwargs):
            calls.append(method)
            if method == "ptz.move":
                raise RuntimeError("offline")
            return {"ok": True}

    monkeypatch.setattr(ptz_worker, "SyncIpcClient", Client)
    worker = ptz_worker.PtzWorker("unix:/offline", "cam", "client")
    worker.move(0.5, 0, 0)
    worker.start()
    try:
        qtbot.waitUntil(lambda: "ptz.stop" in calls)
        qtbot.wait(350)
        assert calls == ["ptz.move", "ptz.stop"]
    finally:
        worker.shutdown()
        worker.wait(3000)


def test_live_tile_and_toolbar_make_ptz_controls_discoverable(qtbot):
    from neurowatch.models.camera import CameraConfig
    from neurowatch.widgets.cell_widget import CellWidget
    from neurowatch.widgets.toolbar import ToolBar

    cell = CellWidget()
    qtbot.addWidget(cell)
    cell.set_camera(CameraConfig(id="cam", name="Gate", url="rtsp://offline/fixture"))
    menu = cell._build_context_menu()
    assert any(action.text() == "PTZ Controls…" for action in menu.actions())
    menu.deleteLater()
    cell.set_mode("playback")
    menu = cell._build_context_menu()
    assert not any(action.text() == "PTZ Controls…" for action in menu.actions())
    menu.deleteLater()
    toolbar = ToolBar()
    qtbot.addWidget(toolbar)
    assert not toolbar._ptz_btn.isEnabled()
    toolbar.set_camera_selected(True)
    with qtbot.waitSignal(toolbar.ptz_requested):
        toolbar._ptz_btn.click()


def test_home_click_survives_focus_change_and_enter_does_not_move_a_camera(dialog, qtbot):
    ready(dialog)
    qtbot.mouseClick(dialog.home, Qt.MouseButton.LeftButton)
    assert dialog._worker.calls[-1] == ("ptz.home", {})
    dialog._worker.result.emit("ptz.home", {"ok": True})
    dialog.name.setFocus()
    before = list(dialog._worker.calls)
    qtbot.keyClick(dialog.name, Qt.Key.Key_Return)
    assert dialog._worker.calls == before
    dialog.stop.click()
    assert dialog._worker.calls[-1] == ("stop", {"force": True})
