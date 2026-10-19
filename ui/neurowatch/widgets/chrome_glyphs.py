"""Window-button symbols painted as strokes rather than typed as characters.

Every font puts ≡, □, ❐, – and ✕ at its own height and size, so the same
title bar looks misaligned from one desktop to the next. Painted symbols are
centred on whole pixels and look the same everywhere.
"""

from __future__ import annotations

from collections.abc import Callable

from PySide6.QtCore import QLineF, QPointF, QRectF
from PySide6.QtGui import QColor, QPainter, QPaintEvent, QPen
from PySide6.QtWidgets import QPushButton, QWidget

GLYPHS = ("menu", "minimize", "maximize", "restore", "close")

#: The symbol's box in logical pixels.
_SIZE = 10


class GlyphButton(QPushButton):
    """A text-less button that paints one of ``GLYPHS``.

    The style sheet still draws the background (hover, focus, pressed);
    ``colors(active)`` gives the stroke colour, ``active`` being hovered,
    focused or pressed.
    """

    def __init__(
        self,
        glyph: str,
        colors: Callable[[bool], str],
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        if glyph not in GLYPHS:
            raise ValueError(f"unknown glyph {glyph!r}")
        self._glyph = glyph
        self._colors = colors

    def glyph(self) -> str:
        return self._glyph

    def set_glyph(self, glyph: str) -> None:
        if glyph not in GLYPHS:
            raise ValueError(f"unknown glyph {glyph!r}")
        if glyph != self._glyph:
            self._glyph = glyph
            self.update()

    def paintEvent(self, event: QPaintEvent) -> None:
        super().paintEvent(event)
        active = self.underMouse() or self.hasFocus() or self.isDown()
        painter = QPainter(self)
        try:
            paint_glyph(painter, self._glyph, QRectF(self.rect()), QColor(self._colors(active)))
        finally:
            painter.end()


def paint_glyph(painter: QPainter, glyph: str, area: QRectF, color: QColor) -> None:
    """Paint ``glyph`` centred in ``area`` with 1 px strokes on pixel centres."""
    # The box's top-left corner on a whole pixel, strokes half a pixel in, so
    # a 1 px line covers exactly one row or column of pixels.
    left = round(area.center().x() - _SIZE / 2) + 0.5
    top = round(area.center().y() - _SIZE / 2) + 0.5
    span = _SIZE - 1
    middle = top + round(span / 2)
    pen = QPen(color, 1.0)
    painter.setPen(pen)
    painter.setBrush(QColor(0, 0, 0, 0))
    if glyph == "menu":
        # Three bars, a little wider than the other symbols.
        for y in (top + 1, middle, top + span - 1):
            painter.drawLine(QLineF(left - 2, y, left + span + 2, y))
    elif glyph == "minimize":
        painter.drawLine(QLineF(left, middle, left + span, middle))
    elif glyph == "maximize":
        painter.drawRect(QRectF(left, top, span, span))
    elif glyph == "restore":
        front = span - 2
        painter.drawRect(QRectF(left, top + 2, front, front))
        # The window behind: only its top and right edges show.
        painter.drawLine(QLineF(left + 2, top + 2, left + 2, top))
        painter.drawLine(QLineF(left + 2, top, left + span, top))
        painter.drawLine(QLineF(left + span, top, left + span, top + front))
        painter.drawLine(QLineF(left + span, top + front, left + front, top + front))
    elif glyph == "close":
        # Diagonals cannot sit on pixel centres; smoothing keeps them even.
        painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        painter.setPen(QPen(color, 1.15))
        a, b = QPointF(left, top), QPointF(left + span, top + span)
        painter.drawLine(QLineF(a, b))
        painter.drawLine(QLineF(QPointF(left, top + span), QPointF(left + span, top)))
