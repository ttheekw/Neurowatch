"""Multi-camera wall interaction: truthful zoom, page navigation, no orphans."""

from __future__ import annotations

from typing import Any, ClassVar

import pytest
from PySide6.QtCore import QEvent, QObject, QPoint, QPointF, Qt, Signal
from PySide6.QtGui import QGuiApplication, QWheelEvent
from PySide6.QtWidgets import QApplication

from neurowatch.ipc.viewer_client import NativeViewerClient
from neurowatch.models.camera import CameraConfig
from neurowatch.widgets.cell_widget import CellWidget
from neurowatch.widgets.native_viewer_surface import NativeViewerSurface
from neurowatch.widgets.page_strip import PageStripWidget
from neurowatch.widgets.video_wall import VideoWallWidget

# ---------------------------------------------------------------------------
# Stubs
# ---------------------------------------------------------------------------


class _FakeFrameReaderThread(QObject):
    frame_ready = Signal(str, bytes, int, int, str)
    instances: ClassVar[list[_FakeFrameReaderThread]] = []

    def __init__(self, cam_id: str, max_emit_fps: float | None = None, parent=None) -> None:
        super().__init__(parent)
        self.cam_id = cam_id
        self.stopped = False
        _FakeFrameReaderThread.instances.append(self)

    def start(self) -> None:
        pass

    def stop(self) -> None:
        self.stopped = True

    def set_max_emit_fps(self, value: float | None) -> None:
        pass


class _ViewerIpc:
    def __init__(self, *, available: bool) -> None:
        self.available = available
        self.sessions = 0
        self.calls: list[tuple[str, dict[str, Any]]] = []

    def call(self, method: str, params: dict[str, Any] | None = None) -> Any:
        self.calls.append((method, dict(params or {})))
        if method == "viewer.capabilities":
            return {"available": self.available, "sinks": ["glimagesink"]}
        if method == "viewer.attach":
            self.sessions += 1
            return {"session_id": f"vw-{self.sessions}"}
        return {"ok": True}


@pytest.fixture
def fake_readers(monkeypatch) -> type[_FakeFrameReaderThread]:
    _FakeFrameReaderThread.instances = []
    monkeypatch.setattr("neurowatch.widgets.video_wall.FrameReaderThread", _FakeFrameReaderThread)
    return _FakeFrameReaderThread


@pytest.fixture
def xcb(monkeypatch) -> None:
    monkeypatch.setattr(QGuiApplication, "platformName", staticmethod(lambda: "xcb"))


def _cams(n: int) -> list[CameraConfig]:
    return [CameraConfig(id=f"cam-{i}", name=f"Cam {i}", url="/missing.avi") for i in range(n)]


def _wheel(widget: CellWidget, dy: int) -> QWheelEvent:
    pos = QPointF(widget.width() / 2, widget.height() / 2)
    event = QWheelEvent(
        pos,
        widget.mapToGlobal(pos),
        QPoint(0, 0),
        QPoint(0, dy),
        Qt.MouseButton.NoButton,
        Qt.KeyboardModifier.NoModifier,
        Qt.ScrollPhase.NoScrollPhase,
        False,
    )
    event.ignore()
    widget.wheelEvent(event)
    return event


def _surface() -> NativeViewerSurface:
    return NativeViewerSurface(NativeViewerClient(ipc=_ViewerIpc(available=True)))  # type: ignore[arg-type]


def _native_cell(qtbot) -> CellWidget:
    cell = CellWidget()
    qtbot.addWidget(cell)
    cell.resize(320, 180)
    cell.set_camera(_cams(1)[0])
    cell.attach_native_surface(_surface())
    return cell


def _action_texts(cell: CellWidget) -> list[str]:
    menu = cell._build_context_menu()
    try:
        return [a.text() for a in menu.actions()]
    finally:
        menu.deleteLater()


