"""The motion picture of the camera dialog: zones drawn over the camera's
picture, with live analysis changes on top. No IPC or worker ownership.

:class:`AnalysisPreview` shows a bounded analysis frame (320×180 grey, from
``motion.status``) and the 16×9 blocks that changed. :class:`ZoneCanvas`
adds the camera snapshot and editable include/exclude polygons in
normalized coordinates; zones can be drawn on the snapshot, the analysis
frame or, before either arrives, an empty 16:9 board.
"""

from __future__ import annotations

import base64
import uuid
from copy import deepcopy
from typing import Any

from PySide6.QtCore import QPointF, QRectF, QSize, Qt, Signal
from PySide6.QtGui import (
    QColor,
    QFont,
    QImage,
    QMouseEvent,
    QPainter,
    QPainterPath,
    QPaintEvent,
    QPen,
    QPixmap,
    QPolygonF,
)
from PySide6.QtWidgets import QSizePolicy, QWidget

from neurowatch.theme import active_colors

#: Zone colours: watched areas amber, ignored areas red.
INCLUDE_COLOR = "#f59e0b"
EXCLUDE_COLOR = "#ef4444"
#: Changed analysis blocks.
BLOCK_COLOR = QColor(56, 189, 248)
MAX_ZONES = 32
MAX_POINTS = 64


def zone_color(zone: dict[str, Any]) -> QColor:
    return QColor(EXCLUDE_COLOR if zone.get("kind") == "exclude" else INCLUDE_COLOR)


class AnalysisPreview(QWidget):
    """A bounded actual analysis picture with its changed blocks."""

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self._image = QImage()
        self._blocks: list[int] = []
        sizing = QSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Preferred)
        sizing.setHeightForWidth(True)
        self.setSizePolicy(sizing)
        self.setMinimumSize(240, 135)
        self.setAccessibleName("Live motion analysis with changed blocks")

    def hasHeightForWidth(self) -> bool:  # noqa: N802
        return True

    def heightForWidth(self, width: int) -> int:  # noqa: N802
        return round(width * 9 / 16)

    def sizeHint(self) -> QSize:
        return QSize(480, 270)

    def has_frame(self) -> bool:
        return not self._image.isNull()

    def active_blocks(self) -> list[int]:
        return list(self._blocks)

    def set_analysis(self, preview: object, blocks: object) -> None:
        self._image = QImage()
        self._blocks = []
        if isinstance(preview, dict) and (preview.get("width"), preview.get("height")) == (
            320,
            180,
        ):
            encoded = preview.get("gray_b64")
            if isinstance(encoded, str) and len(encoded) == 76800:
                try:
                    pixels = base64.b64decode(encoded, validate=True)
                except ValueError:
                    pixels = b""
                if len(pixels) == 57600:
                    self._image = QImage(
                        pixels, 320, 180, 320, QImage.Format.Format_Grayscale8
                    ).copy()
                    if isinstance(blocks, list) and len(blocks) <= 144:
                        self._blocks = sorted(
                            {b for b in blocks if type(b) is int and 0 <= b < 144}
                        )
        self.update()

    def image_rect(self) -> QRectF:
        """Where the picture is drawn: 16:9, centred."""
        scale = min(self.width() / 16, self.height() / 9)
        width, height = 16 * scale, 9 * scale
        return QRectF((self.width() - width) / 2, (self.height() - height) / 2, width, height)

    def _frame_path(self, rect: QRectF) -> QPainterPath:
        path = QPainterPath()
        path.addRoundedRect(rect, 8, 8)
        return path

    def _placeholder_text(self) -> str:
        return "Waiting for analysis pictures"

    def _paint_background(self, painter: QPainter, rect: QRectF) -> bool:
        """Draw the picture; False when there is none."""
        if self._image.isNull():
            return False
        painter.drawImage(rect, self._image)
        return True

    def _paint_blocks(self, painter: QPainter, rect: QRectF) -> None:
        if not self._blocks:
            return
        pen = QColor(BLOCK_COLOR)
        pen.setAlpha(200)
        fill = QColor(BLOCK_COLOR)
        fill.setAlpha(70)
        painter.setPen(QPen(pen, 1))
        painter.setBrush(fill)
        for block in self._blocks:
            painter.drawRect(
                QRectF(
                    rect.left() + block % 16 * rect.width() / 16,
                    rect.top() + block // 16 * rect.height() / 9,
                    rect.width() / 16,
                    rect.height() / 9,
                )
            )

    def _paint_chip(self, painter: QPainter, rect: QRectF, text: str, color: QColor) -> None:
        font = QFont(self.font())
        font.setPixelSize(10)
        font.setWeight(QFont.Weight.Bold)
        painter.setFont(font)
        width = painter.fontMetrics().horizontalAdvance(text) + 22
        chip = QRectF(rect.left() + 8, rect.top() + 8, width, 18)
        painter.setPen(Qt.PenStyle.NoPen)
        painter.setBrush(QColor(0, 0, 0, 170))
        painter.drawRoundedRect(chip, 9, 9)
        painter.setBrush(color)
        painter.drawEllipse(QPointF(chip.left() + 9, chip.center().y()), 3, 3)
        painter.setPen(QColor("#ffffff"))
        painter.drawText(
            chip.adjusted(15, 0, -4, 0),
            int(Qt.AlignmentFlag.AlignVCenter | Qt.AlignmentFlag.AlignLeft),
            text,
        )

    def paintEvent(self, event: QPaintEvent) -> None:
        colors = active_colors()
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing)
        rect = self.image_rect()
        path = self._frame_path(rect)
        painter.fillPath(path, QColor(colors.surface_sunken))
        painter.save()
        painter.setClipPath(path)
        has_picture = self._paint_background(painter, rect)
        if not has_picture:
            painter.setPen(QColor(colors.text_muted))
            painter.drawText(rect, Qt.AlignmentFlag.AlignCenter, self._placeholder_text())
        self._paint_overlay(painter, rect)
        self._paint_blocks(painter, rect)
        if self.has_frame():
            self._paint_chip(painter, rect, "LIVE ANALYSIS", BLOCK_COLOR)
        painter.restore()
        painter.setPen(QPen(QColor(colors.border), 1))
        painter.setBrush(Qt.BrushStyle.NoBrush)
        painter.drawPath(path)

    def _paint_overlay(self, painter: QPainter, rect: QRectF) -> None:
        """Between the picture and the blocks (zones, in the canvas)."""


