"""Live preview frame-rate limits: the model and the Settings dialog."""

from __future__ import annotations

from neurowatch.dialogs.settings_dialog import SettingsDialog
from neurowatch.models.preview_fps import PreviewFps


def test_defaults_keep_the_old_limits_and_pick_by_what_is_shown() -> None:
    limits = PreviewFps.from_config({})
    assert limits == PreviewFps(single=25, wall=15, dense=4, hidden=2)
    assert limits.visible(1, single_view=True) == 25.0
    assert limits.visible(16) == 15.0, "a page of up to 16 tiles"
    assert limits.visible(17) == 4.0


def test_odd_or_out_of_range_values_are_clamped_or_ignored() -> None:
    limits = PreviewFps.from_config(
        {"preview_fps": {"single": 500, "wall": "x", "dense": 0, "hidden": 99}}
    )
    assert limits == PreviewFps(single=60, wall=15, dense=1, hidden=30)
    assert PreviewFps.from_config({"preview_fps": None}) == PreviewFps()
    assert PreviewFps.from_config(None) == PreviewFps()


def test_settings_edit_the_limits(qtbot, monkeypatch) -> None:
    monkeypatch.setattr(SettingsDialog, "_scan_drives", lambda self: None)
    dlg = SettingsDialog({"ui": {"preview_fps": {"single": 30, "wall": 20}}})
    qtbot.addWidget(dlg)
    assert dlg._fps_single.value() == 30 and dlg._fps_wall.value() == 20
    assert dlg._fps_dense.value() == 4 and dlg._fps_hidden.value() == 2, "defaults"
    assert dlg._fps_hidden.maximum() == 30 and dlg._fps_single.maximum() == 60

    dlg._fps_wall.setValue(30)
    dlg._fps_hidden.setValue(1)
    assert dlg.get_config()["ui"]["preview_fps"] == {
        "single": 30,
        "wall": 30,
        "dense": 4,
        "hidden": 1,
    }
