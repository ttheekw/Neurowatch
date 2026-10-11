"""New Window: move a wall page to its own window, and back when it closes."""

from __future__ import annotations

from collections.abc import Iterator
from typing import ClassVar

import pytest
from PySide6.QtCore import QEvent, QPoint, Qt
from PySide6.QtGui import QKeyEvent
from PySide6.QtWidgets import QApplication, QLabel, QPushButton

from neurowatch.dialogs.new_window_dialog import NewWindowDialog, PageSummary, WindowSummary
from neurowatch.dialogs.settings_dialog import SettingsDialog
from neurowatch.main import MainWindow
from neurowatch.models.camera import CameraConfig
from neurowatch.widgets.detached_wall import DetachedWallWindow
from neurowatch.widgets.window_chrome import GRIP_WIDTH, ChromeTitleBar


class _FakeClient:
    _endpoint = "unix:/nonexistent/test-detached.sock"
    calls: ClassVar[list[tuple[str, object]]] = []

    def __init__(self, endpoint: str | None = None) -> None:
        pass

    def is_online(self) -> bool:
        return True

    def call(self, method, params=None):
        _FakeClient.calls.append((method, params))
        if method == "config.get":
            return {
                "version": 3,
                "site_name": "Test Site",
                "wall": {"rows": 3, "cols": 3, "auto_fit": True, "page_interval_sec": 0},
                "ui": {"theme": "dark", "fullscreen": False},
            }
        if method in ("camera.list",):
            return []
        if method == "camera.status_all":
            return {}
        return {"ok": True}


@pytest.fixture
def win(monkeypatch, qtbot) -> Iterator[MainWindow]:
    _FakeClient.calls = []
    monkeypatch.setattr("neurowatch.main.SyncIpcClient", _FakeClient)
    monkeypatch.setattr("neurowatch.ipc.frame_reader.SHM_PREFIX", "/nonexistent/nw-test-")
    window = MainWindow()
    qtbot.addWidget(window)
    yield window
    window.close()
    # Delete it now: every MainWindow restyles all live widgets on startup,
    # so windows left for a later event loop slow down each following test.
    window.deleteLater()
    QApplication.sendPostedEvents(None, QEvent.Type.DeferredDelete)


def _load(win: MainWindow, count: int) -> list[str]:
    win._cameras = [
        CameraConfig(id=f"cam-{i}", name=f"Cam {i}", url="rtsp://x") for i in range(count)
    ]
    win._refresh_all()
    return [c.id for c in win._cameras]


def _ids(start: int, stop: int) -> list[str]:
    return [f"cam-{i}" for i in range(start, stop)]


def _page_ids(win: MainWindow, page: int) -> list[str]:
    return [p.cam_id for p in win._wall.placements() if p.page == page]


def test_new_window_needs_a_page_to_spare(win) -> None:
    _load(win, 9)
    assert not win._toolbar._new_window_btn.isEnabled()
    assert "at least two pages" in win._toolbar._new_window_btn.toolTip()
    assert win._open_detached_window(0) is None

    _load(win, 18)
    assert win._toolbar._new_window_btn.isEnabled()


def test_dialog_offers_every_page_and_picks_the_last(qtbot) -> None:
    pages = [
        PageSummary(i, [((0, 0, 1, 1), f"Cam {i * 9 + j}") for j in range(9)]) for i in range(3)
    ]
    dlg = NewWindowDialog(pages, 3, 3)
    qtbot.addWidget(dlg)
    assert len(dlg._cards) == 3
    assert dlg.selected_page == 2
    qtbot.mouseClick(dlg._cards[0], Qt.MouseButton.LeftButton)
    assert dlg.selected_page == 0
    assert dlg._cards[1].accessibleName() == "Page 2, 9 cameras"


