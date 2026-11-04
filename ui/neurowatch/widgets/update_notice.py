"""Quiet hourly update checks and a keyboard-accessible bottom-bar download link."""

from __future__ import annotations

from collections.abc import Callable
from typing import Any
from urllib.parse import urlencode

from PySide6.QtCore import QObject, Qt, QTimer, QUrl, Signal
from PySide6.QtGui import QDesktopServices, QKeyEvent
from PySide6.QtWidgets import QPushButton, QWidget

from neurowatch.build_info import build_identity
from neurowatch.models.updates import read_latest, release_version
from neurowatch.utils.threads import Worker, retire

CHECK_INTERVAL_MS = 60 * 60 * 1000


class UpdateNotice(QPushButton):
    """A persistent, unobtrusive download action beside the installed version."""

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.edition = "free"
        self._version: str | None = None
        self.setObjectName("updateNotice")
        self.setAccessibleName("NeuroWatch update available")
        self.setCursor(Qt.CursorShape.PointingHandCursor)
        self.setFocusPolicy(Qt.FocusPolicy.StrongFocus)
        self.setFixedHeight(22)
        self.setStyleSheet(
            "QPushButton#updateNotice { color: #93c5fd; background: transparent;"
            " border: 1px solid transparent; padding: 0 6px; font-size: 10px; }"
            "QPushButton#updateNotice:hover { background: #172554; }"
            "QPushButton#updateNotice:focus { border-color: #60a5fa; }"
        )
        self.clicked.connect(self._open)
        self.hide()

    def set_version(self, version: str | None, *, edition: str | None = None) -> None:
        if edition is not None:
            self.edition = "unlimited" if edition == "unlimited" else "free"
        parsed = release_version(version)
        self._version = version if parsed is not None and not parsed[1] else None
        self.setText(f"Update available: {self._version}" if self._version else "")
        detail = f"Download NeuroWatch {self._version}. Your cameras and recordings are kept."
        self.setToolTip(detail if self._version else "")
        self.setAccessibleDescription(self.toolTip())
        self.setVisible(self._version is not None)

    def _open(self) -> None:
        if self._version:
            query = urlencode({"version": self._version, "edition": self.edition})
            QDesktopServices.openUrl(QUrl(f"https://neurowatch.app/downloads?{query}"))

    def keyPressEvent(self, event: QKeyEvent) -> None:
        if event.key() in (Qt.Key.Key_Return, Qt.Key.Key_Enter):
            self.click()
            event.accept()
            return
        super().keyPressEvent(event)


class _UpdateWorker(Worker):
    done = Signal(bool, object)

    def __init__(self, current: str, fetch: Callable[[str], str | None]) -> None:
        super().__init__()  # May outlive a closing window until the bounded read finishes.
        self.current = current
        self.fetch = fetch

    def run(self) -> None:
        try:
            version = self.fetch(self.current)
        except (OSError, ValueError, TypeError):
            self.done.emit(False, None)
        else:
            self.done.emit(True, version)


class UpdateChecker(QObject):
    def __init__(
        self,
        current: str,
        parent: QWidget | None = None,
        *,
        fetch: Callable[[str], str | None] = read_latest,
    ) -> None:
        super().__init__(parent)
        self.current = current
        self.fetch = fetch
        self.notice = UpdateNotice(parent)
        self.worker: _UpdateWorker | None = None
        self._stopped = False
        self.timer = QTimer(self)
        self.timer.setInterval(CHECK_INTERVAL_MS)
        self.timer.timeout.connect(self.check)

    def start(self) -> None:
        self._stopped = False
        self.timer.start()
        QTimer.singleShot(0, self.check)

    def check(self) -> None:
        if self._stopped or self.worker is not None:
            return
        worker = _UpdateWorker(self.current, self.fetch)
        self.worker = worker
        worker.done.connect(self._received)
        worker.finished.connect(self._finished)
        worker.start()

    def _received(self, success: bool, version: object) -> None:
        if success and not self._stopped:
            self.notice.set_version(version if isinstance(version, str) else None)

    def _finished(self) -> None:
        self.worker = None

    def stop(self) -> None:
        self._stopped = True
        self.timer.stop()
        retire(self.worker)


def install(window: Any) -> UpdateChecker:
    checker = UpdateChecker(build_identity().label, window)
    window.add_status_indicator(checker.notice)
    window.add_shutdown_callback(checker.stop)
    checker.start()
    return checker
