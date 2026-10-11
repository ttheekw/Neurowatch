"""Editing the live wall with the mouse: drag to swap/move, corner to resize."""

from __future__ import annotations

from typing import Any

import pytest
from PySide6.QtCore import QEvent, QObject, QPoint, QPointF, Qt, Signal
from PySide6.QtGui import QKeyEvent, QMouseEvent
from PySide6.QtWidgets import QApplication, QMenu

from neurowatch.models.camera import CameraConfig, WallPosition
from neurowatch.models.wall_layout import Placement
from neurowatch.widgets.cell_widget import CellWidget
from neurowatch.widgets.video_wall import VideoWallWidget


class _FakeFrameReaderThread(QObject):
    frame_ready = Signal(str, bytes, int, int, str)

    def __init__(self, cam_id: str, max_emit_fps: float | None = None, parent=None) -> None:
        super().__init__(parent)

    def start(self) -> None:
        pass

    def stop(self) -> None:
        pass

    def set_max_emit_fps(self, value: float | None) -> None:
        pass


@pytest.fixture(autouse=True)
def fake_readers(monkeypatch) -> None:
    monkeypatch.setattr("neurowatch.widgets.video_wall.FrameReaderThread", _FakeFrameReaderThread)


def _cams(n: int, spans: dict[int, tuple[int, int]] | None = None) -> list[CameraConfig]:
    cams = [CameraConfig(id=f"cam-{i}", name=f"Cam {i}", url="/missing.avi") for i in range(n)]
    for i, (rs, cs) in (spans or {}).items():
        cams[i].wall_position = WallPosition(row_span=rs, col_span=cs)
    return cams


def _wall(qtbot, cams: list[CameraConfig], rows: int = 3, cols: int = 3) -> VideoWallWidget:
    wall = VideoWallWidget(viewer_client=None)
    wall._native_available = False
    qtbot.addWidget(wall)
    wall.set_grid(rows, cols)
    wall.set_cameras(cams)
    wall.resize(960, 600)
    wall.show()
    qtbot.waitExposed(wall)
    wall._grid.activate()
    return wall


def _send(cell: CellWidget, kind: QEvent.Type, wall_pos: QPoint, wall: VideoWallWidget) -> None:
    held = (
        Qt.MouseButton.NoButton
        if kind == QEvent.Type.MouseButtonRelease
        else Qt.MouseButton.LeftButton
    )
    local = cell.mapFrom(wall, wall_pos)
    event = QMouseEvent(
        kind,
        QPointF(local),
        QPointF(wall.mapToGlobal(wall_pos)),
        Qt.MouseButton.LeftButton if kind != QEvent.Type.MouseMove else Qt.MouseButton.NoButton,
        held,
        Qt.KeyboardModifier.NoModifier,
    )
    QApplication.sendEvent(cell, event)


def _drag(wall: VideoWallWidget, cell: CellWidget, start: QPoint, *stops: QPoint) -> None:
    """Press on ``cell`` at ``start`` (wall coords) and move through ``stops``."""
    _send(cell, QEvent.Type.MouseButtonPress, start, wall)
    for stop in stops:
        _send(cell, QEvent.Type.MouseMove, stop, wall)


def _center(cell: CellWidget) -> QPoint:
    return cell.geometry().center()


def _grip(cell: CellWidget) -> QPoint:
    return cell.geometry().bottomRight() - QPoint(4, 4)


def _edits(wall: VideoWallWidget) -> list[tuple[list[str], Any, int]]:
    """Each requested edit as (camera ids by position, spans, page)."""
    edits: list[tuple[list[str], Any, int]] = []
    wall.layout_edit_requested.connect(
        lambda placements, spans, page: edits.append(([p.cam_id for p in placements], spans, page))
    )
    return edits


def _apply_edits(wall: VideoWallWidget, cams: list[CameraConfig]) -> None:
    """Store requested edits like MainWindow._on_wall_layout_edit does."""

    def apply(placements: list[Placement], spans: dict, page: int) -> None:
        by_id = {c.id: c for c in cams}
        for p in placements:
            rs, cs = spans.get(p.cam_id, p.span)
            by_id[p.cam_id].wall_position = WallPosition(p.page, p.row, p.col, rs, cs)
        wall.set_cameras([by_id[p.cam_id] for p in placements], page=page)

    wall.layout_edit_requested.connect(apply)


def _empty_slots(wall: VideoWallWidget) -> int:
    return len(wall._slot_geometry) - len(wall._page_cameras())


