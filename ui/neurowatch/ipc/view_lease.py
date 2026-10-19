"""Desktop view lease: which cameras this UI shows, so only those are decoded.

The backend decodes a camera only while a desktop UI's lease names it
(``view.update``). This worker sends the wall's cameras as soon as they change
and renews the lease every few seconds while they are shown; a UI that closes
releases it (``view.release``), and one that crashes simply stops renewing, so
the lease lapses on its own and no decoder outlives the view. Native overlay
sessions carry the same client id and end with the lease.

The lease also names the one camera this UI listens to live
(``audio_camera_id``), so a UI that closes or crashes goes silent too.
"""

from __future__ import annotations

import logging
import threading
import uuid
from collections.abc import Callable, Iterable

from PySide6.QtWidgets import QWidget

from neurowatch.ipc.sync_client import SyncIpcClient
from neurowatch.utils.threads import Worker

logger = logging.getLogger(__name__)

#: This UI process's lease identity, shared with native viewer sessions.
CLIENT_ID: str = f"ui-{uuid.uuid4().hex}"
#: How long the backend keeps decoding after the last renewal.
LEASE_TTL_MS = 10_000
#: Renewal period: well inside the lease, so one missed call costs nothing.
RENEW_INTERVAL_S = 3.0


class ViewLeaseWorker(Worker):
    """Keeps this UI's view lease current from a background thread."""

    def __init__(
        self,
        endpoint: str | None,
        parent: QWidget | None = None,
        *,
        client_id: str = CLIENT_ID,
        client_factory: Callable[[], SyncIpcClient] | None = None,
    ) -> None:
        super().__init__(parent)
        self._client_id = client_id
        self._client_factory = client_factory or (lambda: SyncIpcClient(endpoint=endpoint))
        self._lock = threading.Lock()
        self._cameras: tuple[str, ...] = ()
        self._audio_camera: str | None = None
        self._wake = threading.Event()

    @property
    def cameras(self) -> tuple[str, ...]:
        with self._lock:
            return self._cameras

    def set_cameras(self, camera_ids: Iterable[str]) -> None:
        """Show exactly ``camera_ids``; sent at once. Safe from any thread."""
        cameras = tuple(sorted(set(camera_ids)))
        with self._lock:
            if cameras == self._cameras:
                return
            self._cameras = cameras
        self._wake.set()

    @property
    def audio_camera(self) -> str | None:
        with self._lock:
            return self._audio_camera

    def set_audio_camera(self, camera_id: str | None) -> None:
        """Hear ``camera_id`` live (None: nothing); sent at once."""
        with self._lock:
            if camera_id == self._audio_camera:
                return
            self._audio_camera = camera_id
        self._wake.set()

    def stop(self) -> None:
        """Release the lease and end the thread."""
        self.requestInterruption()
        self._wake.set()
        self.wait(3000)

    def run(self) -> None:
        while not self.isInterruptionRequested():
            self._wake.clear()
            cameras = self.cameras
            audio_camera = self.audio_camera
            try:
                self._client_factory().call(
                    "view.update",
                    {
                        "client_id": self._client_id,
                        "camera_ids": list(cameras),
                        "ttl_ms": LEASE_TTL_MS,
                        "audio_camera_id": audio_camera,
                    },
                )
            except Exception as error:
                # An older or unreachable backend: retried on the next renewal.
                logger.debug("view.update failed: %s", error)
            self._wake.wait(RENEW_INTERVAL_S)
        try:
            self._client_factory().call("view.release", {"client_id": self._client_id})
        except Exception as error:
            logger.debug("view.release failed: %s", error)
