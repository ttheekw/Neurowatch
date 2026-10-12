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