class ZoneCanvas(AnalysisPreview):
    """Motion zones on the camera's picture; click to add points."""

    changed = Signal()

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self._picture = QPixmap()
        self._zones: list[dict[str, Any]] = []
        self.selected = 0
        self._dragging: int | None = None
        self._hover: int | None = None
        self.setMouseTracking(True)
        self.setCursor(Qt.CursorShape.CrossCursor)
        self.setAccessibleName("Motion zones on the camera picture")
        self.setToolTip(
            "Click to add a point, drag a point to move it, right-click a point to remove it"
        )

    def set_snapshot(self, picture: QPixmap) -> None:
        self._picture = picture
        self.update()

    def has_snapshot(self) -> bool:
        return not self._picture.isNull()

    def set_zones(self, zones: list[dict[str, Any]]) -> None:
        self._zones = deepcopy(zones)
        self.selected = min(self.selected, max(0, len(zones) - 1))
        self.update()

    def zones(self) -> list[dict[str, Any]]:
        return deepcopy(self._zones)

    def add_zone(self, kind: str, name: str | None = None) -> bool:
        """A new, empty zone, selected; False at the limit."""
        if len(self._zones) >= MAX_ZONES:
            return False
        self._zones.append(
            {
                "id": str(uuid.uuid4()),
                "name": name or f"Zone {len(self._zones) + 1}",
                "kind": kind,
                "points": [],
            }
        )
        self.selected = len(self._zones) - 1
        self.changed.emit()
        self.update()
        return True

    def image_rect(self) -> QRectF:
        if self._picture.isNull() or self.has_frame():
            return super().image_rect()
        scale = min(self.width() / self._picture.width(), self.height() / self._picture.height())
        width, height = self._picture.width() * scale, self._picture.height() * scale
        return QRectF((self.width() - width) / 2, (self.height() - height) / 2, width, height)

    def image_point(self, point: QPointF) -> list[float]:
        rect = self.image_rect()
        return [
            max(0.0, min(1.0, (point.x() - rect.left()) / max(1.0, rect.width()))),
            max(0.0, min(1.0, (point.y() - rect.top()) / max(1.0, rect.height()))),
        ]

    def append_point(self, point: QPointF) -> None:
        if not self._zones:
            # Clicking an empty picture starts a watched zone.
            self.add_zone("include")
        points = self._zones[self.selected]["points"]
        if len(points) >= MAX_POINTS:
            return
        points.append(self.image_point(point))
        self.changed.emit()
        self.update()

    def _point_at(self, position: QPointF) -> int | None:
        if not self._zones:
            return None
        rect = self.image_rect()
        points = self._zones[self.selected]["points"]
        return next(
            (
                i
                for i, (x, y) in enumerate(points)
                if (
                    QPointF(rect.left() + x * rect.width(), rect.top() + y * rect.height())
                    - position
                ).manhattanLength()
                < 12
            ),
            None,
        )

    def mousePressEvent(self, event: QMouseEvent) -> None:
        if not self.isEnabled() or not self.image_rect().contains(event.position()):
            return
        closest = self._point_at(event.position())
        if event.button() == Qt.MouseButton.RightButton:
            if closest is not None:
                self._zones[self.selected]["points"].pop(closest)
                self.changed.emit()
                self.update()
        elif event.button() == Qt.MouseButton.LeftButton:
            if closest is not None:
                self._dragging = closest
            else:
                self.append_point(event.position())

    def mouseMoveEvent(self, event: QMouseEvent) -> None:
        if self._dragging is None:
            hover = self._point_at(event.position())
            if hover != self._hover:
                self._hover = hover
                self.setCursor(
                    Qt.CursorShape.SizeAllCursor
                    if hover is not None
                    else Qt.CursorShape.CrossCursor
                )
                self.update()
            return
        self._zones[self.selected]["points"][self._dragging] = self.image_point(event.position())
        self.changed.emit()
        self.update()

    def mouseReleaseEvent(self, event: QMouseEvent) -> None:
        self._dragging = None

    def _placeholder_text(self) -> str:
        return "Loading the camera picture…"

    def _paint_background(self, painter: QPainter, rect: QRectF) -> bool:
        # The live analysis frame first: the changed blocks belong to it.
        if super()._paint_background(painter, rect):
            return True
        if not self._picture.isNull():
            painter.drawPixmap(rect, self._picture, QRectF(self._picture.rect()))
            return True
        return False

    def _paint_overlay(self, painter: QPainter, rect: QRectF) -> None:
        for i, zone in enumerate(self._zones):
            if i == self.selected:
                continue
            self._paint_zone(painter, rect, zone, selected=False)
        if 0 <= self.selected < len(self._zones):
            self._paint_zone(painter, rect, self._zones[self.selected], selected=True)

    def _paint_zone(
        self, painter: QPainter, rect: QRectF, zone: dict[str, Any], *, selected: bool
    ) -> None:
        color = zone_color(zone)
        points = [
            QPointF(rect.left() + x * rect.width(), rect.top() + y * rect.height())
            for x, y in zone["points"]
        ]
        fill = QColor(color)
        fill.setAlpha(70 if selected else 40)
        painter.setPen(QPen(color, 2.5 if selected else 1.5))
        painter.setBrush(fill)
        if len(points) >= 3:
            painter.drawPolygon(QPolygonF(points))
        elif len(points) == 2:
            dashed = QPen(color, 2)
            dashed.setStyle(Qt.PenStyle.DashLine)
            painter.setPen(dashed)
            painter.drawPolyline(QPolygonF(points))
        if not selected:
            return
        for index, point in enumerate(points):
            painter.setPen(QPen(color, 2))
            painter.setBrush(QColor("#ffffff"))
            radius = 6.0 if index == self._hover else 4.5
            painter.drawEllipse(point, radius, radius)
