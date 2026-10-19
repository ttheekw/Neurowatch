"""GPU format negotiation retains legacy readers and rejects corrupt frames."""

import contextlib
import struct

import pytest

from neurowatch.ipc import frame_reader as module
from neurowatch.widgets.gpu_video_surface import colour_parameters, gpu_video_available


@pytest.mark.parametrize(
    "enabled,descriptor,extra,expected",
    [
        (True, 34, 0, "nv12:34"),
        (True, 50, 0, "nv12:50"),
        (False, 34, 0, None),
        (True, 0, 0, None),
        (True, 128, 0, None),
        (True, 34, 1, None),
    ],
)
def test_negotiated_reader_validates_nv12(qtbot, monkeypatch, enabled, descriptor, extra, expected):
    class Ring(bytearray):
        def close(self):
            pass

    ring = Ring(64 + 8 * 64)
    monkeypatch.setattr(module, "SLOT_SIZE", 64)
    reader = module.FrameReaderThread("test", max_emit_fps=None)
    reader.set_accept_nv12(enabled)
    data = bytes(range(17))  # odd 3x3 Y plane + 4x2 UV plane
    struct.pack_into("<III", ring, 64, 3, 3, module.NV12_SLOT_FLAG | (21 + extra))
    struct.pack_into("<I", ring, 76, descriptor)
    ring[80:97] = data
    observed = []
    frames = []

    @contextlib.contextmanager
    def open_ring(_path):
        yield ring, None

    monkeypatch.setattr(reader, "_open_ring", open_ring)
    reader.frame_ready.connect(lambda *args: frames.append(args))

    def wait(_timeout):
        observed.append(struct.unpack_from("<I", ring, 8)[0])
        reader._stop_event.set()

    monkeypatch.setattr(reader._stop_event, "wait", wait)
    reader.run()
    assert observed[0] == (module.NV12_CAPABILITY if enabled else 0)
    assert struct.unpack_from("<I", ring, 8)[0] == 0
    assert len(frames) == int(expected is not None)
    if expected:
        assert frames[0] == ("test", data, 3, 3, expected)


def test_gpu_capability_keeps_legacy_rgb(qtbot, monkeypatch):
    class Ring(bytearray):
        def close(self):
            pass

    ring = Ring(64 + 8 * 64)
    struct.pack_into("<III", ring, 64, 2, 2, 12)
    ring[76:88] = bytes(range(12))
    monkeypatch.setattr(module, "SLOT_SIZE", 64)
    reader = module.FrameReaderThread("test")
    reader.set_accept_nv12(True)

    @contextlib.contextmanager
    def open_ring(_path):
        yield ring, None

    monkeypatch.setattr(reader, "_open_ring", open_ring)
    frames = []
    reader.frame_ready.connect(lambda *args: frames.append(args))
    monkeypatch.setattr(reader._stop_event, "wait", lambda _: reader._stop_event.set())
    reader.run()
    assert frames == [("test", bytes(range(12)), 2, 2, "rgb")]


def test_shader_colour_range_and_matrix():
    def rgb(y, u, v, descriptor):
        levels, coefficients = colour_parameters(descriptor)
        y = (y / 255 - levels[0]) * levels[1]
        u = (u / 255 - levels[2]) * levels[3]
        v = (v / 255 - levels[2]) * levels[3]
        return (
            y + coefficients[0] * v,
            y + coefficients[1] * u + coefficients[2] * v,
            y + coefficients[3] * u,
        )

    assert rgb(16, 128, 128, 1) == pytest.approx((0, 0, 0))
    assert rgb(235, 128, 128, 2) == pytest.approx((1, 1, 1))
    assert rgb(0, 128, 128, 18) == pytest.approx((0, 0, 0))
    assert rgb(255, 128, 128, 18) == pytest.approx((1, 1, 1))
    assert rgb(81, 90, 240, 1) == pytest.approx((1, 0, 0), abs=0.01)
    for descriptor in (0, 4, 128):
        with pytest.raises(ValueError):
            colour_parameters(descriptor)


def test_offscreen_uses_rgb(qtbot):
    assert not gpu_video_available()


@pytest.fixture
def gpu_wall(qtbot, monkeypatch):
    from PySide6.QtCore import QObject, Signal
    from PySide6.QtWidgets import QWidget

    from neurowatch.models.camera import CameraConfig
    from neurowatch.widgets import gpu_video_surface, video_wall

    class Reader(QObject):
        frame_ready = Signal(str, bytes, int, int, str)

        def __init__(self, cam_id, max_emit_fps=None, parent=None):
            super().__init__(parent)
            self.enabled = False
            self.stopped = False

        def set_accept_nv12(self, enabled):
            self.enabled = enabled

        def set_max_emit_fps(self, fps):
            self.fps = fps

        def start(self):
            pass

        def stop(self):
            self.stopped = True

    class Surface(QWidget):
        failed = Signal()

        def __init__(self, parent, *_):
            super().__init__(parent)
            self.frames = []
            self.cleaned = False

        def set_frame(self, *args):
            self.frames.append(args)

        def cleanup(self):
            self.cleaned = True

    monkeypatch.setattr(gpu_video_surface, "gpu_video_available", lambda: True)
    monkeypatch.setattr(gpu_video_surface, "GpuVideoSurface", Surface)
    monkeypatch.setattr(video_wall, "FrameReaderThread", Reader)
    wall = video_wall.VideoWallWidget()
    qtbot.addWidget(wall)
    wall._native_available = False
    wall.set_grid(2, 2)
    wall.set_cameras(
        [CameraConfig(id="cam", name="Camera", url="rtsp://h/main", sub_url="rtsp://h/sub")]
    )
    wall.show()
    yield wall
    wall.shutdown()