def test_spanning_tile_takes_four_cells_and_overflows_three_cameras(qtbot) -> None:
    wall = _wall(qtbot, _cams(9, {0: (2, 2)}))

    assert wall.page_count == 2
    assert [c.id for c in wall._page_cameras()] == [f"cam-{i}" for i in range(6)]
    assert wall._slot_geometry[:2] == [(0, 0, 2, 2), (0, 2, 1, 1)]
    big, small = wall._cells[0], wall._cells[1]
    assert big.width() > small.width() * 1.8 and big.height() > small.height() * 1.8
    assert sum(cell.isVisible() for cell in wall._cells) == 6

    wall.set_page(1)
    assert [c.id for c in wall._page_cameras()] == ["cam-6", "cam-7", "cam-8"]
    assert wall._slot_geometry[0] == (0, 0, 1, 1)
    assert sum(cell.isVisible() for cell in wall._cells) == 9  # 3 tiles + 6 empty slots


def test_drag_a_tile_onto_another_swaps_them(qtbot) -> None:
    wall = _wall(qtbot, _cams(4))
    edits = _edits(wall)
    source, target = wall._cells[0], wall._cells[2]

    _drag(wall, source, _center(source), _center(source) + QPoint(30, 0), _center(target))
    assert wall.layout_drag_active
    assert wall._overlay.isVisible()
    preview = wall._overlay.preview
    assert preview is not None and "Swap Cam 0 ↔ Cam 2" in preview.message
    roles = {t.name: t.role for t in preview.tiles}
    assert roles["Cam 0"] == "active" and roles["Cam 2"] == "partner"

    _send(source, QEvent.Type.MouseButtonRelease, _center(target), wall)

    assert not wall.layout_drag_active and not wall._overlay.isVisible()
    assert edits == [
        (["cam-2", "cam-1", "cam-0", "cam-3"], {f"cam-{i}": (1, 1) for i in range(4)}, 0)
    ]


def test_drop_on_empty_slot_moves_the_tile_there(qtbot) -> None:
    wall = _wall(qtbot, _cams(3))
    edits = _edits(wall)
    source, empty = wall._cells[0], wall._cells[8]  # slot (2, 2) is free
    assert wall._slot_geometry[8] == (2, 2, 1, 1)

    _drag(wall, source, _center(source), _center(empty))
    _send(source, QEvent.Type.MouseButtonRelease, _center(empty), wall)

    assert edits and edits[0][0] == ["cam-1", "cam-2", "cam-0"]


def test_small_move_is_a_click_not_a_drag(qtbot) -> None:
    wall = _wall(qtbot, _cams(2))
    edits = _edits(wall)
    cell = wall._cells[0]
    clicks: list[str] = []
    wall.cell_single_click.connect(clicks.append)

    _drag(wall, cell, _center(cell), _center(cell) + QPoint(1, 1))
    _send(cell, QEvent.Type.MouseButtonRelease, _center(cell), wall)

    assert clicks == ["cam-0"]
    assert not wall._overlay.isVisible() and edits == []


def test_escape_cancels_a_drag(qtbot) -> None:
    wall = _wall(qtbot, _cams(4))
    edits = _edits(wall)
    source, target = wall._cells[0], wall._cells[1]

    _drag(wall, source, _center(source), _center(target))
    assert wall.layout_drag_active
    QApplication.sendEvent(
        wall, QKeyEvent(QEvent.Type.KeyPress, Qt.Key.Key_Escape, Qt.KeyboardModifier.NoModifier)
    )
    assert not wall.layout_drag_active and not wall._overlay.isVisible()
    _send(source, QEvent.Type.MouseButtonRelease, _center(target), wall)

    assert edits == []


def test_drag_corner_resizes_and_previews_the_reflow(qtbot) -> None:
    wall = _wall(qtbot, _cams(9))
    edits = _edits(wall)
    first = wall._cells[0]
    assert first.grip_rect().contains(first.mapFrom(wall, _grip(first)))

    _drag(wall, first, _grip(first), _center(wall._cells[4]))  # cell (1, 1)
    preview = wall._overlay.preview
    assert preview is not None
    assert "Cam 0: 2 × 2" in preview.message
    assert "3 cameras move to the next page" in preview.message
    assert len(preview.tiles) == 6

    _send(first, QEvent.Type.MouseButtonRelease, _center(wall._cells[4]), wall)

    assert len(edits) == 1
    order, spans, page = edits[0]
    assert order == [f"cam-{i}" for i in range(9)]
    assert spans["cam-0"] == (2, 2) and page == 0


def test_making_a_tile_big_then_small_leaves_no_empty_slots(qtbot) -> None:
    cams = _cams(9)
    wall = _wall(qtbot, cams)
    _apply_edits(wall, cams)

    # Big: drag Cam 0's corner to cell (1, 1).
    first = wall._cells[0]
    _drag(wall, first, _grip(first), _center(wall._cells[4]))
    _send(first, QEvent.Type.MouseButtonRelease, _center(wall._cells[4]), wall)
    assert wall._slot_geometry[0] == (0, 0, 2, 2)
    assert wall.page_count == 2 and _empty_slots(wall) == 0

    # Small again: drag the grip back into the tile's own top-left cell.
    big = wall._cells[0]
    top_left = wall._grid.cellRect(0, 0).center()
    _drag(wall, big, _grip(big), top_left)
    _send(big, QEvent.Type.MouseButtonRelease, top_left, wall)

    assert wall.page_count == 1
    assert [c.id for c in wall._page_cameras()] == [f"cam-{i}" for i in range(9)]
    assert _empty_slots(wall) == 0


