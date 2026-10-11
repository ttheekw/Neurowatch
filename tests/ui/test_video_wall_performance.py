"""Regression tests for the live-wall performance invariants.

These drive a real ``VideoWallWidget`` (offscreen Qt) with stubbed frame
readers and assert on objects, signals and call counts:

  * frame bursts coalesce to the newest frame and render on the paced tick;
  * frames for unassigned/reassigned cameras never reach the wrong cell;
  * hidden-page readers stay warm at ``_HIDDEN_PAGE_FPS`` and are reused;
  * unchanged camera quality never re-emits ``camera_quality_requested``.
"""

from __future__ import annotations

from typing import ClassVar

import pytest
from PySide6.QtCore import QObject, Signal

from neurowatch.models.camera import CameraConfig
from neurowatch.widgets import cell_widget as cell_widget_module
from neurowatch.widgets.cell_widget import CellWidget
from neurowatch.widgets.video_wall import _HIDDEN_PAGE_FPS, VideoWallWidget

_GRID_FPS = 15.0  # _target_ui_fps() for a multi-tile wall with <= 16 visible cameras


class _FakeFrameReaderThread(QObject):
    frame_ready = Signal(str, bytes, int, int, str)
    instances: ClassVar[list[_FakeFrameReaderThread]] = []

    def __init__(self, cam_id: str, max_emit_fps: float | None = None, parent=None) -> None:
        super().__init__(parent)
        self.cam_id = cam_id
        self.max_emit_fps = max_emit_fps
        self.start_calls = 0
        self.stop_calls = 0
        self.max_emit_fps_updates: list[float | None] = []
        _FakeFrameReaderThread.instances.append(self)

    def start(self) -> None:
        self.start_calls += 1

    def stop(self) -> None:
        self.stop_calls += 1

    def set_max_emit_fps(self, max_emit_fps: float | None) -> None:
        self.max_emit_fps_updates.append(max_emit_fps)
        self.max_emit_fps = max_emit_fps


@pytest.fixture
def fake_reader(monkeypatch) -> type[_FakeFrameReaderThread]:
    _FakeFrameReaderThread.instances = []
    monkeypatch.setattr(
        "neurowatch.widgets.video_wall.FrameReaderThread",
        _FakeFrameReaderThread,
    )
    return _FakeFrameReaderThread


@pytest.fixture
def frame_uploads(monkeypatch) -> list[tuple[CellWidget, bytes]]:
    """Record every CellWidget.update_frame (pixmap upload) while keeping it real."""
    uploads: list[tuple[CellWidget, bytes]] = []
    original = CellWidget.update_frame

    def _spy(self: CellWidget, data: bytes, width: int, height: int, pixel_format: str = "rgb"):
        uploads.append((self, data))
        return original(self, data, width, height, pixel_format)

    monkeypatch.setattr(CellWidget, "update_frame", _spy)
    return uploads


@pytest.fixture
def pixmap_uploads(monkeypatch) -> list[object]:
    """Count real QPixmap.fromImage uploads made by CellWidget.update_frame."""
    uploads: list[object] = []
    original = cell_widget_module.QPixmap

    class _QPixmapSpy:
        def __new__(cls, *args, **kwargs):
            return original(*args, **kwargs)

        @staticmethod
        def fromImage(image, *args, **kwargs):  # noqa: N802 — mirrors Qt's API
            uploads.append(image)
            return original.fromImage(image, *args, **kwargs)

    monkeypatch.setattr(cell_widget_module, "QPixmap", _QPixmapSpy)
    return uploads


def _frame(tag: int) -> bytes:
    # A valid 2x2 RGB24 frame whose first byte identifies it.
    return bytes([tag]) + b"\0" * 11


def _cams(count: int, *, sub: bool = False) -> list[CameraConfig]:
    return [
        CameraConfig(
            id=f"cam-{i:02d}",
            name=f"Camera {i}",
            url=f"rtsp://host/{i}/main",
            sub_url=f"rtsp://host/{i}/sub" if sub else "",
        )
        for i in range(count)
    ]


def _make_wall(qtbot, rows: int, cols: int, cameras: list[CameraConfig]) -> VideoWallWidget:
    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall._native_available = False  # raw-frame (SHM) path only
    wall.set_grid(rows, cols)
    wall.set_cameras(cameras)
    return wall


def _cell_of(wall: VideoWallWidget, cam_id: str) -> CellWidget:
    return wall._cells[wall._cell_by_cam_id[cam_id]]


