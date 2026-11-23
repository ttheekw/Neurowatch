"""Compact camera health and stream topology; reads never open media consumers."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

from PySide6.QtCore import QPointF, QSize, Qt
from PySide6.QtGui import QColor, QPainter, QPaintEvent, QPen, QPolygonF, QResizeEvent
from PySide6.QtWidgets import (
    QFrame,
    QGridLayout,
    QHBoxLayout,
    QLabel,
    QSizePolicy,
    QToolButton,
    QVBoxLayout,
    QWidget,
)

from neurowatch.models.stream_flow import FlowNode, FlowRow, flatten, stream_flow
from neurowatch.utils.network import format_bitrate

_COLORS = {"green": "#6ee7b7", "yellow": "#fcd34d", "red": "#fda4af", "unknown": "#94a3b8"}


def _tone(state: object) -> str:
    if state == "streaming":
        return "green"
    # "disconnected": nothing needs the stream, so it is not open. A lost
    # camera is "retrying" (yellow); red is never shown for an unused stream.
    return "yellow" if state in ("idle", "connecting", "retrying") else "unknown"


def _number(value: object, digits: int = 1) -> str:
    return f"{value:.{digits}f}" if isinstance(value, (int, float)) else "—"


@dataclass(frozen=True)
class CameraHealthReadout:
    decode: str
    render: str
    decode_drops: str
    render_drops: str
    latency: str
    summary: str


def camera_health_readout(
    health: dict[str, Any] | None, streams: list[dict[str, Any]], streams_reported: bool
) -> CameraHealthReadout:
    """Desktop work is distinct from incoming video; unmeasured values stay explicit."""
    values = health or {}
    decoded = values.get("decoder_fps")
    rendered = values.get("renderer_fps")
    desktop = any((row.get("roles") or {}).get("desktop", 0) > 0 for row in streams)
    decode = f"{_number(decoded)} fps" if decoded is not None else "Not reported"
    if decoded == 0 and streams_reported and not desktop:
        decode = "Idle (no desktop view)"
    render = f"{_number(rendered)} fps" if rendered is not None else "Not reported"
    decode_drops = str(values["decoder_drops"]) if "decoder_drops" in values else "Not reported"
    render_drops = (
        str(values.get("renderer_drops", "Not reported"))
        if rendered is not None or values.get("renderer_drops", 0) > 0
        else "Not reported"
    )
    delay = values.get("end_to_end_latency_ms")
    # Older recorders defaulted to zero even without a capture-to-display probe.
    latency = "Not measured"
    if delay is not None and delay >= 0 and (delay > 0 or rendered is not None):
        latency = f"{_number(delay, 1 if 0 < delay < 1 else 0)} ms"
    summary = "Desktop decoder idle" if decode.startswith("Idle") else f"Desktop decode: {decode}"
    return CameraHealthReadout(decode, render, decode_drops, render_drops, latency, summary)


def _part(parent: QWidget) -> QWidget:
    widget = QWidget(parent)
    widget.setObjectName("cameraPart")
    return widget


def _label(text: str = "", *, color: str = "#dbe7f4", size: int = 12) -> QLabel:
    label = QLabel(text)
    label.setTextFormat(Qt.TextFormat.PlainText)
    label.setWordWrap(True)
    label.setStyleSheet(f"background:transparent; border:none; color:{color}; font-size:{size}px;")
    label.setMinimumWidth(0)
    label.setSizePolicy(QSizePolicy.Policy.Ignored, QSizePolicy.Policy.Preferred)
    return label


class _CameraIcon(QWidget):
    def __init__(self, parent: QWidget) -> None:
        super().__init__(parent)
        self.color = _COLORS["unknown"]
        self.setObjectName("cameraPart")
        self.setFixedSize(28, 28)
        self.setAccessibleName("Camera")

    def paintEvent(self, event: QPaintEvent) -> None:
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing)
        painter.scale(28 / 48, 28 / 48)
        painter.setPen(QPen(QColor(self.color), 2))
        painter.setBrush(QColor("#0a1625"))
        painter.drawRoundedRect(1, 1, 46, 46, 11, 11)
        painter.drawRoundedRect(10, 15, 20, 14, 3, 3)
        painter.drawPolygon(
            QPolygonF([QPointF(30, 19), QPointF(38, 15), QPointF(38, 29), QPointF(30, 25)])
        )
        painter.drawLine(17, 30, 17, 35)
        painter.drawLine(12, 35, 24, 35)


class _StreamRail(QWidget):
    """Owns the connector pixels so opaque theme containers cannot cover them."""

    def __init__(self, parent: QWidget) -> None:
        super().__init__(parent)
        self.setObjectName("cameraPart")
        self.setFixedWidth(28)
        self.color = _COLORS["unknown"]
        self.last = False

    def paintEvent(self, event: QPaintEvent) -> None:
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing)
        painter.setPen(QPen(QColor(self.color), 1.5))
        x = self.width() / 2
        y = min(14, self.height() / 2)
        painter.drawLine(QPointF(x, 0), QPointF(x, y if self.last else self.height()))
        painter.drawLine(QPointF(x, y), QPointF(self.width(), y))
        painter.setBrush(QColor(self.color))
        painter.drawEllipse(QPointF(self.width() - 2, y), 2, 2)


#: Box borders per component: what each box is reads at a glance.
_BOX_COLORS = {
    "stream": "#dbe7f4",
    "buffer": "#7dd3fc",
    "decoder": "#fbbf24",
    "transcode": "#f59e0b",
    "plugin": "#c4b5fd",
    "consumer": "#64748b",
}
_GUIDE = 18  # px per tree level
_ELBOW = 11  # px from a row's top to its box's first line


def _chip(text: str, color: str) -> QLabel:
    chip = QLabel(text)
    chip.setTextFormat(Qt.TextFormat.PlainText)
    chip.setStyleSheet(
        f"background:#10233a; border:1px solid {color}; border-radius:3px; color:{color};"
        "font-size:10px; font-weight:600; padding:0px 3px;"
    )
    chip.setFixedHeight(15)
    chip.setSizePolicy(QSizePolicy.Policy.Fixed, QSizePolicy.Policy.Fixed)
    return chip


class _Guides(QWidget):
    """The tree's connectors for one row, painted here so opaque theme
    containers cannot cover them. Dashed: shared decoded pictures."""

    def __init__(self, parent: QWidget) -> None:
        super().__init__(parent)
        self.setObjectName("cameraPart")
        self.color = _COLORS["unknown"]
        self.row: FlowRow | None = None

    def show_row(self, row: FlowRow, color: str) -> None:
        self.row, self.color = row, color
        self.setFixedWidth(_GUIDE * row.depth)
        self.update()

    def paintEvent(self, event: QPaintEvent) -> None:
        row = self.row
        if row is None or row.depth == 0:
            return
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing)
        solid = QPen(QColor(self.color), 1.3)
        painter.setPen(solid)
        height = self.height()
        for level, continues in enumerate(row.guides, start=1):
            if continues:
                x = (level - 1) * _GUIDE + _GUIDE / 2
                painter.drawLine(QPointF(x, 0), QPointF(x, height))
        x = (row.depth - 1) * _GUIDE + _GUIDE / 2
        y = min(_ELBOW, height / 2)
        painter.drawLine(QPointF(x, 0), QPointF(x, height if not row.last else y))
        branch = QPen(QColor(self.color), 1.3)
        if row.node.dashed:
            branch.setStyle(Qt.PenStyle.CustomDashLine)
            branch.setDashPattern([2.5, 2.0])
        painter.setPen(branch)
        end = self.width() - 1
        painter.drawLine(QPointF(x, y), QPointF(end, y))
        painter.setPen(solid)
        painter.drawLine(QPointF(end - 3, y - 3), QPointF(end, y))
        painter.drawLine(QPointF(end - 3, y + 3), QPointF(end, y))


class _ElidedLabel(QLabel):
    """One line, shortened with "…" when the card is narrow; the full text
    stays in the tooltip and accessible name."""

    def __init__(self, color: str) -> None:
        super().__init__()
        self.full = ""
        self.setTextFormat(Qt.TextFormat.PlainText)
        self.setStyleSheet(f"background:transparent; border:none; color:{color}; font-size:11px;")
        self.setSizePolicy(QSizePolicy.Policy.Ignored, QSizePolicy.Policy.Fixed)
        self.setMinimumWidth(24)

    def set_full(self, text: str) -> None:
        self.full = text
        self.setToolTip(text)
        self._elide()
        self.updateGeometry()

    def natural_width(self) -> int:
        return self.fontMetrics().horizontalAdvance(self.full) + 2

    def sizeHint(self) -> QSize:
        return QSize(self.natural_width(), super().sizeHint().height())

    def resizeEvent(self, event: QResizeEvent) -> None:
        super().resizeEvent(event)
        self._elide()

    def _elide(self) -> None:
        self.setText(
            self.fontMetrics().elidedText(self.full, Qt.TextElideMode.ElideRight, self.width())
        )


class _FlowBox(QFrame):
    """One component: a chip (codec), its name and its measurements."""

    def __init__(self, parent: QWidget) -> None:
        super().__init__(parent)
        self.setObjectName("flowBox")
        self.setSizePolicy(QSizePolicy.Policy.Maximum, QSizePolicy.Policy.Preferred)
        layout = QHBoxLayout(self)
        layout.setContentsMargins(5, 1, 5, 1)
        layout.setSpacing(5)
        self.box_layout = layout
        self.chip_holder = _part(self)
        self.chip_layout = QHBoxLayout(self.chip_holder)
        self.chip_layout.setContentsMargins(0, 0, 0, 0)
        self.chip: QLabel | None = None
        self.title = _label(size=11)
        self.title.setWordWrap(False)
        self.title.setSizePolicy(QSizePolicy.Policy.Preferred, QSizePolicy.Policy.Preferred)
        self.detail = _ElidedLabel("#a5b4c9")
        layout.addWidget(self.chip_holder)
        layout.addWidget(self.title)
        layout.addWidget(self.detail, 1)

    def show_node(self, node: FlowNode, tone: str) -> None:
        color = _COLORS[tone] if node.kind == "stream" else _BOX_COLORS.get(node.kind, "")
        self.setProperty("kind", node.kind)
        style = "dashed" if node.dashed or not color else "solid"
        border = f"1px {style} {color or '#3b5170'}"
        self.setStyleSheet(
            f"QFrame#flowBox {{background:#0f1f33; border:{border}; border-radius:5px;}}"
        )
        if (self.chip.text() if self.chip else None) != node.chip:
            if self.chip is not None:
                self.chip_layout.removeWidget(self.chip)
                self.chip.setParent(None)
                self.chip.deleteLater()
                self.chip = None
            if node.chip:
                self.chip = _chip(node.chip, "#7dd3fc")
                self.chip.setAccessibleName(f"{node.chip} video")
                self.chip_layout.addWidget(self.chip)
        self.chip_holder.setVisible(self.chip is not None)
        self.title.setText(node.title)
        title_color = _COLORS[tone] if node.kind == "stream" else "#dbe7f4"
        if node.kind == "note":
            title_color = "#a5b4c9"
        self.title.setStyleSheet(
            f"background:transparent; border:none; color:{title_color}; font-size:11px;"
            + (
                "font-weight:600;"
                if node.kind in ("stream", "buffer", "decoder", "transcode")
                else ""
            )
        )
        self.detail.set_full(node.detail)
        self.detail.setVisible(bool(node.detail))
        self.setToolTip(node.description)
        self.updateGeometry()

    def sizeHint(self) -> QSize:
        """Its whole text on one line when there is room."""
        hint = super().sizeHint()
        margins = self.box_layout.contentsMargins()
        spacing = self.box_layout.spacing()
        width = margins.left() + margins.right()
        width += self.title.fontMetrics().horizontalAdvance(self.title.text())
        if self.chip is not None:
            width += self.chip.sizeHint().width() + spacing
        if self.detail.isVisibleTo(self):
            width += spacing + self.detail.natural_width()
        return QSize(max(hint.width(), width + 4), hint.height())


class _FlowRowWidget(QWidget):
    def __init__(self, parent: QWidget) -> None:
        super().__init__(parent)
        self.setObjectName("cameraPart")
        layout = QHBoxLayout(self)
        layout.setContentsMargins(0, 1, 0, 1)
        layout.setSpacing(0)
        self.guides = _Guides(self)
        self.box = _FlowBox(self)
        layout.addWidget(self.guides)
        layout.addWidget(self.box)
        layout.addStretch(1)
        self.row: FlowRow | None = None

    def show_row(self, row: FlowRow, tone: str) -> None:
        self.row = row
        self.guides.show_row(row, _COLORS[tone])
        self.box.show_node(row.node, tone)
        node = row.node
        spoken = node.description or ", ".join(
            part for part in (node.chip, node.title, node.detail) if part
        )
        self.setAccessibleName(spoken)


def _stream_name(stream: str) -> str:
    return {"main": "Main", "sub": "Sub", "main+sub": "Main / Sub"}.get(
        stream, stream.replace("other_", "Other ")
    )


class _StreamRow(QFrame):
    """One stream as a tree: stream → frame buffer → consumers and decoders."""

    def __init__(self, parent: QWidget) -> None:
        super().__init__(parent)
        self.setObjectName("streamBranch")
        layout = QVBoxLayout(self)
        layout.setContentsMargins(4, 3, 2, 4)
        layout.setSpacing(0)
        self.tree = _part(self)
        self.tree_layout = QVBoxLayout(self.tree)
        self.tree_layout.setContentsMargins(0, 0, 0, 0)
        self.tree_layout.setSpacing(0)
        layout.addWidget(self.tree)
        self.cache_label = _label(color="#a5b4c9", size=11)
        layout.addWidget(self.cache_label)
        self.rows: list[_FlowRowWidget] = []

    def update_reading(
        self,
        row: dict[str, Any],
        plugins: list[dict[str, Any]],
        details: bool,
        *,
        desktop_decoder: str | None = None,
        desktop_fps: object = None,
        shared_elsewhere: str | None = None,
    ) -> None:
        tone = _tone(row.get("state"))
        self.setProperty("tone", tone)
        self.setProperty("stream", row.get("stream"))
        flow = flatten(
            stream_flow(
                row,
                plugins,
                bitrate=format_bitrate(row.get("video_bps")),
                desktop_decoder=desktop_decoder,
                desktop_fps=desktop_fps,
                shared_elsewhere=shared_elsewhere,
            )
        )
        while len(self.rows) > len(flow):
            widget = self.rows.pop()
            self.tree_layout.removeWidget(widget)
            widget.setParent(None)
            widget.deleteLater()
        while len(self.rows) < len(flow):
            self.rows.append(_FlowRowWidget(self.tree))
            self.tree_layout.addWidget(self.rows[-1])
        for widget, flow_row in zip(self.rows, flow, strict=True):
            widget.show_row(flow_row, tone)
        available = row.get("state") != "unavailable"
        consumers = row.get("consumers", "—") if available else "—"
        self.cache_label.setText(
            f"Audio {format_bitrate(row.get('audio_bps'))} · "
            f"{consumers} consumer{'' if consumers == 1 else 's'} (subscriptions, not people)"
        )
        self.cache_label.setVisible(details)
        self.setToolTip(self.cache_label.text())

    def boxes(self) -> list[tuple[str, str, str]]:
        """(kind, title, detail) of each box, in order (tests, accessibility)."""
        return [(w.row.node.kind, w.row.node.title, w.row.node.detail) for w in self.rows if w.row]


class _CameraCard(QFrame):
    def __init__(self, parent: QWidget) -> None:
        super().__init__(parent)
        self.setObjectName("cameraDiagnostic")
        self.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Maximum)
        layout = QVBoxLayout(self)
        layout.setContentsMargins(8, 6, 8, 6)
        layout.setSpacing(3)
        header = _part(self)
        header_layout = QHBoxLayout(header)
        header_layout.setContentsMargins(0, 0, 0, 0)
        header_layout.setSpacing(8)
        self.icon = _CameraIcon(header)
        self.identity = _label(size=13)
        self.state = _label(color="#a5b4c9", size=11)
        self.state.setSizePolicy(QSizePolicy.Policy.Preferred, QSizePolicy.Policy.Preferred)
        self.details_button = QToolButton(header)
        self.details_button.setText("Details")
        self.details_button.setCheckable(True)
        self.details_button.setAccessibleName("Show audio and health details")
        self.details_button.setToolButtonStyle(Qt.ToolButtonStyle.ToolButtonTextBesideIcon)
        self.details_button.setArrowType(Qt.ArrowType.RightArrow)
        self.details_button.toggled.connect(self._show_details)
        header_layout.addWidget(self.icon)
        header_layout.addWidget(self.identity, 1)
        header_layout.addWidget(self.state)
        header_layout.addWidget(self.details_button)
        layout.addWidget(header)
        branches = _part(self)
        self.branches = QGridLayout(branches)
        self.branches.setContentsMargins(0, 0, 0, 0)
        self.branches.setSpacing(0)
        self.branches.setColumnStretch(1, 1)
        self.empty = _label(color="#a5b4c9", size=11)
        self.branches.addWidget(self.empty, 0, 1)
        layout.addWidget(branches)
        self.health = _label(color="#a5b4c9", size=11)
        layout.addWidget(self.health)
        self.health_details = _label(color="#a5b4c9", size=11)
        self.health_details.setVisible(False)
        layout.addWidget(self.health_details)
        self.stream_rows: list[_StreamRow] = []
        self.rails: list[_StreamRail] = []

    def _show_details(self, shown: bool) -> None:
        self.details_button.setArrowType(
            Qt.ArrowType.DownArrow if shown else Qt.ArrowType.RightArrow
        )
        self.health_details.setVisible(shown)
        for row in self.stream_rows:
            row.cache_label.setVisible(shown)

    def update_reading(
        self,
        name: str,
        health: dict[str, Any] | None,
        streams: list[dict[str, Any]],
        reported: bool,
        plugins: dict[str, list[dict[str, Any]]],
        desktop_decoder: str | None = None,
    ) -> None:
        tones = {_tone(row.get("state")) for row in streams}
        tone = next((key for key in ("green", "yellow", "red") if key in tones), "unknown")
        self.setProperty("tone", tone)
        self.icon.color = _COLORS[tone]
        self.icon.update()
        self.identity.setText(name)
        state = str(health.get("state", "Not reported")) if health else "Health not reported"
        self.state.setText(state.replace("_", " ").capitalize())
        while len(self.stream_rows) > len(streams):
            for widget in (self.stream_rows.pop(), self.rails.pop()):
                self.branches.removeWidget(widget)
                widget.setParent(None)
                widget.deleteLater()
        for index, stream in enumerate(streams):
            if index == len(self.stream_rows):
                self.stream_rows.append(_StreamRow(self))
                self.rails.append(_StreamRail(self))
                self.branches.addWidget(self.rails[-1], index, 0)
                self.branches.addWidget(self.stream_rows[-1], index, 1)
            # A shared plugin may borrow another stream's video wall pictures
            # (a Main tile serving a plugin on Sub).
            walls = [
                str(other.get("stream"))
                for other in streams
                if other is not stream
                and ((other.get("roles") or {}).get("desktop") or other.get("always_decoded"))
            ]
            self.stream_rows[index].update_reading(
                stream,
                plugins.get(str(stream.get("stream")), []),
                self.details_button.isChecked(),
                desktop_decoder=desktop_decoder,
                desktop_fps=(health or {}).get("decoder_fps"),
                shared_elsewhere=_stream_name(walls[0]) if walls else None,
            )
            rail = self.rails[index]
            rail.color = _COLORS[_tone(stream.get("state"))]
            rail.last = index == len(streams) - 1
            rail.update()
        self.empty.setVisible(not streams)
        self.empty.setText(
            "Stream connection details not reported by this backend."
            if not reported
            else "No stream connections reported."
        )
        readout = camera_health_readout(health, streams, reported)
        self.health.setText(readout.summary)
        self.health.setToolTip(
            "Desktop decoding runs only while a desktop view needs pictures. "
            "Incoming camera FPS is shown on each stream. Render FPS and end-to-end "
            "latency appear only when measured."
        )
        self.health_details.setText(
            f"Desktop decode: {readout.decode} · Render: {readout.render} · "
            f"Frame drops (decode / render): {readout.decode_drops} / {readout.render_drops} · "
            f"End-to-end latency: {readout.latency}"
        )


class CameraDiagnostics(QWidget):
    """Stable compact camera cards, with up to three columns when there is room."""

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setObjectName("cameraDiagnostics")
        self.setStyleSheet("""
            QWidget#cameraDiagnostics, QWidget#cameraPart {background:transparent;}
            QFrame#cameraDiagnostic {background:#0c192a; border:1px solid #2c435d;
                                     border-radius:8px;}
            QFrame#streamBranch {background:transparent; border:none;
                                border-bottom:1px solid #25354a;}
            QToolButton {background:transparent; color:#a5b4c9; border:none;
                         padding:3px 5px; font-size:11px;}
            QToolButton:hover {background:#192e45; color:#f1f5f9; border-radius:4px;}
            QToolButton:focus {border:1px solid #67e8f9;}
        """)
        self.cards: dict[str, _CameraCard] = {}
        self.layout_ = QGridLayout(self)
        self.layout_.setContentsMargins(0, 0, 0, 0)
        self.layout_.setSpacing(6)
        self.layout_.setAlignment(Qt.AlignmentFlag.AlignTop)
        self.empty = _label("No cameras configured.", color="#a5b4c9")
        self.layout_.addWidget(self.empty, 0, 0)
        self._order: list[str] = []
        self._columns = 0

    def _arrange(self) -> None:
        columns = 3 if self.width() >= 1020 else 2 if self.width() >= 680 else 1
        order = sorted(self.cards, key=lambda key: self.cards[key].identity.text().lower())
        if columns == self._columns and order == self._order:
            return
        self._columns, self._order = columns, order
        self.layout_.removeWidget(self.empty)
        for card in self.cards.values():
            self.layout_.removeWidget(card)
        for index, camera_id in enumerate(order):
            self.layout_.addWidget(
                self.cards[camera_id], index // columns, index % columns, Qt.AlignmentFlag.AlignTop
            )
        self.layout_.addWidget(self.empty, (len(order) + columns - 1) // columns, 0)
        for column in range(3):
            self.layout_.setColumnStretch(column, 1 if column < columns else 0)
            self.layout_.setColumnMinimumWidth(column, 0)

    def resizeEvent(self, event: QResizeEvent) -> None:
        super().resizeEvent(event)
        self._arrange()

    def update_snapshot(
        self,
        health: list[dict[str, Any]],
        streams: object,
        names: dict[str, str],
        *,
        plugin_consumers: object = None,
        decoder: object = None,
    ) -> None:
        """Merge measured sources; plugin names are desktop-only active binding reports.

        ``decoder``: what decodes the video wall (``video_acceleration.label``)."""
        by_health = {str(row["camera_id"]): row for row in health if row.get("camera_id")}
        by_stream: dict[str, list[dict[str, Any]]] = {}
        if isinstance(streams, list):
            for row in streams:
                if isinstance(row, dict) and row.get("camera_id"):
                    by_stream.setdefault(str(row["camera_id"]), []).append(row)
        by_plugin: dict[str, dict[str, list[dict[str, Any]]]] = {}
        if isinstance(plugin_consumers, list):
            for row in plugin_consumers:
                if not isinstance(row, dict) or not isinstance(row.get("name"), str):
                    continue
                if row["name"].strip():
                    by_plugin.setdefault(str(row.get("camera_id")), {}).setdefault(
                        str(row.get("stream")), []
                    ).append(row)
        ids = set(names) | set(by_health) | set(by_stream)
        for camera_id in set(self.cards) - ids:
            card = self.cards.pop(camera_id)
            self.layout_.removeWidget(card)
            card.setParent(None)
            card.deleteLater()
        self.empty.setVisible(not ids)
        for camera_id in ids:
            if camera_id not in self.cards:
                self.cards[camera_id] = _CameraCard(self)
            self.cards[camera_id].update_reading(
                names.get(camera_id, camera_id),
                by_health.get(camera_id),
                by_stream.get(camera_id, []),
                isinstance(streams, list),
                by_plugin.get(camera_id, {}),
                decoder if isinstance(decoder, str) and decoder else None,
            )
        self._arrange()