def test_tile_too_big_for_its_spot_moves_and_others_fill_its_place(qtbot) -> None:
    cams = _cams(9)
    wall = _wall(qtbot, cams)
    _apply_edits(wall, cams)

    wall.set_tile_size("cam-2", 2, 2)  # top-right tile: no room to grow there

    assert _empty_slots(wall) == 0
    by_id = {p.cam_id: p for p in wall.placements()}
    assert by_id["cam-2"].slot == (1, 0, 2, 2)
    assert by_id["cam-3"].slot == (0, 2, 1, 1)


def test_saved_layout_is_shown_exactly_after_reload(qtbot) -> None:
    cams = _cams(9)
    wall = _wall(qtbot, cams)
    _apply_edits(wall, cams)
    wall.set_tile_size("cam-2", 2, 2)
    shown = wall.placements()

    reloaded = [CameraConfig.from_dict(c.to_dict()) for c in reversed(cams)]
    other = _wall(qtbot, reloaded)

    assert other.placements() == shown


def test_resize_is_clamped_to_the_grid_edge(qtbot) -> None:
    wall = _wall(qtbot, _cams(3))
    edits = _edits(wall)
    last = wall._cells[2]  # top-right tile (0, 2)

    beyond = wall.rect().bottomRight() + QPoint(200, 200)
    _drag(wall, last, _grip(last), beyond)
    _send(last, QEvent.Type.MouseButtonRelease, beyond, wall)

    assert edits[0][1]["cam-2"] == (3, 1)  # full height, one column (grid edge)


def test_layout_is_not_editable_in_solo_view(qtbot) -> None:
    wall = _wall(qtbot, _cams(3))
    assert wall._cells[0].layout_editable
    wall.set_solo_camera("cam-1")
    wall.set_grid(1, 1)
    assert not wall._cells[0].layout_editable
    edits = _edits(wall)
    wall.set_tile_size("cam-1", 2, 2)
    wall.drop_camera("cam-0", 0)
    assert edits == []


def test_tile_size_menu_requests_a_size(qtbot) -> None:
    wall = _wall(qtbot, _cams(4))
    edits = _edits(wall)
    menu = wall._cells[1]._build_context_menu()
    size_menu = next(m for m in menu.findChildren(QMenu) if m.title() == "Tile Size")
    labels = [a.text() for a in size_menu.actions()]
    assert labels[:3] == ["1 × 1", "2 × 2", "3 × 3"]
    assert size_menu.actions()[0].isChecked()

    next(a for a in size_menu.actions() if a.text() == "2 × 2").trigger()

    assert edits and edits[0][1]["cam-1"] == (2, 2)


def test_sidebar_drag_previews_then_drops_as_a_swap(qtbot) -> None:
    wall = _wall(qtbot, _cams(12))  # cam-10 is on page 2
    edits = _edits(wall)
    target = wall._cells[4]

    target.external_drag_moved.emit("cam-10", wall.mapToGlobal(_center(target)))
    preview = wall._overlay.preview
    assert wall._overlay.isVisible() and preview is not None
    assert "Swap Cam 10 ↔ Cam 4" in preview.message
    assert "Esc" not in preview.message  # the platform drag handles Esc

    wall.drop_camera("cam-10", 4)

    assert not wall._overlay.isVisible()
    order = edits[0][0]
    assert order[4] == "cam-10" and order[10] == "cam-4"


def test_page_change_cancels_a_drag(qtbot) -> None:
    wall = _wall(qtbot, _cams(12))
    source = wall._cells[0]
    _drag(wall, source, _center(source), _center(wall._cells[3]))
    assert wall.layout_drag_active
    wall.set_page(1)
    assert not wall.layout_drag_active and not wall._overlay.isVisible()


def test_set_cameras_can_keep_the_shown_page(qtbot) -> None:
    cams = _cams(12)
    wall = _wall(qtbot, cams)
    wall.set_cameras(cams, page=1)
    assert wall.page == 1
    wall.set_cameras(cams, page=7)
    assert wall.page == 1  # clamped to the last page
    wall.set_cameras(cams)
    assert wall.page == 0


# ── Moving a tile to another page: hold it at the left or right edge ─────


