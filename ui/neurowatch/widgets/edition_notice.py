"""Status-bar notice of a free build holding cameras back (``models.edition``)."""

from __future__ import annotations

from PySide6.QtCore import Qt, QUrl, Signal
from PySide6.QtGui import QDesktopServices, QMouseEvent
from PySide6.QtWidgets import QWidget

from neurowatch.models.edition import Edition
from neurowatch.widgets.status_bar_widget import StatusIndicator


def open_unlimited(edition: Edition) -> None:
    """Open the downloads page at NeuroWatch Unlimited in the browser."""
    QDesktopServices.openUrl(QUrl(edition.unlimited_url))


class EditionNotice(StatusIndicator):
    """Shown only while cameras are over the limit; a click opens the
    downloads page."""

    clicked = Signal()

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__("", "alert", parent)
        self.setObjectName("editionNotice")
        self.setAccessibleName("Camera limit")
        self.setCursor(Qt.CursorShape.PointingHandCursor)
        self._edition = Edition()
        self.clicked.connect(lambda: open_unlimited(self._edition))
        self.hide()

    def set_edition(self, edition: Edition) -> None:
        if edition == self._edition:
            return
        self._edition = edition
        self.setText(edition.notice)
        self.setToolTip(edition.notice_detail)
        self.setAccessibleDescription(edition.notice_detail)
        self.setVisible(bool(edition.notice))

    def mousePressEvent(self, event: QMouseEvent) -> None:
        if event.button() == Qt.MouseButton.LeftButton:
            self.clicked.emit()
            event.accept()
            return
        super().mousePressEvent(event)
