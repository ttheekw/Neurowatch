"""Minute-accurate weekly schedule painted with a selectable time brush."""

from __future__ import annotations

from PySide6.QtCore import QPoint, QRectF, Qt, Signal
from PySide6.QtGui import QColor, QKeyEvent, QMouseEvent, QPainter, QPaintEvent, QPen
from PySide6.QtWidgets import QWidget

from neurowatch.models.recording_policy import DAYS
from neurowatch.theme import active_colors

#: Colours of the recording modes, shared with the legend.
MODE_COLORS = {"continuous": "#3b82f6", "events": "#f59e0b", "off": "#64748b"}


_MODE_NAMES = {"continuous": "Continuous", "events": "Events only", "off": "Off"}


def mode_color(mode: str) -> str:
    return MODE_COLORS.get(mode, MODE_COLORS["continuous"])


class RecordingSchedule(QWidget):
    painted = Signal(int, int, int, int, object)
    LEFT, TOP, ROW = 44, 24, 30

    def __init__(self) -> None:
        super().__init__()
        self._slots: list[str | None] = [None] * 10080
        self.default = "continuous"
        self.brush: str | None = "events"
        self.step = 60
        self._anchor: tuple[int, int] | None = None
        self._cursor = (0, 0)
        self.setMinimumSize(360, self.TOP + 7 * self.ROW + 2)
        self.setCursor(Qt.CursorShape.CrossCursor)
        self.setMouseTracking(True)
        self.setFocusPolicy(Qt.FocusPolicy.StrongFocus)
        self.setAccessibleName("Weekly recording schedule")
        self.setAccessibleDescription(
            "Drag to paint days and times. Arrow keys move; Space paints; Delete restores default."
        )

    def slots(self) -> list[str | None]:
        return self._slots.copy()

    def set_schedule(self, slots: list[str | None], default: str) -> None:
        self._slots = slots.copy()
        self.default = default
        self.update()

    def _cell(self, point: QPoint) -> tuple[int, int]:
        day = max(0, min(6, (point.y() - self.TOP) // self.ROW))
        minute = int((point.x() - self.LEFT) * 1440 / max(1, self.width() - self.LEFT - 2))
        minute = max(0, min(1440 - self.step, minute // self.step * self.step))
        return day, minute

    def _rect(self, day: int, start: int, end: int, days: int = 1) -> QRectF:
        scale = (self.width() - self.LEFT - 2) / 1440
        return QRectF(
            self.LEFT + start * scale,
            self.TOP + day * self.ROW,
            (end - start) * scale,
            days * self.ROW,
        )

    def paintEvent(self, event: QPaintEvent) -> None:
        painter = QPainter(self)
        colors = active_colors()
        muted = QColor(colors.text_muted)
        enabled = self.isEnabled()
        font = painter.font()
        font.setPixelSize(11)
        painter.setFont(font)
        painter.setPen(muted)
        for hour in range(0, 25, 3):
            x = self._rect(0, hour * 60, hour * 60).x()
            painter.drawText(
                QRectF(max(self.LEFT - 12, min(x - 13, self.width() - 28)), 0, 26, 20),
                Qt.AlignmentFlag.AlignCenter,
                f"{hour:02}:00" if hour in (0, 12, 24) else f"{hour:02}",
            )
        for day, label in enumerate(DAYS):
            painter.setPen(muted)
            painter.drawText(
                QRectF(0, self.TOP + day * self.ROW, self.LEFT - 8, self.ROW),
                Qt.AlignmentFlag.AlignVCenter | Qt.AlignmentFlag.AlignLeft,
                label,
            )
            start = 0
            while start < 1440:
                mode = self._slots[day * 1440 + start]
                end = start + 1
                while end < 1440 and self._slots[day * 1440 + end] == mode:
                    end += 1
                color = QColor(mode_color(mode or self.default))
                if mode is None:
                    color.setAlpha(55)
                if not enabled:
                    color.setAlpha(color.alpha() // 2)
                painter.fillRect(self._rect(day, start, end).adjusted(0, 1, 0, -1), color)
                start = end
        line = QColor(colors.window)
        line.setAlpha(160)
        painter.setPen(line)
        for hour in range(1, 24):
            x = int(self._rect(0, hour * 60, hour * 60).x())
            painter.drawLine(x, self.TOP, x, self.TOP + 7 * self.ROW)
        frame = QColor(colors.border)
        painter.setPen(frame)
        painter.setBrush(Qt.BrushStyle.NoBrush)
        painter.drawRect(
            QRectF(self.LEFT, self.TOP, self.width() - self.LEFT - 2, 7 * self.ROW).adjusted(
                0, 0, 0, -1
            )
        )
        if self.hasFocus() or self._anchor:
            a = self._anchor or self._cursor
            b = self._cursor
            painter.setPen(QPen(QColor(colors.text), 2))
            painter.drawRect(
                self._rect(
                    min(a[0], b[0]),
                    min(a[1], b[1]),
                    max(a[1], b[1]) + self.step,
                    abs(a[0] - b[0]) + 1,
                )
            )

    def mousePressEvent(self, event: QMouseEvent) -> None:
        if (
            event.button() == Qt.MouseButton.LeftButton
            and event.position().x() >= self.LEFT
            and self.TOP <= event.position().y() < self.TOP + 7 * self.ROW
        ):
            self._anchor = self._cursor = self._cell(event.position().toPoint())
            self.setFocus()
            self.update()
        else:
            super().mousePressEvent(event)

    def mouseMoveEvent(self, event: QMouseEvent) -> None:
        cell = self._cell(event.position().toPoint())
        if self._anchor:
            self._cursor = cell
            self.update()
        day, minute = cell
        mode = self._slots[day * 1440 + minute]
        self.setToolTip(
            f"{DAYS[day]} {minute // 60:02}:{minute % 60:02} · "
            f"{_MODE_NAMES.get(mode or self.default, mode or self.default)}"
            + (" (default)" if mode is None else "")
            + "\nDrag to paint; Delete restores default."
        )

    def mouseReleaseEvent(self, event: QMouseEvent) -> None:
        if event.button() == Qt.MouseButton.LeftButton and self._anchor:
            a, b = self._anchor, self._cell(event.position().toPoint())
            self._anchor = None
            self._cursor = b
            self.painted.emit(
                min(a[0], b[0]),
                max(a[0], b[0]),
                min(a[1], b[1]),
                max(a[1], b[1]) + self.step,
                self.brush,
            )
            self.update()
        else:
            super().mouseReleaseEvent(event)

    def keyPressEvent(self, event: QKeyEvent) -> None:
        day, minute = self._cursor
        key = event.key()
        if key in (Qt.Key.Key_Space, Qt.Key.Key_Delete, Qt.Key.Key_Backspace):
            self.painted.emit(
                day,
                day,
                minute,
                minute + self.step,
                self.brush if key == Qt.Key.Key_Space else None,
            )
        elif key in (Qt.Key.Key_Left, Qt.Key.Key_Right, Qt.Key.Key_Up, Qt.Key.Key_Down):
            day += (key == Qt.Key.Key_Down) - (key == Qt.Key.Key_Up)
            minute += self.step * ((key == Qt.Key.Key_Right) - (key == Qt.Key.Key_Left))
            self._cursor = (max(0, min(6, day)), max(0, min(1440 - self.step, minute)))
        else:
            super().keyPressEvent(event)
        self.update()