def _assert_reader_set_retuned(
    wall: VideoWallWidget,
    readers: dict[str, _FakeFrameReaderThread],
    visible_ids: set[str],
    visible_fps: float,
) -> None:
    assert wall._readers == readers
    for cam_id, reader in readers.items():
        assert reader is wall._readers[cam_id]
        assert reader.start_calls == 1, f"{cam_id} reader was restarted"
        assert reader.stop_calls == 0, f"{cam_id} reader was stopped"
        expected = visible_fps if cam_id in visible_ids else _HIDDEN_PAGE_FPS
        assert reader.max_emit_fps == expected, cam_id
        if cam_id not in visible_ids:
            assert reader.max_emit_fps == 2.0, cam_id


# ---------------------------------------------------------------------------
# (1) Burst coalescing and paced render
# ---------------------------------------------------------------------------


def test_frame_burst_is_coalesced_until_render_timer_tick(fake_reader, frame_uploads, qtbot):
    wall = _make_wall(qtbot, 2, 2, _cams(4))
    cell = _cell_of(wall, "cam-00")
    reader = wall._readers["cam-00"]

    # First frame renders immediately so a new camera is not left blank.
    reader.frame_ready.emit("cam-00", _frame(0), 2, 2, "rgb")
    assert frame_uploads == [(cell, _frame(0))]
    frame_uploads.clear()

    # Burst through the real reader signal; the event loop is not run in
    # between, so the render timer cannot fire.
    for tag in range(1, 21):
        reader.frame_ready.emit("cam-00", _frame(tag), 2, 2, "rgb")

    assert frame_uploads == [], "no pixmap upload may happen before the render tick"
    assert wall._pending_frames["cam-00"][0] == _frame(20)
    assert wall._render_timer.isActive()

    # Let the real paced QTimer fire.
    qtbot.waitUntil(lambda: bool(frame_uploads), timeout=1000)
    assert frame_uploads == [(cell, _frame(20))]
    assert wall._pending_frames == {}

    # Further ticks without new frames must not re-render anything.
    qtbot.wait(wall._render_timer.interval() * 3)
    assert frame_uploads == [(cell, _frame(20))]


def test_frame_burst_makes_one_real_pixmap_upload_on_render_tick(
    fake_reader, pixmap_uploads, qtbot
):
    wall = _make_wall(qtbot, 2, 2, _cams(4))
    reader = wall._readers["cam-00"]

    reader.frame_ready.emit("cam-00", _frame(0), 2, 2, "rgb")
    assert len(pixmap_uploads) == 1
    pixmap_uploads.clear()

    for tag in range(1, 21):
        reader.frame_ready.emit("cam-00", _frame(tag), 2, 2, "rgb")

    assert pixmap_uploads == []
    assert wall._pending_frames["cam-00"][0] == _frame(20)

    wall._render_timer.timeout.emit()

    assert len(pixmap_uploads) == 1
    assert wall._pending_frames == {}


def test_render_tick_uploads_one_frame_per_visible_camera(fake_reader, frame_uploads, qtbot):
    wall = _make_wall(qtbot, 2, 2, _cams(4))
    for cam_id, reader in wall._readers.items():
        reader.frame_ready.emit(cam_id, _frame(0), 2, 2, "rgb")  # prime cache
    frame_uploads.clear()

    for tag in range(1, 6):
        for cam_id, reader in wall._readers.items():
            reader.frame_ready.emit(cam_id, _frame(tag), 2, 2, "rgb")
    assert frame_uploads == []

    wall._render_timer.timeout.emit()

    assert sorted((wall._cells.index(c), d) for c, d in frame_uploads) == [
        (i, _frame(5)) for i in range(4)
    ]


# ---------------------------------------------------------------------------
# (2) Unassigned / reassigned cameras cannot hit the wrong cell
# ---------------------------------------------------------------------------


def test_hidden_page_frame_is_cached_but_never_rendered(fake_reader, frame_uploads, qtbot):
    wall = _make_wall(qtbot, 2, 2, _cams(8))
    hidden_reader = wall._readers["cam-05"]
    assert "cam-05" not in wall._cell_by_cam_id

    hidden_reader.frame_ready.emit("cam-05", _frame(1), 2, 2, "rgb")
    hidden_reader.frame_ready.emit("cam-05", _frame(2), 2, 2, "rgb")
    wall._render_timer.timeout.emit()

    assert frame_uploads == []
    assert "cam-05" not in wall._pending_frames
    assert wall._last_frame_by_id["cam-05"][0] == _frame(2)


