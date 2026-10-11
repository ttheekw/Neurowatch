"""Focused tests for the native viewer integration in the video wall.

Covers:
  * When ``viewer.capabilities`` reports available, the wall attaches a
    ``NativeViewerSurface`` for each visible camera and does NOT start the
    legacy ``FrameReaderThread``.
  * When the backend reports unavailable, the legacy raw-frame path stays
    intact and no attach is attempted.
  * Grid rebuilds preserve unchanged-index native sessions; reassigned or
    removed sessions are released.
  * ``NativeViewerClient`` never sends URLs, usernames, or passwords to the
    backend — only the camera id and a numeric window handle.

These stubbed Qt tests validate native-session lifecycle and handle plumbing
only. They do not prove compositor pixels or hardware frame-rate behavior.
"""
from __future__ import annotations

from typing import Any, ClassVar

import pytest
from PySide6.QtCore import QObject, Signal
from PySide6.QtGui import QGuiApplication

from neurowatch.ipc.viewer_client import NativeViewerClient, ViewerCapabilities
from neurowatch.models.camera import CameraConfig
from neurowatch.widgets.video_wall import VideoWallWidget


@pytest.fixture(autouse=True)
def x11_platform(monkeypatch):
    # Stubbed sessions exercise X11 IPC without creating real overlay sinks.
    monkeypatch.setattr(QGuiApplication, "platformName", lambda: "xcb")

# ---------------------------------------------------------------------------
# Stubs
# ---------------------------------------------------------------------------


class _FakeFrameReaderThread(QObject):
    frame_ready = Signal(str, bytes, int, int, str)
    instances: ClassVar[list[_FakeFrameReaderThread]] = []

    def __init__(self, cam_id: str, max_emit_fps: float | None = None, parent=None) -> None:
        super().__init__(parent)
        self.cam_id = cam_id
        self.started = False
        self.stopped = False
        _FakeFrameReaderThread.instances.append(self)

    def start(self) -> None:
        self.started = True

    def stop(self) -> None:
        self.stopped = True

    def set_max_emit_fps(self, value: float | None) -> None:  # pragma: no cover — trivial
        pass


class _StubIpc:
    """Records every JSON-RPC call made by NativeViewerClient."""

    def __init__(self, *, available: bool) -> None:
        self._available = available
        self._next_session = 0
        self.calls: list[tuple[str, dict[str, Any]]] = []

    def call(self, method: str, params: dict[str, Any] | None = None) -> Any:
        params = params or {}
        self.calls.append((method, dict(params)))
        if method == "viewer.capabilities":
            if self._available:
                return {
                    "available": True,
                    "preferred_sink": "glimagesink",
                    "sinks": ["glimagesink"],
                }
            return {
                "available": False,
                "preferred_sink": None,
                "sinks": [],
                "reason": "no_overlay_sink",
            }
        if method == "viewer.attach":
            self._next_session += 1
            return {"session_id": f"vw-test-{self._next_session}"}
        if method == "viewer.detach":
            return {"ok": True}
        raise AssertionError(f"unexpected method: {method}")


def _stub_frame_reader(monkeypatch) -> type[_FakeFrameReaderThread]:
    _FakeFrameReaderThread.instances = []
    monkeypatch.setattr(
        "neurowatch.widgets.video_wall.FrameReaderThread",
        _FakeFrameReaderThread,
    )
    return _FakeFrameReaderThread


def _cam(cid: str, url: str = "rtsp://alice:s3cret@camera.example/live") -> CameraConfig:
    return CameraConfig(id=cid, name=cid.title(), url=url)


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


def test_native_viewer_used_when_backend_reports_available(monkeypatch, qtbot):
    fake_reader = _stub_frame_reader(monkeypatch)
    ipc = _StubIpc(available=True)
    client = NativeViewerClient(ipc=ipc)  # type: ignore[arg-type]

    wall = VideoWallWidget(viewer_client=client)
    qtbot.addWidget(wall)
    wall.set_grid(1, 1)

    wall.set_cameras([_cam("cam-native")])

    assert wall._native_available is True
    assert "cam-native" in wall._native_surfaces, (
        "native surface must be created when capabilities.available is True"
    )
    assert wall._native_surfaces["cam-native"].cam_id == "cam-native"
    assert fake_reader.instances == [], (
        "FrameReaderThread must not be started on the native path"
    )
    # Exactly one attach call, no URL/credentials in params.
    attach_calls = [c for c in ipc.calls if c[0] == "viewer.attach"]
    assert len(attach_calls) == 1
    _, params = attach_calls[0]
    assert set(params.keys()) == {"camera_id", "window_handle", "stream", "client_id"}
    assert params["stream"] == "main"
    assert params["camera_id"] == "cam-native"
    assert isinstance(params["window_handle"], int)
    assert params["window_handle"] > 0
    for forbidden in ("alice", "s3cret", "camera.example", "rtsp://"):
        for _, p in ipc.calls:
            for value in p.values():
                assert forbidden not in str(value), (
                    f"IPC params leaked {forbidden!r}: {p}"
                )


