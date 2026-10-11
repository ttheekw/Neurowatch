"""Camera audio in the desktop app: the tile's speaker, listening live on the
wall (one camera at a time), and hearing one camera in playback."""

from __future__ import annotations

from typing import Any

from PySide6.QtCore import QPoint, QPointF, Qt
from PySide6.QtGui import QMouseEvent
from PySide6.QtWidgets import QApplication

from neurowatch.models.camera import CameraConfig
from neurowatch.widgets.cell_widget import AUDIO_AVAILABLE, AUDIO_NONE, AUDIO_ON, CellWidget


def _click(widget, pos: QPoint) -> None:
    for kind in (QMouseEvent.Type.MouseButtonPress, QMouseEvent.Type.MouseButtonRelease):
        event = QMouseEvent(
            kind,
            QPointF(pos),
            QPointF(widget.mapToGlobal(pos)),
            Qt.MouseButton.LeftButton,
            Qt.MouseButton.LeftButton,
            Qt.KeyboardModifier.NoModifier,
        )
        QApplication.sendEvent(widget, event)


def _cams(n: int) -> list[CameraConfig]:
    return [CameraConfig(id=f"cam-{i}", name=f"Cam {i}", url="/missing.avi") for i in range(n)]


# ---------------------------------------------------------------------------
# The tile's speaker
# ---------------------------------------------------------------------------


def test_a_tile_shows_a_speaker_only_for_a_camera_with_audio(qtbot) -> None:
    cell = CellWidget()
    qtbot.addWidget(cell)
    cell.resize(320, 180)
    cell.set_camera(_cams(1)[0])
    cell.show()
    qtbot.waitExposed(cell)
    clicks: list[str] = []
    cell.audio_toggle_requested.connect(clicks.append)

    cell.grab()  # paint: lays out the speaker
    assert cell._audio_rect.isNull(), "no audio, no speaker"

    cell.set_audio(AUDIO_AVAILABLE)
    cell.grab()
    speaker = cell._audio_rect
    assert not speaker.isNull() and speaker.right() > cell.width() - 10, "top right"
    _click(cell, speaker.center())
    assert clicks == ["cam-0"]
    assert "hear this camera" in cell.toolTip()

    cell.set_audio(AUDIO_ON)
    menu = cell._build_context_menu()
    assert menu.actions()[0].text() == "Stop Listening"
    cell.set_mode("playback")
    assert cell._build_context_menu().actions()[0].text() == "Mute Audio"

    cell.set_camera(_cams(2)[1])
    assert cell.audio_state == AUDIO_NONE, "another camera: its own audio"


# ---------------------------------------------------------------------------
# Listening live on the wall
# ---------------------------------------------------------------------------


def test_the_wall_marks_the_camera_heard(qtbot, monkeypatch) -> None:
    from neurowatch.widgets import video_wall

    class Reader:
        def __init__(self, *_a: Any, **_k: Any) -> None:
            self.frame_ready = type("S", (), {"connect": lambda *_: None})()

        def start(self) -> None: ...
        def stop(self) -> None: ...
        def set_max_emit_fps(self, _fps: float) -> None: ...

    monkeypatch.setattr(video_wall, "FrameReaderThread", Reader)
    wall = video_wall.VideoWallWidget()
    qtbot.addWidget(wall)
    wall._native_available = False
    wall.set_grid(1, 2)
    wall.set_cameras(_cams(3))
    toggled: list[str] = []
    wall.listen_toggled.connect(toggled.append)

    wall.set_camera_audio("cam-0", "AAC")
    wall.set_camera_audio("cam-2", "G.711 A-law")
    assert wall._cells[0].audio_state == AUDIO_AVAILABLE
    assert wall._cells[1].audio_state == AUDIO_NONE, "cam-1 has no audio"
    wall._cells[0].audio_toggle_requested.emit("cam-0")
    assert toggled == ["cam-0"]

    wall.set_listening("cam-0")
    assert wall._cells[0].audio_state == AUDIO_ON
    assert wall.visible_camera_ids() == {"cam-0", "cam-1"}
    wall.set_page(1)
    assert wall._cells[0].audio_state == AUDIO_AVAILABLE, "cam-2 shown, not heard"
    wall.set_camera_audio("cam-2", None)
    assert wall._cells[0].audio_state == AUDIO_NONE