def test_moving_a_page_opens_a_window_with_its_cameras(win) -> None:
    _load(win, 18)
    before = {p.cam_id: p.slot for p in win._wall.placements()}

    window = win._open_detached_window(1)

    assert isinstance(window, DetachedWallWindow) and window.isVisible()
    assert window.camera_ids == _ids(9, 18)
    assert {p.cam_id: p.slot for p in window.wall.placements()} == {
        cam_id: before[cam_id] for cam_id in _ids(9, 18)
    }
    assert window.windowTitle() == "NeuroWatch — Window 2 · 9 cameras"
    # No desktop border: the window draws the app's own title strip.
    assert window.windowFlags() & Qt.WindowType.FramelessWindowHint
    assert window._chrome.isVisible()
    assert window._chrome._title.text() == "NeuroWatch — Window 2 · 9 cameras"
    assert [c.id for c in win._wall._page_cameras()] == _ids(0, 9)
    assert win._wall.page_count == 1
    assert not win._toolbar._new_window_btn.isEnabled()
    # Each camera is read by one window only, and all visible ones decode.
    assert set(win._wall._readers).isdisjoint(window.wall._readers)
    assert set(window.wall._readers) == set(_ids(9, 18))
    assert set(win._view_lease.cameras) == set(_ids(0, 18))


def test_closing_the_window_puts_cameras_back_where_they_were(win, qtbot) -> None:
    _load(win, 27)
    before = win._wall.placements()
    window = win._open_detached_window(1)
    assert window is not None
    assert _page_ids(win, 1) == _ids(18, 27)  # page 3 moved up meanwhile

    window.close()

    assert win._detached == []
    assert win._wall.placements() == before
    assert win._toolbar._new_window_btn.isEnabled()
    assert set(win._view_lease.cameras) == set(win._wall.desktop_demand())


def test_cameras_come_back_at_the_end_after_the_main_layout_changed(win, qtbot) -> None:
    _load(win, 27)
    window = win._open_detached_window(1)
    assert window is not None

    win._wall.set_tile_size("cam-0", 2, 2)  # main wall: 18 cameras, reflowed
    qtbot.waitUntil(lambda: win._positions_worker is None, timeout=2000)
    saved = [p for m, p in _FakeClient.calls if m == "camera.set_positions"][-1]
    parked = {e["id"]: e["wall_position"]["page"] for e in saved if e["id"] in _ids(9, 18)}
    assert set(parked.values()) == {3}  # after the main wall's three pages

    window.close()

    order = [p.cam_id for p in win._wall.placements()]
    assert order[-9:] == _ids(9, 18)
    # They first fill the free cells of the main wall's last page.
    assert len(win._wall._page_cameras()) == 6  # 2x2 tile + 5
    win._wall.set_page(2)
    assert len(win._wall._page_cameras()) == len(win._wall._slot_geometry) == 9


def test_several_windows_can_be_open(win) -> None:
    _load(win, 27)
    before = win._wall.placements()
    first = win._open_detached_window(2)
    second = win._open_detached_window(1)
    assert first is not None and second is not None
    assert (first.number, second.number) == (2, 3)
    assert first.camera_ids == _ids(18, 27) and second.camera_ids == _ids(9, 18)
    assert win._wall.page_count == 1 and not win._can_open_window()

    first.close()
    assert win._wall.page_count == 2
    third = win._open_detached_window(1)
    assert third is not None and third.number == 2  # numbers are reused

    second.close()
    third.close()
    assert win._wall.placements() == before


def test_status_and_recording_reach_tiles_in_other_windows(win) -> None:
    _load(win, 18)
    window = win._open_detached_window(1)
    assert window is not None

    win._on_poll_done({"cam-12": "offline"}, {}, {"cam-12": "recording"}, True)

    cell = window.wall._cell_for_cam_id("cam-12")
    assert cell is not None
    assert cell._status == "offline" and cell._recording_state == "recording"