def test_native_grid_uses_substream_and_keeps_unchanged_sessions(monkeypatch, qtbot):
    _stub_frame_reader(monkeypatch)
    ipc = _StubIpc(available=True)
    wall = VideoWallWidget(viewer_client=NativeViewerClient(ipc=ipc))
    qtbot.addWidget(wall)
    wall.set_grid(2, 2)
    cameras = [
        CameraConfig(id="cam-dual", url="rtsp://camera/main", sub_url="rtsp://camera/sub"),
        _cam("cam-main-only"),
    ]
    wall.set_cameras(cameras)

    attaches = [params for method, params in ipc.calls if method == "viewer.attach"]
    assert [(p["camera_id"], p["stream"]) for p in attaches] == [
        ("cam-dual", "sub"),
        ("cam-main-only", "main"),
    ]

    calls_before = len(ipc.calls)
    surfaces_before = dict(wall._native_surfaces)
    wall.set_grid(3, 3)
    wall.set_cameras(cameras)
    assert wall._native_surfaces == surfaces_before
    assert ipc.calls[calls_before:] == []

    wall.set_grid(1, 1)
    assert wall._native_surfaces["cam-dual"] is surfaces_before["cam-dual"]
    assert not any(method == "viewer.attach" for method, _ in ipc.calls[calls_before:])


@pytest.mark.parametrize("solo_id", ["cam-a", "cam-b"])
def test_native_solo_switches_to_main_and_back_to_sub(monkeypatch, qtbot, solo_id):
    _stub_frame_reader(monkeypatch)
    ipc = _StubIpc(available=True)
    wall = VideoWallWidget(viewer_client=NativeViewerClient(ipc=ipc))
    qtbot.addWidget(wall)
    wall.set_grid(2, 2)
    wall.set_cameras([
        CameraConfig(id=cid, url=f"rtsp://{cid}/main", sub_url=f"rtsp://{cid}/sub")
        for cid in ("cam-a", "cam-b")
    ])
    initial_surface = wall._native_surfaces[solo_id]
    calls_before = len(ipc.calls)

    # Match MainWindow's double-click entry and exit order. cam-a retains
    # cell index zero throughout; cam-b moves from cell one to cell zero.
    wall.set_solo_camera(solo_id)
    wall.set_grid(1, 1)
    new_attaches = [p for m, p in ipc.calls[calls_before:] if m == "viewer.attach"]
    assert new_attaches[-1]["camera_id"] == solo_id
    assert new_attaches[-1]["stream"] == "main"
    assert all(p["stream"] == "sub" for p in new_attaches[:-1])
    if solo_id == "cam-a":
        assert wall._native_surfaces[solo_id] is initial_surface

    calls_before = len(ipc.calls)
    wall.set_grid(2, 2)
    wall.set_solo_camera(None)
    new_attaches = [p for m, p in ipc.calls[calls_before:] if m == "viewer.attach"]
    assert new_attaches
    assert all(p["stream"] == "sub" for p in new_attaches)
    calls_before = len(ipc.calls)
    wall.set_cameras(wall._cameras)
    assert ipc.calls[calls_before:] == []


def test_native_auto_restores_substream_after_manual_main_pick(monkeypatch, qtbot):
    _stub_frame_reader(monkeypatch)
    ipc = _StubIpc(available=True)
    wall = VideoWallWidget(viewer_client=NativeViewerClient(ipc=ipc))
    qtbot.addWidget(wall)
    wall.set_grid(2, 2)
    wall.set_cameras([
        CameraConfig(id="cam-dual", url="rtsp://camera/main", sub_url="rtsp://camera/sub"),
    ])

    wall.set_view_stream("cam-dual", "1")
    wall.set_view_stream("cam-dual", None)

    attaches = [p for m, p in ipc.calls if m == "viewer.attach"]
    assert [p.get("stream") for p in attaches] == ["sub", "1", "sub"]


