"""Page navigator — shown below the video wall when there are multiple pages."""

from __future__ import annotations

from PySide6.QtCore import QEvent, QObject, Qt, Signal
from PySide6.QtGui import QColor, QMouseEvent, QPainter, QPaintEvent
from PySide6.QtWidgets import QHBoxLayout, QLabel, QSizePolicy, QToolButton, QWidget

_STRIP_HEIGHT = 24
_DOT_SIZE = 10
_DOT_SPACING = 6
_DOT_ACTIVE = QColor(78, 154, 241)
_DOT_INACTIVE = QColor(60, 60, 80)
# Beyond this many pages dots are too small to aim at; arrows + label remain.
_MAX_DOTS = 24
_NAV_STYLE = """
QToolButton {
    background: transparent; color: #c0c0d4; border: none;
    font-size: 16px; padding: 0 10px;
}
QToolButton:hover { color: #ffffff; background: #1e1e36; }
QToolButton:disabled { color: #3a3a52; }
"""


class _DotCanvas(QWidget):
    """Draws the dot row and handles click-to-page."""

    page_clicked = Signal(int)
    #: The page whose dot the pointer is over, or -1 when it is over none.
    page_hovered = Signal(int)

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self._count = 1
        self._current = 0
        self._hovered = -1
        self.setFixedHeight(_STRIP_HEIGHT)
        self.setCursor(Qt.CursorShape.PointingHandCursor)
        self.setMouseTracking(True)

    def set_state(self, count: int, current: int) -> None:
        self._count = max(1, count)
        self._current = max(0, min(current, self._count - 1))
        self.update()

    def paintEvent(self, _event: QPaintEvent) -> None:
        painter = QPainter(self)
        total_w = self._count * (_DOT_SIZE + _DOT_SPACING) - _DOT_SPACING
        x0 = (self.width() - total_w) // 2
        y = (self.height() - _DOT_SIZE) // 2

        for i in range(self._count):
            x = x0 + i * (_DOT_SIZE + _DOT_SPACING)
            painter.setBrush(_DOT_ACTIVE if i == self._current else _DOT_INACTIVE)
            painter.setPen(Qt.PenStyle.NoPen)
            painter.drawEllipse(x, y, _DOT_SIZE, _DOT_SIZE)
        painter.end()

    def page_at(self, x: float) -> int:
        """The page whose dot is at ``x``, or -1."""
        total_w = self._count * (_DOT_SIZE + _DOT_SPACING) - _DOT_SPACING
        x0 = (self.width() - total_w) // 2
        rel_x = x - x0
        if rel_x < 0:
            return -1
        idx = int(rel_x // (_DOT_SIZE + _DOT_SPACING))
        return idx if 0 <= idx < self._count else -1

    def mousePressEvent(self, event: QMouseEvent) -> None:
        if event.button() == Qt.MouseButton.LeftButton:
            idx = self.page_at(event.position().x())
            if idx >= 0:
                self.page_clicked.emit(idx)

    def mouseMoveEvent(self, event: QMouseEvent) -> None:
        self._hover(self.page_at(event.position().x()))
        super().mouseMoveEvent(event)

    def leaveEvent(self, event: QEvent) -> None:
        self._hover(-1)
        super().leaveEvent(event)

    def _hover(self, page: int) -> None:
        if page != self._hovered:
            self._hovered = page
            self.page_hovered.emit(page)


class PageStripWidget(QWidget):
    """Previous/next arrows, a page label and clickable dots below the wall."""

    page_clicked = Signal(int)
    #: The page the pointer is about to pick (a dot, or an arrow's page), so
    #: its video can start before the click; -1 when it moves away.
    page_hovered = Signal(int)

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self._count = 1
        self._current = 0
        self.setFixedHeight(_STRIP_HEIGHT)
        self.setStyleSheet("background-color: #0d0d1a;")

        layout = QHBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(4)

        self._prev_btn = QToolButton()
        self._prev_btn.setText("\u2039")
        self._prev_btn.setToolTip("Previous page (PgUp)")
        self._prev_btn.setStyleSheet(_NAV_STYLE)
        self._prev_btn.setCursor(Qt.CursorShape.PointingHandCursor)
        self._prev_btn.clicked.connect(lambda: self.page_clicked.emit(self._current - 1))
        layout.addWidget(self._prev_btn)

        self._canvas = _DotCanvas()
        self._canvas.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Fixed)
        self._canvas.page_clicked.connect(self.page_clicked)
        self._canvas.page_hovered.connect(self.page_hovered)
        layout.addWidget(self._canvas)

        self._label = QLabel()
        self._label.setAlignment(Qt.AlignmentFlag.AlignCenter)
        self._label.setToolTip("Home / End: first / last page")
        self._label.setStyleSheet("color: #a0a0c0; font-size: 11px; padding: 0 6px;")
        layout.addWidget(self._label)

        self._next_btn = QToolButton()
        self._next_btn.setText("\u203a")
        self._next_btn.setToolTip("Next page (PgDn)")
        self._next_btn.setStyleSheet(_NAV_STYLE)
        self._next_btn.setCursor(Qt.CursorShape.PointingHandCursor)
        self._next_btn.clicked.connect(lambda: self.page_clicked.emit(self._current + 1))
        layout.addWidget(self._next_btn)
        self._prev_btn.installEventFilter(self)
        self._next_btn.installEventFilter(self)

    def eventFilter(self, watched: QObject, event: QEvent) -> bool:  # noqa: N802
        if event.type() == QEvent.Type.Enter and watched in (self._prev_btn, self._next_btn):
            step = -1 if watched is self._prev_btn else 1
            page = self._current + step
            self.page_hovered.emit(page if 0 <= page < self._count else -1)
        elif event.type() == QEvent.Type.Leave and watched in (self._prev_btn, self._next_btn):
            self.page_hovered.emit(-1)
        return False

    def set_state(self, count: int, current: int) -> None:
        self._count = max(1, count)
        self._current = max(0, min(current, self._count - 1))
        self._canvas.set_state(self._count, self._current)
        show_dots = self._count <= _MAX_DOTS
        self._canvas.setVisible(show_dots)
        # Without dots the label takes the centre of the strip.
        self._label.setSizePolicy(
            QSizePolicy.Policy.Preferred if show_dots else QSizePolicy.Policy.Expanding,
            QSizePolicy.Policy.Fixed,
        )
        self._label.setText(f"Page {self._current + 1} of {self._count}")
        self._prev_btn.setEnabled(self._current > 0)
        self._next_btn.setEnabled(self._current < self._count - 1)
        self.setVisible(self._count > 1)