def test_window_tiles_can_be_viewed_alone(win) -> None:
    _load(win, 18)
    window = win._open_detached_window(1)
    assert window is not None
    assert all(c.layout_editable for c in window.wall._cells if c._camera is not None)

    window.wall.cell_expand_requested.emit("cam-12")
    assert window.wall._solo_camera_id == "cam-12" and window.wall.capacity == 1
    assert window.desktop_demand() == ["cam-12"]
    QApplication.sendEvent(
        window, QKeyEvent(QEvent.Type.KeyPress, Qt.Key.Key_Escape, Qt.KeyboardModifier.NoModifier)
    )
    assert window.wall._solo_camera_id is None and window.wall.capacity == 9

    window.showMinimized()
    assert window.desktop_demand() == []


# ── Each window's own grid and layout ───────────────────────────────────


def test_tiles_move_and_resize_in_a_window_without_touching_the_main_layout(win) -> None:
    _load(win, 18)
    window = win._open_detached_window(1)
    assert window is not None
    _FakeClient.calls = []

    window.wall.set_tile_size("cam-9", 2, 2)  # first tile, as from its corner
    assert window.pages()[0]["cam-9"].row_span == 2
    assert window.page_count == 2  # three cameras flowed to a second page
    assert window.cycling

    window.wall.drop_camera("cam-17", 0)  # swap with the big tile
    first = window.pages()[0]
    assert (first["cam-17"].row_span, first["cam-17"].col_span) == (2, 2)
    assert "cam-9" in window.camera_ids

    # The main wall's saved layout is not the window's.
    assert not [m for m, _p in _FakeClient.calls if m == "camera.set_positions"]
    cam9 = next(c for c in win._cameras if c.id == "cam-9")
    assert cam9.tile_span == (1, 1)

    window.close()
    assert win._wall.page_count == 2
    assert _page_ids(win, 1) == _ids(9, 18)  # back where they were, as they were


def test_a_new_window_can_have_its_own_grid(win) -> None:
    _load(win, 18)
    window = win._open_detached_window(1, grid=(2, 2))
    assert window is not None
    assert window.grid == (2, 2) and window.wall.capacity == 4
    assert window.page_count == 3  # nine cameras at 2×2
    assert window._controls.grid.text() == "⊞ 2×2"
    assert window.cycling
    assert win._wall.capacity == 9  # the main wall keeps its grid


def test_the_grid_menu_relays_the_window_out_keeping_what_is_on_screen(win, qtbot) -> None:
    _load(win, 18)
    window = win._open_detached_window(1, grid=(2, 2))
    assert window is not None
    window.go_to_page(2)
    qtbot.waitUntil(lambda: window.page == 2, timeout=2000)
    on_screen = window.page_camera_ids(2)

    three = next(a for a in window._controls.grid_menu.actions() if a.data() == (3, 3))
    three.trigger()

    assert window.grid == (3, 3) and window.page_count == 1
    assert set(on_screen) <= set(window.page_camera_ids(window.page))
    assert three.isChecked() and window._controls.grid.text() == "⊞ 3×3"
    assert not window.cycling  # one page: nothing to cycle
    assert not win._autoscroll_timer.isActive()


def test_pages_added_later_flow_into_the_windows_grid(win) -> None:
    _load(win, 27)
    window = win._open_detached_window(2, grid=(4, 4))
    assert window is not None and window.page_count == 1  # 9 of 16 cells
    assert win._move_page_to_window(1, window)
    assert window.page_count == 2
    assert window.page_camera_ids(0)[-7:] == _ids(9, 16)  # free cells filled first
    assert window.page_camera_ids(1) == _ids(16, 18)


def test_the_page_clock_skips_a_window_while_a_tile_is_dragged(win) -> None:
    window = _two_page_window(win)
    window.wall._layout_drag = object()  # type: ignore[assignment]
    window.advance()
    assert window.page == 1
    window.wall._layout_drag = None