def test_failed_native_quality_switch_falls_back_to_frames(monkeypatch, qtbot):
    fake_reader = _stub_frame_reader(monkeypatch)

    class _MainAttachFailsIpc(_StubIpc):
        def call(self, method, params=None):
            if method == "viewer.attach" and params.get("stream") == "main":
                from neurowatch.ipc.sync_client import IpcError

                self.calls.append((method, dict(params)))
                raise IpcError(-32603, "Main overlay unavailable")
            return super().call(method, params)

    ipc = _MainAttachFailsIpc(available=True)
    wall = VideoWallWidget(viewer_client=NativeViewerClient(ipc=ipc))
    qtbot.addWidget(wall)
    wall.set_grid(2, 2)
    wall.set_cameras([
        CameraConfig(id="cam-dual", url="rtsp://camera/main", sub_url="rtsp://camera/sub"),
    ])
    assert "cam-dual" in wall._native_surfaces

    wall.set_solo_camera("cam-dual")
    wall.set_grid(1, 1)

    assert wall._native_surfaces == {}
    assert wall._cells[0]._native_surface is None
    assert "cam-dual" in wall._native_fallback
    assert [r.cam_id for r in fake_reader.instances if r.started and not r.stopped] == ["cam-dual"]
    calls_before = len(ipc.calls)
    wall.set_cameras(wall._cameras)
    assert ipc.calls[calls_before:] == []


def test_legacy_frame_reader_used_when_native_unavailable(monkeypatch, qtbot):
    fake_reader = _stub_frame_reader(monkeypatch)
    ipc = _StubIpc(available=False)
    client = NativeViewerClient(ipc=ipc)  # type: ignore[arg-type]

    wall = VideoWallWidget(viewer_client=client)
    qtbot.addWidget(wall)
    wall.set_grid(1, 1)

    wall.set_cameras([_cam("cam-fallback")])

    assert wall._native_available is False
    assert wall._native_surfaces == {}
    assert [r.cam_id for r in fake_reader.instances] == ["cam-fallback"]
    assert not any(c[0] == "viewer.attach" for c in ipc.calls), (
        "must not attempt viewer.attach when capabilities report unavailable"
    )


def test_removing_camera_tears_down_native_session(monkeypatch, qtbot):
    _stub_frame_reader(monkeypatch)
    ipc = _StubIpc(available=True)
    client = NativeViewerClient(ipc=ipc)  # type: ignore[arg-type]

    wall = VideoWallWidget(viewer_client=client)
    qtbot.addWidget(wall)
    wall.set_grid(1, 1)

    wall.set_cameras([_cam("cam-1"), _cam("cam-2")])
    assert list(wall._native_surfaces.keys()) == ["cam-1"]

    # Remove the visible camera — its native surface must be released.
    wall.set_cameras([_cam("cam-2")])

    assert "cam-1" not in wall._native_surfaces
    assert "cam-2" in wall._native_surfaces
    detach_calls = [c for c in ipc.calls if c[0] == "viewer.detach"]
    assert any(
        "vw-test-" in c[1].get("session_id", "") for c in detach_calls
    ), f"expected viewer.detach for old session, got {detach_calls}"


def test_native_wall_prewarms_hidden_pages_with_low_rate_readers(monkeypatch, qtbot):
    fake_reader = _stub_frame_reader(monkeypatch)
    ipc = _StubIpc(available=True)
    wall = VideoWallWidget(viewer_client=NativeViewerClient(ipc=ipc))  # type: ignore[arg-type]
    qtbot.addWidget(wall)
    wall.set_grid(1, 1)
    wall.set_cameras([_cam("cam-1"), _cam("cam-2"), _cam("cam-3")])

    assert set(wall._native_surfaces) == {"cam-1"}
    assert {reader.cam_id for reader in fake_reader.instances if not reader.stopped} == {
        "cam-2",
        "cam-3",
    }
    wall._on_frame_ready("cam-2", b"\0" * 12, 2, 2, "rgb")

    wall.set_page(1)

    assert set(wall._native_surfaces) == {"cam-2"}
    assert set(wall._readers) == {"cam-1", "cam-3"}
    assert wall._cells[0]._pixmap is not None


