"""Motion settings: the camera model keeps them, the Motion tab changes only
the camera-events switch, and ``motion.status`` reads as plain text."""

from __future__ import annotations

from neurowatch.models.camera import CameraConfig
from neurowatch.models.motion import (
    camera_event_topics,
    camera_events_status,
    source_of,
    uses_camera_events,
    with_camera_events,
)

MOTION = {
    "enabled": False,
    "source": "onvif",
    "sensitivity": 7,
    "cooldown_sec": 10.0,
    "min_duration_sec": 0.5,
    "zones": [{"id": "z1", "name": "", "kind": "exclude", "points": [[0, 0], [1, 0], [1, 1]]}],
}


def _camera(**extra: object) -> CameraConfig:
    return CameraConfig.from_dict(
        {"id": "c1", "name": "Door", "url": "rtsp://10.0.0.5:554/s1", **extra}
    )


def test_camera_model_round_trips_motion_and_onvif():
    onvif = {"device_url": "http://10.0.0.5/onvif/device_service"}
    cam = _camera(motion=MOTION, onvif=onvif)
    out = cam.to_dict()
    assert out["motion"] == MOTION
    assert out["onvif"] == onvif
    # Nothing known: left out, so the backend keeps what it has.
    bare = _camera().to_dict()
    assert "motion" not in bare
    assert "onvif" not in bare


def test_sources_follow_the_backend_rules():
    assert source_of({}) == "off"
    assert source_of({"enabled": True}) == "builtin"
    assert source_of({"enabled": True, "source": "onvif"}) == "onvif"
    assert uses_camera_events({"source": "both"})
    assert not uses_camera_events({"source": "builtin"})


def test_switching_camera_events_keeps_the_builtin_detector_and_settings():
    on = with_camera_events({"source": "builtin", "sensitivity": 3}, True)
    assert on == {"source": "both", "sensitivity": 3, "enabled": False}
    assert with_camera_events(on, False)["source"] == "builtin"
    assert with_camera_events({}, True) == {"source": "onvif", "enabled": False}
    assert with_camera_events({"enabled": True}, False)["source"] == "builtin"


def test_camera_events_status_text():
    def status(onvif: object, source: str = "onvif") -> dict:
        return {"cameras": {"c1": {"source": source, "onvif": onvif, "active": []}}}

    subscribed = status({"state": "subscribed", "topics": ["RuleEngine/LineDetector/Crossed"]})
    subscribed["cameras"]["c1"]["kinds"] = ["motion", "line_crossing"]
    text, tone = camera_events_status(subscribed, "c1")
    assert tone == "success"
    assert text.endswith("It reports motion and line crossing.")
    assert camera_event_topics(subscribed, "c1") == ["RuleEngine/LineDetector/Crossed"]
    pushed = status({"state": "subscribed", "delivery": "push"})
    assert "sends them to NeuroWatch as they happen" in camera_events_status(pushed, "c1")[0]
    pulled = status({"state": "subscribed", "delivery": "pull", "note": "a firewall"})
    assert (
        "asks the camera for them. Sending them directly did not work: a firewall."
        in (camera_events_status(pulled, "c1")[0])
    )
    quiet = status({"state": "subscribed", "topics": []})
    assert "Nothing reported yet" in camera_events_status(quiet, "c1")[0]
    text, tone = camera_events_status(
        status({"state": "unauthorized", "detail": "the camera asks for a username"}), "c1"
    )
    assert tone == "error"
    assert "username" in text
    assert camera_events_status(status(None, source="off"), "c1")[0] == "Camera events are off."
    assert camera_events_status({}, "c1")[1] == "info"
    assert camera_events_status(status(None), "c1")[1] == "info"


def test_motion_tab_sends_motion_only_when_changed(qtbot):
    from neurowatch.dialogs.camera_dialog import AddEditCameraDialog

    cam = _camera(motion=MOTION, onvif={"device_url": "http://10.0.0.5/onvif/device_service"})
    dlg = AddEditCameraDialog(camera=cam)
    qtbot.addWidget(dlg)
    assert dlg._camera_events.isChecked()
    unchanged = dlg.get_config()
    assert unchanged.motion == MOTION
    assert unchanged.onvif == cam.onvif

    dlg._camera_events.setChecked(False)
    off = dlg.get_config().to_dict()
    assert off["motion"]["source"] == "off"
    assert off["motion"]["zones"] == MOTION["zones"]


def test_motion_tab_on_a_new_camera(qtbot):
    from neurowatch.dialogs.camera_dialog import AddEditCameraDialog

    dlg = AddEditCameraDialog()
    qtbot.addWidget(dlg)
    assert not dlg._camera_events.isChecked()
    assert "motion" not in dlg.get_config().to_dict()
    dlg._camera_events.setChecked(True)
    assert dlg.get_config().to_dict()["motion"] == {"source": "onvif", "enabled": False}


def test_builtin_controls_keep_camera_detection_zones_and_saved_credentials(qtbot):
    from neurowatch.dialogs.camera_dialog import AddEditCameraDialog

    camera = _camera(motion=MOTION, username="admin", password_b64="c2VjcmV0")
    dialog = AddEditCameraDialog(camera=camera)
    qtbot.addWidget(dialog)
    dialog._builtin_events.setChecked(True)
    dialog._motion_sensitivity.setValue(8)
    dialog._motion_cooldown.setValue(5.0)
    updated = dialog.get_config()
    assert updated.motion["source"] == "both"
    assert updated.motion["sensitivity"] == 8
    assert updated.motion["cooldown_sec"] == 5.0
    assert updated.motion["zones"] == MOTION["zones"]
    assert updated.password_b64 == "c2VjcmV0"


