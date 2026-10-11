from __future__ import annotations

from PySide6.QtCore import QBuffer, QByteArray, Qt
from PySide6.QtGui import QImage
from PySide6.QtWidgets import QListWidgetItem, QMessageBox

from neurowatch.dialogs.camera_dialog import AddEditCameraDialog
from neurowatch.main import MainWindow
from neurowatch.models.camera import CameraConfig
from neurowatch.widgets.camera_panel import CameraPanel
from neurowatch.widgets.cell_widget import CellWidget


def test_dialog_allows_file_protocol_path(monkeypatch, qtbot):
    dlg = AddEditCameraDialog()
    qtbot.addWidget(dlg)

    # Manual URLs (including local files) are entered on the Advanced tab.
    dlg._tabs.setCurrentWidget(dlg._advanced_tab)
    dlg._a_name.setText("Recorded Clip")
    dlg._a_url.setText("/tmp/demo.mp4")
    dlg._a_protocol.setCurrentText("File")

    warned = {"called": False}

    def _warn(*_args, **_kwargs):
        warned["called"] = True
        return QMessageBox.StandardButton.Ok

    monkeypatch.setattr(QMessageBox, "warning", _warn)

    dlg._on_accept()

    assert dlg.result() == dlg.DialogCode.Accepted
    assert not warned["called"]
    cfg = dlg.get_config()
    assert cfg.protocol == "file"
    assert cfg.name == "Recorded Clip"
    assert cfg.url == "/tmp/demo.mp4"


def test_dialog_test_connection_updates_status(monkeypatch, qtbot):
    image = QImage(2, 2, QImage.Format.Format_RGB32)
    image.fill(Qt.GlobalColor.red)
    buf = QByteArray()
    qbuf = QBuffer(buf)
    qbuf.open(QBuffer.OpenModeFlag.WriteOnly)
    image.save(qbuf, "JPEG")
    thumb_b64 = bytes(buf.toBase64()).decode()

    seen: list[tuple[str, dict | None]] = []

    class _FakeClient:
        def __init__(self, endpoint=None):
            self.endpoint = endpoint

        def call(self, method, params=None):
            seen.append((method, params))
            return {
                "ok": True,
                "width": 1280,
                "height": 720,
                "fps": 25,
                "thumbnail_b64": thumb_b64,
            }

    # The dialog's worker thread resolves the client from its own module.
    monkeypatch.setattr("neurowatch.dialogs.camera_dialog.SyncIpcClient", _FakeClient)

    dlg = AddEditCameraDialog()
    qtbot.addWidget(dlg)
    dlg._tabs.setCurrentWidget(dlg._advanced_tab)
    dlg._a_url.setText("rtsp://cam/stream")

    dlg._on_test()
    assert not dlg._test_btn.isEnabled(), "button is disabled while the test runs"

    qtbot.waitUntil(lambda: "Connected" in dlg._status_label.text(), timeout=2000)
    assert "1280×720" in dlg._status_label.text()
    assert dlg._test_btn.isEnabled()
    assert dlg._preview.pixmap() is not None and not dlg._preview.pixmap().isNull()
    # The dialog shows the test picture, so it asks for a decoded preview.
    assert seen == [("camera.test", {"url": "rtsp://cam/stream", "preview": True})]


def test_camera_panel_drag_mime_contains_camera_id(qtbot):
    panel = CameraPanel()
    qtbot.addWidget(panel)

    panel.set_cameras([CameraConfig(id="cam-1", name="Cam 1", url="rtsp://x")])
    item = panel._list.item(0)
    assert isinstance(item, QListWidgetItem)

    mime = panel._list.mimeData([item])
    assert mime is not None
    assert mime.text() == "neurowatch/cam_id:cam-1"


def test_empty_cell_click_emits_add_signal(qtbot):
    cell = CellWidget()
    qtbot.addWidget(cell)

    with qtbot.waitSignal(cell.add_camera_requested, timeout=1000):
        qtbot.mouseClick(cell, Qt.MouseButton.LeftButton)


def test_cell_update_frame_renders_rgb24(qtbot):
    # The SHM ring carries RGB24 frames.
    cell = CellWidget()
    qtbot.addWidget(cell)

    width, height = 4, 4
    frame = bytes([255, 0, 0] * (width * height))

    cell.update_frame(frame, width, height)

    assert cell._pixmap is not None
    assert not cell._pixmap.isNull()
    assert cell._pixmap.width() == width
    assert cell._pixmap.height() == height
    pixel = cell._pixmap.toImage().pixelColor(0, 0)
    assert (pixel.red(), pixel.green(), pixel.blue()) == (255, 0, 0)


