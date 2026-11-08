"""The status bar's notice of analytics plugins that stopped answering: an
amber mark and the plugin's name; a click opens the Plugins tab."""

from __future__ import annotations

from collections.abc import Sequence

from PySide6.QtCore import Qt, Signal
from PySide6.QtGui import QMouseEvent
from PySide6.QtWidgets import QWidget

from neurowatch.models.plugins import offline_notice
from neurowatch.widgets.status_bar_widget import StatusIndicator

#: The tray's "problem" amber.
_AMBER = "#f59e0b"


class PluginOfflineNotice(StatusIndicator):
    clicked = Signal()

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__("", "alert", parent)
        self.setObjectName("pluginOfflineNotice")
        self.setAccessibleName("Plugins offline")
        self.setStyleSheet(f"color: {_AMBER}; font-weight: 600;")
        self._names: tuple[str, ...] = ()
        self._can_open = False
        self.hide()

    def set_offline(self, names: Sequence[str], *, can_open: bool = True) -> None:
        """The plugins offline now (none: hidden). ``can_open``: a click may
        open the Plugins tab (administrators)."""
        self.setCursor(
            Qt.CursorShape.PointingHandCursor if can_open else Qt.CursorShape.ArrowCursor
        )
        self._can_open = can_open
        if tuple(names) == self._names:
            return
        self._names = tuple(names)
        text, tooltip = offline_notice(self._names)
        self.setText(text)
        self.setToolTip(tooltip)
        self.setAccessibleDescription(tooltip)
        self.setVisible(bool(text))

    def mousePressEvent(self, event: QMouseEvent) -> None:
        if event.button() == Qt.MouseButton.LeftButton and self._can_open:
            self.clicked.emit()
            event.accept()
            return
        super().mousePressEvent(event)
