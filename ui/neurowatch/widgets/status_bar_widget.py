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

import logging
import shutil
import time
from collections.abc import Mapping
from pathlib import Path
from queue import Empty, SimpleQueue
from threading import Thread

from PySide6.QtCore import QPointF, QRectF, QSize, Qt, QTimer
from PySide6.QtGui import QColor, QPainter, QPaintEvent, QPen, QPolygonF
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
logger = logging.getLogger(__name__)
_DISK_TIMEOUT_SECONDS = 2.0
_MAX_DISK_READERS = 2


def recording_disk_path(path: str) -> Path:
    """Nearest existing ancestor, before a new recording folder is created.

    Filesystem checks belong on the disk reader thread: a disconnected network
    mount can stall even an exists() call.
    """
    folder = Path(path).expanduser().absolute()
    while not folder.exists() and folder.parent != folder:
        folder = folder.parent
    return folder


def _read_disk(path: str, results: SimpleQueue[tuple[str, int | None]]) -> None:
    try:
        free = shutil.disk_usage(recording_disk_path(path)).free
    except (OSError, ValueError, RuntimeError) as exc:
        logger.debug("Could not read recording disk space for %s: %s", path, exc)
        free = None
    results.put((path, free))


#: Every reading (extensions' indicators too): one size, centred, without
#: the bar's own background and border. Scoped to the bar: a style sheet
#: without a selector would give every label the bar's top border too,
#: pushing its text a pixel down.
_LABEL_RULE = (
    "QWidget#statusBar QLabel { background: transparent; border: none;"
    " font-size: 10px; padding: 0 6px; margin: 0; }"
)


def _bar_style(own_background: bool) -> str:
    if own_background:
        bar = (
            f"QWidget#statusBar {{ background-color: {_BG}; border-top: 1px solid {_SEP_COLOR}; }}"
        )
    else:
        bar = "QWidget#statusBar { background: transparent; border: none; }"
    return bar + _LABEL_RULE


def _stat(text: str, bright: bool = False) -> QLabel:
    lbl = QLabel(text)
    lbl.setAlignment(Qt.AlignmentFlag.AlignVCenter | Qt.AlignmentFlag.AlignLeft)
    lbl.setStyleSheet(f"color: {_TEXT_BRIGHT if bright else _TEXT};")
    return lbl


def _sep() -> QFrame:
    """A short divider between readings, centred like the text."""
    f = QFrame()
    f.setObjectName("statusSeparator")
    f.setFixedSize(1, 12)
    f.setStyleSheet(f"QFrame#statusSeparator {{ background-color: {_SEP_COLOR}; border: none; }}")
    return f


class StatusIndicator(QLabel):
    """A reading led by a small mark (dot, square, diamond, ring, alert),
    drawn here and centred on the text's capitals. Font symbols (●, ▣, ◌)
    sit lower than the words, differently in every font, and some fonts
    lack them. The mark takes the text's colour (``color:`` in a style
    sheet). Extensions use it for their indicators too."""

    MARKS = ("dot", "square", "diamond", "ring", "alert")
    _MARK_WIDTH = 8
    _GAP = 5

    def __init__(self, text: str = "", mark: str = "dot", parent: QWidget | None = None) -> None:
        super().__init__(text, parent)
        self._mark = mark
        self.setAlignment(Qt.AlignmentFlag.AlignVCenter | Qt.AlignmentFlag.AlignLeft)

    def mark(self) -> str:
        return self._mark

    def set_mark(self, mark: str) -> None:
        if mark != self._mark:
            self._mark = mark
            self.update()

    def sizeHint(self) -> QSize:
        hint = super().sizeHint()
        return QSize(hint.width() + self._MARK_WIDTH + self._GAP, hint.height())

    def minimumSizeHint(self) -> QSize:  # noqa: N802
        hint = super().minimumSizeHint()
        return QSize(hint.width() + self._MARK_WIDTH + self._GAP, hint.height())

    def mark_center(self) -> QPointF:
        """Where the mark is drawn: level with the middle of the capitals."""
        area = self.contentsRect()
        metrics = self.fontMetrics()
        baseline = area.top() + (area.height() - metrics.height()) / 2 + metrics.ascent()
        return QPointF(area.left() + self._MARK_WIDTH / 2, baseline - metrics.capHeight() / 2)

    def paintEvent(self, _event: QPaintEvent) -> None:
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing)
        color = self.palette().color(self.foregroundRole())
        area = self.contentsRect()
        text_area = area.adjusted(self._MARK_WIDTH + self._GAP, 0, 0, 0)
        painter.setPen(color)
        painter.setFont(self.font())
        painter.drawText(
            text_area,
            int(Qt.AlignmentFlag.AlignVCenter | Qt.AlignmentFlag.AlignLeft),
            self.text(),
        )
        center = self.mark_center()
        r = self._MARK_WIDTH / 2
        painter.setPen(Qt.PenStyle.NoPen)
        painter.setBrush(color)
        if self._mark == "square":
            painter.drawRoundedRect(QRectF(center.x() - r, center.y() - r, 2 * r, 2 * r), 1.5, 1.5)
        elif self._mark == "diamond":
            painter.drawPolygon(
                QPolygonF(
                    [
                        QPointF(center.x(), center.y() - r),
                        QPointF(center.x() + r, center.y()),
                        QPointF(center.x(), center.y() + r),
                        QPointF(center.x() - r, center.y()),
                    ]
                )
            )
        elif self._mark == "ring":
            painter.setPen(QPen(color, 1.4))
            painter.setBrush(Qt.BrushStyle.NoBrush)
            painter.drawEllipse(center, r - 0.8, r - 0.8)
        elif self._mark == "alert":
            painter.drawEllipse(center, r, r)
            painter.setPen(QPen(QColor(_BG), 1.5))  # the "!" cut out of the dot
            painter.drawLine(
                QPointF(center.x(), center.y() - r + 2), QPointF(center.x(), center.y() + 0.5)
            )
            painter.drawPoint(QPointF(center.x(), center.y() + r - 1.8))
        else:
            painter.drawEllipse(center, r - 1, r - 1)