def test_zone_editor_changes_round_trip_in_camera_settings(qtbot):
    from PySide6.QtCore import QPointF
    from PySide6.QtGui import QPixmap

    from neurowatch.dialogs.camera_dialog import AddEditCameraDialog

    dialog = AddEditCameraDialog(camera=_camera(motion={"source": "builtin", "zones": []}))
    qtbot.addWidget(dialog)
    picture = QPixmap(640, 360)
    picture.fill()
    dialog._zone_canvas.set_snapshot(picture)
    dialog._add_zone("include")
    for point in (QPointF(10, 10), QPointF(200, 10), QPointF(200, 150)):
        dialog._zone_canvas.append_point(point)
    zones = dialog.get_config().motion["zones"]
    assert len(zones) == 1
    assert zones[0]["kind"] == "include"
    assert len(zones[0]["points"]) == 3


def test_camera_edits_preserve_new_recording_policy_fields(qtbot):
    from neurowatch.dialogs.camera_dialog import AddEditCameraDialog

    recording = {
        "mode": "events",
        "weekly_rules": [],
        "event_pre_sec": 12,
        "event_post_sec": 20,
        "non_event_keep_hours": 2,
        "local": {"enabled": True},
    }
    camera = _camera(recording=recording)
    dialog = AddEditCameraDialog(camera=camera)
    qtbot.addWidget(dialog)
    updated = dialog.get_config().to_dict()["recording"]
    for key in ("mode", "weekly_rules", "event_pre_sec", "event_post_sec", "non_event_keep_hours"):
        assert updated[key] == recording[key]


def test_low_cpu_mode_round_trips_and_explains_detection_tradeoff(qtbot):
    from neurowatch.dialogs.camera_dialog import AddEditCameraDialog

    dialog = AddEditCameraDialog(camera=_camera(motion={**MOTION, "low_cpu": True}))
    qtbot.addWidget(dialog)
    assert dialog._motion_low_cpu.isChecked()
    assert "miss" in dialog._motion_low_cpu.toolTip().lower()
    assert dialog.get_config().motion["low_cpu"] is True
    dialog._motion_low_cpu.setChecked(False)
    assert dialog.get_config().motion["low_cpu"] is False


def test_live_analysis_uses_bounded_actual_frames_and_clears_stale_overlay(qtbot):
    import base64

    from neurowatch.dialogs.camera_dialog import AddEditCameraDialog

    dialog = AddEditCameraDialog(camera=_camera(motion={"source": "builtin"}))
    qtbot.addWidget(dialog)
    preview = {
        "width": 320,
        "height": 180,
        "gray_b64": base64.b64encode(bytes([40]) * 57600).decode(),
    }
    builtin = {"state": "running", "analysis_fps": 5, "score": 0.1, "blocks": [0, 143]}
    dialog._on_motion_status({"cameras": {"c1": {"builtin": builtin, "preview": preview}}})
    assert dialog._analysis_preview.has_frame()
    assert dialog._analysis_preview.active_blocks() == [0, 143]
    assert dialog._motion_score.value() == 10
    dialog._on_motion_status({"cameras": {"c1": {"builtin": {"state": "backoff"}}}})
    assert not dialog._analysis_preview.has_frame()
    assert dialog._analysis_preview.active_blocks() == []
    assert dialog._motion_score.value() == 0
    preview["gray_b64"] = "too short"
    dialog._on_motion_status({"cameras": {"c1": {"builtin": builtin, "preview": preview}}})
    assert not dialog._analysis_preview.has_frame()


def test_analysis_preview_polling_is_visible_single_camera_and_coalesced(qtbot, monkeypatch):
    from neurowatch.dialogs import camera_dialog

    calls = []
    monkeypatch.setattr(camera_dialog._RpcWorker, "start", lambda worker: calls.append(worker))
    dialog = camera_dialog.AddEditCameraDialog(
        camera=_camera(motion={"source": "builtin"}),
        ipc_endpoint="unix:/tmp/motion-test-unused.sock",
    )
    qtbot.addWidget(dialog)
    dialog._load_motion_status()
    assert len(calls) == 1
    assert calls[0]._endpoint == "unix:/tmp/motion-test-unused.sock"
    assert calls[0]._params == {"camera_id": "c1", "include_preview": False}
    dialog._load_motion_status()
    assert len(calls) == 1
    dialog._on_motion_status({"cameras": {}})
    for i in range(dialog._tabs.count()):
        if dialog._tabs.tabText(i) == "Motion":
            dialog._tabs.setCurrentIndex(i)
    assert dialog._test_panel.isHidden(), "connection testing must not crowd motion controls"
    monkeypatch.setattr(dialog, "isVisible", lambda: True)
    dialog._load_motion_status()
    assert calls[-1]._params == {"camera_id": "c1", "include_preview": True}
    dialog._stop_background_work()
    assert dialog._motion_worker is None
