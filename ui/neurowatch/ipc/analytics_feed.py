"""Analytics plugins on the desktop: which plugins analyse each camera
(``plugins.cameras``, every few seconds) and their drawings as they change
(``plugins.overlays``, a long poll like ``events.wait``). The long poll also
wakes when a plugin's figures (its counts) change: the cameras are then read
again at once, so a count shows with the boxes that made it.

One background thread for both. Short waits keep stopping it quick; a
recorder that does not answer is asked again after a pause.
"""

from __future__ import annotations

import logging
import time
from collections.abc import Callable
from typing import Any

from PySide6.QtCore import Signal

from neurowatch.ipc.sync_client import SyncIpcClient
from neurowatch.models.analytics import CameraPlugin, parse_cameras, parse_overlays
from neurowatch.utils.threads import Worker

logger = logging.getLogger(__name__)

#: Each long poll's wait; drawings still arrive at once.
WAIT_MS = 1500
#: How often the cameras' plugins are read again.
CAMERAS_EVERY_S = 5.0
#: While a plugin is on its way: the walls keep its drawings back until they know
#: which layers it shows, and a tile waits on it meanwhile.
CAMERAS_STARTING_EVERY_S = 1.0
#: A plugin still not connected this long after opening is simply off.
STARTING_LIMIT_S = 30.0
RETRY_MS = 2000


def next_cameras_read(cameras: dict[str, list[CameraPlugin]], since_opened_s: float) -> float:
    """Seconds until the plugins are read again: soon while one is on its way."""
    starting = any(
        plugin.state in ("starting", "waiting")
        or (plugin.state == "offline" and since_opened_s < STARTING_LIMIT_S)
        for plugins in cameras.values()
        for plugin in plugins
    )
    return CAMERAS_STARTING_EVERY_S if starting else CAMERAS_EVERY_S


class AnalyticsFeedWorker(Worker):
    #: ``{camera_id: [CameraPlugin]}`` when it changed.
    cameras = Signal(object)
    #: ``{camera_id: [Drawing]}``: everything drawn now, after each change.
    drawings = Signal(object)

    def __init__(
        self,
        endpoint: str | None,
        *,
        client_factory: Callable[[], Any] | None = None,
    ) -> None:
        # No parent: a running worker is retired at shutdown, never destroyed.
        super().__init__()
        self._client_factory = client_factory or (lambda: SyncIpcClient(endpoint=endpoint))

    def run(self) -> None:
        client = self._client_factory()
        seq: int | None = None
        known: object = None
        figures: object = None
        next_cameras = 0.0
        began = time.monotonic()
        while not self.isInterruptionRequested():
            try:
                if time.monotonic() >= next_cameras:
                    raw = client.call("plugins.cameras", {})
                    now = time.monotonic()
                    cameras = parse_cameras(raw)
                    next_cameras = now + next_cameras_read(cameras, now - began)
                    if raw != known:
                        known = raw
                        self.cameras.emit(cameras)
                params: dict[str, Any] = {}
                if seq is not None:
                    params = {"after": seq, "timeout_ms": WAIT_MS}
                result = client.call("plugins.overlays", params, timeout=WAIT_MS / 1000 + 5)
                new_seq = result.get("seq") if isinstance(result, dict) else None
                if isinstance(new_seq, int) and new_seq != seq:
                    seq = new_seq
                    self.drawings.emit(parse_overlays(result, time.monotonic()))
                    # Older recorders send no figures here: they keep the usual reads.
                    new_figures = result.get("figures")
                    if new_figures is not None and new_figures != figures:
                        if figures is not None:
                            next_cameras = 0.0
                        figures = new_figures
            except Exception as exc:  # the recorder restarts, or refuses
                logger.debug("Analytics feed: %s", exc)
                seq = None
                self._pause(RETRY_MS)

    def _pause(self, ms: int) -> None:
        for _ in range(ms // 100):
            if self.isInterruptionRequested():
                return
            self.msleep(100)