def test_removed_camera_frame_is_discarded(fake_reader, frame_uploads, qtbot):
    cameras = _cams(4)
    wall = _make_wall(qtbot, 2, 2, cameras)
    removed_reader = wall._readers["cam-00"]

    wall.set_cameras(cameras[1:])
    assert removed_reader.stop_calls == 1
    frame_uploads.clear()

    # A frame already queued by the stopped reader arrives late.
    removed_reader.frame_ready.emit("cam-00", _frame(9), 2, 2, "rgb")
    wall._render_timer.timeout.emit()

    assert frame_uploads == []
    assert "cam-00" not in wall._last_frame_by_id
    assert "cam-00" not in wall._pending_frames


def test_reassigned_cell_never_shows_previous_camera_frame(fake_reader, frame_uploads, qtbot):
    wall = _make_wall(qtbot, 2, 2, _cams(8))
    old_reader = wall._readers["cam-00"]
    cell0 = wall._cells[0]
    old_reader.frame_ready.emit("cam-00", _frame(1), 2, 2, "rgb")
    old_reader.frame_ready.emit("cam-00", _frame(2), 2, 2, "rgb")  # pending
    assert "cam-00" in wall._pending_frames
    frame_uploads.clear()

    wall.set_page(1)  # cell 0 now shows cam-04
    assert cell0._camera is not None and cell0._camera.id == "cam-04"
    assert wall._pending_frames == {}

    # cam-00 is now hidden: late and new frames must not paint cell 0.
    old_reader.frame_ready.emit("cam-00", _frame(3), 2, 2, "rgb")
    wall._render_timer.timeout.emit()
    assert frame_uploads == []
    assert cell0._pixmap is None

    # cam-04's own frame goes to cell 0.
    wall._readers["cam-04"].frame_ready.emit("cam-04", _frame(7), 2, 2, "rgb")
    assert frame_uploads == [(cell0, _frame(7))]


def test_stale_pending_frames_are_dropped_on_assignment_and_status_transitions(
    fake_reader, pixmap_uploads, qtbot
):
    wall = _make_wall(qtbot, 2, 2, _cams(8))

    reader0 = wall._readers["cam-00"]
    reader0.frame_ready.emit("cam-00", _frame(1), 2, 2, "rgb")
    pixmap_uploads.clear()
    reader0.frame_ready.emit("cam-00", _frame(2), 2, 2, "rgb")
    assert "cam-00" in wall._pending_frames

    wall.set_page(1)
    assert wall._pending_frames == {}
    wall._render_timer.timeout.emit()
    assert pixmap_uploads == []

    reader0.frame_ready.emit("cam-00", _frame(3), 2, 2, "rgb")
    wall._render_timer.timeout.emit()
    assert pixmap_uploads == []

    reader4 = wall._readers["cam-04"]
    reader4.frame_ready.emit("cam-04", _frame(4), 2, 2, "rgb")
    pixmap_uploads.clear()
    reader4.frame_ready.emit("cam-04", _frame(5), 2, 2, "rgb")
    assert "cam-04" in wall._pending_frames

    wall.set_grid(1, 2)
    assert wall._pending_frames == {}
    pixmap_uploads.clear()
    wall._render_timer.timeout.emit()
    assert pixmap_uploads == []

    wall.set_solo_camera("cam-05")
    wall._readers["cam-05"].frame_ready.emit("cam-05", _frame(6), 2, 2, "rgb")
    pixmap_uploads.clear()
    wall._readers["cam-05"].frame_ready.emit("cam-05", _frame(7), 2, 2, "rgb")
    assert "cam-05" in wall._pending_frames

    wall.set_solo_camera(None)
    assert wall._pending_frames == {}
    pixmap_uploads.clear()
    wall._render_timer.timeout.emit()
    assert pixmap_uploads == []

    wall._readers["cam-00"].frame_ready.emit("cam-00", _frame(8), 2, 2, "rgb")
    pixmap_uploads.clear()
    wall._readers["cam-00"].frame_ready.emit("cam-00", _frame(9), 2, 2, "rgb")
    assert "cam-00" in wall._pending_frames

    wall.set_cell_status("cam-00", "offline")
    assert "cam-00" not in wall._pending_frames
    wall._render_timer.timeout.emit()
    assert pixmap_uploads == []


# ---------------------------------------------------------------------------
# (3) Warm hidden-page readers
# ---------------------------------------------------------------------------


