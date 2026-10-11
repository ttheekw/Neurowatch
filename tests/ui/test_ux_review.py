"""UX review regressions: no dead controls, clear statuses, legible placeholders."""

from __future__ import annotations

from unittest.mock import Mock

from PySide6.QtWidgets import QComboBox, QLabel, QPushButton

from neurowatch.models.camera import CameraConfig
from neurowatch.theme import contrast_ratio
from neurowatch.widgets import cell_widget
from neurowatch.widgets.camera_panel import CameraPanel, _CameraItem
from neurowatch.widgets.cell_widget import CellStatus, CellWidget
from neurowatch.widgets.performance_page import PerformancePage
from neurowatch.widgets.playback_timeline import PlaybackTimeline
from neurowatch.widgets.toolbar import ToolBar


def _connected(obj, signature: str) -> bool:
    meta = obj.metaObject()
    return obj.isSignalConnected(meta.method(meta.indexOfSignal(signature)))


def test_toolbar_and_camera_panel_have_no_dead_controls(qtbot):
    toolbar = ToolBar()
    panel = CameraPanel()
    qtbot.addWidget(toolbar)
    qtbot.addWidget(panel)

    for owner in (toolbar, panel):
        for button in owner.findChildren(QPushButton):
            assert _connected(button, "clicked(bool)"), f"{button.text()!r} does nothing"
    for combo in toolbar.findChildren(QComboBox):
        assert _connected(combo, "currentIndexChanged(int)"), combo.toolTip()


def test_camera_names_line_up_whether_or_not_recording(qtbot):
    panel = CameraPanel()
    qtbot.addWidget(panel)
    panel.resize(210, 300)
    panel.set_cameras(
        [
            CameraConfig(id="a", name="A", url="rtsp://a"),
            CameraConfig(id="b", name="B", url="rtsp://b"),
        ]
    )
    panel.set_camera_recording("a", "recording")
    panel.show()
    qtbot.waitExposed(panel)

    def name_x(cam_id: str) -> int:
        item = panel._item_widgets[cam_id]
        label = next(lbl for lbl in item.findChildren(QLabel) if lbl.text() in {"A", "B"})
        return label.x()

    assert name_x("a") == name_x("b")


def test_statuses_are_spelled_out(qtbot):
    item = _CameraItem(CameraConfig(id="a", name="A", url="rtsp://a"))
    qtbot.addWidget(item)
    labels = {}
    for status in ("streaming", "connecting", "reconnecting", "error", "offline"):
        item.set_status(status)
        labels[status] = item._badge.text()
    assert labels == {
        "streaming": "Live",
        "connecting": "Connecting",
        "reconnecting": "Reconnecting",
        "error": "Error",
        "offline": "Offline",
    }

    cell = CellWidget()
    qtbot.addWidget(cell)
    cell.set_camera(CameraConfig(id="a", name="A", url="rtsp://a"))
    cell.set_status(CellStatus.CONNECTING)
    assert cell._badge()[0] == "CONNECTING"
    cell.set_status(CellStatus.ERROR)
    assert cell._badge()[0] == "ERROR"
    painter = Mock()
    cell._draw_offline_text(painter, cell.rect())
    assert painter.drawText.call_args.args[-1] == "ERROR"


def test_empty_slot_text_is_legible():
    background = cell_widget._BG.name()
    assert contrast_ratio(cell_widget._EMPTY_TEXT.name(), background) >= 4.5
    assert contrast_ratio(cell_widget._ASSIGN_TEXT.name(), background) >= 3.0


def test_playback_controls_wait_for_loaded_recordings(qtbot):
    timeline = PlaybackTimeline()
    qtbot.addWidget(timeline)
    buttons = timeline._ctrl.findChildren(QPushButton)
    assert buttons
    assert not any(button.isEnabled() for button in buttons)

    timeline.set_camera_rows([{"label": "Door", "segments": [{"start_ms": 0, "end_ms": 60_000}]}])
    assert all(button.isEnabled() for button in buttons)

    timeline.set_camera_rows([])
    assert not any(button.isEnabled() for button in buttons)


class _NoClient:
    def call(self, method, params=None):
        return {}


def _banner(page: PerformancePage, kind: str, severity: str, explanation: str) -> str:
    page._on_snapshot(
        {"bottleneck": {"kind": kind, "severity": severity, "explanation": explanation}}
    )
    return page.bottleneck_text()


def test_diagnostics_banner_reads_as_a_sentence(qtbot):
    page = PerformancePage(ipc_client=_NoClient(), refresh_interval_ms=0)
    qtbot.addWidget(page)

    assert _banner(page, "none", "ok", "All sources healthy") == "All sources healthy"
    assert _banner(page, "none", "ok", "") == "No bottleneck detected"
    assert (
        _banner(page, "decode_render", "warning", "Frames late")
        == "Decode/render · Warning — Frames late"
    )