def test_grid_rebuild_preserves_native_session_when_cell_index_unchanged(monkeypatch, qtbot):
    """A grid geometry change must not restart native sessions for cameras
    that keep the same cell index in the new layout — releasing/reattaching
    tears down the backend GStreamer pipeline and stalls that stream for
    seconds on X11. cam-a is index 0 both in the 1x1 grid and in the 2x2
    grid it resizes to, so its session must survive untouched."""
    _stub_frame_reader(monkeypatch)
    ipc = _StubIpc(available=True)
    client = NativeViewerClient(ipc=ipc)  # type: ignore[arg-type]

    wall = VideoWallWidget(viewer_client=client)
    qtbot.addWidget(wall)
    wall.set_grid(1, 1)
    wall.set_cameras([_cam("cam-a")])
    assert list(wall._native_surfaces.keys()) == ["cam-a"]
    first_surface = wall._native_surfaces["cam-a"]
    calls_before_resize = len(ipc.calls)

    # Resize the grid — cam-a is still page index 0, so its native session
    # must be preserved exactly (same surface, same backend session).
    wall.set_grid(2, 2)

    assert "cam-a" in wall._native_surfaces
    assert wall._native_surfaces["cam-a"] is first_surface, (
        "grid rebuild must not destroy/recreate the surface for a camera "
        "that keeps the same cell index"
    )
    assert wall._cell_by_cam_id["cam-a"] == 0
    assert wall._cells[0]._native_surface is first_surface, (
        "the preserved surface must be reparented onto the new cell widget"
    )
    assert ipc.calls[calls_before_resize:] == [], (
        "no viewer.detach/viewer.attach (or any other backend RPC) may be "
        f"issued for a preserved cell, got {ipc.calls[calls_before_resize:]}"
    )


def test_grid_rebuild_releases_only_reassigned_or_removed_native_sessions(monkeypatch, qtbot):
    """A camera that falls off the visible page during a grid resize must
    still have its native session released, even though a sibling camera
    that keeps its cell index is preserved untouched."""
    _stub_frame_reader(monkeypatch)
    ipc = _StubIpc(available=True)
    client = NativeViewerClient(ipc=ipc)  # type: ignore[arg-type]

    wall = VideoWallWidget(viewer_client=client)
    qtbot.addWidget(wall)
    wall.set_grid(1, 2)
    wall.set_cameras([_cam("cam-a"), _cam("cam-b")])
    assert set(wall._native_surfaces) == {"cam-a", "cam-b"}
    cam_a_surface = wall._native_surfaces["cam-a"]

    # Shrinking to 1x1 drops cam-b off the visible page; cam-a keeps index 0.
    wall.set_grid(1, 1)

    assert "cam-b" not in wall._native_surfaces
    assert wall._native_surfaces["cam-a"] is cam_a_surface
    detach_calls = [c for c in ipc.calls if c[0] == "viewer.detach"]
    assert len(detach_calls) == 1
    attach_calls = [c for c in ipc.calls if c[0] == "viewer.attach"]
    # Two initial attaches (cam-a, cam-b) — no reattach for either camera.
    assert len(attach_calls) == 2


def test_hard_attach_failure_falls_back_to_frame_reader(monkeypatch, qtbot):
    """If viewer.attach errors after capabilities said available, the cell
    must fall back to FrameReaderThread instead of silently rendering
    nothing.  A subsequent set_cameras() must not retry native (fallback
    is remembered for the current visibility window)."""
    fake_reader = _stub_frame_reader(monkeypatch)

    class _AttachFailsIpc(_StubIpc):
        def call(self, method, params=None):  # type: ignore[override]
            if method == "viewer.attach":
                self.calls.append((method, dict(params or {})))
                # Simulate the backend rejecting the attach.  IpcError is
                # what SyncIpcClient would raise on an error response.
                from neurowatch.ipc.sync_client import IpcError

                raise IpcError(-32603, "Native viewer unavailable: overlay_attach")
            return super().call(method, params)

    ipc = _AttachFailsIpc(available=True)
    client = NativeViewerClient(ipc=ipc)  # type: ignore[arg-type]

    wall = VideoWallWidget(viewer_client=client)
    qtbot.addWidget(wall)
    wall.set_grid(1, 1)
    wall.set_cameras([_cam("cam-hardfail")])

    # Capabilities said "available" — one attach attempt should be made.
    attach_calls = [c for c in ipc.calls if c[0] == "viewer.attach"]
    assert len(attach_calls) == 1
    # Attach failed, so the surface must have been torn back out and the
    # legacy FrameReaderThread must have been started for the cell.
    assert wall._native_surfaces == {}
    assert [r.cam_id for r in fake_reader.instances] == ["cam-hardfail"]
    assert "cam-hardfail" in wall._native_fallback


def test_capabilities_reason_never_leaks_paths(monkeypatch, qtbot):
    """viewer_client caches capabilities and never surfaces URL-like reasons."""
    ipc = _StubIpc(available=False)
    client = NativeViewerClient(ipc=ipc)  # type: ignore[arg-type]
    caps = client.capabilities()
    assert isinstance(caps, ViewerCapabilities)
    assert caps.available is False
    assert caps.reason == "no_overlay_sink"
    # Cached — no second RPC.
    calls_before = len(ipc.calls)
    client.capabilities()
    assert len(ipc.calls) == calls_before


