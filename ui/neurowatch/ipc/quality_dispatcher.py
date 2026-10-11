"""Bounded, per-camera ordered live-stream requests for the video wall."""

from __future__ import annotations

from PySide6.QtCore import QObject, QRunnable, QThreadPool, Signal

from neurowatch.ipc.sync_client import SyncIpcClient


class _QualityJobSignals(QObject):
    finished = Signal(str, str, object)  # camera ID, requested stream, error or None


class _QualityJob(QRunnable):
    def __init__(self, cam_id: str, stream: str, endpoint: str) -> None:
        super().__init__()
        self.cam_id = cam_id
        self.stream = stream
        self.endpoint = endpoint
        self.signals = _QualityJobSignals()

    def run(self) -> None:
        error: Exception | None = None
        try:
            SyncIpcClient(endpoint=self.endpoint).call(
                "camera.set_stream", {"id": self.cam_id, "stream": self.stream}
            )
        except Exception as exc:
            error = exc
        self.signals.finished.emit(self.cam_id, self.stream, error)


class CameraQualityDispatcher(QObject):
    """Run up to ten IPC calls at once, never overlapping calls for one camera.

    Requests made while a camera is in flight replace its pending request. This
    preserves the latest page/solo choice without restarting a pipeline for
    every intermediate choice.
    """

    failed = Signal(str, str, object)  # camera ID, stream, error

    def __init__(self, endpoint: str, parent: QObject | None = None) -> None:
        super().__init__(parent)
        self._endpoint = endpoint
        self._pool = QThreadPool(self)
        self._pool.setMaxThreadCount(10)
        self._pending: dict[str, str] = {}
        self._inflight: dict[str, _QualityJob] = {}
        self._closed = False

    def request(self, cam_id: str, stream: str) -> None:
        """Ask for ``stream`` ("main", "sub" or a stream id) on ``cam_id``."""
        if self._closed:
            return
        self._pending[cam_id] = stream
        if cam_id not in self._inflight:
            self._start_next(cam_id)

    def _start_next(self, cam_id: str) -> None:
        stream = self._pending.pop(cam_id)
        job = _QualityJob(cam_id, stream, self._endpoint)
        self._inflight[cam_id] = job
        job.signals.finished.connect(self._on_finished)
        self._pool.start(job)

    def _on_finished(self, cam_id: str, stream: str, error: object) -> None:
        self._inflight.pop(cam_id, None)
        if self._closed:
            return
        pending = self._pending.get(cam_id)
        if pending is not None:
            if pending == stream and error is None:
                self._pending.pop(cam_id)
            else:
                self._start_next(cam_id)
                return
        if error is not None:
            self.failed.emit(cam_id, stream, error)

    def shutdown(self) -> None:
        self._closed = True
        self._pending.clear()
        self._pool.clear()
        self._pool.waitForDone()
        self._inflight.clear()
