"""Bottom status bar — disk, CPU, MEM, network, decoder, backend, extensions, version.

CPU, memory and network are the backend's readings (``stats.get_performance``,
polled by the main window and passed to ``set_performance``), the same the
Diagnostics page shows, on every platform; only the free disk space is read
here.

In compact view ("Hide panels") the main window shows the same bar in its
title strip (``set_inline``), without the version, and in fullscreen over
the video while that strip is hidden (``set_overlay``), laid out as at the
bottom.
"""

from __future__ import annotations

import shutil
from collections.abc import Mapping

from PySide6.QtCore import Qt, QTimer
from PySide6.QtWidgets import QFrame, QHBoxLayout, QLabel, QSizePolicy, QWidget

from neurowatch import build_info
from neurowatch.utils.network import format_bitrate

_HEIGHT = 24
_BG = "#0a0a14"
#: The bar's background as RGB, for a translucent copy of it.
BAR_RGB = (0x0A, 0x0A, 0x14)
_TEXT = "#7878a0"
_TEXT_BRIGHT = "#a0a0c0"
_SEP_COLOR = "#1a1a30"


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

        # Free disk space is read here; the other readings come from the backend.
        self._timer = QTimer(self)
        self._timer.setInterval(2000)
        self._timer.timeout.connect(self._update_disk)
        self._timer.start()
        self._update_disk()
        self.set_performance(None)

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

    def _update_disk(self) -> None:
        try:
            usage = shutil.disk_usage("/")
            gb_free = usage.free / (1024**3)
            self._disk_lbl.setText(f"Disk: {gb_free:.0f} GB Free")
        except OSError:
            self._disk_lbl.setText("Disk: --")

    def set_performance(self, snapshot: object) -> None:
        """Show the backend's CPU, memory and network readings (the
        ``stats.get_performance`` result); None while the backend is offline."""
        process = snapshot.get("process") if isinstance(snapshot, Mapping) else None
        process = process if isinstance(process, Mapping) else {}
        cpu = process.get("system_cpu_percent")
        memory = process.get("system_memory_percent")
        network = process.get("network")
        self._cpu_lbl.setText(f"CPU: {cpu:.0f}%" if isinstance(cpu, int | float) else "CPU: --")
        self._mem_lbl.setText(
            f"MEM: {memory:.0f}%" if isinstance(memory, int | float) else "MEM: --"
        )
        if isinstance(network, Mapping):
            self._net_lbl.setText(
                f"NET ↓ {format_bitrate(network.get('rx_bits_per_sec'))}"
                f"  ↑ {format_bitrate(network.get('tx_bits_per_sec'))}"
            )
        else:
            self._net_lbl.setText("NET: --")