def test_dialog_picks_the_new_windows_grid(qtbot) -> None:
    tiles = [((r, c, 1, 1), "Cam") for r in range(3) for c in range(3)]
    pages = [PageSummary(0, tiles), PageSummary(1, tiles)]
    dlg = NewWindowDialog(pages, 3, 3, windows=[WindowSummary(2, 1, 9)])
    qtbot.addWidget(dlg)
    dlg.show()
    assert dlg.selected_grid == (3, 3)  # the main grid by default
    assert dlg._grid_hint.text() == "Page 2: 9 cameras on 1 page at 3×3."
    two = next(b for b in dlg._grids.buttons() if b.text() == "2×2")
    two.click()
    assert dlg.selected_grid == (2, 2)
    assert dlg._grid_hint.text() == (
        "Page 2: 9 cameras on 3 pages at 2×2 — the window cycles through them."
    )
    dlg._destination_cards[1].click()  # an open window keeps its own grid
    assert not dlg._grid_box.isVisible()
    dlg._destination_cards[0].click()
    assert dlg._grid_box.isVisible()

    custom = NewWindowDialog(pages, 2, 5)  # a custom main grid is offered first
    qtbot.addWidget(custom)
    assert custom.selected_grid == (2, 5)
    assert custom._grids.buttons()[0].text() == "5×2"


def test_new_window_opens_with_the_grid_chosen_in_the_dialog(win, monkeypatch) -> None:
    _load(win, 18)

    def accept(dlg: NewWindowDialog) -> int:
        next(b for b in dlg._grids.buttons() if b.text() == "2×2").click()
        return NewWindowDialog.DialogCode.Accepted

    monkeypatch.setattr(NewWindowDialog, "exec", accept)
    win._on_new_window()
    assert len(win._detached) == 1
    assert win._detached[0].grid == (2, 2)


def test_edits_reach_the_window_and_an_empty_window_closes(win, qtbot) -> None:
    _load(win, 10)
    window = win._open_detached_window(1)
    assert window is not None and window.camera_ids == ["cam-9"]

    win._cameras[9].name = "Gate"
    win._refresh_all()
    assert window.wall._cell_for_cam_id("cam-9")._camera.name == "Gate"  # type: ignore[union-attr]

    win._cameras = win._cameras[:9]  # cam-9 removed
    win._refresh_all()
    qtbot.waitUntil(lambda: not win._detached, timeout=2000)
    assert [c.id for c in win._wall._page_cameras()] == _ids(0, 9)


def test_closing_the_main_window_closes_the_others(win) -> None:
    _load(win, 18)
    window = win._open_detached_window(1)
    assert window is not None
    win.shutdown()
    assert not window.isVisible() and win._detached == []


def test_settings_and_new_window_dialogs_have_no_desktop_border(qtbot) -> None:
    pages = [PageSummary(i, [((0, 0, 1, 1), "Cam")]) for i in range(2)]
    headings = {SettingsDialog: "Settings", NewWindowDialog: "Move a page to a new window"}
    for dlg in (SettingsDialog({}), NewWindowDialog(pages, 1, 1)):
        qtbot.addWidget(dlg)
        assert dlg.windowFlags() & Qt.WindowType.FramelessWindowHint
        bar = dlg.findChild(ChromeTitleBar)
        # The header carries the heading; the body does not repeat it.
        assert bar is not None and bar._title.text() == headings[type(dlg)]
        assert not [w for w in dlg.findChildren(QLabel) if w.objectName() == "dialogTitle"]
        close = next(b for b in bar.findChildren(QPushButton) if b.property("role") == "close")
        dlg.show()
        qtbot.mouseClick(close, Qt.MouseButton.LeftButton)
        assert not dlg.isVisible()