def test_mainwindow_wall_drop_swaps_and_saves_wall_positions(monkeypatch, qtbot):
    calls: list[tuple[str, object]] = []

    class _FakeClient:
        _endpoint = "unix:/nonexistent/test-v1-part2.sock"

        def __init__(self, endpoint: str | None = None) -> None:
            pass

        def is_online(self) -> bool:
            return True

        def call(self, method, params=None):
            calls.append((method, params))
            # Settings are owned by the backend and fetched over IPC.
            if method == "config.get":
                return {
                    "version": 1,
                    "site_name": "Test Site",
                    "wall": {"rows": 2, "cols": 2, "auto_fit": True, "page_interval_sec": 1},
                    "ui": {"theme": "dark", "fullscreen": False},
                }
            if method == "camera.list":
                return []
            if method == "camera.status_all":
                return {}
            return {"ok": True}

    monkeypatch.setattr("neurowatch.main.SyncIpcClient", _FakeClient)
    monkeypatch.setattr("neurowatch.ipc.frame_reader.SHM_PREFIX", "/nonexistent/nw-test-")

    win = MainWindow()
    qtbot.addWidget(win)
    try:
        win._cameras = [
            CameraConfig(id="a", name="A", url="rtsp://a"),
            CameraConfig(id="b", name="B", url="rtsp://b"),
            CameraConfig(id="c", name="C", url="rtsp://c"),
        ]
        win._refresh_all()

        # Camera "c" dropped from the sidebar on the first tile swaps with "a".
        win._wall.drop_camera("c", 0)

        assert [c.id for c in win._cameras] == ["c", "b", "a"]
        assert win._cameras[0].wall_position is not None
        assert win._cameras[0].wall_position.page == 0
        assert win._cameras[0].wall_position.row == 0
        assert win._cameras[0].wall_position.col == 0
        assert [c.id for c in win._wall._page_cameras()] == ["c", "b", "a"]
        assert win._autoscroll_timer.isActive()
        assert win._autoscroll_timer.interval() == 1000
        assert win._title_bar._site_label.text() == "Test Site"
        qtbot.waitUntil(lambda: win._positions_worker is None, timeout=2000)
        position_calls = [p for m, p in calls if m == "camera.set_positions"]
        assert position_calls, "a layout edit must persist wall positions to the backend"
        assert position_calls[-1][0] == {
            "id": "c",
            "wall_position": {"page": 0, "row": 0, "col": 0, "row_span": 1, "col_span": 1},
        }
        assert win._ignore_config_changes_until > 0, "our own save must not prompt a reload"
    finally:
        win.close()


def test_mainwindow_tile_resize_reflows_and_keeps_the_page(monkeypatch, qtbot):
    class _FakeClient:
        _endpoint = "unix:/nonexistent/test-v1-part2.sock"

        def __init__(self, endpoint: str | None = None) -> None:
            pass

        def is_online(self) -> bool:
            return True

        def call(self, method, params=None):
            if method == "config.get":
                return {
                    "version": 3,
                    "site_name": "Test Site",
                    "wall": {"rows": 3, "cols": 3, "auto_fit": True, "page_interval_sec": 0},
                    "ui": {"theme": "dark", "fullscreen": False},
                }
            if method == "camera.list":
                return []
            if method == "camera.status_all":
                return {}
            return {"ok": True}

    monkeypatch.setattr("neurowatch.main.SyncIpcClient", _FakeClient)
    monkeypatch.setattr("neurowatch.ipc.frame_reader.SHM_PREFIX", "/nonexistent/nw-test-")

    win = MainWindow()
    qtbot.addWidget(win)
    try:
        win._cameras = [
            CameraConfig(id=f"cam-{i}", name=f"Cam {i}", url="rtsp://x") for i in range(12)
        ]
        win._refresh_all(page=1)
        assert win._wall.page == 1

        # Enlarge the first tile on page 2 (cam-9) to 2x2: page 2 of a 3x3
        # wall keeps showing it, now as a big tile.
        win._wall.set_tile_size("cam-9", 2, 2)

        assert win._wall.page == 1
        assert win._cameras[9].tile_span == (2, 2)
        assert win._cameras[9].wall_position is not None
        assert (win._cameras[9].wall_position.page, win._cameras[9].wall_position.row) == (1, 0)
        qtbot.waitUntil(lambda: win._positions_worker is None, timeout=2000)
    finally:
        win.close()