def test_listening_is_one_camera_and_ends_when_it_leaves_the_screen(qtbot, monkeypatch) -> None:
    from neurowatch.main import MainWindow

    class Client:
        _endpoint = "unix:/nonexistent/test-audio.sock"

        def __init__(self, *_a: Any, **_k: Any) -> None: ...

        def is_online(self) -> bool:
            return True

        def call(self, method, params=None):
            if method == "config.get":
                return {"wall": {"rows": 1, "cols": 2}}
            if method == "camera.list":
                return [{"id": f"cam-{i}", "name": str(i), "url": "/missing.avi"} for i in range(4)]
            return {}

    monkeypatch.setattr("neurowatch.main.SyncIpcClient", Client)
    monkeypatch.setattr("neurowatch.ipc.frame_reader.SHM_PREFIX", "/nonexistent/nw-test-")
    window = MainWindow()
    qtbot.addWidget(window)
    try:
        window._on_poll_done(
            {}, {"cam-0": {"codec": "H264", "audio": "AAC"}, "cam-1": {"codec": "H264"}}, {}, True
        )
        assert window._wall._cells[0].audio_state == AUDIO_AVAILABLE

        window._wall._cells[0].audio_toggle_requested.emit("cam-0")
        assert window._listening == "cam-0"
        assert window._view_lease.audio_camera == "cam-0", "the lease names it"
        assert window._wall._cells[0].audio_state == AUDIO_ON

        window._wall._cells[0].audio_toggle_requested.emit("cam-0")
        assert window._listening is None, "the speaker again: stop"
        assert window._view_lease.audio_camera is None

        window._toggle_listen("cam-0")
        window._wall.set_page(1)  # cam-0 leaves the screen
        window._sync_view_demand()
        assert window._listening is None, "a page change ends it"
        assert window._view_lease.audio_camera is None
    finally:
        window.close()


# ---------------------------------------------------------------------------
# Playback: one camera heard at a time
# ---------------------------------------------------------------------------


def _playback_view(qtbot):
    from neurowatch.widgets.playback_view import PlaybackView

    class Client:
        endpoint = "unix:/nonexistent/test-playback-audio.sock"

    view = PlaybackView(Client(), _cams(3))  # type: ignore[arg-type]
    qtbot.addWidget(view)
    sent: list[tuple[str, dict]] = []
    view._queue_controls = sent.extend  # type: ignore[method-assign]
    for i in range(3):
        cell = CellWidget()
        cell.set_camera(_cams(3)[i])
        cell.set_mode("playback")
        cell.audio_toggle_requested.connect(lambda _c, sid=f"s{i}": view._toggle_audio(sid))
        view._cells[f"s{i}"] = cell
    return view, sent


def test_playback_hears_the_first_camera_with_audio(qtbot) -> None:
    view, sent = _playback_view(qtbot)
    statuses = [{"has_audio": False}, {"has_audio": True}, {"has_audio": True}]
    view._on_audio_status(["s0", "s1", "s2"], statuses)

    assert view.audio_session == "s1", "the first with audio"
    assert sent == [("playback.set_audio", {"session_id": "s1", "enabled": True})]
    states = [view._cells[s].audio_state for s in ("s0", "s1", "s2")]
    assert states == [AUDIO_NONE, AUDIO_ON, AUDIO_AVAILABLE]


def test_a_tiles_speaker_moves_the_sound_or_mutes_it(qtbot) -> None:
    view, sent = _playback_view(qtbot)
    view._on_audio_status(["s0", "s1", "s2"], [{"has_audio": True}] * 3)
    sent.clear()

    view._cells["s2"].audio_toggle_requested.emit("cam-2")
    assert view.audio_session == "s2"
    assert sent == [
        ("playback.set_audio", {"session_id": "s0", "enabled": False}),
        ("playback.set_audio", {"session_id": "s2", "enabled": True}),
    ], "one camera at a time"
    sent.clear()

    view._cells["s2"].audio_toggle_requested.emit("cam-2")
    assert view.audio_session is None
    assert sent == [("playback.set_audio", {"session_id": "s2", "enabled": False})]
    # Muted by hand: a later status check does not turn audio back on.
    view._on_audio_status(["s0", "s1", "s2"], [{"has_audio": True}] * 3)
    assert view.audio_session is None