def test_window_resizes_from_its_edges_and_drops_grips_when_maximized(win) -> None:
    _load(win, 18)
    window = win._open_detached_window(1)
    assert window is not None
    window.resize(800, 500)
    resizer = window._resizer
    assert resizer.edges_at(QPoint(1, 250)) == Qt.Edge.LeftEdge
    assert resizer.edges_at(QPoint(799, 499)) == Qt.Edge.RightEdge | Qt.Edge.BottomEdge
    assert resizer.edges_at(QPoint(400, 250)) == Qt.Edge(0)
    assert window._layout.contentsMargins().left() == GRIP_WIDTH

    window.showMaximized()
    assert window._layout.contentsMargins().left() == 0
    assert resizer.edges_at(QPoint(1, 250)) == Qt.Edge(0)
    window.showFullScreen()
    assert not window._chrome.isVisible()


def test_tiles_keep_their_picture_and_reader_when_moving_between_windows(win) -> None:
    _load(win, 18)
    frame = bytes(2 * 2 * 3)
    win._wall._on_frame_ready("cam-12", frame, 2, 2, "rgb")  # seen earlier
    reader = win._wall._readers["cam-12"]
    panel_rebuilds: list[int] = []
    win._camera_panel.set_cameras = lambda cams: panel_rebuilds.append(len(cams))  # type: ignore[method-assign]

    window = win._open_detached_window(1)
    assert window is not None
    assert window.wall._readers["cam-12"] is reader  # not stopped and restarted
    cell = window.wall._cell_for_cam_id("cam-12")
    assert cell is not None and cell._pixmap is not None  # a picture at once
    assert cell._refreshing  # until its next frame arrives
    window.wall._on_frame_ready("cam-12", frame, 2, 2, "rgb")
    assert "cam-12" in window.wall._last_frame_by_id

    window.close()
    assert win._wall._readers["cam-12"] is reader
    assert "cam-12" in win._wall._last_frame_by_id
    assert panel_rebuilds == []  # moving cameras leaves the sidebar alone


# ── Several pages in one window ─────────────────────────────────────────


def _two_page_window(win: MainWindow) -> DetachedWallWindow:
    """Your example: 3 pages; page 2 to a new window, then the page left
    after page 1 (the former page 3) into that same window."""
    _load(win, 27)
    window = win._open_detached_window(1)
    assert window is not None
    assert win._toolbar._new_window_btn.isEnabled()  # main still has 2 pages
    assert win._move_page_to_window(1, window)
    return window


def test_a_remaining_page_can_join_an_open_window(win) -> None:
    window = _two_page_window(win)

    assert window.page_count == 2
    assert window.page_camera_ids(0) == _ids(9, 18)
    assert window.page_camera_ids(1) == _ids(18, 27)
    assert window.page == 1  # shows the page it just got
    assert [c.id for c in window.wall._page_cameras()] == _ids(18, 27)
    assert win._wall.page_count == 1 and [c.id for c in win._wall._page_cameras()] == _ids(0, 9)
    assert not win._toolbar._new_window_btn.isEnabled()
    assert not window._controls.add_page.isEnabled()  # main keeps its last page
    # Pages stay whole, and only the page on screen is decoded.
    assert window.wall.page_count == 2
    assert window.desktop_demand() == sorted(_ids(18, 27))
    assert set(win._view_lease.cameras) == set(_ids(0, 9)) | set(_ids(18, 27))
    assert window.windowTitle() == "NeuroWatch — Window 2 · 18 cameras"


def test_a_window_with_two_pages_cycles_on_the_shared_page_clock(win, qtbot) -> None:
    window = _two_page_window(win)
    controls = window._controls
    assert window.cycling and controls._paging.isVisible()
    assert controls.indicator.text() == "Page 2 / 2"
    assert controls.cycle.text() == "⏸ 10 s"
    assert not hasattr(window, "_progress")  # no loading bar under the strip
    # The main window's clock runs for it even with auto-scroll off there.
    assert not win._main_autoscroll_on()
    assert win._autoscroll_timer.isActive() and win._autoscroll_timer.interval() == 10_000

    main_page = win._wall.page
    win._on_page_clock()  # one tick
    qtbot.waitUntil(lambda: window.page == 0, timeout=2000)
    assert controls.indicator.text() == "Page 1 / 2"
    assert win._wall.page == main_page  # main auto-scroll is off


