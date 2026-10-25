"""NeuroWatch Cloud in the desktop app: the status bar light, which opens
Settings → Cloud (linking lives there)."""

from __future__ import annotations

from collections.abc import Callable
from dataclasses import replace
from typing import Any

from PySide6.QtCore import Qt, QTimer, Signal
from PySide6.QtGui import QMouseEvent
from PySide6.QtWidgets import QWidget

from neurowatch.cloud.cloud_link import CloudIndicator, clock_warning, cloud_indicator
from neurowatch.ipc.sync_client import SyncIpcClient
from neurowatch.utils.threads import Worker
from neurowatch.widgets.status_bar_widget import StatusIndicator

_RED = "#ef4444"
_GREEN = "#22c55e"
_GREEN_DIM = "#14532d"
_BLINK_MS = 600


class CloudIndicatorLabel(StatusIndicator):
    """Cloud link: red offline, green connected, blinking green while live
    video or playback is being sent to NeuroWatch Cloud. Clicked: ``clicked``."""

    clicked = Signal()

    def __init__(self, parent: QWidget | None = None) -> None:
        # The bar draws the dot, level with the text, and sizes the text.
        super().__init__("Cloud", "dot", parent)
        self.setAccessibleName("NeuroWatch Cloud status")
        self.setCursor(Qt.CursorShape.PointingHandCursor)
        self._state = ""
        self._bright = True
        self._blink = QTimer(self)
        self._blink.setInterval(_BLINK_MS)
        self._blink.timeout.connect(self._toggle_blink)
        self.set_indicator(CloudIndicator("offline", "NeuroWatch Cloud: not linked"))

    def mousePressEvent(self, event: QMouseEvent) -> None:
        if event.button() == Qt.MouseButton.LeftButton:
            self.clicked.emit()
            event.accept()
            return
        super().mousePressEvent(event)

    def set_indicator(self, indicator: CloudIndicator) -> None:
        self.setToolTip(f"{indicator.tooltip}\nClick for Settings → Cloud")
        self.setAccessibleDescription(indicator.tooltip)
        if indicator.state == self._state:
            return  # re-sent every poll; do not restart the blink
        self._state = indicator.state
        self._bright = True
        if indicator.state == "active":
            self._blink.start()
        else:
            self._blink.stop()
        self._apply_color()

    def _toggle_blink(self) -> None:
        self._bright = not self._bright
        self._apply_color()

    def _apply_color(self) -> None:
        if self._state == "offline":
            color = _RED
        elif self._state == "active" and not self._bright:
            color = _GREEN_DIM
        else:
            color = _GREEN
        self.setStyleSheet(f"color: {color};")


def desktop_indicator(status: dict[str, Any] | None) -> CloudIndicator:
    """The light for a ``cloud.status`` result; its tooltip also carries the
    clock warning when this computer's clock is off NeuroWatch Cloud's."""
    indicator = cloud_indicator(status)
    warning = clock_warning(status)
    return replace(indicator, tooltip=f"{indicator.tooltip}\n{warning}") if warning else indicator


class _CloudPoll(Worker):
    """``cloud.status`` every 2 s, off the GUI thread."""

    polled = Signal(object)  # the status, or None if it could not be read

    def __init__(self, endpoint: str, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self._endpoint = endpoint

    def run(self) -> None:
        while not self.isInterruptionRequested():
            try:
                status = SyncIpcClient(endpoint=self._endpoint).call("cloud.status")
            except Exception:
                status = None  # backend down, or no cloud in this backend
            self.polled.emit(status if isinstance(status, dict) else None)
            # Sleep in 200 ms steps so an interruption is quick.
            for _ in range(10):
                if self.isInterruptionRequested():
                    return
                self.msleep(200)


def install(window: Any, open_settings: Callable[[], object]) -> None:
    """Add the cloud light to the status bar; clicking it calls ``open_settings``
    (Settings → Cloud)."""
    label = CloudIndicatorLabel()
    label.clicked.connect(open_settings)
    window.add_status_indicator(label)
    poll = _CloudPoll(window.ipc_endpoint, window)
    poll.polled.connect(lambda status: label.set_indicator(desktop_indicator(status)))
    poll.start()

    def stop() -> None:
        poll.requestInterruption()
        poll.wait()

    window.add_shutdown_callback(stop)
