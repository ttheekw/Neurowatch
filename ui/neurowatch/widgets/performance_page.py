"""Performance diagnostics page.

Renders a periodic snapshot of the backend's ``stats.get_performance``
JSON-RPC response. Every value shown here is measured — the page never
fabricates zeros for missing sources. When a collector reports itself as
unavailable, the page renders the *reason* string so operators can tell
"no GPU" apart from "permission denied" apart from "backend offline".

Threading model:
- The IPC call runs on a private ``_StatsWorker(QThread)`` so the main
  Qt thread never blocks on socket I/O.
- Rendering happens back on the main thread via signal/slot.
"""

from __future__ import annotations

import logging
from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

from PySide6.QtCore import Qt, QThread, QTimer, Signal
from PySide6.QtGui import QCloseEvent
from PySide6.QtWidgets import (
    QFrame,
    QGridLayout,
    QHBoxLayout,
    QLabel,
    QScrollArea,
    QSizePolicy,
    QVBoxLayout,
    QWidget,
)

from neurowatch.utils.network import format_bitrate

logger = logging.getLogger(__name__)


# Severity → hex color. Kept out of a stylesheet string so the color used
# for the top-of-page bottleneck banner is discoverable from tests.
_SEVERITY_COLORS = {
    "ok": "#3fbf6f",
    "warning": "#e0a020",
    "critical": "#d0453d",
}
_DEFAULT_BOTTLENECK_COLOR = "#5a5a80"

_ACCEL_PRESENTATION = {
    "vaapi": (
        "▣",
        "VA-API",
        "Hardware video decoding is active through VA-API.",
        "#67e8f9",
    ),
    "nvdec": (
        "◆",
        "NVIDIA NVDEC",
        "Hardware video decoding is active on NVIDIA NVDEC.",
        "#a3e635",
    ),
    "d3d11": (
        "▣",
        "Direct3D 11",
        "Hardware video decoding is active through Direct3D 11.",
        "#67e8f9",
    ),
    "v4l2_m2m": (
        "▤",
        "V4L2 M2M",
        "Hardware video decoding is active through V4L2 M2M.",
        "#c4b5fd",
    ),
    "software": (
        "◌",
        "CPU Decode",
        "Hardware decoding is unavailable; video is decoded in software.",
        "#fbbf24",
    ),
    "blocked": (
        "!",
        "Local live view blocked",
        "Update GStreamer to a supported runtime containing the 1.24.6 VA DMA-BUF fix.",
        "#ff8b81",
    ),
}


# ---------------------------------------------------------------------------
# Worker
# ---------------------------------------------------------------------------


class _StatsWorker(QThread):
    """Runs one ``stats.get_performance`` call and emits the result."""

    snapshot_ready = Signal(object)  # dict OR Exception

    def __init__(self, factory: Callable[[], Any]) -> None:
        super().__init__()
        self._factory = factory

    def run(self) -> None:
        try:
            client = self._factory()
            result = client.call("stats.get_performance")
            self.snapshot_ready.emit(result)
        except Exception as e:
            self.snapshot_ready.emit(e)


# ---------------------------------------------------------------------------
# Snapshot text helpers — pure functions, unit-testable via public helpers.
# ---------------------------------------------------------------------------


def _fmt_bytes(n: int | float | None) -> str:
    if n is None:
        return "—"
    n = float(n)
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if n < 1024.0:
            return f"{n:.1f} {unit}"
        n /= 1024.0
    return f"{n:.1f} PiB"


def _fmt_pct(v: float | int | None, precision: int = 1) -> str:
    if v is None:
        return "—"
    return f"{float(v):.{precision}f}%"


def _fmt_number(v: float | int | None, precision: int = 1) -> str:
    if v is None:
        return "—"
    return f"{float(v):.{precision}f}"


# ---------------------------------------------------------------------------
# Widget
# ---------------------------------------------------------------------------


@dataclass
class _Row:
    key: QLabel
    val: QLabel


_BOTTLENECK_NAMES = {
    "cpu": "CPU",
    "gpu": "GPU",
    "ram": "Memory",
    "ipc": "IPC",
    "decode_render": "Decode/render",
}


def _bottleneck_sentence(kind: str, severity: str, explanation: str) -> str:
    """Banner text in words, e.g. ``CPU · Critical — CPU saturated at 96%``."""
    if kind.lower() == "none":
        return explanation or "No bottleneck detected"
    name = _BOTTLENECK_NAMES.get(kind.lower(), kind.upper())
    text = f"{name} · {severity.capitalize()}"
    return f"{text} — {explanation}" if explanation else text