def test_yuv_bursts_wait_for_render_tick_without_video_pixmaps(gpu_wall, monkeypatch):
    from neurowatch.widgets import cell_widget

    wall = gpu_wall

    # Existing overlay cache still uses pixmaps; video uploads must not.
    class NoVideoPixmap:
        @staticmethod
        def fromImage(*args):  # noqa: N802
            raise AssertionError("YUV video must not create a pixmap")

    monkeypatch.setattr(cell_widget, "QPixmap", NoVideoPixmap)
    wall._on_frame_ready("cam", bytes([81] * 4 + [90, 240]), 2, 2, "nv12:1")
    surface = wall._cells[0]._gpu_surface
    assert len(surface.frames) == 1
    for tag in range(20):
        wall._on_frame_ready("cam", bytes([tag] * 4 + [90, 240]), 2, 2, "nv12:1")
    assert len(surface.frames) == 1
    wall._flush_pending_frames()
    assert len(surface.frames) == 2
    assert surface.frames[-1][0][0] == 19
    wall._flush_pending_frames()
    assert len(surface.frames) == 2


def test_gpu_failure_clears_yuv_and_keeps_readers_for_rgb(gpu_wall):
    wall = gpu_wall
    reader = wall._readers["cam"]
    wall._on_frame_ready("cam", bytes([81] * 4 + [90, 240]), 2, 2, "nv12:1")
    surface = wall._cells[0]._gpu_surface
    surface.failed.emit()
    assert surface.cleaned
    assert not reader.enabled and not reader.stopped
    assert not wall._gpu_enabled
    assert wall._cells[0]._gpu_surface is None
    wall._on_frame_ready("cam", bytes([81] * 4 + [90, 240]), 2, 2, "nv12:1")
    assert "cam" not in wall._last_frame_by_id
    wall._on_frame_ready("cam", bytes([255, 0, 0] * 4), 2, 2, "rgb")
    assert wall._cells[0]._pixmap is not None


def test_page_change_reuses_gpu_surface_with_cached_frame(gpu_wall):
    """A page flip must not recreate the GL surface (it flashes the tile black):
    the same surface shows the newly visible camera's cached frame at once."""
    from neurowatch.models.camera import CameraConfig

    wall = gpu_wall
    wall.set_grid(1, 1)
    wall.set_cameras(
        [
            CameraConfig(id="cam", name="Camera", url="rtsp://h/main", sub_url="rtsp://h/sub"),
            CameraConfig(id="cam2", name="Camera 2", url="rtsp://h2/main", sub_url="rtsp://h2/sub"),
        ]
    )
    wall._on_frame_ready("cam", bytes([81] * 4 + [90, 240]), 2, 2, "nv12:1")
    wall.set_page(1)
    wall._on_frame_ready("cam2", bytes([82] * 4 + [90, 240]), 2, 2, "nv12:1")
    surface = wall._cells[0]._gpu_surface
    assert surface is not None
    wall._last_frame_at["cam"] -= 60  # its decoder stopped a minute ago

    wall.set_page(0)

    cell = wall._cells[0]
    assert cell._gpu_surface is surface
    assert not surface.cleaned
    assert surface.isVisible()
    assert surface.frames[-1][0][0] == 81  # cam's cached frame, painted on the flip
    assert cell._refreshing


def test_gpu_wall_window_is_gl_ready_before_it_is_shown(gpu_wall, qtbot, monkeypatch):
    """A hidden GL child exists from construction: the first video surface,
    added after the window is shown, must not make Qt recreate the window
    (on screen: the app closes and opens again at startup)."""
    from PySide6.QtOpenGLWidgets import QOpenGLWidget

    from neurowatch.widgets import gpu_video_surface, video_wall

    placeholders = gpu_wall.findChildren(QOpenGLWidget, "glWindowPlaceholder")
    assert len(placeholders) == 1 and placeholders[0].isHidden()

    monkeypatch.setattr(gpu_video_surface, "gpu_video_available", lambda: False)
    rgb_wall = video_wall.VideoWallWidget()
    qtbot.addWidget(rgb_wall)
    assert not rgb_wall._gpu_enabled
    assert not rgb_wall.findChildren(QOpenGLWidget), "no GL without GPU preview"
