"""Alert toasts: desktop alerts shown in the window's top-right corner.

Used when no tray icon shows them as system notifications (a source
checkout runs without a tray). Each card stays a few seconds, at most
three at once (newest on top); clicking one opens the Events tab.
"""

from __future__ import annotations

from typing import Any

from PySide6.QtCore import QEvent, QObject, Qt, QTimer, Signal
from PySide6.QtGui import QEnterEvent, QMouseEvent
from PySide6.QtWidgets import QFrame, QHBoxLayout, QLabel, QPushButton, QVBoxLayout, QWidget

from neurowatch.theme import active_colors

SHOW_MS = 8000
MAX_SHOWN = 3
WIDTH = 340
MARGIN = 16


class _Toast(QFrame):
    clicked = Signal(object)
    closed = Signal(object)

    def __init__(self, alert: dict[str, Any], parent: QWidget) -> None:
        super().__init__(parent)
        self.alert = alert
        self.setObjectName("alertToast")
        self.setCursor(Qt.CursorShape.PointingHandCursor)
        self.setFixedWidth(WIDTH)
        c = active_colors()
        self.setStyleSheet(
            f"QFrame#alertToast {{ background: {c.surface_raised};"
            f" border: 1px solid {c.border_strong};"
            f" border-left: 4px solid {c.warning}; border-radius: 10px; }}"
            f"QLabel {{ background: transparent; border: none; color: {c.text}; }}"
            f"QLabel#alertToastMessage {{ color: {c.text_muted}; font-size: 12px; }}"
            f"QPushButton {{ background: transparent; border: none; color: {c.text_muted};"
            f" font-size: 14px; padding: 0 4px; }}"
            f"QPushButton:hover {{ color: {c.text}; }}"
        )
        layout = QVBoxLayout(self)
        layout.setContentsMargins(14, 10, 8, 12)
        layout.setSpacing(4)
        top = QHBoxLayout()
        top.setSpacing(6)
        self.title = QLabel(str(alert.get("title") or "Alert"))
        self.title.setTextFormat(Qt.TextFormat.PlainText)
        self.title.setStyleSheet("font-weight: 700; font-size: 13px;")
        self.title.setWordWrap(True)
        top.addWidget(self.title, 1)
        close = QPushButton("✕")
        close.setAccessibleName("Dismiss alert")
        close.setCursor(Qt.CursorShape.PointingHandCursor)
        close.clicked.connect(lambda: self.closed.emit(self))
        top.addWidget(close, 0, Qt.AlignmentFlag.AlignTop)
        layout.addLayout(top)
        self.message = QLabel(str(alert.get("message") or ""))
        self.message.setObjectName("alertToastMessage")
        self.message.setTextFormat(Qt.TextFormat.PlainText)
        self.message.setWordWrap(True)
        layout.addWidget(self.message)
        self._timer = QTimer(self)
        self._timer.setSingleShot(True)
        self._timer.timeout.connect(lambda: self.closed.emit(self))
        self._timer.start(SHOW_MS)

    def enterEvent(self, event: QEnterEvent) -> None:  # hovering keeps it up
        self._timer.stop()
        super().enterEvent(event)

    def leaveEvent(self, event: QEvent) -> None:
        self._timer.start(SHOW_MS // 2)
        super().leaveEvent(event)

    def mouseReleaseEvent(self, event: QMouseEvent) -> None:
        if event.button() == Qt.MouseButton.LeftButton:
            self.clicked.emit(self.alert)
            self.closed.emit(self)
        super().mouseReleaseEvent(event)


class AlertToasts(QObject):
    """Shows toasts over ``host`` (a window's central widget)."""

    opened = Signal(object)  # the alert dict

    def __init__(self, host: QWidget) -> None:
        super().__init__(host)
        self._host = host
        self._toasts: list[_Toast] = []
        host.installEventFilter(self)

    @property
    def shown(self) -> list[_Toast]:
        return list(self._toasts)

    def show_alert(self, alert: dict[str, Any]) -> None:
        toast = _Toast(alert, self._host)
        toast.clicked.connect(self.opened)
        toast.closed.connect(self._dismiss)
        self._toasts.insert(0, toast)
        while len(self._toasts) > MAX_SHOWN:
            self._dismiss(self._toasts[-1])
        toast.show()
        toast.raise_()
        self._place()

    def _dismiss(self, toast: _Toast) -> None:
        if toast in self._toasts:
            self._toasts.remove(toast)
            toast.hide()
            toast.deleteLater()
            self._place()

    def _place(self) -> None:
        y = MARGIN
        for toast in self._toasts:
            toast.adjustSize()
            toast.move(self._host.width() - toast.width() - MARGIN, y)
            toast.raise_()
            y += toast.height() + 8

    def eventFilter(self, watched: QObject, event: QEvent) -> bool:  # noqa: N802
        if watched is self._host and event.type() == QEvent.Type.Resize:
            self._place()
        return False