class PerformancePage(QWidget):
    """Diagnostics page — CPU/RAM/GPU/IPC/per-camera + current bottleneck."""

    # Emitted on the main thread after each refresh reply (snapshot or error)
    # has been rendered into the page.
    snapshot_rendered = Signal()

    def __init__(
        self,
        ipc_client: Any = None,
        client_factory: Callable[[], Any] | None = None,
        refresh_interval_ms: int = 2000,
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        if ipc_client is None and client_factory is None:
            raise ValueError("PerformancePage requires ipc_client or client_factory")
        self._client = ipc_client
        self._client_factory = client_factory or (lambda: ipc_client)
        self._refresh_interval_ms = int(refresh_interval_ms)
        self._active_worker: _StatsWorker | None = None
        self._last_snapshot: dict[str, Any] | None = None
        self._last_error: str | None = None

        self.setObjectName("PerformancePage")
        self.setStyleSheet("QWidget#PerformancePage { background-color: #12121c; color: #c8c8dc; }")
        self._build_ui()

        self._timer = QTimer(self)
        self._timer.setSingleShot(False)
        self._timer.timeout.connect(self.refresh_now)

    # ------------------------------------------------------------------
    # Public API used by main window + tests
    # ------------------------------------------------------------------

    def start_refresh(self) -> None:
        """Kick off the first refresh and start the periodic timer."""
        if self._refresh_interval_ms > 0:
            self._timer.start(self._refresh_interval_ms)
        self.refresh_now()

    def stop_refresh(self) -> None:
        self._timer.stop()

    def refresh_now(self) -> None:
        """Start an asynchronous refresh; ``snapshot_rendered`` fires when done.

        Returns immediately. A request already in flight is not duplicated —
        the next timer tick picks up fresh data instead.
        """
        if self._active_worker is not None:
            return
        worker = _StatsWorker(self._client_factory)
        worker.snapshot_ready.connect(self._on_snapshot)
        worker.finished.connect(lambda w=worker: self._on_worker_finished(w))
        worker.finished.connect(worker.deleteLater)
        self._active_worker = worker
        worker.start()

    # ------------------------------------------------------------------
    # Test-oriented accessors
    # ------------------------------------------------------------------

    def snapshot_text(self) -> str:
        """Concatenated, human-readable text of the entire page.
        Used by tests to assert content without coupling to layout."""
        parts: list[str] = []
        for widget in (
            self._process_grid_container,
            self._gpu_container,
            self._ipc_grid_container,
            self._cameras_container,
            self._bottleneck_container,
        ):
            for child in widget.findChildren(QLabel):
                text = child.text()
                if text:
                    parts.append(text)
        if self._last_error is not None:
            parts.append(f"error: {self._last_error}")
        return "\n".join(parts)

    def gpu_utilization_text(self) -> str:
        return self._gpu_utilization_label.text()

    def acceleration_text(self) -> str:
        return f"{self._accel_badge_label.text()} {self._accel_detail_label.text()}"

    def bottleneck_text(self) -> str:
        return self._bottleneck_label.text()

    def bottleneck_style(self) -> str:
        return self._bottleneck_label.styleSheet()

    # ------------------------------------------------------------------
    # Rendering
    # ------------------------------------------------------------------

    def _build_ui(self) -> None:
        root = QVBoxLayout(self)
        root.setContentsMargins(24, 24, 24, 24)
        root.setSpacing(16)

        title = QLabel("System Performance")
        title.setStyleSheet("font-size: 18px; font-weight: bold; color: #c8c8dc;")
        root.addWidget(title)

        self._bottleneck_container = self._section_frame()
        b_layout = QVBoxLayout(self._bottleneck_container)
        b_layout.setContentsMargins(12, 8, 12, 8)
        self._bottleneck_label = QLabel("Awaiting first snapshot…")
        self._bottleneck_label.setWordWrap(True)
        self._bottleneck_label.setStyleSheet(
            "font-size: 13px; padding: 4px; color: #ffffff; "
            f"background: {_DEFAULT_BOTTLENECK_COLOR}; border-radius: 4px;"
        )
        b_layout.addWidget(self._bottleneck_label)
        root.addWidget(self._bottleneck_container)

        # Scrollable body for very-many-camera deployments.
        scroll = QScrollArea()
        scroll.setWidgetResizable(True)
        scroll.setStyleSheet(
            "QScrollArea { border: none; background: #12121c; }"
            "QScrollArea > QWidget > QWidget { background: #12121c; }"
        )
        body = QWidget()
        body.setObjectName("performanceBody")
        body.setStyleSheet("QWidget#performanceBody { background-color: #12121c; }")
        body_layout = QVBoxLayout(body)
        body_layout.setContentsMargins(0, 0, 0, 0)
        body_layout.setSpacing(12)

        # Process + IPC row.
        row = QHBoxLayout()
        self._process_grid_container = self._labeled_section("Backend process")
        self._process_grid = self._grid_of(
            self._process_grid_container,
            (
                "Backend CPU",
                "Backend RSS",
                "Backend Mem %",
                "System CPU",
                "System Memory",
                "Network down",
                "Network up",
                "Siblings",
            ),
        )
        cpu_scale_help = (
            "Top-style process CPU: 100% equals one fully used logical CPU core. "
            "Use System CPU for whole-machine utilization."
        )
        self._process_grid["Backend CPU"].key.setText("Backend CPU (one-core scale):")
        self._process_grid["Backend CPU"].key.setToolTip(cpu_scale_help)
        self._process_grid["Backend CPU"].val.setToolTip(cpu_scale_help)
        row.addWidget(
            self._process_grid_container,
            0,
            Qt.AlignmentFlag.AlignTop,
        )

        self._ipc_grid_container = self._labeled_section("RPC control plane")
        self._ipc_grid = self._grid_of(
            self._ipc_grid_container,
            (
                "Requests",
                "Messages",
                "Bytes in",
                "Bytes out",
                "Avg handler time",
                "p95 handler time",
                "Drops",
                "Backpressure",
                "In-flight",
            ),
        )
        row.addWidget(
            self._ipc_grid_container,
            0,
            Qt.AlignmentFlag.AlignTop,
        )
        body_layout.addLayout(row)

        # Compact decoder strip. Keeping this separate from the variable-height
        # camera list prevents a small status summary from becoming a tall,
        # mostly empty box when many cameras are present.
        self._gpu_container = self._section_frame()
        self._gpu_container.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Maximum)
        gpu_layout = QHBoxLayout(self._gpu_container)
        gpu_layout.setContentsMargins(12, 8, 12, 8)
        gpu_layout.setSpacing(10)
        decoder_heading = QLabel("ACTIVE DECODER")
        decoder_heading.setStyleSheet("font-size: 10px; font-weight: 700; color: #7878a0;")
        gpu_layout.addWidget(decoder_heading)
        self._accel_badge_label = QLabel("◌ Decoder unknown")
        self._accel_badge_label.setAlignment(Qt.AlignmentFlag.AlignCenter)
        self._accel_badge_label.setStyleSheet(
            "color: #7878a0; background: transparent; border: none; "
            "font-size: 12px; font-weight: 700; padding: 0 4px;"
        )
        self._accel_detail_label = QLabel("Waiting for backend decoder information…")
        self._accel_detail_label.setWordWrap(True)
        self._accel_detail_label.setStyleSheet("font-size: 11px; color: #a0a0c0;")
        self._accel_detail_label.setSizePolicy(
            QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Preferred
        )
        self._gpu_status_label = QLabel("—")
        self._gpu_status_label.setStyleSheet("font-size: 10px; color: #a0a0c0;")
        self._gpu_utilization_label = QLabel("—")
        self._gpu_memory_label = QLabel("—")
        self._gpu_decoder_label = QLabel("—")
        for metric_label in (
            self._gpu_utilization_label,
            self._gpu_memory_label,
            self._gpu_decoder_label,
        ):
            metric_label.setStyleSheet("font-size: 10px; color: #a0a0c0;")
        self._gpu_reason_label = QLabel("")
        self._gpu_reason_label.setWordWrap(True)
        self._gpu_reason_label.setStyleSheet("font-size: 10px; color: #e0a020;")
        gpu_layout.addWidget(self._accel_badge_label)
        gpu_layout.addWidget(self._accel_detail_label, stretch=1)

        telemetry_layout = QGridLayout()
        telemetry_layout.setContentsMargins(0, 0, 0, 0)
        telemetry_layout.setHorizontalSpacing(10)
        telemetry_layout.setVerticalSpacing(1)
        telemetry_layout.addWidget(self._gpu_status_label, 0, 0, 1, 2)
        telemetry_layout.addWidget(self._gpu_utilization_label, 1, 0)
        telemetry_layout.addWidget(self._gpu_decoder_label, 1, 1)
        telemetry_layout.addWidget(self._gpu_memory_label, 2, 0, 1, 2)
        telemetry_layout.addWidget(self._gpu_reason_label, 3, 0, 1, 2)
        gpu_layout.addLayout(telemetry_layout)
        body_layout.addWidget(self._gpu_container)

        self._cameras_container = self._labeled_section("Per-camera health")
        self._cameras_body_layout = QVBoxLayout()
        cameras_wrapper = QWidget()
        cameras_wrapper.setLayout(self._cameras_body_layout)
        self._cameras_empty_label = QLabel("No cameras configured.")
        self._cameras_empty_label.setStyleSheet("color: #7878a0; font-size: 11px;")
        self._cameras_body_layout.addWidget(self._cameras_empty_label)
        cameras_section_layout = self._cameras_container.layout()
        assert cameras_section_layout is not None  # installed by _labeled_section
        cameras_section_layout.addWidget(cameras_wrapper)
        body_layout.addWidget(self._cameras_container)

        # Footer with capture timestamp.
        self._captured_label = QLabel("Last capture: —")
        self._captured_label.setStyleSheet("color: #7878a0; font-size: 10px;")
        body_layout.addWidget(self._captured_label)
        body_layout.addStretch(1)

        scroll.setWidget(body)
        root.addWidget(scroll, stretch=1)

    def _section_frame(self) -> QFrame:
        f = QFrame()
        f.setObjectName("diagnosticSection")
        f.setFrameShape(QFrame.Shape.StyledPanel)
        f.setStyleSheet(
            "QFrame#diagnosticSection { background: #1a1a26; "
            "border: 1px solid #23233a; border-radius: 6px; }"
        )
        return f

    def _labeled_section(self, heading: str) -> QWidget:
        f = self._section_frame()
        layout = QVBoxLayout(f)
        layout.setContentsMargins(12, 10, 12, 10)
        layout.setSpacing(8)
        heading_label = QLabel(heading)
        heading_label.setStyleSheet(
            "font-weight: bold; color: #a0a0c0; font-size: 12px; text-transform: uppercase;"
        )
        layout.addWidget(heading_label)
        f.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Preferred)
        return f

    def _grid_of(self, container: QWidget, labels: tuple[str, ...]) -> dict[str, _Row]:
        grid_widget = QWidget()
        grid = QGridLayout(grid_widget)
        grid.setContentsMargins(0, 0, 0, 0)
        grid.setHorizontalSpacing(16)
        grid.setVerticalSpacing(4)
        rows: dict[str, _Row] = {}
        for idx, name in enumerate(labels):
            k = QLabel(f"{name}:")
            k.setStyleSheet("color: #7878a0; font-size: 11px;")
            v = QLabel("—")
            v.setStyleSheet("color: #c8c8dc; font-size: 12px;")
            v.setAlignment(Qt.AlignmentFlag.AlignLeft | Qt.AlignmentFlag.AlignVCenter)
            grid.addWidget(k, idx, 0)
            grid.addWidget(v, idx, 1)
            rows[name] = _Row(key=k, val=v)
        container_layout = container.layout()
        assert container_layout is not None  # installed by _labeled_section
        container_layout.addWidget(grid_widget)
        return rows

    # ------------------------------------------------------------------
    # Snapshot handling
    # ------------------------------------------------------------------

    def _on_worker_finished(self, worker: _StatsWorker) -> None:
        if self._active_worker is worker:
            self._active_worker = None

    def _on_snapshot(self, payload: Any) -> None:
        if isinstance(payload, Exception):
            self._last_error = str(payload)
            self._render_error(str(payload))
        elif not isinstance(payload, dict):
            self._last_error = f"unexpected response type: {type(payload).__name__}"
            self._render_error(self._last_error)
        else:
            self._last_error = None
            self._last_snapshot = payload
            self._render_snapshot(payload)
        self.snapshot_rendered.emit()

    def _render_error(self, reason: str) -> None:
        self._bottleneck_label.setText(f"Backend unavailable: {reason}")
        self._bottleneck_label.setStyleSheet(
            "font-size: 13px; padding: 4px; color: #ffffff; "
            f"background: {_SEVERITY_COLORS['critical']}; border-radius: 4px;"
        )
        for row in self._process_grid.values():
            row.val.setText("—")
        for row in self._ipc_grid.values():
            row.val.setText("—")
        self._render_acceleration({})
        self._gpu_status_label.setText("Hardware telemetry unavailable")
        self._gpu_utilization_label.setText("—")
        self._gpu_memory_label.setText("—")
        self._gpu_decoder_label.setText("—")
        self._gpu_reason_label.setText(f"error: {reason}")
        self._captured_label.setText("Last capture: never (backend error)")

    def _render_snapshot(self, snap: dict[str, Any]) -> None:
        # Process
        proc = snap.get("process", {}) or {}
        self._process_grid["Backend CPU"].val.setText(_fmt_pct(proc.get("cpu_percent")))
        self._process_grid["Backend RSS"].val.setText(_fmt_bytes(proc.get("rss_bytes")))
        self._process_grid["Backend Mem %"].val.setText(_fmt_pct(proc.get("memory_percent")))
        self._process_grid["System CPU"].val.setText(_fmt_pct(proc.get("system_cpu_percent")))
        self._process_grid["System Memory"].val.setText(_fmt_pct(proc.get("system_memory_percent")))
        network = proc.get("network") if isinstance(proc.get("network"), dict) else {}
        self._process_grid["Network down"].val.setText(
            format_bitrate(network.get("rx_bits_per_sec")) if network else "—"
        )
        self._process_grid["Network up"].val.setText(
            format_bitrate(network.get("tx_bits_per_sec")) if network else "—"
        )
        siblings = proc.get("siblings") or []
        if siblings:
            sib_text = ", ".join(
                f"{s.get('label', '?')} (pid {s.get('pid', '?')}, "
                f"{_fmt_pct(s.get('cpu_percent'))} CPU, {_fmt_bytes(s.get('rss_bytes'))})"
                for s in siblings
            )
        else:
            sib_text = "none"
        self._process_grid["Siblings"].val.setText(sib_text)

        # IPC
        ipc = snap.get("ipc", {}) or {}
        self._ipc_grid["Requests"].val.setText(str(ipc.get("requests_total", 0)))
        self._ipc_grid["Messages"].val.setText(str(ipc.get("messages_total", 0)))
        self._ipc_grid["Bytes in"].val.setText(_fmt_bytes(ipc.get("bytes_in_total")))
        self._ipc_grid["Bytes out"].val.setText(_fmt_bytes(ipc.get("bytes_out_total")))
        self._ipc_grid["Avg handler time"].val.setText(
            f"{_fmt_number(ipc.get('avg_latency_ms'))} ms"
        )
        self._ipc_grid["p95 handler time"].val.setText(
            f"{_fmt_number(ipc.get('p95_latency_ms'))} ms"
        )
        self._ipc_grid["Drops"].val.setText(str(ipc.get("drops_total", 0)))
        self._ipc_grid["Backpressure"].val.setText(str(ipc.get("backpressure_events_total", 0)))
        self._ipc_grid["In-flight"].val.setText(str(ipc.get("inflight", 0)))

        # Active decoder and optional GPU telemetry are intentionally separate:
        # VA-API can be actively decoding even when its driver exposes no live
        # utilization counters.
        self._render_acceleration(snap.get("video_acceleration") or {})

        # GPU telemetry
        gpu = snap.get("gpu", {}) or {}
        status = gpu.get("status")
        if status == "available":
            metrics = gpu.get("metrics") or {}
            name = metrics.get("name", "GPU")
            source = metrics.get("source", "?")
            self._gpu_status_label.setText(f"Telemetry device: {name}")
            util = metrics.get("utilization_percent")
            self._gpu_utilization_label.setText(
                f"Utilization: {_fmt_pct(util)}" if util is not None else "Utilization: unavailable"
            )
            used = metrics.get("memory_used_bytes")
            total = metrics.get("memory_total_bytes")
            if used is not None and total is not None:
                self._gpu_memory_label.setText(f"Memory: {_fmt_bytes(used)} / {_fmt_bytes(total)}")
            else:
                self._gpu_memory_label.setText("Memory: unavailable")
            dec = metrics.get("decoder_utilization_percent")
            self._gpu_decoder_label.setText(
                f"Decoder: {_fmt_pct(dec)}" if dec is not None else "Decoder: unavailable"
            )
            self._gpu_reason_label.setText(f"Metrics source: {source}")
            self._gpu_reason_label.setStyleSheet("font-size: 10px; color: #7878a0;")
        else:
            reason = gpu.get("reason") or "no GPU counter source detected"
            self._gpu_status_label.setText("Hardware telemetry unavailable")
            # Explicitly clear — do NOT show fabricated 0%.
            self._gpu_utilization_label.setText("Utilization: —")
            self._gpu_memory_label.setText("Memory: —")
            self._gpu_decoder_label.setText("Decoder: —")
            self._gpu_reason_label.setText(reason)
            self._gpu_reason_label.setStyleSheet("font-size: 10px; color: #e0a020;")

        # Cameras
        self._render_cameras(snap.get("cameras") or [])

        # Bottleneck
        b = snap.get("bottleneck", {}) or {}
        kind = str(b.get("kind", "none"))
        severity = str(b.get("severity", "ok")).lower()
        explanation = str(b.get("explanation", ""))
        color = _SEVERITY_COLORS.get(severity, _DEFAULT_BOTTLENECK_COLOR)
        self._bottleneck_label.setText(_bottleneck_sentence(kind, severity, explanation))
        self._bottleneck_label.setStyleSheet(
            "font-size: 13px; padding: 6px 10px; color: #ffffff; "
            f"background: {color}; border-radius: 4px;"
        )

        captured = str(snap.get("captured_at_utc", "—"))
        self._captured_label.setText(f"Last capture: {captured}")

    def _render_acceleration(self, acceleration: object) -> None:
        info = acceleration if isinstance(acceleration, dict) else {}
        kind = str(info.get("kind", "unknown")).lower()
        icon, label, detail, color = _ACCEL_PRESENTATION.get(
            kind,
            (
                "◌",
                "Decoder unknown",
                "The backend did not report its active video decoder.",
                "#7878a0",
            ),
        )
        self._accel_badge_label.setText(f"{icon} {label}")
        self._accel_badge_label.setAccessibleName(f"Video acceleration: {label}")
        self._accel_badge_label.setStyleSheet(
            f"color: {color}; background: transparent; border: none; "
            "font-size: 12px; font-weight: 700; padding: 0 4px;"
        )
        self._accel_detail_label.setText(detail)

    def _render_cameras(self, cams: list[dict[str, Any]]) -> None:
        # Clear existing rows.
        while self._cameras_body_layout.count() > 0:
            item = self._cameras_body_layout.takeAt(0)
            w = item.widget() if item else None
            if w is not None:
                w.deleteLater()
        if not cams:
            self._cameras_empty_label = QLabel("No cameras configured.")
            self._cameras_empty_label.setStyleSheet("color: #7878a0; font-size: 11px;")
            self._cameras_body_layout.addWidget(self._cameras_empty_label)
            return
        for cam in cams:
            row = QWidget()
            layout = QGridLayout(row)
            layout.setContentsMargins(0, 0, 0, 0)
            layout.setHorizontalSpacing(12)
            id_label = QLabel(str(cam.get("camera_id", "?")))
            id_label.setStyleSheet("color: #c8c8dc; font-size: 12px; font-weight: bold;")
            state_label = QLabel(str(cam.get("state", "?")))
            state_label.setStyleSheet("color: #a0a0c0; font-size: 11px;")
            dec = cam.get("decoder_fps")
            ren = cam.get("renderer_fps")
            rates_label = QLabel(
                f"decoder: {_fmt_number(dec)} fps • renderer: {_fmt_number(ren)} fps"
            )
            rates_label.setStyleSheet("color: #c8c8dc; font-size: 11px;")
            drops_label = QLabel(
                f"drops: decoder {cam.get('decoder_drops', 0)} / "
                f"renderer {cam.get('renderer_drops', 0)}"
            )
            drops_label.setStyleSheet("color: #a0a0c0; font-size: 11px;")
            e2e = cam.get("end_to_end_latency_ms")
            latency_label = QLabel(
                f"end-to-end: {_fmt_number(e2e)} ms" if e2e is not None else "end-to-end: —"
            )
            latency_label.setStyleSheet("color: #a0a0c0; font-size: 11px;")
            layout.addWidget(id_label, 0, 0)
            layout.addWidget(state_label, 0, 1)
            layout.addWidget(rates_label, 1, 0, 1, 2)
            layout.addWidget(drops_label, 2, 0, 1, 2)
            layout.addWidget(latency_label, 3, 0, 1, 2)
            self._cameras_body_layout.addWidget(row)

    def closeEvent(self, event: QCloseEvent) -> None:
        self.stop_refresh()
        worker = self._active_worker
        if worker is not None:
            # Drop the late reply: the page is going away.
            try:
                worker.snapshot_ready.disconnect(self._on_snapshot)
            except (RuntimeError, TypeError):
                pass
            worker.wait()
            self._active_worker = None
        super().closeEvent(event)