# ---------------------------------------------------------------------------
# Truthful zoom
# ---------------------------------------------------------------------------


def test_native_cell_does_not_pretend_to_zoom_video(qtbot):
    cell = _native_cell(qtbot)
    assert cell.zoom_supported is False

    event = _wheel(cell, 120)

    assert cell._zoom == 1.0, "native overlay pixels cannot follow a Qt-side zoom"
    assert not event.isAccepted(), "wheel must not be swallowed as a fake zoom"
    texts = _action_texts(cell)
    assert "Reset Zoom" not in texts
    assert not any("Aspect" in t for t in texts), "overlay ignores Qt-side aspect lock too"
    assert "zoom" not in cell.toolTip().lower()


def test_native_video_child_passes_double_clicks_to_parent_cell(qtbot):
    cell = _native_cell(qtbot)
    surface = cell._native_surface
    assert surface is not None
    assert surface.testAttribute(Qt.WidgetAttribute.WA_TransparentForMouseEvents)


def test_attaching_native_surface_clears_fallback_zoom(qtbot):
    cell = CellWidget()
    qtbot.addWidget(cell)
    cell.resize(320, 180)
    cell.set_camera(_cams(1)[0])
    assert cell.zoom_supported is True
    _wheel(cell, 120)
    _wheel(cell, 120)
    cell._pan = QPoint(30, 10)
    assert cell._zoom > 1.0

    cell.attach_native_surface(_surface())

    assert cell._zoom == 1.0
    assert cell._pan == QPoint(0, 0)
    cell.grab()  # paint: no zoom badge / reset hit-area may remain
    assert cell._zoom_reset_rect.isNull()


def test_fallback_cell_zoom_is_real_and_discoverable(qtbot):
    cell = CellWidget()
    qtbot.addWidget(cell)
    cell.resize(320, 180)
    cell.set_camera(_cams(1)[0])

    event = _wheel(cell, 120)

    assert event.isAccepted()
    assert cell._zoom == 1.25
    assert "Reset Zoom" in _action_texts(cell)
    assert "zoom" in cell.toolTip().lower()


def test_detaching_native_surface_restores_fallback_zoom(qtbot):
    cell = _native_cell(qtbot)
    cell.detach_native_surface()
    assert cell.zoom_supported is True
    _wheel(cell, 120)
    assert cell._zoom == 1.25


# ---------------------------------------------------------------------------
# Orphans / stale frames
# ---------------------------------------------------------------------------


def test_native_surface_follows_camera_when_cells_reorder(qtbot, fake_readers, xcb):
    ipc = _ViewerIpc(available=True)
    wall = VideoWallWidget(viewer_client=NativeViewerClient(ipc=ipc))
    qtbot.addWidget(wall)
    wall.set_grid(1, 2)
    a, b = _cams(2)
    wall.set_cameras([a, b])

    wall.set_cameras([b, a])  # both still visible, swapped cells

    for cam in (a, b):
        cell = wall._cell_for_cam_id(cam.id)
        surface = wall._native_surfaces[cam.id]
        assert cell is not None and cell._camera is not None and cell._camera.id == cam.id
        assert cell._native_surface is surface, f"{cam.id} video shown in another cell"
        assert surface.is_native_active and surface.cam_id == cam.id
    attached = set(wall._native_surfaces.values())
    live_children = {s for s in wall.findChildren(NativeViewerSurface) if s.parent() is not None}
    assert live_children == attached, "released surfaces must not linger in the wall"


def test_grid_rebuild_removes_old_cells_immediately(qtbot, fake_readers):
    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall._native_available = False
    wall.set_grid(4, 4)
    wall.set_cameras(_cams(3))

    wall.set_grid(2, 2)

    cells = wall.findChildren(CellWidget)
    assert len(cells) == 4, "old cells must be unparented, not left hidden until deleteLater"