def _send_wall(wall: VideoWallWidget, kind: QEvent.Type, wall_pos: QPoint) -> None:
    """A mouse event the wall receives itself (it grabs the mouse once a
    held tile brings another page into view)."""
    held = (
        Qt.MouseButton.NoButton
        if kind == QEvent.Type.MouseButtonRelease
        else Qt.MouseButton.LeftButton
    )
    event = QMouseEvent(
        kind,
        QPointF(wall_pos),
        QPointF(wall.mapToGlobal(wall_pos)),
        Qt.MouseButton.LeftButton if kind != QEvent.Type.MouseMove else Qt.MouseButton.NoButton,
        held,
        Qt.KeyboardModifier.NoModifier,
    )
    QApplication.sendEvent(wall, event)


def _right_edge(wall: VideoWallWidget) -> QPoint:
    return QPoint(wall.width() - 10, wall.height() // 2)


def _left_edge(wall: VideoWallWidget) -> QPoint:
    return QPoint(10, wall.height() // 2)


def test_holding_a_tile_at_the_right_edge_moves_it_onto_the_next_page(qtbot) -> None:
    cams = _cams(12)  # 3x3: page 1 has cam-0..8, page 2 cam-9..11
    wall = _wall(qtbot, cams)
    edits = _edits(wall)
    shown: list[int] = []
    wall.drag_page_changed.connect(shown.append)
    source = wall._cells[0]

    _drag(wall, source, _center(source), _center(source) + QPoint(30, 0))
    preview = wall._overlay.preview
    assert preview is not None
    assert [(e.side, e.label, e.active) for e in preview.edges] == [(1, "Page 2", False)]

    _send(source, QEvent.Type.MouseMove, _right_edge(wall), wall)
    preview = wall._overlay.preview
    assert preview is not None and preview.edges[0].active
    assert "Hold to show page 2" in preview.message
    qtbot.waitUntil(lambda: wall.page == 1, timeout=2000)
    assert shown == [1]
    from PySide6.QtWidgets import QWidget

    assert QWidget.mouseGrabber() is wall
    assert wall.layout_drag_active

    # The wall carries the drag on: drop Cam 0 onto Cam 10 on page 2.
    target = wall._cells[1]
    _send_wall(wall, QEvent.Type.MouseMove, _center(target))
    preview = wall._overlay.preview
    assert preview is not None and "Swap Cam 0 ↔ Cam 10" in preview.message
    assert [(e.side, e.label) for e in preview.edges] == [(-1, "Page 1")]
    _send_wall(wall, QEvent.Type.MouseButtonRelease, _center(target))

    assert not wall.layout_drag_active and QWidget.mouseGrabber() is None
    order, _spans, page = edits[-1]
    assert page == 1
    assert order.index("cam-10") == 0 and order.index("cam-0") == 10


def test_a_tile_can_go_back_a_page_and_only_where_pages_are(qtbot) -> None:
    cams = _cams(12)
    wall = _wall(qtbot, cams)
    _apply_edits(wall, cams)
    wall.set_page(1)
    source = wall._cells[0]  # Cam 9

    _drag(wall, source, _center(source), _center(source) + QPoint(30, 0), _right_edge(wall))
    preview = wall._overlay.preview
    assert preview is not None and [e.side for e in preview.edges] == [-1]  # no page 3
    qtbot.wait(800)
    assert wall.page == 1  # nothing to the right

    _send(source, QEvent.Type.MouseMove, _left_edge(wall), wall)
    qtbot.waitUntil(lambda: wall.page == 0, timeout=2000)
    _send_wall(wall, QEvent.Type.MouseMove, _center(wall._cells[4]))  # onto Cam 4
    _send_wall(wall, QEvent.Type.MouseButtonRelease, _center(wall._cells[4]))
    assert wall.page == 0
    assert [c.id for c in wall._page_cameras()][4] == "cam-9"


def test_esc_after_a_page_flip_cancels_and_frees_the_mouse(qtbot) -> None:
    from PySide6.QtWidgets import QWidget

    wall = _wall(qtbot, _cams(12))
    edits = _edits(wall)
    source = wall._cells[0]
    _drag(wall, source, _center(source), _center(source) + QPoint(30, 0), _right_edge(wall))
    qtbot.waitUntil(lambda: wall.page == 1, timeout=2000)

    QApplication.sendEvent(
        wall, QKeyEvent(QEvent.Type.KeyPress, Qt.Key.Key_Escape, Qt.KeyboardModifier.NoModifier)
    )
    assert not wall.layout_drag_active and QWidget.mouseGrabber() is None
    assert edits == []


def test_resizing_never_changes_page(qtbot) -> None:
    wall = _wall(qtbot, _cams(12))
    cell = wall._cells[2]  # top-right tile: its grip is at the right edge
    _drag(wall, cell, _grip(cell), _right_edge(wall))
    preview = wall._overlay.preview
    assert preview is not None and preview.edges == []
    qtbot.wait(800)
    assert wall.page == 0
