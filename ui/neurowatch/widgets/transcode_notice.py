"""The status bar's notice that the recorder converts a camera's video for a
browser: amber while ``system.status`` lists a transcode; a click opens
Diagnostics."""

from __future__ import annotations

from PySide6.QtCore import Qt, Signal
from PySide6.QtGui import QMouseEvent
from PySide6.QtWidgets import QWidget

from neurowatch.models.transcode import Transcode, parse_transcodes, summary, tooltip
from neurowatch.widgets.status_bar_widget import StatusIndicator

#: The tray's "problem" amber, as the other notices.
_AMBER = "#f59e0b"


class TranscodeNotice(StatusIndicator):
    clicked = Signal()

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__("", "alert", parent)
        self.setObjectName("transcodeNotice")
        self.setAccessibleName("Transcoding for a browser")
        self.setStyleSheet(f"color: {_AMBER}; font-weight: 600;")
        self.setCursor(Qt.CursorShape.PointingHandCursor)
        self._transcodes: list[Transcode] = []
        self.hide()

    def set_status(self, status: object) -> None:
        """Each poll's ``system.status``; text and tooltip change only when
        they differ (re-setting a shown tooltip flickers)."""
        transcodes = parse_transcodes(status)
        if transcodes == self._transcodes:
            return
        self._transcodes = transcodes
        if not transcodes:
            self.hide()
            return
        self.setText(summary(transcodes))
        self.setToolTip(tooltip(transcodes))
        self.setAccessibleDescription(tooltip(transcodes))
        self.show()

    def mousePressEvent(self, event: QMouseEvent) -> None:
        if event.button() == Qt.MouseButton.LeftButton and self._transcodes:
            self.clicked.emit()
            event.accept()
            return
        super().mousePressEvent(event)