def test_refresh_keeps_same_camera_frame_but_never_shows_another_cameras_frame(qtbot, fake_readers):
    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall._native_available = False
    wall.set_grid(1, 2)
    a, b = _cams(2)
    wall.set_cameras([a, b])
    wall._on_frame_ready(a.id, b"\0" * 12, 2, 2, "rgb")
    assert wall._cells[0]._pixmap is not None

    wall.set_cameras([a, b])  # unrelated refresh (e.g. another camera edited)
    assert wall._cells[0]._pixmap is not None, "refresh must not flash the placeholder"

    wall.set_cameras([b, a])
    assert wall._cells[0]._pixmap is None, "cell 0 now shows cam-1 and must drop cam-0's frame"


def test_set_page_clamps_to_existing_pages(qtbot, fake_readers):
    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall._native_available = False
    wall.set_grid(1, 2)
    wall.set_cameras(_cams(3))

    wall.set_page(7)

    assert wall.page == 1
    assert [c._camera.id if c._camera else None for c in wall._cells] == ["cam-2", None]


def test_warmed_page_opens_live_and_stale_page_refreshing(qtbot, fake_readers):
    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall._native_available = False
    wall.set_grid(1, 1)
    wall.set_cameras(_cams(3))
    assert wall.page_demand(1) == ["cam-1"]
    assert wall.desktop_demand() == ["cam-0"]

    wall._on_frame_ready("cam-1", b"\0" * 12, 2, 2, "rgb")  # its decoder was warmed
    wall.set_page(1)
    assert not wall._cells[0]._refreshing, "a current picture is shown as live"

    wall._on_frame_ready("cam-2", b"\0" * 12, 2, 2, "rgb")
    wall._last_frame_at["cam-2"] -= 60  # decoded a minute ago, then stopped
    wall.set_page(2)
    assert wall._cells[0]._pixmap is not None and wall._cells[0]._refreshing

    wall.set_solo_camera("cam-0")
    assert wall.page_demand(0) == []


@pytest.mark.parametrize("size", [(1600, 1100), (2400, 900), (900, 1400)])
def test_tiles_keep_the_video_shape_with_even_gaps(qtbot, fake_readers, size):
    """Cells are 16:9 whatever the wall's shape: the picture fills its tile,
    every gap between tiles is the same, and spare space is one centred
    margin around the grid."""
    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall._native_available = False
    wall.set_grid(3, 3)
    wall.set_cameras(_cams(9))
    wall.resize(*size)
    wall.show()
    qtbot.waitExposed(wall)
    QApplication.processEvents()  # the layout settles on the next event pass

    rects = [cell.geometry() for cell in wall._cells]
    for rect in rects:
        assert abs(rect.width() / rect.height() - 16 / 9) < 0.02, rect
    lefts = sorted({r.left() for r in rects})
    tops = sorted({r.top() for r in rects})
    col_gaps = {lefts[i + 1] - rects[0].width() - lefts[i] for i in range(2)}
    row_gaps = {tops[i + 1] - rects[0].height() - tops[i] for i in range(2)}
    assert col_gaps | row_gaps <= {0, 1, 2}, "the same thin gap everywhere"
    grid = rects[0].united(rects[-1])
    assert abs(grid.left() - (wall.width() - grid.right() - 1)) <= 2, "centred"
    assert abs(grid.top() - (wall.height() - grid.bottom() - 1)) <= 2, "centred"
    # The spare margin follows the size; it must never stop the window shrinking.
    assert wall.minimumSizeHint().width() <= 3 * 120 + 10
    assert wall.minimumSizeHint().height() <= 3 * 68 + 10

    wall.set_solo_camera("cam-4")
    QApplication.processEvents()
    solo = wall._cells[0].geometry()
    assert abs(solo.width() / solo.height() - 16 / 9) < 0.02


# ---------------------------------------------------------------------------
# Page strip
# ---------------------------------------------------------------------------


