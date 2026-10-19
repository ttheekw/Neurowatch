"""Desktop alerts as the recorder fires them (``alerts.desktop``).

A rule with a "This computer" channel puts its alerts in a short numbered
list on the recorder; this worker asks for new ones every couple of seconds
and emits each. It starts from the current number (nothing old is shown),
and quietly waits while the recorder is down; a recorder that restarted
numbers from zero again and is followed from there.
"""

from __future__ import annotations

import logging
from collections.abc import Callable
from typing import Any

from PySide6.QtCore import QThread, Signal

from neurowatch.service import probe
from neurowatch.utils.threads import Worker

logger = logging.getLogger(__name__)

POLL_MS = 2000

Call = Callable[[str, dict[str, Any]], Any]


def _ask(method: str, params: dict[str, Any], endpoint: str | None) -> Any:
    """One quiet call (None when the recorder does not answer)."""
    return probe.request(method, params, endpoint=endpoint)


class AlertFeedWorker(Worker):
    """Emits ``alert`` (a dict: title, message, camera_id, event_id, kind, at_ms)."""

    alert = Signal(object)

    def __init__(self, endpoint: str | None = None, call: Call | None = None) -> None:
        super().__init__()
        self._call: Call = call or (lambda method, params: _ask(method, params, endpoint))
        self._after: int | None = None

    def poll_once(self) -> list[dict[str, Any]]:
        """One ask; the alerts that are new since the last one."""
        params = {} if self._after is None else {"after": self._after}
        reply = self._call("alerts.desktop", params)
        if not isinstance(reply, dict) or not isinstance(reply.get("seq"), int):
            return []
        first = self._after is None
        self._after = reply["seq"]
        if first:
            return []
        alerts = reply.get("alerts")
        return [a for a in alerts if isinstance(a, dict)] if isinstance(alerts, list) else []

    def run(self) -> None:
        while not self.isInterruptionRequested():
            try:
                for alert in self.poll_once():
                    self.alert.emit(alert)
            except Exception:
                logger.exception("Reading desktop alerts failed")
            waited = 0
            while waited < POLL_MS and not self.isInterruptionRequested():
                QThread.msleep(100)
                waited += 100