def test_page_navigation_throttles_and_reuses_warm_readers(fake_reader, qtbot):
    cameras = _cams(16)
    wall = _make_wall(qtbot, 2, 2, cameras)
    page0 = {f"cam-{i:02d}" for i in range(4)}
    page1 = {f"cam-{i:02d}" for i in range(4, 8)}
    readers = dict(wall._readers)

    assert wall.page_count == 4
    assert len(fake_reader.instances) == 16
    assert set(readers) == {cam.id for cam in cameras}
    for cam_id, reader in readers.items():
        expected = _GRID_FPS if cam_id in page0 else _HIDDEN_PAGE_FPS
        assert reader.max_emit_fps == expected, cam_id
        assert reader.start_calls == 1

    wall.set_page(1)

    assert len(fake_reader.instances) == 16, "page navigation must not create readers"
    for cam_id, reader in wall._readers.items():
        assert reader is readers[cam_id], f"{cam_id} reader was replaced"
        assert reader.stop_calls == 0
        assert reader.start_calls == 1
        expected = _GRID_FPS if cam_id in page1 else _HIDDEN_PAGE_FPS
        assert reader.max_emit_fps == expected, cam_id
    for cam_id in page0:
        assert readers[cam_id].max_emit_fps_updates[-1] == _HIDDEN_PAGE_FPS

    wall.set_page(0)

    assert len(fake_reader.instances) == 16
    assert wall._readers == readers
    for cam_id, reader in wall._readers.items():
        assert reader is readers[cam_id]
        assert reader.stop_calls == 0
        assert reader.start_calls == 1
        expected = _GRID_FPS if cam_id in page0 else _HIDDEN_PAGE_FPS
        assert reader.max_emit_fps == expected, cam_id


def test_hidden_readers_stay_at_exact_two_fps_across_grid_and_solo_changes(
    fake_reader, qtbot
):
    cameras = _cams(8, sub=True)
    wall = _make_wall(qtbot, 2, 2, cameras)
    readers = dict(wall._readers)

    _assert_reader_set_retuned(
        wall,
        readers,
        {f"cam-{i:02d}" for i in range(4)},
        _GRID_FPS,
    )

    wall.set_grid(1, 2)
    _assert_reader_set_retuned(wall, readers, {"cam-00", "cam-01"}, _GRID_FPS)

    wall.set_page(2)
    _assert_reader_set_retuned(wall, readers, {"cam-04", "cam-05"}, _GRID_FPS)

    wall.set_solo_camera("cam-05")
    wall.set_grid(1, 1)
    _assert_reader_set_retuned(wall, readers, {"cam-05"}, 25.0)

    wall.set_grid(1, 2)
    wall.set_solo_camera(None)
    _assert_reader_set_retuned(wall, readers, {"cam-00", "cam-01"}, _GRID_FPS)


# ---------------------------------------------------------------------------
# (4) No duplicate quality requests
# ---------------------------------------------------------------------------


def test_unchanged_quality_emits_no_duplicate_requests(fake_reader, qtbot):
    cameras = _cams(12, sub=True) + [
        CameraConfig(id=f"main-{i}", name=f"Main {i}", url=f"rtsp://host/m{i}") for i in range(4)
    ]
    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall._native_available = False
    requests: list[tuple[str, str]] = []
    wall.camera_quality_requested.connect(lambda cid, stream: requests.append((cid, stream)))

    wall.set_grid(2, 2)
    wall.set_cameras(cameras)

    assert sorted(requests) == sorted((cam.id, "sub" if cam.sub_url else "main") for cam in cameras)
    assert len(requests) == 16
    requests.clear()

    for page in (1, 2, 3, 0, 3, 1):
        wall.set_page(page)
    wall.set_grid(2, 2)  # same geometry: no-op
    wall.set_grid(3, 3)
    wall.set_grid(4, 4)
    wall.set_grid(2, 2)
    wall.set_page(2)
    wall.set_cameras(list(cameras))  # unrelated refresh with the same cameras

    assert requests == []

    # A 1x1 grid is still the wall: hidden cameras must not all switch to main.
    wall.set_grid(1, 1)
    assert requests == []
    wall.set_page(5)
    wall.set_page(0)
    assert requests == []


def test_solo_quality_changes_are_exact_and_idempotent(fake_reader, qtbot):
    cameras = _cams(4, sub=True)
    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall._native_available = False
    requests: list[tuple[str, str]] = []
    wall.camera_quality_requested.connect(lambda cid, stream: requests.append((cid, stream)))

    wall.set_grid(2, 2)
    wall.set_cameras(cameras)
    assert requests == [(cam.id, "sub") for cam in cameras]
    requests.clear()

    wall.set_solo_camera("cam-02")
    assert requests == [("cam-02", "main")]
    wall.set_grid(1, 1)
    assert requests == [("cam-02", "main")]
    requests.clear()

    wall.set_solo_camera("cam-02")
    wall.set_page(0)
    wall.set_grid(1, 1)
    assert requests == []

    wall.set_grid(2, 2)
    assert requests == []

    wall.set_solo_camera(None)
    assert requests == [("cam-02", "sub")]
    requests.clear()
    wall.set_solo_camera(None)
    wall.set_page(0)
    wall.set_grid(2, 2)
    assert requests == []