def test_every_window_changes_pages_together_at_one_interval(win, qtbot) -> None:
    _load(win, 45)  # 5 pages: two windows of two pages each, main keeps one
    first = win._open_detached_window(4)
    second = win._open_detached_window(3)
    assert first is not None and second is not None
    assert win._move_page_to_window(2, first) and win._move_page_to_window(1, second)
    assert first.page == 1 and second.page == 1

    win._on_page_clock()
    qtbot.waitUntil(lambda: first.page == 0 and second.page == 0, timeout=2000)

    # An interval picked in one window applies to every window.
    thirty = next(a for a in second._controls.interval_menu.actions() if a.data() == 30)
    thirty.trigger()
    assert first._controls.cycle.text() == second._controls.cycle.text() == "⏸ 30 s"
    assert win._autoscroll_timer.interval() == 30_000
    assert win._toolbar._autoscroll_spin.value() == 30  # shown, not switched on
    assert not win._main_autoscroll_on()

    # Pausing is per window: the clock moves only the other one.
    first._controls.cycle.click()
    assert not first.cycling and second.cycling
    win._on_page_clock()
    qtbot.waitUntil(lambda: second.page == 1, timeout=2000)
    assert first.page == 0


def test_windows_follow_the_main_auto_scroll_interval_and_back(win) -> None:
    window = _two_page_window(win)
    win._toolbar.set_auto_scroll(15)
    win._on_auto_scroll_changed(15)
    assert window._controls.cycle.text() == "⏸ 15 s"
    assert win._autoscroll_timer.interval() == 15_000

    sixty = next(a for a in window._controls.interval_menu.actions() if a.data() == 60)
    sixty.trigger()
    assert win._config["wall"]["page_interval_sec"] == 60
    assert win._toolbar._autoscroll_btn.isChecked()
    assert win._toolbar._autoscroll_spin.value() == 60
    assert win._autoscroll_timer.interval() == 60_000


def test_a_page_picked_by_hand_restarts_the_clock_for_everyone(win, qtbot) -> None:
    window = _two_page_window(win)
    win._autoscroll_timer.start(10_000)
    qtbot.wait(300)
    window._controls.next.click()
    qtbot.waitUntil(lambda: window.page == 0, timeout=2000)
    assert win._autoscroll_timer.remainingTime() > 9_500

    QApplication.sendEvent(
        window,
        QKeyEvent(QEvent.Type.KeyPress, Qt.Key.Key_PageDown, Qt.KeyboardModifier.NoModifier),
    )
    qtbot.waitUntil(lambda: window.page == 1, timeout=2000)


def test_the_clock_stops_when_nothing_changes_pages(win) -> None:
    window = _two_page_window(win)
    assert win._autoscroll_timer.isActive()
    window._controls.cycle.click()  # paused
    assert window._controls.cycle.text() == "⏵ 10 s"
    assert not win._autoscroll_timer.isActive()
    window._controls.cycle.click()
    assert win._autoscroll_timer.isActive()
    window._controls.return_page.click()  # one page left: nothing to cycle
    assert not win._autoscroll_timer.isActive()


def test_single_view_pauses_cycling_and_keeps_the_page(win, qtbot) -> None:
    window = _two_page_window(win)
    window.wall.cell_expand_requested.emit("cam-20")
    assert not window.cycling
    assert not window._controls.next.isEnabled()
    QApplication.sendEvent(
        window, QKeyEvent(QEvent.Type.KeyPress, Qt.Key.Key_Escape, Qt.KeyboardModifier.NoModifier)
    )
    assert window.cycling and window.page == 1


