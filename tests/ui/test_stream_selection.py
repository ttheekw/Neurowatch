"""Per-view stream selection (Stream 1 … N) and ONVIF stream discovery."""

from __future__ import annotations

from typing import ClassVar

import pytest
from PySide6.QtCore import QObject, Signal
from PySide6.QtWidgets import QMenu

from neurowatch.models.camera import CameraConfig, CameraStream
from neurowatch.widgets.cell_widget import CellWidget
from neurowatch.widgets.video_wall import VideoWallWidget


class _FakeFrameReaderThread(QObject):
    frame_ready = Signal(str, bytes, int, int, str)
    instances: ClassVar[list[_FakeFrameReaderThread]] = []

    def __init__(self, cam_id: str, max_emit_fps: float | None = None, parent=None) -> None:
        super().__init__(parent)
        self.cam_id = cam_id
        _FakeFrameReaderThread.instances.append(self)

    def start(self) -> None:
        pass

    def stop(self) -> None:
        pass

    def set_max_emit_fps(self, max_emit_fps: float | None) -> None:
        pass


@pytest.fixture
def fake_reader(monkeypatch) -> type[_FakeFrameReaderThread]:
    _FakeFrameReaderThread.instances = []
    monkeypatch.setattr("neurowatch.widgets.video_wall.FrameReaderThread", _FakeFrameReaderThread)
    return _FakeFrameReaderThread


def _three_stream_cam(index: int) -> CameraConfig:
    base = f"rtsp://host/{index}"
    return CameraConfig(
        id=f"cam-{index:02d}",
        name=f"Camera {index}",
        url=f"{base}/s1",
        sub_url=f"{base}/s2",
        streams=[
            CameraStream(id="1", url=f"{base}/s1", name="Main", width=2560, height=1440),
            CameraStream(id="2", url=f"{base}/s2", name="Sub", width=640, height=360),
            CameraStream(id="3", url=f"{base}/s3", name="Third", width=1280, height=720),
        ],
    )


def _wall(qtbot, rows: int, cols: int, cameras: list[CameraConfig]):
    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall._native_available = False
    requests: list[tuple[str, str]] = []
    wall.camera_quality_requested.connect(lambda cid, stream: requests.append((cid, stream)))
    wall.set_grid(rows, cols)
    wall.set_cameras(cameras)
    return wall, requests


# ── Model ────────────────────────────────────────────────────────────────


def test_camera_without_a_stream_list_offers_main_and_sub():
    cam = CameraConfig(url="rtsp://h/main", sub_url="rtsp://h/sub")
    assert [(s.id, s.url) for s in cam.stream_options()] == [
        ("1", "rtsp://h/main"),
        ("2", "rtsp://h/sub"),
    ]
    assert [s.id for s in CameraConfig(url="rtsp://h/main").stream_options()] == ["1"]


def test_stream_list_round_trips_through_dict():
    cam = _three_stream_cam(1)
    back = CameraConfig.from_dict(cam.to_dict())
    assert back.streams == cam.streams
    assert back.streams[2].label() == "Stream 3 · 1280×720 · Third"


# ── Video wall ───────────────────────────────────────────────────────────


def test_picking_a_stream_switches_only_that_camera(fake_reader, qtbot):
    cameras = [_three_stream_cam(i) for i in range(4)]
    wall, requests = _wall(qtbot, 2, 2, cameras)
    assert requests == [(cam.id, "sub") for cam in cameras]
    requests.clear()

    wall.set_view_stream("cam-01", "3")
    assert requests == [("cam-01", "3")]
    assert wall._cells[wall._cell_by_cam_id["cam-01"]]._stream_choice == "3"

    requests.clear()
    wall.set_view_stream("cam-01", "3")  # unchanged: no pipeline restart
    wall.set_view_stream("cam-01", "9")  # unknown stream: ignored
    assert requests == []

    wall.set_view_stream("cam-01", None)  # back to Auto
    assert requests == [("cam-01", "sub")]


def test_a_stream_pick_lasts_only_for_the_current_view(fake_reader, qtbot):
    cameras = [_three_stream_cam(i) for i in range(8)]
    wall, requests = _wall(qtbot, 2, 2, cameras)
    wall.set_view_stream("cam-00", "1")
    requests.clear()

    wall.set_page(1)
    assert ("cam-00", "sub") in requests
    wall.set_page(0)
    assert wall._cells[wall._cell_by_cam_id["cam-00"]]._stream_choice is None


