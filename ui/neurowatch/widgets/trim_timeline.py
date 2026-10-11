"""Playback timeline with a trim selection, for clip export.

Everything ``PlaybackTimeline`` does (ruler, recorded blocks, playhead,
zoom, pan, seek, play/speed controls), plus:

  - Two yellow handles marking the clip's start and end; drag one to move it
    (the preview then seeks to it, so the cut point can be seen)
  - The time outside the clip is shaded
  - ``trim_changed(start_ms, end_ms)`` whenever a handle is moved by the user
"""

from __future__ import annotations

from PySide6.QtCore import QRect, Qt, Signal
from PySide6.QtGui import QColor, QMouseEvent, QPainter, QPaintEvent
from PySide6.QtWidgets import QWidget

from neurowatch.widgets.playback_timeline import _RULER_H, _SCROLLBAR_H, PlaybackTimeline

_OUTSIDE = QColor(0, 0, 0, 120)
_HANDLE = QColor(250, 204, 21)  # amber
_HANDLE_EDGE = QColor(120, 90, 0)
_SELECTION_EDGE = QColor(250, 204, 21, 160)

#: How close (px) to a handle a press grabs it.
HANDLE_GRAB_PX = 8
_HANDLE_W = 4
#: Shortest clip the handles allow.
MIN_TRIM_MS = 1000

START = "start"
END = "end"


class TrimTimeline(PlaybackTimeline):
    """``PlaybackTimeline`` with draggable start/end handles."""

    trim_changed = Signal(object, object)  # start_ms, end_ms (epoch ms)

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self._trim: tuple[int, int] | None = None
        self._grabbed: str | None = None
        self.setAccessibleName("Clip timeline")

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def trim(self) -> tuple[int, int] | None:
        return self._trim

    def playhead(self) -> int:
        """The playhead's time (epoch ms)."""
        return self._playhead_ms

    def set_trim(self, start_ms: int, end_ms: int) -> None:
        """Place the handles (no ``trim_changed``): kept within the range and
        at least ``MIN_TRIM_MS`` apart where the range allows."""
        self._trim = self._clamped(int(start_ms), int(end_ms))
        self.update()

    def clear_trim(self) -> None:
        self._trim = None
        self._grabbed = None
        self.update()

    def handle_x(self, which: str) -> int | None:
        """Where a handle is drawn, or ``None`` (no clip, or out of view)."""
        if self._trim is None:
            return None
        ms = self._trim[0] if which == START else self._trim[1]
        if not self._view_start_ms <= ms <= self._view_end_ms:
            return None
        return self._ms_to_x(ms)

    # ------------------------------------------------------------------
    # Painting
    # ------------------------------------------------------------------

    def paintEvent(self, event: QPaintEvent) -> None:
        super().paintEvent(event)
        if self._trim is None:
            return
        painter = QPainter(self)
        track_left, track_right, _ = self._track_bounds()
        track_h = self._track_height()
        top, height = _RULER_H, max(1, track_h - _RULER_H)
        start_x = max(track_left, min(track_right, self._ms_to_x(self._trim[0])))
        end_x = max(track_left, min(track_right, self._ms_to_x(self._trim[1])))
        if start_x > track_left:
            painter.fillRect(QRect(track_left, top, start_x - track_left, height), _OUTSIDE)
        if end_x < track_right:
            painter.fillRect(QRect(end_x, top, track_right - end_x, height), _OUTSIDE)
        painter.setPen(_SELECTION_EDGE)
        painter.drawLine(start_x, top, end_x, top)
        painter.drawLine(start_x, track_h - 1, end_x, track_h - 1)
        for which in (START, END):
            x = self.handle_x(which)
            if x is None:
                continue
            rect = QRect(x - _HANDLE_W // 2, top, _HANDLE_W, height)
            painter.fillRect(rect, _HANDLE)
            painter.setPen(_HANDLE_EDGE)
            painter.drawRect(rect.adjusted(0, 0, -1, -1))
            # A grip in the middle, wider than the bar.
            grip = QRect(x - 5, top + height // 2 - 8, 10, 16)
            painter.fillRect(grip, _HANDLE)
            painter.drawRect(grip.adjusted(0, 0, -1, -1))
        painter.end()

    # ------------------------------------------------------------------
    # Mouse: a press near a handle drags it; anywhere else seeks as usual
    # ------------------------------------------------------------------

    def mousePressEvent(self, event: QMouseEvent) -> None:
        if event.button() == Qt.MouseButton.LeftButton:
            grabbed = self._handle_at(event.position().x(), event.position().y())
            if grabbed is not None:
                self._grabbed = grabbed
                event.accept()
                return
        super().mousePressEvent(event)

    def mouseMoveEvent(self, event: QMouseEvent) -> None:
        if self._grabbed is not None:
            self._drag_to(event.position().x())
            event.accept()
            return
        near = self._handle_at(event.position().x(), event.position().y())
        if near is not None:
            self.setCursor(Qt.CursorShape.SizeHorCursor)
        else:
            self.unsetCursor()
        super().mouseMoveEvent(event)

    def mouseReleaseEvent(self, event: QMouseEvent) -> None:
        if self._grabbed is not None and event.button() == Qt.MouseButton.LeftButton:
            which, self._grabbed = self._grabbed, None
            self._drag_to(event.position().x())
            if self._trim is not None:
                # Show the picture at the cut point.
                at = self._trim[0] if which == START else self._trim[1]
                self.set_playhead(at)
                self.seek_requested.emit(at)
            event.accept()
            return
        super().mouseReleaseEvent(event)

    # ------------------------------------------------------------------
    # Helpers
    # ------------------------------------------------------------------

    def _track_height(self) -> int:
        return self.height() - self._ctrl.height() - _SCROLLBAR_H - 2

    def _ms_to_x(self, ms: int) -> int:
        track_left, _, track_w = self._track_bounds()
        view_dur = max(1, self._view_end_ms - self._view_start_ms)
        return int(track_left + (ms - self._view_start_ms) / view_dur * track_w)

    def _handle_at(self, x: float, y: float) -> str | None:
        """The handle under (x, y), the nearer one when both are close."""
        if self._trim is None or not _RULER_H <= y <= self._track_height():
            return None
        best: tuple[float, str] | None = None
        for which in (END, START):  # END first: at a zero gap, drag the end out
            hx = self.handle_x(which)
            if hx is None:
                continue
            distance = abs(hx - x)
            if distance <= HANDLE_GRAB_PX and (best is None or distance < best[0]):
                best = (distance, which)
        return best[1] if best else None

    def _drag_to(self, x: float) -> None:
        if self._trim is None or self._grabbed is None:
            return
        ms = self._x_to_ms(x)
        if ms is None:
            return
        start, end = self._trim
        if self._grabbed == START:
            start = min(ms, end - MIN_TRIM_MS)
        else:
            end = max(ms, start + MIN_TRIM_MS)
        trim = self._clamped(start, end)
        if trim != self._trim:
            self._trim = trim
            self.update()
            self.trim_changed.emit(trim[0], trim[1])

    def _clamped(self, start: int, end: int) -> tuple[int, int]:
        low, high = self._range_start_ms, self._range_end_ms
        start = max(low, min(start, high))
        end = max(low, min(end, high))
        if end - start < MIN_TRIM_MS:
            end = min(high, start + MIN_TRIM_MS)
            start = max(low, end - MIN_TRIM_MS)
        return start, end