def test_page_strip_has_discoverable_prev_next_and_label(qtbot):
    strip = PageStripWidget()
    qtbot.addWidget(strip)
    clicked: list[int] = []
    strip.page_clicked.connect(clicked.append)

    strip.set_state(4, 1)
    assert not strip.isHidden()
    assert "2" in strip._label.text() and "4" in strip._label.text()
    strip._prev_btn.click()
    strip._next_btn.click()
    assert clicked == [0, 2]

    strip.set_state(4, 0)
    assert not strip._prev_btn.isEnabled() and strip._next_btn.isEnabled()
    strip.set_state(4, 3)
    assert strip._prev_btn.isEnabled() and not strip._next_btn.isEnabled()
    assert "PgUp" in strip._prev_btn.toolTip() and "PgDn" in strip._next_btn.toolTip()

    strip.set_state(1, 0)
    assert strip.isHidden()


# ---------------------------------------------------------------------------
# MainWindow navigation
# ---------------------------------------------------------------------------


def _window(
    qtbot,
    monkeypatch,
    *,
    cams: int,
    rows: int = 1,
    cols: int = 1,
    interval: int = 0,
    native: bool = False,
):
    from neurowatch.main import MainWindow

    class Client:
        _endpoint = "unix:/nonexistent/test-wall-nav.sock"

        def __init__(self, *args, **kwargs) -> None:
            pass

        def is_online(self) -> bool:
            return True

        def call(self, method, params=None):
            if method == "config.get":
                return {"wall": {"rows": rows, "cols": cols, "page_interval_sec": interval}}
            if method == "camera.list":
                return [
                    {"id": f"nav-{i}", "name": str(i), "url": "/missing.avi"} for i in range(cams)
                ]
            if method == "viewer.capabilities":
                return {"available": native, "sinks": ["glimagesink"]}
            if method == "viewer.attach":
                return {"session_id": f"s-{params['camera_id']}"}
            return {}

    monkeypatch.setattr("neurowatch.main.SyncIpcClient", Client)
    monkeypatch.setattr("neurowatch.ipc.frame_reader.SHM_PREFIX", "/nonexistent/nw-test-")
    window = MainWindow()
    qtbot.addWidget(window)
    return window


def test_rapid_manual_navigation_is_not_dropped(qtbot, monkeypatch):
    window = _window(qtbot, monkeypatch, cams=4)
    try:
        window._on_next_page()
        window._on_next_page()
        window._on_next_page()
        assert window._wall.page == 3
        assert window._wall.graphicsEffect() is None
        assert window._toolbar._page_label.text() == "Page 4 / 4"
    finally:
        window.close()


def test_page_change_paints_cached_frames_at_once(qtbot, monkeypatch):
    """Changing page shows the new page in the same event-loop turn, with no
    fade through black and no graphics effect compositing the wall."""
    window = _window(qtbot, monkeypatch, cams=2)
    try:
        wall = window._wall
        wall._last_frame_by_id["nav-1"] = (bytes([200, 10, 10] * 4), 2, 2, "rgb")

        window._on_next_page()

        assert wall.page == 1
        assert wall.graphicsEffect() is None
        cell = wall._cells[0]
        assert cell._camera.id == "nav-1"
        assert cell._pixmap is not None and cell._refreshing
    finally:
        window.close()


def test_manual_navigation_resets_auto_scroll_countdown(qtbot, monkeypatch):
    window = _window(qtbot, monkeypatch, cams=3, interval=1)
    try:
        assert window._autoscroll_timer.isActive()
        for navigate in (
            window._on_next_page,
            lambda: window._on_page_strip_clicked(window._wall.page),
            window._on_prev_page,
            window._on_prev_page,  # already on first page: still manual intent
        ):
            qtbot.wait(450)
            navigate()
            assert window._autoscroll_timer.remainingTime() > 900
    finally:
        window.close()