def test_a_full_screen_pick_is_dropped_on_leaving_solo(fake_reader, qtbot):
    cameras = [_three_stream_cam(i) for i in range(4)]
    wall, requests = _wall(qtbot, 2, 2, cameras)
    wall.set_solo_camera("cam-02")
    wall.set_grid(1, 1)
    assert requests[-1] == ("cam-02", "main")

    wall.set_view_stream("cam-02", "3")
    assert requests[-1] == ("cam-02", "3")

    wall.set_grid(2, 2)
    wall.set_solo_camera(None)
    assert requests[-1] == ("cam-02", "sub")


# ── Tile menu ────────────────────────────────────────────────────────────


def _stream_menu(menu: QMenu) -> QMenu | None:
    return next((m for m in menu.findChildren(QMenu) if m.title().startswith("Stream")), None)


def _stream_menu_labels(cell: CellWidget) -> list[str]:
    stream_menu = _stream_menu(cell._build_context_menu())
    return [] if stream_menu is None else [a.text() for a in stream_menu.actions()]


def test_tile_menu_lists_every_stream_with_auto_first(qtbot):
    cell = CellWidget()
    qtbot.addWidget(cell)
    cell.set_camera(_three_stream_cam(1))
    assert _stream_menu_labels(cell) == [
        "Auto",
        "Stream 1 · 2560×1440 · Main",
        "Stream 2 · 640×360 · Sub",
        "Stream 3 · 1280×720 · Third",
    ]

    emitted: list[tuple[str, object]] = []
    cell.stream_requested.connect(lambda cid, sid: emitted.append((cid, sid)))
    stream_menu = _stream_menu(cell._build_context_menu())
    assert stream_menu is not None
    stream_menu.actions()[3].trigger()
    stream_menu.actions()[0].trigger()
    assert emitted == [("cam-01", "3"), ("cam-01", None)]


def test_tile_menu_hides_stream_choice_for_a_single_stream(qtbot):
    cell = CellWidget()
    qtbot.addWidget(cell)
    cell.set_camera(CameraConfig(id="solo", url="rtsp://h/only"))
    assert _stream_menu_labels(cell) == []


# ── Camera dialog ────────────────────────────────────────────────────────


def test_onvif_discovery_keeps_every_profile_highest_resolution_first(qtbot):
    from neurowatch.dialogs.camera_dialog import AddEditCameraDialog

    dlg = AddEditCameraDialog()
    qtbot.addWidget(dlg)
    dlg._tabs.setCurrentWidget(dlg._simple_tab)
    dlg._s_host.setText("10.0.0.9")
    # The backend's camera.discovery.profiles answer, without suggested streams.
    dlg._on_discover_done(
        {
            "candidate": {"candidate_id": "c1", "address": "10.0.0.9", "state": "ready"},
            "streams": [
                {
                    "stream_id": "s1",
                    "name": "SubStream",
                    "url": "rtsp://10.0.0.9/s2",
                    "width": 640,
                    "height": 360,
                },
                {
                    "stream_id": "s2",
                    "name": "MainStream",
                    "url": "rtsp://10.0.0.9/s1",
                    "width": 2560,
                    "height": 1440,
                },
                {
                    "stream_id": "s3",
                    "name": "ThirdStream",
                    "url": "rtsp://10.0.0.9/s3",
                    "width": 1280,
                    "height": 720,
                },
            ],
        }
    )
    cam = dlg.get_config()

    assert cam.url == "rtsp://10.0.0.9/s1"  # recorded and full-screen default
    assert cam.sub_url == "rtsp://10.0.0.9/s2"  # lowest resolution in grids
    assert [(s.id, s.name, s.width) for s in cam.streams] == [
        ("1", "MainStream", 2560),
        ("2", "ThirdStream", 1280),
        ("3", "SubStream", 640),
    ]


def test_manual_streams_add_extra_urls_and_keep_known_details(qtbot):
    from neurowatch.dialogs.camera_dialog import AddEditCameraDialog

    dlg = AddEditCameraDialog(camera=_three_stream_cam(4))
    qtbot.addWidget(dlg)
    dlg._tabs.setCurrentIndex(1)
    assert dlg._a_more_streams.toPlainText() == "rtsp://host/4/s3"

    dlg._a_more_streams.setPlainText("rtsp://host/4/s3\nrtsp://host/4/s4\n")
    cam = dlg.get_config()
    assert [(s.id, s.url, s.width) for s in cam.streams] == [
        ("1", "rtsp://host/4/s1", 2560),
        ("2", "rtsp://host/4/s2", 640),
        ("3", "rtsp://host/4/s3", 1280),
        ("4", "rtsp://host/4/s4", None),
    ]
