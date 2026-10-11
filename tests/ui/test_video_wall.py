from __future__ import annotations

from typing import ClassVar

from PySide6.QtCore import QObject, Signal

from neurowatch.models.camera import CameraConfig
from neurowatch.widgets.video_wall import VideoWallWidget


class _FakeFrameReaderThread(QObject):
    frame_ready = Signal(str, bytes, int, int, str)
    instances: ClassVar[list[_FakeFrameReaderThread]] = []

    def __init__(self, cam_id: str, max_emit_fps: float | None = None, parent=None) -> None:
        super().__init__(parent)
        self.cam_id = cam_id
        self.max_emit_fps = max_emit_fps
        self.started = False
        self.stopped = False
        self.max_emit_fps_updates: list[float | None] = []
        self.instances.append(self)

    def start(self) -> None:
        self.started = True

    def stop(self) -> None:
        self.stopped = True

    def set_max_emit_fps(self, max_emit_fps: float | None) -> None:
        self.max_emit_fps_updates.append(max_emit_fps)
        self.max_emit_fps = max_emit_fps


def _stub_frame_reader(monkeypatch) -> type[_FakeFrameReaderThread]:
    _FakeFrameReaderThread.instances = []
    monkeypatch.setattr(
        "neurowatch.widgets.video_wall.FrameReaderThread",
        _FakeFrameReaderThread,
    )
    return _FakeFrameReaderThread


def test_page_change_keeps_hidden_readers_warm_and_uses_cached_frame(monkeypatch, qtbot):
    _stub_frame_reader(monkeypatch)
    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall._native_available = False
    wall.set_grid(1, 2)
    cameras = [CameraConfig(id=f"cam-{i}", name=str(i), url="/missing.avi") for i in range(4)]
    wall.set_cameras(cameras)
    for camera in cameras:
        wall.set_cell_status(camera.id, "streaming")
    statuses = []
    wall.camera_status_changed.connect(lambda cid, state: statuses.append((cid, state)))
    previous_readers = dict(wall._readers)
    assert set(previous_readers) == {camera.id for camera in cameras}
    assert previous_readers["cam-0"].max_emit_fps == 15.0
    assert previous_readers["cam-2"].max_emit_fps == 2.0
    wall._on_frame_ready("cam-2", b"\0" * 12, 2, 2, "rgb")

    wall.set_page(1)
    assert wall._readers == previous_readers
    assert not any(reader.stopped for reader in previous_readers.values())
    assert wall._readers["cam-0"].max_emit_fps == 2.0
    assert wall._readers["cam-2"].max_emit_fps == 15.0
    assert wall._cells[0]._pixmap is not None
    assert not statuses
    assert all(cell._status == "streaming" for cell in wall._cells)
    wall._on_frame_ready("cam-0", b"\0" * 12, 2, 2, "rgb")
    assert not statuses  # Queued old-page frames must not affect status.

    wall.set_cell_status("cam-0", "offline")  # Real backend update while offscreen.
    wall.set_page(0)
    assert wall._cells[0]._status == "offline"
    assert wall._cells[1]._status == "streaming"
    assert not statuses


def test_grid_rebuild_preserves_status_and_resets_frame_timing(monkeypatch, qtbot):
    _stub_frame_reader(monkeypatch)
    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall._native_available = False
    wall.set_grid(2, 2)
    wall.set_cameras([CameraConfig(id="cam", name="Camera", url="/missing.avi")])
    wall._on_frame_ready("cam", b"\0" * 12, 2, 2, "rgb")
    statuses = []
    wall.camera_status_changed.connect(lambda cid, state: statuses.append((cid, state)))
    wall.set_grid(4, 4)
    assert wall._cells[0]._status == "streaming"
    assert not statuses
    assert not wall._fps_state
    assert wall._readers["cam"].max_emit_fps == 15.0
    wall._on_frame_ready("cam", b"\0" * 12, 2, 2, "rgb")
    assert statuses == []


def test_dense_wall_coalesces_queued_frames_before_pixmap_upload(monkeypatch, qtbot):
    _stub_frame_reader(monkeypatch)
    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall._native_available = False
    wall.set_grid(4, 4)
    wall.set_cameras([CameraConfig(id="cam", name="Camera", url="/missing.avi")])

    rendered: list[bytes] = []
    wall._cells[0].update_frame = lambda data, *_args: rendered.append(data)  # type: ignore[method-assign]
    wall._on_frame_ready("cam", b"first", 2, 2, "rgb")
    for number in range(10):
        wall._on_frame_ready("cam", bytes([number]), 2, 2, "rgb")

    # First connection is immediate, but a burst cannot enqueue ten expensive
    # RGB->QPixmap conversions.  The next wall tick paints only the newest.
    assert rendered == [b"first"]
    wall._flush_pending_frames()
    assert rendered == [b"first", bytes([9])]
    assert wall._render_timer.interval() <= 34

    wall._on_frame_ready("cam", b"stale", 2, 2, "rgb")
    wall.set_cell_status("cam", "offline")
    wall._flush_pending_frames()
    assert rendered == [b"first", bytes([9])]


def test_empty_grid_slots_do_not_reduce_visible_camera_fps(monkeypatch, qtbot):
    fake_reader = _stub_frame_reader(monkeypatch)
    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall._native_available = False
    wall.set_grid(4, 8)
    wall.set_cameras(
        [CameraConfig(id=f"cam-{number}", name=str(number), url="/missing.avi")
         for number in range(16)]
    )

    assert len(fake_reader.instances) == 16
    assert {reader.max_emit_fps for reader in fake_reader.instances} == {15.0}


