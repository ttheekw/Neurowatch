"""Bounded drawing-history prefetch for desktop playback; RPC stays off the GUI."""

from __future__ import annotations

import logging

from PySide6.QtCore import QObject, Signal

from neurowatch.ipc.sync_client import SyncIpcClient
from neurowatch.models.drawing_history import DrawingWindow, read_window
from neurowatch.utils.threads import Worker, retire

logger = logging.getLogger(__name__)


class _HistoryWorker(Worker):
    done = Signal(object)

    def __init__(self, endpoint: str, cameras: list[str], at_ms: int) -> None:
        super().__init__()
        self.endpoint = endpoint
        self.cameras = cameras
        self.at_ms = at_ms

    def run(self) -> None:
        try:
            client = SyncIpcClient(endpoint=self.endpoint)
            self.done.emit(
                read_window(client, self.cameras, self.at_ms, self.isInterruptionRequested)
            )
        except Exception as error:
            self.done.emit(error)


class PlaybackAnalytics(QObject):
    ready = Signal()
    problem = Signal(str)

    def __init__(self, endpoint: str, parent: QObject | None = None) -> None:
        super().__init__(parent)
        self.endpoint = endpoint
        self.window: DrawingWindow | None = None
        self._worker: _HistoryWorker | None = None
        self._wanted: tuple[list[str], int] | None = None

    def request(self, cameras: list[str], at_ms: int) -> None:
        self._wanted = (list(cameras), at_ms)
        if not cameras or (self.window and self.window.contains(cameras, at_ms)):
            return
        if self._worker is not None:
            if (
                cameras != self._worker.cameras
                or not self._worker.at_ms - 10_000 <= at_ms <= self._worker.at_ms + 30_000
            ):
                self._worker.requestInterruption()
            return
        worker = _HistoryWorker(self.endpoint, list(cameras), at_ms)
        self._worker = worker
        worker.done.connect(self._done)
        worker.start()

    def _done(self, result: object) -> None:
        worker = self._worker
        self._worker = None
        if worker is None:
            return
        if not worker.isInterruptionRequested():
            if isinstance(result, DrawingWindow):
                self.window = result
                self.ready.emit()
                if result.limited:
                    self.problem.emit("Some drawing history was limited for this view.")
            elif isinstance(result, Exception):
                logger.debug("Playback drawing history unavailable: %s", result)
                self.window = DrawingWindow(
                    tuple(worker.cameras), worker.at_ms - 10_000, worker.at_ms + 30_000, {}
                )
                self.problem.emit(str(result))
        retire(worker)
        if self._wanted:
            self.request(*self._wanted)

    def reset(self) -> None:
        self._wanted = None
        self.window = None
        worker, self._worker = self._worker, None
        if worker is not None:
            worker.requestInterruption()
            try:
                worker.done.disconnect(self._done)
            except (RuntimeError, TypeError):
                pass
            retire(worker)
