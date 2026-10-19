"""One serialized, bounded PTZ command stream; Stop takes priority over renewals."""

from __future__ import annotations

import threading
import time
from collections import deque
from typing import Any

from PySide6.QtCore import Signal

from neurowatch.ipc.sync_client import SyncIpcClient
from neurowatch.utils.threads import Worker


class PtzWorker(Worker):
    result = Signal(str, object)

    def __init__(self, endpoint: str | None, camera_id: str, client_id: str) -> None:
        super().__init__()
        self._endpoint = endpoint
        self._base = {"camera_id": camera_id, "client_id": client_id}
        self._condition = threading.Condition()
        self._commands: deque[tuple[str, dict]] = deque()
        self._velocity: tuple[float, float, float] | None = None
        self._generation = 0
        self._stop_pending = False
        self._force_stop = False
        self._closing = False

    def command(self, method: str, params: dict | None = None) -> bool:
        with self._condition:
            if self._closing or len(self._commands) >= 8:
                return False
            self._commands.append((method, params or {}))
            self._condition.notify()
        return True

    def move(self, pan: float, tilt: float, zoom: float) -> None:
        with self._condition:
            if self._closing:
                return
            self._generation += 1
            self._velocity = (pan, tilt, zoom)
            self._condition.notify()

    def halt(self, *, force: bool = False) -> None:
        with self._condition:
            self._generation += 1
            self._velocity = None
            self._stop_pending = True
            self._force_stop |= force
            # A pending preset must not move the camera after focus is lost.
            self._commands.clear()
            self._condition.notify()

    def shutdown(self) -> None:
        with self._condition:
            self._closing = True
        self.halt()

    def run(self) -> None:
        client = SyncIpcClient(self._endpoint)
        next_move = 0.0
        params: dict[str, Any]
        while True:
            with self._condition:
                while True:
                    generation = self._generation
                    if self._stop_pending:
                        self._stop_pending = False
                        method, params = "ptz.stop", ({"force": True} if self._force_stop else {})
                        self._force_stop = False
                        break
                    if self._closing:
                        return
                    if self._commands:
                        method, params = self._commands.popleft()
                        break
                    if self._velocity is not None and time.monotonic() >= next_move:
                        pan, tilt, zoom = self._velocity
                        method, params = "ptz.move", {"pan": pan, "tilt": tilt, "zoom": zoom}
                        next_move = time.monotonic() + 0.25
                        break
                    wait = max(0.001, next_move - time.monotonic()) if self._velocity else None
                    self._condition.wait(wait)
            try:
                result: Any = client.call(
                    method,
                    {**self._base, **params},
                    timeout=5 if method == "ptz.capabilities" else 2.5,
                )
            except Exception as error:
                result = error
                if method == "ptz.move":
                    with self._condition:
                        if self._generation == generation:
                            self._velocity = None
                            self._stop_pending = True
            self.result.emit(method, result)
