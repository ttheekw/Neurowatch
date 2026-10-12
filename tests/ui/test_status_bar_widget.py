"""Status-bar host metrics work without optional third-party packages."""

from __future__ import annotations

import neurowatch.widgets.status_bar_widget as status_mod


def test_proc_fallback_populates_cpu_and_memory(qtbot, monkeypatch):
    samples = iter([(100, 1_000), (120, 1_100)])
    monkeypatch.setattr(status_mod, "_HAS_PSUTIL", False)
    monkeypatch.setattr(status_mod, "_read_proc_cpu_sample", lambda: next(samples))
    monkeypatch.setattr(status_mod, "_read_proc_memory_used_mb", lambda: 2048.0)

    bar = status_mod.StatusBarWidget()
    qtbot.addWidget(bar)
    assert bar._cpu_lbl.text() == "CPU: …"
    assert bar._mem_lbl.text() == "MEM: 2048 MB"

    bar._update_stats()

    assert bar._cpu_lbl.text() == "CPU: 80%"
    assert bar._mem_lbl.text() == "MEM: 2048 MB"


def test_acceleration_badge_distinguishes_hardware_and_software(qtbot):
    bar = status_mod.StatusBarWidget()
    qtbot.addWidget(bar)

    bar.set_video_acceleration({"kind": "vaapi", "hardware": True})
    assert bar._accel_lbl.text() == "▣ VA-API"
    assert "hardware" in bar._accel_lbl.toolTip().lower()
    assert "border: none" in bar._accel_lbl.styleSheet()
    assert "background: transparent" in bar._accel_lbl.styleSheet()

    bar.set_video_acceleration({"kind": "nvdec", "hardware": True})
    assert bar._accel_lbl.text() == "◆ NVIDIA NVDEC"

    bar.set_video_acceleration({"kind": "d3d11", "hardware": True})
    assert bar._accel_lbl.text() == "▣ Direct3D 11"
    assert "hardware" in bar._accel_lbl.toolTip().lower()

    bar.set_video_acceleration({"kind": "software", "hardware": False})
    assert bar._accel_lbl.text() == "◌ CPU Decode"

    bar.set_video_acceleration(
        {
            "kind": "blocked",
            "hardware": False,
            "reason": "GStreamer update required",
        }
    )
    assert bar._accel_lbl.text() == "! Update GStreamer"
    assert bar._accel_lbl.toolTip() == "GStreamer update required"
