"""Bottom status bar — disk, CPU, MEM, network, decoder, backend, extensions, version.

In compact view ("Hide panels") the main window shows the same bar in its
title strip (``set_inline``), without the version, and in fullscreen over
the video while that strip is hidden (``set_overlay``), laid out as at the
bottom.
"""

from __future__ import annotations

import shutil
from importlib.util import find_spec

from PySide6.QtCore import Qt, QTimer
from PySide6.QtWidgets import QFrame, QHBoxLayout, QLabel, QSizePolicy, QWidget

from neurowatch import build_info
from neurowatch.utils.network import NetworkMeter, format_bitrate

_HEIGHT = 24
_BG = "#0a0a14"
#: The bar's background as RGB, for a translucent copy of it.
BAR_RGB = (0x0A, 0x0A, 0x14)
_TEXT = "#7878a0"
_TEXT_BRIGHT = "#a0a0c0"
_SEP_COLOR = "#1a1a30"

_HAS_PSUTIL = find_spec("psutil") is not None


def _read_proc_cpu_sample() -> tuple[int, int] | None:
    """Return Linux aggregate (idle ticks, total ticks), if procfs is available."""
    try:
        with open("/proc/stat", encoding="utf-8") as stat_file:
            first_line = stat_file.readline()
        fields = [int(value) for value in first_line.split()[1:]]
    except (OSError, ValueError):
        return None
    if len(fields) < 4:
        return None
    idle = fields[3] + (fields[4] if len(fields) > 4 else 0)
    return idle, sum(fields)


def _read_proc_memory_used_mb() -> float | None:
    """Return used system memory from Linux MemAvailable, in MiB."""
    try:
        lines = open("/proc/meminfo", encoding="utf-8")
    except OSError:
        return None
    total_kb: int | None = None
    available_kb: int | None = None
    try:
        for line in lines:
            if line.startswith("MemTotal:"):
                total_kb = int(line.split()[1])
            elif line.startswith("MemAvailable:"):
                available_kb = int(line.split()[1])
    except (IndexError, ValueError):
        return None
    finally:
        lines.close()
    if total_kb is None or available_kb is None:
        return None
    return max(0, total_kb - available_kb) / 1024


def _stat(text: str, bright: bool = False) -> QLabel:
    lbl = QLabel(text)
    lbl.setStyleSheet(
        f"color: {'#a0a0c0' if bright else '#7878a0'}; font-size: 10px; padding: 0 6px;"
    )
    return lbl


def _sep() -> QFrame:
    f = QFrame()
    f.setFrameShape(QFrame.Shape.VLine)
    f.setFixedWidth(1)
    f.setStyleSheet(f"color: {_SEP_COLOR};")
    return f


