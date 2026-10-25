"""Status bar: the backend's readings (every platform), decoder and backend badges."""

from __future__ import annotations

from pathlib import Path
from threading import Event
from types import SimpleNamespace

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

    bar.set_video_acceleration({"kind": "d3d12", "hardware": True})
    assert (bar._accel_lbl.text(), bar._accel_lbl.mark()) == ("Direct3D 12", "square")

    bar.set_video_acceleration({"kind": "qsv", "hardware": True})
    assert (bar._accel_lbl.text(), bar._accel_lbl.mark()) == ("Quick Sync", "square")
    assert "quick sync" in bar._accel_lbl.toolTip().lower()

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


def test_recording_disk_path_uses_the_nearest_existing_parent(tmp_path, monkeypatch) -> None:
    monkeypatch.setenv("HOME", str(tmp_path))
    monkeypatch.setenv("USERPROFILE", str(tmp_path))
    assert status_mod.recording_disk_path("~/not-created/camera") == tmp_path
    recordings = tmp_path / "recordings"
    recordings.mkdir()
    assert status_mod.recording_disk_path(str(recordings / "camera")) == recordings
    assert status_mod.recording_disk_path(str(recordings)) == recordings


def test_disk_label_reads_the_recording_drive_and_refreshes_on_change(
    qtbot, tmp_path, monkeypatch
) -> None:
    paths: list[Path] = []

    def usage(path: Path) -> SimpleNamespace:
        paths.append(path)
        return SimpleNamespace(free=(120 if path == tmp_path else 42) * 1024**3)

    monkeypatch.setattr(status_mod.shutil, "disk_usage", usage)
    bar = status_mod.StatusBarWidget()
    qtbot.addWidget(bar)
    assert bar._disk_lbl.text() == "Disk: --" and paths == []
    configured = str(tmp_path / "future-recordings" / "camera")
    bar.set_storage_path(configured)
    qtbot.waitUntil(lambda: not bar._disk_pending)
    assert paths == [tmp_path]
    assert bar._disk_lbl.text() == "Disk: 120 GB Free"
    assert bar._disk_lbl.toolTip() == f"Free space where recordings are saved: {configured}"

    other = tmp_path / "other-drive"
    other.mkdir()
    bar.set_storage_path(str(other))
    qtbot.waitUntil(lambda: not bar._disk_pending)
    assert paths == [tmp_path, other]
    assert bar._disk_lbl.text() == "Disk: 42 GB Free"
    bar.set_storage_path("")
    assert bar._disk_lbl.text() == "Disk: --"


def test_disk_read_errors_show_unknown(qtbot, tmp_path, monkeypatch) -> None:
    def unavailable(path: Path) -> SimpleNamespace:
        raise OSError("drive disconnected")

    monkeypatch.setattr(status_mod.shutil, "disk_usage", unavailable)
    bar = status_mod.StatusBarWidget()
    qtbot.addWidget(bar)
    bar.set_storage_path(str(tmp_path))
    qtbot.waitUntil(lambda: not bar._disk_pending)
    assert bar._disk_lbl.text() == "Disk: --"


def test_a_stalled_disk_read_never_blocks_qt_or_starts_more_readers(
    qtbot, tmp_path, monkeypatch
) -> None:
    entered, release = Event(), Event()
    paths: list[Path] = []

    def slow_usage(path: Path) -> SimpleNamespace:
        paths.append(path)
        entered.set()
        assert release.wait(timeout=5), "test must release the stalled disk reader"
        return SimpleNamespace(free=12 * 1024**3)

    monkeypatch.setattr(status_mod.shutil, "disk_usage", slow_usage)
    bar = status_mod.StatusBarWidget()
    qtbot.addWidget(bar)
    bar.set_storage_path(str(tmp_path))
    try:
        qtbot.waitUntil(entered.is_set)
        monkeypatch.setattr(status_mod, "_DISK_TIMEOUT_SECONDS", 0)
        bar._poll_disk_result()
        assert bar._disk_lbl.text() == "Disk: --"
        bar._update_disk()
        bar._update_disk()
        assert paths == [tmp_path] and bar._disk_pending
    finally:
        release.set()
        qtbot.waitUntil(lambda: not bar._disk_pending)
    assert bar._disk_lbl.text() == "Disk: 12 GB Free"


def test_a_result_from_the_previous_path_cannot_replace_the_new_reading(
    qtbot, tmp_path, monkeypatch
) -> None:
    entered, release = Event(), Event()
    other = tmp_path / "other-drive"
    other.mkdir()

    def usage(path: Path) -> SimpleNamespace:
        if path == tmp_path:
            entered.set()
            assert release.wait(timeout=5)
        return SimpleNamespace(free=(9 if path == tmp_path else 87) * 1024**3)

    monkeypatch.setattr(status_mod.shutil, "disk_usage", usage)
    bar = status_mod.StatusBarWidget()
    qtbot.addWidget(bar)
    bar.set_storage_path(str(tmp_path))
    try:
        qtbot.waitUntil(entered.is_set)
        bar.set_storage_path(str(other))
    finally:
        release.set()
        qtbot.waitUntil(lambda: not bar._disk_pending)
    assert bar._disk_lbl.text() == "Disk: 87 GB Free"
    assert bar._disk_lbl.toolTip().endswith(str(other))


def test_a_stalled_previous_drive_does_not_block_a_new_drive(qtbot, tmp_path, monkeypatch) -> None:
    entered, release = Event(), Event()
    healthy = tmp_path / "healthy"
    healthy.mkdir()

    def usage(path: Path) -> SimpleNamespace:
        if path == tmp_path:
            entered.set()
            assert release.wait(timeout=5)
        return SimpleNamespace(free=(9 if path == tmp_path else 87) * 1024**3)

    monkeypatch.setattr(status_mod.shutil, "disk_usage", usage)
    bar = status_mod.StatusBarWidget()
    qtbot.addWidget(bar)
    bar.set_storage_path(str(tmp_path))
    try:
        qtbot.waitUntil(entered.is_set)
        bar.set_storage_path(str(healthy))
        qtbot.waitUntil(lambda: bar._disk_lbl.text() == "Disk: 87 GB Free", timeout=1000)
        assert not release.is_set(), "the healthy drive must not wait for the old drive"
    finally:
        release.set()
        qtbot.waitUntil(lambda: not bar._disk_pending)
