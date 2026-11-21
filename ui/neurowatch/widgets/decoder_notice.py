"""The status bar's notice of a graphics card that can decode video but
decodes nothing, its driver (or software) missing: an amber mark, "Intel GPU
driver missing"; a click opens the Diagnostics tab, which says what to install."""

from __future__ import annotations

from PySide6.QtCore import Qt, Signal
from PySide6.QtGui import QMouseEvent
from PySide6.QtWidgets import QWidget

from neurowatch.models.video_decoder import AccelerationNotice, acceleration_notice
from neurowatch.widgets.status_bar_widget import StatusIndicator

#: The tray's "problem" amber.
_AMBER = "#f59e0b"


class DecoderDriverNotice(StatusIndicator):
    clicked = Signal()

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__("", "alert", parent)
        self.setObjectName("decoderDriverNotice")
        self.setAccessibleName("Hardware video decoding unavailable")
        self.setStyleSheet(f"color: {_AMBER}; font-weight: 600;")
        self.setCursor(Qt.CursorShape.PointingHandCursor)
        self._notice: AccelerationNotice | None = None
        self.hide()

    @property
    def notice(self) -> AccelerationNotice | None:
        return self._notice

    def set_acceleration(self, acceleration: object) -> None:
        """A ``video_acceleration`` report: shown while it names a graphics
        card without its driver, hidden otherwise."""
        notice = acceleration_notice(acceleration)
        if notice == self._notice:
            return  # re-sent every poll
        self._notice = notice
        if notice is None:
            self.hide()
            return
        self.setText(notice.short)
        tooltip = f"{notice.text}\nClick for Diagnostics."
        self.setToolTip(tooltip)
        self.setAccessibleDescription(notice.text)
        self.show()

    def mousePressEvent(self, event: QMouseEvent) -> None:
        if event.button() == Qt.MouseButton.LeftButton and self._notice is not None:
            self.clicked.emit()
            event.accept()
            return
        super().mousePressEvent(event)