def test_grid_camera_without_sub_stream_starts_main_reader(monkeypatch, qtbot):
    fake_reader = _stub_frame_reader(monkeypatch)
    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall.set_grid(2, 2)
    quality_requests: list[tuple[str, str]] = []
    wall.camera_quality_requested.connect(
        lambda cam_id, stream: quality_requests.append((cam_id, stream))
    )

    wall.set_cameras(
        [CameraConfig(id="cam-main", name="Main only", url="rtsp://user:pass@host/main")]
    )

    assert [(reader.cam_id, reader.started) for reader in fake_reader.instances] == [
        ("cam-main", True)
    ]
    assert quality_requests == [("cam-main", "main")]
    assert wall._no_sub_ids == set()


def test_grid_camera_with_sub_stream_prefers_sub_reader(monkeypatch, qtbot):
    fake_reader = _stub_frame_reader(monkeypatch)
    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall.set_grid(2, 2)
    quality_requests: list[tuple[str, str]] = []
    wall.camera_quality_requested.connect(
        lambda cam_id, stream: quality_requests.append((cam_id, stream))
    )

    wall.set_cameras(
        [
            CameraConfig(
                id="cam-sub",
                name="Sub capable",
                url="rtsp://user:pass@host/main",
                sub_url="rtsp://user:pass@host/sub",
            )
        ]
    )

    assert [(reader.cam_id, reader.started) for reader in fake_reader.instances] == [
        ("cam-sub", True)
    ]
    assert quality_requests == [("cam-sub", "sub")]
    assert wall._no_sub_ids == set()


def test_detected_codec_label_includes_input_resolution(monkeypatch, qtbot):
    _stub_frame_reader(monkeypatch)
    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall._native_available = False
    wall.set_grid(1, 1)
    wall.set_cameras([CameraConfig(id="cam-0", name="0", url="rtsp://cam/stream")])

    wall.set_camera_codec("cam-0", "H264")  # resolution not negotiated yet
    assert wall._cells[0]._encoding == "H264"

    wall.set_camera_codec("cam-0", "H264", 1920, 1080)
    assert wall._cells[0]._encoding == "H264 1920×1080"
    assert wall._encoding_by_id["cam-0"] == "H264 1920×1080"


def test_only_visible_cameras_are_asked_to_be_decoded(monkeypatch, qtbot):
    _stub_frame_reader(monkeypatch)
    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall._native_available = False
    demands: list[list[str]] = []
    wall.view_demand_changed.connect(demands.append)
    wall.set_grid(1, 2)
    cameras = [CameraConfig(id=f"cam-{i}", name=str(i), url="/missing.avi") for i in range(4)]
    wall.set_cameras(cameras)
    assert demands[-1] == ["cam-0", "cam-1"]
    # Hidden pages keep their readers but are not decoded.
    assert set(wall._readers) == {camera.id for camera in cameras}

    wall.set_page(1)
    assert demands[-1] == ["cam-2", "cam-3"]
    wall.set_solo_camera("cam-3")
    assert demands[-1] == ["cam-3"]
    count = len(demands)
    wall.set_solo_camera("cam-3")
    assert len(demands) == count, "an unchanged demand is not resent"


def test_a_cached_picture_reads_refreshing_until_its_decoder_delivers(monkeypatch, qtbot):
    _stub_frame_reader(monkeypatch)
    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall._native_available = False
    wall.set_grid(1, 1)
    cameras = [CameraConfig(id=f"cam-{i}", name=str(i), url="/missing.avi") for i in range(2)]
    wall.set_cameras(cameras)
    for camera in cameras:
        wall.set_cell_status(camera.id, "streaming")
    wall._on_frame_ready("cam-0", b"\0" * 12, 2, 2, "rgb")
    wall._flush_pending_frames()
    wall.set_page(1)
    wall._last_frame_at["cam-0"] -= 60  # its decoder stopped a minute ago
    wall.set_page(0)
    cell = wall._cells[0]
    assert cell._refreshing and cell._badge()[0] == "REFRESHING"
    wall._on_frame_ready("cam-0", b"\0" * 12, 2, 2, "rgb")
    wall._flush_pending_frames()
    assert not cell._refreshing and cell._badge()[0] == "LIVE"


def test_a_recovering_camera_stays_live_while_the_backend_status_catches_up(monkeypatch, qtbot):
    from neurowatch.widgets import video_wall

    _stub_frame_reader(monkeypatch)
    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall._native_available = False
    wall.set_grid(2, 2)
    wall.set_cameras([CameraConfig(id="cam", name="Camera", url="/missing.avi")])
    shown: list[str] = []
    wall.camera_status_changed.connect(lambda _cam, status: shown.append(status))
    now = [1000.0]
    monkeypatch.setattr(video_wall.time, "monotonic", lambda: now[0])

    # Offline at start; then the desktop decoder reconnects first.
    assert wall.set_cell_status("cam", "offline") == "offline"
    wall._on_frame_ready("cam", b"back", 2, 2, "rgb")
    # The backend's status still lags: pictures keep arriving, so it stays live.
    for _ in range(4):
        now[0] += 0.5
        wall._on_frame_ready("cam", b"frame", 2, 2, "rgb")
        assert wall.set_cell_status("cam", "offline") == "streaming"
    assert shown == ["streaming"], "no flip back and forth"
    assert wall._cells[0]._status == "streaming"

    # Pictures stop: the next poll shows the outage.
    now[0] += 2.0
    assert wall.set_cell_status("cam", "offline") == "offline"

    # A camera that was live for a while and drops is shown offline at once.
    wall._on_frame_ready("cam", b"back", 2, 2, "rgb")
    now[0] += video_wall._RECOVERY_GRACE_S + 1
    wall._on_frame_ready("cam", b"frame", 2, 2, "rgb")
    assert wall.set_cell_status("cam", "offline") == "offline"