class StatusBarWidget(QWidget):
    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setFixedHeight(_HEIGHT)
        self.setAttribute(Qt.WidgetAttribute.WA_StyledBackground)
        self.setStyleSheet(f"background-color: {_BG}; border-top: 1px solid {_SEP_COLOR};")

        self._last_cpu_sample: tuple[int, int] | None = None

        layout = QHBoxLayout(self)
        layout.setContentsMargins(8, 0, 8, 0)
        layout.setSpacing(0)

        self._disk_lbl = _stat("Disk: --")
        layout.addWidget(self._disk_lbl)
        layout.addWidget(_sep())

        self._cpu_lbl = _stat("CPU: --")
        layout.addWidget(self._cpu_lbl)
        layout.addWidget(_sep())

        self._mem_lbl = _stat("MEM: --")
        layout.addWidget(self._mem_lbl)
        layout.addWidget(_sep())

        # Download and upload over the host's network interfaces.
        self._network = NetworkMeter()
        self._net_lbl = _stat("NET: --")
        self._net_lbl.setToolTip("Network traffic of this computer: download ↓ and upload ↑")
        # Room for wide readings, so the bar does not shift as the rates change.
        widest = "NET ↓ 888 Mbps  ↑ 888 Mbps"
        self._net_lbl.setMinimumWidth(self._net_lbl.fontMetrics().horizontalAdvance(widest) + 16)
        layout.addWidget(self._net_lbl)

        # Flexible spacer
        self._spacer = QWidget()
        self._spacer.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Preferred)
        layout.addWidget(self._spacer)

        # Active backend decoder. This reports the accelerator Neurowatch
        # selected, which is distinct from whether utilization counters exist.
        self._accel_lbl = QLabel("◌ Decoder: —")
        self._accel_lbl.setAccessibleName("Video acceleration unknown")
        self._accel_lbl.setStyleSheet(
            "color: #7878a0; background: transparent; border: none; "
            "font-size: 10px; font-weight: 600; padding: 1px 7px;"
        )
        layout.addWidget(self._accel_lbl)
        layout.addWidget(_sep())

        # Backend status dot
        self._backend_lbl = QLabel("● Backend")
        self._backend_lbl.setStyleSheet("color: #22c55e; font-size: 10px; padding: 0 6px;")
        layout.addWidget(self._backend_lbl)
        # Extensions' indicators go here, before the version (add_indicator).
        self._version_sep = _sep()
        layout.addWidget(self._version_sep)

        # Version on right
        self._version_lbl = _stat(build_info.build_identity().display)
        self._version_lbl.setTextFormat(Qt.TextFormat.PlainText)
        layout.addWidget(self._version_lbl)

        # Start update timer. The first procfs sample primes the CPU delta;
        # subsequent samples contain a real utilization percentage.
        self._timer = QTimer(self)
        self._timer.setInterval(2000)
        self._timer.timeout.connect(self._update_stats)
        self._timer.start()
        self._update_stats()

    def set_inline(self, inline: bool) -> None:
        """Shown in a title strip (compact view) rather than at the bottom:
        no bar of its own, and only the readings the strip lacks."""
        self._set_layout(full=not inline, own_background=not inline)

    def set_overlay(self) -> None:
        """Shown over the video: laid out exactly as at the bottom, on the
        holder's translucent background instead of its own (a background of
        its own would repeat behind every reading)."""
        self._set_layout(full=True, own_background=False)

    def _set_layout(self, *, full: bool, own_background: bool) -> None:
        self.setAttribute(Qt.WidgetAttribute.WA_StyledBackground, own_background)
        self.setStyleSheet(
            f"background-color: {_BG}; border-top: 1px solid {_SEP_COLOR};"
            if own_background
            else "background: transparent;"
        )
        for widget in (self._version_sep, self._version_lbl):
            widget.setVisible(full)
        self._spacer.setVisible(full)

    def set_backend_status(self, online: bool) -> None:
        if online:
            self._backend_lbl.setText("● Backend")
            self._backend_lbl.setStyleSheet("color: #22c55e; font-size: 10px; padding: 0 6px;")
        else:
            self._backend_lbl.setText("● Backend Offline")
            self._backend_lbl.setStyleSheet("color: #ef4444; font-size: 10px; padding: 0 6px;")

    def add_indicator(self, widget: QWidget) -> None:
        """Show an extension's indicator after the backend dot, before the version."""
        layout = self.layout()
        assert isinstance(layout, QHBoxLayout)
        index = layout.indexOf(self._version_sep)
        layout.insertWidget(index, _sep())
        layout.insertWidget(index + 1, widget)

    def set_video_acceleration(self, acceleration: object) -> None:
        """Render the decoder selected by the backend as a compact badge."""
        info = acceleration if isinstance(acceleration, dict) else {}
        kind = str(info.get("kind", "unknown")).lower()
        badges = {
            "vaapi": ("▣", "VA-API", "#67e8f9", "#12303a", "Intel/AMD VA-API hardware decode"),
            "nvdec": ("◆", "NVIDIA NVDEC", "#a3e635", "#243415", "NVIDIA hardware decode"),
            "d3d11": ("▣", "Direct3D 11", "#67e8f9", "#12303a", "Direct3D 11 hardware decode"),
            "v4l2_m2m": ("▤", "V4L2 M2M", "#c4b5fd", "#29213d", "V4L2 hardware decode"),
            "software": ("◌", "CPU Decode", "#fbbf24", "#3a2b12", "Software video decode"),
            "blocked": (
                "!",
                "Update GStreamer",
                "#ff8b81",
                "#3a1717",
                str(
                    info.get("reason")
                    or "Local live view is unavailable until GStreamer is updated"
                ),
            ),
        }
        icon, label, color, _background, tooltip = badges.get(
            kind,
            ("◌", "Decoder: —", "#7878a0", "#151522", "Video decoder is unknown"),
        )
        self._accel_lbl.setText(f"{icon} {label}")
        self._accel_lbl.setAccessibleName(f"Video acceleration: {label}")
        self._accel_lbl.setToolTip(tooltip)
        self._accel_lbl.setStyleSheet(
            f"color: {color}; background: transparent; border: none; "
            "font-size: 10px; font-weight: 600; padding: 1px 7px;"
        )

    def _update_stats(self) -> None:
        rate = self._network.sample()
        if rate is None:
            self._net_lbl.setText("NET: --")
        else:
            self._net_lbl.setText(
                f"NET ↓ {format_bitrate(rate.rx_bits_per_second)}"
                f"  ↑ {format_bitrate(rate.tx_bits_per_second)}"
            )

        # Disk
        try:
            usage = shutil.disk_usage("/")
            gb_free = usage.free / (1024**3)
            self._disk_lbl.setText(f"Disk: {gb_free:.0f} GB Free")
        except Exception:
            self._disk_lbl.setText("Disk: --")

        # CPU and MEM via psutil (optional dependency)
        if _HAS_PSUTIL:
            import psutil

            try:
                cpu = psutil.cpu_percent(interval=None)
                mem = psutil.virtual_memory()
                mem_mb = mem.used / (1024**2)
                self._cpu_lbl.setText(f"CPU: {cpu:.0f}%")
                self._mem_lbl.setText(f"MEM: {mem_mb:.0f} MB")
            except Exception:
                pass
        else:
            sample = _read_proc_cpu_sample()
            if sample is not None and self._last_cpu_sample is not None:
                idle, total = sample
                last_idle, last_total = self._last_cpu_sample
                delta_total = total - last_total
                delta_idle = idle - last_idle
                if delta_total > 0:
                    cpu = max(0.0, min(100.0, (delta_total - delta_idle) / delta_total * 100))
                    self._cpu_lbl.setText(f"CPU: {cpu:.0f}%")
            elif sample is None:
                self._cpu_lbl.setText("CPU: --")
            else:
                self._cpu_lbl.setText("CPU: …")
            self._last_cpu_sample = sample

            mem_mb = _read_proc_memory_used_mb()
            if mem_mb is None:
                self._mem_lbl.setText("MEM: --")
            else:
                self._mem_lbl.setText(f"MEM: {mem_mb:.0f} MB")
