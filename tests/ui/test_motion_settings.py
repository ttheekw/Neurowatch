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
    """The recorder knows only off and onvif: NeuroWatch's own detection is the
    Motion plugin, bound like any other."""
    assert source_of({}) == "off"
    assert source_of({"enabled": True}) == "off"
    assert source_of({"enabled": True, "source": "onvif"}) == "onvif"
    assert uses_camera_events({"source": "onvif"})
    assert not uses_camera_events({"source": "off"})


def test_switching_camera_events_keeps_the_other_settings():
    on = with_camera_events({"source": "off", "sensitivity": 3}, True)
    assert on == {"source": "onvif", "sensitivity": 3, "enabled": False}
    assert with_camera_events(on, False) == {"source": "off", "sensitivity": 3, "enabled": False}
    assert with_camera_events({}, True) == {"source": "onvif", "enabled": False}
    assert with_camera_events({"enabled": True}, False)["source"] == "off"


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


def test_the_camera_dialog_has_no_detector_of_its_own(qtbot):
    from neurowatch.dialogs.camera_dialog import AddEditCameraDialog

    for motion in (MOTION, {"source": "off"}, None):
        extra = {"motion": motion} if motion is not None else {}
        dialog = AddEditCameraDialog(camera=_camera(**extra))
        qtbot.addWidget(dialog)
        assert not hasattr(dialog, "_builtin_events")
        assert not hasattr(dialog, "_zone_canvas"), "zones belong to the motion plugin now"


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


def test_motion_tab_reads_camera_events_and_plugins_once_at_a_time(qtbot, monkeypatch):
    from neurowatch.dialogs import camera_dialog

    calls = []
    monkeypatch.setattr(camera_dialog._RpcWorker, "start", lambda worker: calls.append(worker))
    dialog = camera_dialog.AddEditCameraDialog(
        camera=_camera(motion={"source": "onvif"}),
        ipc_endpoint="unix:/tmp/motion-test-unused.sock",
    )
    qtbot.addWidget(dialog)
    dialog._load_motion_status()
    assert [(w._method, w._params) for w in calls] == [
        ("motion.status", {"camera_id": "c1"}),
        ("plugins.status", {}),
    ]
    assert calls[0]._endpoint == "unix:/tmp/motion-test-unused.sock"
    dialog._load_motion_status()
    assert len(calls) == 2, "one request of each kind at a time"
    for i in range(dialog._tabs.count()):
        if dialog._tabs.tabText(i) == "Detection":
            dialog._tabs.setCurrentIndex(i)
    assert dialog._test_panel.isHidden(), "connection testing must not crowd motion settings"

    dialog._on_plugins_status(
        {
            "plugins": [
                {
                    "id": "motion",
                    "state": "connected",
                    "manifest": {"id": "nw.motion", "name": "Motion detection"},
                    "bindings": {
                        "c1": {
                            "state": "active",
                            "plugin": {"state": "running", "fps": 5.0, "decoder": "avdec_h264"},
                        }
                    },
                }
            ]
        }
    )
    assert dialog._plugins_lbl.text() == (
        "Analysed by Motion detection "
        "(running · 5 pictures a second · decoded in the plugin with avdec_h264)."
    )
    dialog._on_plugins_status({"plugins": []})
    assert dialog._plugins_lbl.text() == "No plugin analyses this camera."
    dialog._on_plugins_status(RuntimeError("not allowed"))
    assert dialog._plugins_lbl.text() == ""
    dialog._stop_background_work()
    assert dialog._motion_worker is None and dialog._plugins_worker is None