@pytest.mark.parametrize("platform", ["wayland", "wayland-egl", "offscreen", "minimal"])
def test_unsupported_qt_platform_uses_frames_without_sending_handle(monkeypatch, qtbot, platform):
    from neurowatch.ipc.viewer_client import ViewerUnavailableError

    monkeypatch.setattr(QGuiApplication, "platformName", lambda: platform)
    fake_reader = _stub_frame_reader(monkeypatch)
    ipc = _StubIpc(available=True)
    client = NativeViewerClient(ipc=ipc)
    wall = VideoWallWidget(viewer_client=client)
    qtbot.addWidget(wall)
    wall.set_grid(1, 1)
    wall.set_cameras([_cam("cam-wayland")])

    assert wall._native_surfaces == {}
    assert [reader.cam_id for reader in fake_reader.instances] == ["cam-wayland"]
    with pytest.raises(ViewerUnavailableError):
        client.attach("cam-wayland", 1)
    with pytest.raises(ViewerUnavailableError):
        client.set_window("vw-existing", 1)
    assert not any(method in {"viewer.attach", "viewer.set_window"} for method, _ in ipc.calls)
    wall.close()
    assert all(reader.stopped for reader in fake_reader.instances)


class _WaylandSessionIpc(_StubIpc):
    """Backend view of a Wayland session (XWayland DISPLAY may also exist)."""

    def call(self, method: str, params: dict[str, Any] | None = None) -> Any:
        if method == "viewer.capabilities":
            self.calls.append((method, dict(params or {})))
            return {
                "available": False,
                "preferred_sink": None,
                "sinks": [],
                "reason": "cross_process_wayland_unsupported",
            }
        return super().call(method, params)


@pytest.mark.parametrize(
    ("platform", "ipc_factory"),
    [
        # Native Wayland Qt platform: even a backend claiming availability
        # must not be handed a (process-local) Wayland handle.
        ("wayland", lambda: _StubIpc(available=True)),
        # XWayland: Qt runs on xcb, but the backend detects the Wayland session.
        ("xcb", lambda: _WaylandSessionIpc(available=True)),
    ],
    ids=["wayland", "xwayland"],
)
def test_wayland_and_xwayland_never_create_native_surfaces(
    monkeypatch, qtbot, platform, ipc_factory
):
    from neurowatch.widgets import video_wall as video_wall_module

    monkeypatch.setattr(QGuiApplication, "platformName", lambda: platform)
    fake_reader = _stub_frame_reader(monkeypatch)
    created_surfaces: list[object] = []
    real_surface = video_wall_module.NativeViewerSurface

    def _spy_surface(*args: Any, **kwargs: Any) -> Any:
        surface = real_surface(*args, **kwargs)
        created_surfaces.append(surface)
        return surface

    monkeypatch.setattr(video_wall_module, "NativeViewerSurface", _spy_surface)
    ipc = ipc_factory()
    wall = VideoWallWidget(viewer_client=NativeViewerClient(ipc=ipc))  # type: ignore[arg-type]
    qtbot.addWidget(wall)
    wall.set_grid(2, 2)
    cameras = [_cam(f"cam-{i}") for i in range(6)]
    wall.set_cameras(cameras)
    wall.set_page(1)
    wall.set_grid(1, 1)
    wall.set_solo_camera("cam-3")

    assert created_surfaces == []
    assert wall._native_surfaces == {}
    assert not wall.has_native_surfaces
    assert all(cell._native_surface is None for cell in wall._cells)
    assert set(wall._readers) == {cam.id for cam in cameras}
    assert len(fake_reader.instances) == len(cameras)
    assert all(reader.started and not reader.stopped for reader in fake_reader.instances)
    assert not any(
        method in {"viewer.attach", "viewer.set_window"} for method, _ in ipc.calls
    )


def test_close_detaches_native_sessions_once(monkeypatch, qtbot):
    _stub_frame_reader(monkeypatch)
    ipc = _StubIpc(available=True)
    wall = VideoWallWidget(viewer_client=NativeViewerClient(ipc=ipc))
    qtbot.addWidget(wall)
    wall.set_grid(1, 1)
    wall.set_cameras([_cam("cam-close")])
    wall.close()
    wall.shutdown()
    assert wall._native_surfaces == {}
    assert sum(method == "viewer.detach" for method, _ in ipc.calls) == 1
