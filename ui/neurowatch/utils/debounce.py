"""Debounced callback using QTimer."""

from __future__ import annotations

from collections.abc import Callable

from PySide6.QtCore import QTimer


class Debounced:
    """Delays callback execution until N ms have elapsed since the last trigger."""

    def __init__(self, callback: Callable[[], None], delay_ms: int = 500) -> None:
        self._timer = QTimer()
        self._timer.setSingleShot(True)
        self._timer.setInterval(delay_ms)
        self._timer.timeout.connect(callback)

    def trigger(self) -> None:
        """Reset the timer. Callback fires delay_ms after the last trigger call."""
        self._timer.start()

    def flush(self) -> None:
        """Fire immediately if pending, then stop timer."""
        if self._timer.isActive():
            self._timer.stop()
            self._timer.timeout.emit()