class StatusBarWidget(QWidget):
    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setObjectName("statusBar")
        self.setFixedHeight(_HEIGHT)
        self.setAttribute(Qt.WidgetAttribute.WA_StyledBackground)
        self.setStyleSheet(_bar_style(own_background=True))

        layout = QHBoxLayout(self)
        # 1 px at the top is the bar's border: centre the readings below it.
        layout.setContentsMargins(8, 1, 8, 0)
        layout.setSpacing(0)
        layout.setAlignment(Qt.AlignmentFlag.AlignVCenter)

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
        self._accel_lbl = StatusIndicator("Decoder: —", "ring")
        self._accel_shown: tuple[str, object] | None = None
        self._accel_lbl.setAccessibleName("Video acceleration unknown")
        self._accel_lbl.setAlignment(Qt.AlignmentFlag.AlignVCenter)
        self._accel_lbl.setStyleSheet(f"color: {_TEXT}; font-weight: 600;")
        layout.addWidget(self._accel_lbl)
        layout.addWidget(_sep())

        # Backend status dot
        self._backend_lbl = StatusIndicator("Backend", "dot")
        self._backend_lbl.setAlignment(Qt.AlignmentFlag.AlignVCenter)
        self._backend_lbl.setStyleSheet("color: #22c55e;")
        layout.addWidget(self._backend_lbl)
        # Extensions' indicators go here, before the version (add_indicator).
        self._version_sep = _sep()
        layout.addWidget(self._version_sep)

        # Version on right
        self._version_lbl = _stat(build_info.build_identity().display)
        self._version_lbl.setTextFormat(Qt.TextFormat.PlainText)
        layout.addWidget(self._version_lbl)

        # Free disk space is read here; the other readings come from the backend.
        self._storage_path = ""
        self._disk_pending = False
        self._disk_started = 0.0
        self._disk_reads: dict[str, float] = {}
        self._disk_results: SimpleQueue[tuple[str, int | None]] = SimpleQueue()
        self._disk_poll = QTimer(self)
        self._disk_poll.setInterval(100)
        self._disk_poll.timeout.connect(self._poll_disk_result)
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
        self.setStyleSheet(_bar_style(own_background))
        # The top border, when the bar has one, takes 1 px above the readings.
        layout = self.layout()
        if layout is not None:
            layout.setContentsMargins(8, 1 if own_background else 0, 8, 0)
        for widget in (self._version_sep, self._version_lbl):
            widget.setVisible(full)
        self._spacer.setVisible(full)

    def set_backend_status(self, online: bool) -> None:
        if online:
            self._backend_lbl.setText("Backend")
            self._backend_lbl.setStyleSheet("color: #22c55e;")
        else:
            self._backend_lbl.setText("Backend Offline")
            self._backend_lbl.setStyleSheet("color: #ef4444;")

    def add_indicator(self, widget: QWidget) -> None:
        """Show an extension's indicator after the backend dot, before the version."""
        layout = self.layout()
        assert isinstance(layout, QHBoxLayout)
        index = layout.indexOf(self._version_sep)
        layout.insertWidget(index, _sep())
        layout.insertWidget(index + 1, widget)

    def set_video_acceleration(self, acceleration: object) -> None:
        """Render the decoder selected by the backend as a compact badge.

        Called at load and with every performance reading: the backend picks
        its decoder when it starts, and a backend started before sign-in
        restarts itself once the graphics card is available."""
        info = acceleration if isinstance(acceleration, Mapping) else {}
        shown = (str(info.get("kind", "unknown")).lower(), info.get("reason"))
        if shown == self._accel_shown:
            return  # re-sent every poll: restyling is not free
        self._accel_shown = shown
        kind = shown[0]
        badges = {
            "vaapi": ("square", "VA-API", "#67e8f9", "Intel/AMD VA-API hardware decode"),
            "nvdec": ("diamond", "NVIDIA NVDEC", "#a3e635", "NVIDIA hardware decode"),
            "d3d11": ("square", "Direct3D 11", "#67e8f9", "Direct3D 11 hardware decode"),
            "d3d12": ("square", "Direct3D 12", "#67e8f9", "Direct3D 12 hardware decode"),
            "qsv": ("square", "Quick Sync", "#67e8f9", "Intel Quick Sync hardware decode"),
            "v4l2_m2m": ("square", "V4L2 M2M", "#c4b5fd", "V4L2 hardware decode"),
            "software": (
                "ring",
                "CPU Decode",
                "#fbbf24",
                str(info.get("reason") or "Software video decode"),
            ),
            "blocked": (
                "alert",
                "Update GStreamer",
                "#ff8b81",
                str(
                    info.get("reason")
                    or "Local live view is unavailable until GStreamer is updated"
                ),
            ),
        }
        icon, label, color, tooltip = badges.get(
            kind, ("ring", "Decoder: —", _TEXT, "Video decoder is unknown")
        )
        self._accel_lbl.setText(label)
        self._accel_lbl.set_mark(icon)
        self._accel_lbl.setAccessibleName(f"Video acceleration: {label}")
        self._accel_lbl.setToolTip(tooltip)
        self._accel_lbl.setStyleSheet(f"color: {color}; font-weight: 600;")

    def set_storage_path(self, path: str) -> None:
        """Refresh the recording drive at load, Settings save and config reload."""
        if path == self._storage_path:
            return
        self._storage_path = path
        self._disk_pending = False
        self._disk_lbl.setText("Disk: --")
        self._disk_lbl.setToolTip(f"Free space where recordings are saved: {path}")
        self._update_disk()

    def _update_disk(self) -> None:
        if not self._storage_path or self._disk_pending:
            return
        if self._storage_path in self._disk_reads:
            self._disk_pending = True
            self._disk_started = self._disk_reads[self._storage_path]
            return
        if len(self._disk_reads) >= _MAX_DISK_READERS:
            return
        self._disk_pending = True
        self._disk_started = time.monotonic()
        self._disk_reads[self._storage_path] = self._disk_started
        # One read per path, with a spare slot so switching away from a hung
        # drive can recover. The total stays bounded even if several mounts
        # hang; daemon readers hold no widgets and cannot block app exit.
        Thread(
            target=_read_disk,
            args=(self._storage_path, self._disk_results),
            daemon=True,
            name="recording-disk-space",
        ).start()
        self._disk_poll.start()

    def _poll_disk_result(self) -> None:
        try:
            path, free = self._disk_results.get_nowait()
        except Empty:
            if (
                self._disk_pending
                and time.monotonic() - self._disk_started >= _DISK_TIMEOUT_SECONDS
            ):
                self._disk_lbl.setText("Disk: --")
            return
        self._disk_reads.pop(path, None)
        if not self._disk_reads:
            self._disk_poll.stop()
        if path != self._storage_path:
            self._update_disk()  # A Settings change superseded this read.
        else:
            self._disk_pending = False
            if free is None:
                self._disk_lbl.setText("Disk: --")
            else:
                self._disk_lbl.setText(f"Disk: {free / (1024**3):.0f} GB Free")

    def set_performance(self, snapshot: object) -> None:
        """Show the backend's CPU, memory and network readings (the
        ``stats.get_performance`` result); None while the backend is offline."""
        if isinstance(snapshot, Mapping) and "video_acceleration" in snapshot:
            self.set_video_acceleration(snapshot["video_acceleration"])
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
