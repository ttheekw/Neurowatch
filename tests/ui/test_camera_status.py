"""Recording readiness is online; recording must not recolor connectivity."""
from __future__ import annotations

import pytest

from neurowatch.models.camera import CameraConfig, connection_status
from neurowatch.widgets.camera_panel import CameraPanel
from neurowatch.widgets.video_wall import VideoWallWidget


@pytest.mark.parametrize("state", ["offline", "error", "connecting", "reconnecting"])
def test_real_connection_states_are_not_masked(state):
    assert connection_status(state) == state


def test_recording_ready_remains_online_between_frames_and_polls(qtbot):
    camera = CameraConfig(id="recording-camera", name="Recording camera")
    panel = CameraPanel()
    wall = VideoWallWidget()
    qtbot.addWidget(panel)
    qtbot.addWidget(wall)
    panel.set_cameras([camera])
    # Exercise frame/poll ordering without starting a real SHM reader.
    wall._cell_by_cam_id[camera.id] = 0
    wall._cameras = [camera]
    wall._cells[0].set_camera(camera)
    wall.camera_status_changed.connect(panel.set_camera_status)
    panel._filter = "online"

    for _ in range(5):
        wall._on_frame_ready(camera.id, bytes(12), 2, 2, "rgb")
        panel.set_camera_status(camera.id, "recording_ready")
        wall.set_cell_status(camera.id, "recording_ready")
        panel.set_camera_recording(camera.id, "recording")
        item = panel._item_widgets[camera.id]
        assert item._badge.text() == "Live"
        assert item._status == wall._cells[0]._status == "streaming"
        assert "#22c55e" in item._dot.styleSheet()
        assert "#ef4444" in item._recording_dot.styleSheet()
        assert not item._recording_dot.isHidden()

    panel._rebuild_list()
    assert not panel._item_widgets[camera.id]._recording_dot.isHidden()
    panel.set_camera_status(camera.id, "offline")
    wall.set_cell_status(camera.id, "offline")
    assert camera.id not in panel._item_widgets  # Online filter reflects a real outage.
    assert wall._cells[0]._status == "offline"


def test_recording_updates_do_not_modify_connection_dot(qtbot):
    panel = CameraPanel()
    qtbot.addWidget(panel)
    camera = CameraConfig(id="camera", name="Camera")
    panel.set_cameras([camera])
    panel.set_camera_status(camera.id, "recording_ready")
    item = panel._item_widgets[camera.id]
    connection_style = item._dot.styleSheet()
    for recording_state in ("recording", "scheduled", "idle"):
        panel.set_camera_recording(camera.id, recording_state)
        assert item._dot.styleSheet() == connection_style
        assert item._badge.text() == "Live"
    assert item._recording_dot.isHidden()
