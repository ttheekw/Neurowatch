"""Push-to-talk: start, renew while held, and stop a ``talk.*`` session on
one thread, in order. Releasing always wins over a renewal still waiting."""

from __future__ import annotations

import threading
import time
from typing import Any

from PySide6.QtCore import Signal

from neurowatch.ipc.sync_client import SyncIpcClient
from neurowatch.utils.threads import Worker

#: Renewals come well inside the recorder's 3 s lease.
RENEW_S = 1.0


class TalkWorker(Worker):
    """``state`` emits ("connecting" | "talking" | "stopped" | "error", detail)."""

    state = Signal(str, str)

    def __init__(
        self, endpoint: str | None, camera_id: str, client_id: str, client: Any = None
    ) -> None:
        super().__init__()
        self._endpoint = endpoint
        self._client = client
        self._camera_id = camera_id
        self._client_id = client_id
        self._condition = threading.Condition()
        self._want = False
        self._source = "microphone"
        self._closing = False

    def press(self, source: str = "microphone") -> None:
        with self._condition:
            self._want = True
            self._source = source
            self._condition.notify()

    def release(self) -> None:
        with self._condition:
            self._want = False
            self._condition.notify()

    def shutdown(self) -> None:
        with self._condition:
            self._want = False
            self._closing = True
            self._condition.notify()

    def run(self) -> None:
        client = self._client or SyncIpcClient(self._endpoint)
        talking = False
        next_renew = 0.0
        base = {"client_id": self._client_id}
        while True:
            with self._condition:
                while True:
                    want, closing, source = self._want, self._closing, self._source
                    if want and not talking:
                        action = "start"
                        break
                    if talking and not want:
                        action = "stop"
                        break
                    if closing:
                        return
                    if talking and time.monotonic() >= next_renew:
                        action = "renew"
                        break
                    self._condition.wait(
                        max(0.01, next_renew - time.monotonic()) if talking else None
                    )
            try:
                if action == "start":
                    self.state.emit("connecting", "")
                    result = client.call(
                        "talk.start",
                        {**base, "camera_id": self._camera_id, "source": source},
                        timeout=15,
                    )
                    talking = True
                    next_renew = time.monotonic() + RENEW_S
                    codec = result.get("codec", "") if isinstance(result, dict) else ""
                    self.state.emit("talking", str(codec))
                elif action == "renew":
                    client.call("talk.renew", base, timeout=3)
                    next_renew = time.monotonic() + RENEW_S
                else:
                    client.call("talk.stop", base, timeout=3)
                    talking = False
                    self.state.emit("stopped", "")
            except Exception as exc:
                talking = False
                with self._condition:
                    # A failed start is not retried until the button is pressed again.
                    self._want = False
                self.state.emit("error", str(exc))