def test_return_page_gives_back_just_the_page_on_screen(win) -> None:
    window = _two_page_window(win)
    assert window.page == 1

    window._controls.return_page.click()

    assert window.page_count == 1 and window.camera_ids == _ids(9, 18)
    assert not window.cycling and not window._controls._paging.isVisible()
    assert win._wall.page_count == 2
    assert set(_page_ids(win, 1)) == set(_ids(18, 27))
    assert win._toolbar._new_window_btn.isEnabled()


def test_closing_a_two_page_window_restores_the_main_wall(win) -> None:
    _load(win, 27)
    before = win._wall.placements()
    window = win._open_detached_window(1)
    assert window is not None and win._move_page_to_window(1, window)
    window.close()
    assert win._wall.placements() == before


def test_plus_page_offers_the_main_pages_for_that_window(win, monkeypatch) -> None:
    _load(win, 27)
    window = win._open_detached_window(2)
    assert window is not None
    seen: dict[str, object] = {}

    def accept(dlg: NewWindowDialog) -> int:
        seen["window"] = dlg.selected_window
        seen["ok"] = dlg._buttons.button(dlg._buttons.StandardButton.Ok).text()
        return NewWindowDialog.DialogCode.Accepted

    monkeypatch.setattr(NewWindowDialog, "exec", accept)
    window._controls.add_page.click()

    assert seen == {"window": 2, "ok": "Add to Window 2"}
    assert window.page_count == 2  # the main wall's last page joined it
    assert window.page_camera_ids(1) == _ids(9, 18)


def test_dialog_offers_open_windows_only_when_there_are_some(qtbot) -> None:
    pages = [PageSummary(i, [((0, 0, 1, 1), "Cam")]) for i in range(2)]
    plain = NewWindowDialog(pages, 1, 1)
    qtbot.addWidget(plain)
    assert plain._destination_cards == [] and plain.selected_window is None
    assert plain._buttons.button(plain._buttons.StandardButton.Ok).text() == "Open Window"

    windows = [WindowSummary(2, 1, 9), WindowSummary(3, 2, 18)]
    dlg = NewWindowDialog(pages, 1, 1, windows=windows)
    qtbot.addWidget(dlg)
    assert [c.text() for c in dlg._destination_cards] == ["New window", "Window 2", "Window 3"]
    assert dlg.selected_window is None  # a new window unless one is chosen
    dlg._destination_cards[2].click()
    assert dlg.selected_window == 3
    assert dlg._buttons.button(dlg._buttons.StandardButton.Ok).text() == "Add to Window 3"
    assert "2 pages · 18 cameras" in dlg._destination_cards[2].accessibleName()

    preset = NewWindowDialog(pages, 1, 1, windows=windows, destination=2)
    qtbot.addWidget(preset)
    assert preset.selected_window == 2


def test_main_layout_edit_parks_every_window_page_after_the_main_pages(win, qtbot) -> None:
    window = _two_page_window(win)
    _FakeClient.calls = []
    win._wall.set_tile_size("cam-0", 2, 2)  # main: 9 cameras on 2 pages now
    qtbot.waitUntil(lambda: win._positions_worker is None, timeout=2000)
    saved = {
        e["id"]: e["wall_position"]["page"]
        for e in [p for m, p in _FakeClient.calls if m == "camera.set_positions"][-1]
    }
    assert {saved[c] for c in _ids(9, 18)} == {2}
    assert {saved[c] for c in _ids(18, 27)} == {3}
    assert window.page_count == 2


def test_empty_slots_in_a_window_invite_a_tile_not_a_new_camera(win, monkeypatch) -> None:
    _load(win, 12)
    window = win._open_detached_window(1)  # 3 cameras on a 3×3 window
    assert window is not None
    empty = next(c for c in window.wall._cells if c._camera is None and c.isVisible())
    assert empty._empty_hint == "Drag a tile here"
    opened: list[bool] = []
    monkeypatch.setattr(win, "_on_add_camera", lambda: opened.append(True))
    empty.add_camera_requested.emit()
    assert opened == []  # cameras are added in the main window
    assert win._wall._cells[0]._empty_hint == "Click to Add Camera"