def test_auto_scroll_decodes_the_next_page_just_before_showing_it(qtbot, monkeypatch):
    from neurowatch import main

    window = _window(qtbot, monkeypatch, cams=3, interval=5)
    try:
        assert window._prewarm_timer.isActive()
        # Fires the lead time before the clock's next tick.
        lead = window._autoscroll_timer.remainingTime() - window._prewarm_timer.remainingTime()
        assert abs(lead - main._PREWARM_LEAD_MS) < 250
        assert set(window._view_lease.cameras) == {"nav-0"}

        window._prewarm_next_pages()
        assert window._wall.page == 0
        assert set(window._view_lease.cameras) == {"nav-0", "nav-1"}

        window._on_page_clock()
        assert window._wall.page == 1
        assert set(window._view_lease.cameras) == {"nav-1"}, "old page released"
        assert window._prewarm_timer.isActive(), "next warm-up scheduled"
    finally:
        window.close()


def test_manual_navigation_to_the_warmed_page_keeps_it_decoding(qtbot, monkeypatch):
    window = _window(qtbot, monkeypatch, cams=3, interval=5)
    try:
        window._prewarm_next_pages()
        sent: list[set[str]] = []
        monkeypatch.setattr(window._view_lease, "set_cameras", lambda ids: sent.append(set(ids)))

        window._on_next_page()

        assert window._wall.page == 1
        assert sent and all("nav-1" in ids for ids in sent), "warm decoder never dropped"
        # Browsing by hand: the page left and the next one stay warm a while.
        assert sent[-1] == {"nav-0", "nav-1", "nav-2"}
        window._browse_warm_timer.timeout.emit()
        assert sent[-1] == {"nav-1"}, "then only the visible page decodes"
    finally:
        window.close()


def test_browsing_by_hand_keeps_the_pages_around_warm_for_a_while(qtbot, monkeypatch):
    from neurowatch import main

    window = _window(qtbot, monkeypatch, cams=5)
    try:
        window._on_page_strip_clicked(2)  # a dot click
        assert window._wall.page == 2
        # The page left (0) and both neighbours (1, 3): back or on is instant.
        assert set(window._view_lease.cameras) == {"nav-0", "nav-1", "nav-2", "nav-3"}
        assert window._browse_warm_timer.isActive()
        assert abs(window._browse_warm_timer.interval() - main._BROWSE_WARM_MS) < 1
        window._on_next_page()  # stepping on: still warm around the new page
        assert set(window._view_lease.cameras) == {"nav-2", "nav-3", "nav-4"}
        window._browse_warm_timer.timeout.emit()
        assert set(window._view_lease.cameras) == {"nav-3"}
    finally:
        window.close()


def test_pointing_at_a_page_dot_starts_its_video_before_the_click(qtbot, monkeypatch):
    window = _window(qtbot, monkeypatch, cams=5)
    try:
        window._page_strip.page_hovered.emit(4)
        assert set(window._view_lease.cameras) == {"nav-0", "nav-4"}
        window._page_strip.page_hovered.emit(-1)  # pointer left the dot
        assert window._hover_warm_timer.isActive(), "lingers briefly"
        assert "nav-4" in window._view_lease.cameras
        window._hover_warm_timer.timeout.emit()
        assert set(window._view_lease.cameras) == {"nav-0"}
        window._page_strip.page_hovered.emit(0)  # the page already shown
        assert set(window._view_lease.cameras) == {"nav-0"}
    finally:
        window.close()


