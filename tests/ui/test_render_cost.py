"""Per-frame paint cost: text overlays and list styling must not redo work each frame.

Profiling a 16-camera wall on a real display showed about half of the busy
main thread in re-creating fonts and laying out the same tile text for every
video frame, and a further ~15% in restyling unchanged camera-list rows on
every status poll.
"""

from __future__ import annotations

from neurowatch.models.camera import CameraConfig
from neurowatch.widgets import cell_widget as cell_widget_module
from neurowatch.widgets.camera_panel import CameraPanel
from neurowatch.widgets.cell_widget import CellWidget

_FRAME = bytes(16 * 9 * 3)  # black 16x9 RGB frame


def _streaming_cell(qtbot, monkeypatch) -> CellWidget:
    monkeypatch.setattr(cell_widget_module.time, "strftime", lambda _fmt: "12:00:00")
    cell = CellWidget()
    qtbot.addWidget(cell)
    cell.resize(480, 270)
    cell.set_camera(CameraConfig(id="cam-1", name="Front door"))
    cell.set_stream_info("H264", 15.0)
    cell.set_recording("recording")
    cell.update_frame(_FRAME, 16, 9, "rgb")
    return cell


def _count_renders(monkeypatch) -> list[str]:
    calls: list[str] = []
    for name in ("_render_label_strip", "_render_info_strip"):
        original = getattr(CellWidget, name)

        def spy(self, *args, _original=original, _name=name):
            calls.append(_name)
            return _original(self, *args)

        monkeypatch.setattr(CellWidget, name, spy)
    return calls


def test_tile_text_is_laid_out_once_not_for_every_video_frame(qtbot, monkeypatch):
    cell = _streaming_cell(qtbot, monkeypatch)
    renders = _count_renders(monkeypatch)

    for _ in range(10):
        cell.update_frame(_FRAME, 16, 9, "rgb")
        cell.grab()

    assert sorted(renders) == ["_render_info_strip", "_render_label_strip"]


def test_tile_text_is_redrawn_when_what_it_shows_changes(qtbot, monkeypatch):
    cell = _streaming_cell(qtbot, monkeypatch)
    renders = _count_renders(monkeypatch)
    cell.grab()
    renders.clear()

    cell.set_status("offline")
    cell.grab()
    assert "_render_label_strip" in renders, "LIVE -> OFFLINE badge"

    renders.clear()
    monkeypatch.setattr(cell_widget_module.time, "strftime", lambda _fmt: "12:00:01")
    cell.grab()
    assert renders == ["_render_info_strip"], "the clock ticked"


def test_tile_overlays_are_still_painted(qtbot, monkeypatch):
    cell = _streaming_cell(qtbot, monkeypatch)
    image = cell.grab().toImage()

    def has_overlay(x0: int, y0: int, x1: int, y1: int) -> bool:
        return any(
            image.pixelColor(x, y).lightness() > 40
            for x in range(x0, x1, 2)
            for y in range(y0, y1, 2)
        )

    assert has_overlay(2, 2, 160, 18), "name label and LIVE badge"
    assert has_overlay(140, 250, 340, 268), "clock / codec / fps strip"


def test_camera_list_rows_are_not_restyled_when_nothing_changed(qtbot, monkeypatch):
    panel = CameraPanel()
    qtbot.addWidget(panel)
    panel.set_cameras([CameraConfig(id="cam-1", name="Door")])
    panel.set_camera_status("cam-1", "recording_ready")
    panel.set_camera_recording("cam-1", "recording")
    item = panel._item_widgets["cam-1"]
    restyled: list[str] = []
    for name in ("_dot", "_badge", "_recording_dot"):
        widget = getattr(item, name)
        monkeypatch.setattr(widget, "setStyleSheet", lambda _sheet, n=name: restyled.append(n))

    for _ in range(5):  # one status poll every 2 s
        panel.set_camera_status("cam-1", "recording_ready")
        panel.set_camera_recording("cam-1", "recording")

    assert restyled == []
    panel.set_camera_status("cam-1", "offline")
    assert "_dot" in restyled and "_badge" in restyled
