"""Status bar: the backend's readings (every platform), decoder and backend badges."""

from __future__ import annotations

import neurowatch.widgets.status_bar_widget as status_mod


def test_cpu_memory_and_network_are_the_backends_readings(qtbot):
    bar = status_mod.StatusBarWidget()
    qtbot.addWidget(bar)
    # Before the first reading (and while the backend is offline).
    assert bar._cpu_lbl.text() == "CPU: --"
    assert bar._mem_lbl.text() == "MEM: --"
    assert bar._net_lbl.text() == "NET: --"

    bar.set_performance(
        {
            "process": {
                "system_cpu_percent": 37.6,
                "system_memory_percent": 61.2,
                "network": {"rx_bits_per_sec": 12_340_000, "tx_bits_per_sec": 1_024_000_000},
            }
        }
    )
    assert bar._cpu_lbl.text() == "CPU: 38%"
    assert bar._mem_lbl.text() == "MEM: 61%"
    assert bar._net_lbl.text() == "NET ↓ 12.3 Mbps  ↑ 1.02 Gbps"

    # The first sample has no CPU delta and no network rate yet.
    bar.set_performance({"process": {"system_memory_percent": 50, "network": None}})
    assert bar._cpu_lbl.text() == "CPU: --" and bar._mem_lbl.text() == "MEM: 50%"
    assert bar._net_lbl.text() == "NET: --"
    bar.set_performance(None)
    assert bar._mem_lbl.text() == "MEM: --"


def test_the_bar_reads_no_linux_only_files():
    source = status_mod.__file__
    text = open(source, encoding="utf-8").read()
    assert "/proc" not in text, "CPU, memory and network come from the backend"


def test_acceleration_badge_distinguishes_hardware_and_software(qtbot):
    bar = status_mod.StatusBarWidget()
    qtbot.addWidget(bar)

    bar.set_video_acceleration({"kind": "vaapi", "hardware": True})
    assert (bar._accel_lbl.text(), bar._accel_lbl.mark()) == ("VA-API", "square")
    assert "hardware" in bar._accel_lbl.toolTip().lower()
    # No box of its own: the bar's label rule (shared by every reading).
    assert "QLabel { background: transparent; border: none;" in bar.styleSheet()
    assert "border" not in bar._accel_lbl.styleSheet()
    assert "background" not in bar._accel_lbl.styleSheet()

    bar.set_video_acceleration({"kind": "nvdec", "hardware": True})
    assert (bar._accel_lbl.text(), bar._accel_lbl.mark()) == ("NVIDIA NVDEC", "diamond")

    bar.set_video_acceleration({"kind": "d3d11", "hardware": True})
    assert (bar._accel_lbl.text(), bar._accel_lbl.mark()) == ("Direct3D 11", "square")
    assert "hardware" in bar._accel_lbl.toolTip().lower()

    bar.set_video_acceleration({"kind": "software", "hardware": False})
    assert (bar._accel_lbl.text(), bar._accel_lbl.mark()) == ("CPU Decode", "ring")

    bar.set_video_acceleration(
        {
            "kind": "blocked",
            "hardware": False,
            "reason": "GStreamer update required",
        }
    )
    assert (bar._accel_lbl.text(), bar._accel_lbl.mark()) == ("Update GStreamer", "alert")
    assert bar._accel_lbl.toolTip() == "GStreamer update required"


def test_the_badge_follows_a_backend_that_restarted_once_the_gpu_was_available(qtbot):
    """A backend started before sign-in decodes on the CPU (and says why) until
    it restarts itself; the badge follows the performance poll, not only the
    first load."""
    bar = status_mod.StatusBarWidget()
    qtbot.addWidget(bar)
    waiting = {
        "kind": "software",
        "hardware": False,
        "reason": "The graphics card is not available to NeuroWatch yet",
    }
    bar.set_performance({"process": {}, "video_acceleration": waiting})
    assert bar._accel_lbl.text() == "CPU Decode"
    assert bar._accel_lbl.toolTip() == waiting["reason"]

    bar.set_performance({"process": {}, "video_acceleration": {"kind": "vaapi", "hardware": True}})
    assert bar._accel_lbl.text() == "VA-API"
    bar.set_performance(None)  # backend offline: the last known decoder stays
    assert bar._accel_lbl.text() == "VA-API"


def test_every_reading_is_centred_the_same_way(qtbot):
    """A style sheet without a selector gave every label the bar's top border
    (text a pixel low) and the decoder badge its own padding (another height)."""
    from PySide6.QtWidgets import QLabel

    from neurowatch.widgets.status_bar_widget import StatusBarWidget

    bar = StatusBarWidget()
    qtbot.addWidget(bar)
    bar.set_video_acceleration({"kind": "blocked"})
    extension = QLabel("● Cloud")
    extension.setStyleSheet("color: #22c55e; font-size: 10px; padding: 0 6px;")
    bar.add_indicator(extension)
    for mode in ("bottom", "inline", "overlay"):
        if mode == "inline":
            bar.set_inline(True)
        elif mode == "overlay":
            bar.set_overlay()
        bar.resize(1200, 24)
        bar.show()
        qtbot.waitExposed(bar)
        labels = [label for label in bar.findChildren(QLabel) if label.isVisible()]
        assert len(labels) >= 6
        boxes = {(label.geometry().y(), label.geometry().height()) for label in labels}
        assert len(boxes) == 1, (mode, boxes)
        inset = 1 if mode == "bottom" else 0  # the bottom bar's top border
        ((top, height),) = boxes
        free = bar.height() - inset
        assert abs((top - inset) - (free - height - (top - inset))) <= 1, (mode, top, height)
        assert all(label.contentsRect().y() == 0 for label in labels), mode


def test_indicator_marks_sit_level_with_the_text_in_any_font(qtbot):
    """Font symbols (●, ▣) sat a pixel or two below the words, and some
    fonts lack ◌: the marks are drawn, centred on the capitals."""
    from PySide6.QtGui import QFont

    for size in (9, 10, 13):
        label = status_mod.StatusIndicator("Backend", "dot")
        qtbot.addWidget(label)
        font = QFont(label.font())
        font.setPixelSize(size)
        label.setFont(font)
        label.resize(label.sizeHint().width(), 24)
        metrics = label.fontMetrics()
        area = label.contentsRect()
        baseline = area.top() + (area.height() - metrics.height()) / 2 + metrics.ascent()
        assert abs(label.mark_center().y() - (baseline - metrics.capHeight() / 2)) < 0.01
        assert label.sizeHint().width() > metrics.horizontalAdvance("Backend")


def test_an_added_indicator_sits_before_the_version(qtbot):
    from PySide6.QtWidgets import QLabel

    from neurowatch.widgets.status_bar_widget import StatusBarWidget

    bar = StatusBarWidget()
    qtbot.addWidget(bar)
    light = QLabel("● Example")
    bar.add_indicator(light)

    layout = bar.layout()
    assert (
        layout.indexOf(bar._backend_lbl) < layout.indexOf(light) < layout.indexOf(bar._version_lbl)
    )