def test_the_page_strip_reports_the_page_under_the_pointer(qtbot):
    from PySide6.QtCore import QPointF
    from PySide6.QtGui import QMouseEvent
    from PySide6.QtWidgets import QApplication

    from neurowatch.widgets.page_strip import PageStripWidget

    strip = PageStripWidget()
    qtbot.addWidget(strip)
    strip.resize(400, strip.height())
    strip.set_state(4, 1)
    strip.show()
    seen: list[int] = []
    strip.page_hovered.connect(seen.append)
    canvas = strip._canvas
    for page in range(4):
        x = next(px for px in range(canvas.width()) if canvas.page_at(px) == page)
        move = QMouseEvent(
            QMouseEvent.Type.MouseMove,
            QPointF(x, canvas.height() / 2),
            QPointF(canvas.mapToGlobal(canvas.rect().center())),
            Qt.MouseButton.NoButton,
            Qt.MouseButton.NoButton,
            Qt.KeyboardModifier.NoModifier,
        )
        QApplication.sendEvent(canvas, move)
    assert seen == [0, 1, 2, 3]
    QApplication.sendEvent(canvas, QEvent(QEvent.Type.Leave))
    assert seen[-1] == -1
    QApplication.sendEvent(strip._next_btn, QEvent(QEvent.Type.Enter))
    assert seen[-1] == 2, "the next arrow points at the next page"


def test_no_warm_up_without_auto_scroll(qtbot, monkeypatch):
    window = _window(qtbot, monkeypatch, cams=3)
    try:
        assert not window._prewarm_timer.isActive()
        window._prewarm_next_pages()
        assert set(window._view_lease.cameras) == {"nav-0"}
    finally:
        window.close()


def test_page_change_with_native_overlays_skips_wall_compositing(qtbot, monkeypatch, xcb):
    window = _window(qtbot, monkeypatch, cams=2, native=True)
    try:
        assert window._wall.has_native_surfaces

        window._on_next_page()

        assert window._wall.page == 1, "native overlays switch pages immediately"
        assert window._wall.graphicsEffect() is None
        assert window._toolbar._page_label.text() == "Page 2 / 2"
    finally:
        window.close()


def test_expanded_tile_toggles_back_to_original_wall_page(qtbot, monkeypatch, fake_readers):
    window = _window(qtbot, monkeypatch, cams=5, rows=2, cols=2)
    try:
        window._wall.set_page(1)
        assert window._wall.page == 1
        assert set(window._wall._readers) == {f"nav-{i}" for i in range(5)}

        window._on_expand_cell("nav-4")
        assert window._pre_solo_grid == (2, 2, 1)
        assert window._wall.page_count == 1
        assert window._wall._cells[0]._camera.id == "nav-4"
        assert set(window._wall._readers) == {f"nav-{i}" for i in range(5)}

        window._wall.cell_expand_requested.emit("nav-4")
        assert window._pre_solo_grid is None
        assert (window._wall._rows, window._wall._cols) == (2, 2)
        assert window._wall.page == 1
        assert window._wall._cells[0]._camera.id == "nav-4"
    finally:
        window.close()


def test_home_end_keys_jump_to_first_and_last_page(qtbot, monkeypatch):
    from PySide6.QtCore import QEvent
    from PySide6.QtGui import QKeyEvent

    def key(k: Qt.Key) -> QKeyEvent:
        return QKeyEvent(QEvent.Type.KeyPress, k, Qt.KeyboardModifier.NoModifier)

    window = _window(qtbot, monkeypatch, cams=5)
    try:
        window.keyPressEvent(key(Qt.Key.Key_End))
        assert window._wall.page == 4
        window.keyPressEvent(key(Qt.Key.Key_Home))
        assert window._wall.page == 0
    finally:
        window.close()
        QApplication.processEvents()


def test_saving_a_camera_keeps_the_wall_on_its_page() -> None:
    """A saved, added or removed camera (and a settings reload) re-lays the
    wall; it jumped back to the first page."""
    from unittest.mock import MagicMock

    from neurowatch.main import MainWindow

    window = MagicMock()
    window._wall.page = 2
    MainWindow._refresh_walls(window)
    assert window._wall.set_cameras.call_args.kwargs["page"] == 2
    MainWindow._refresh_walls(window, page=0)  # a caller that picks a page still does
    assert window._wall.set_cameras.call_args.kwargs["page"] == 0
