"""Events as they happen, for the desktop UI (``events.open`` + ``events.wait``).

The IPC has no push stream: ``events.wait`` is a long poll that answers as
soon as something happens. This worker keeps one going in a background
thread and emits what arrives. It asks with short waits, so stopping it
(closing the app) never waits long; a backend that restarts is followed
from its new start (``reset``), after reading its events still going on.
"""

from __future__ import annotations

import logging
from collections.abc import Callable
from typing import Any

from PySide6.QtCore import Signal

from neurowatch.ipc.sync_client import SyncIpcClient
from neurowatch.models.events import ACTIVITY_KINDS
from neurowatch.utils.threads import Worker

logger = logging.getLogger(__name__)

#: Each long poll's wait; events still arrive at once.
WAIT_MS = 1500
#: Pause before asking again after the backend did not answer.
RETRY_MS = 2000
EVENT_PAGE_SIZE = 1000


def read_events(
    client: Any,
    params: dict[str, Any],
    interrupted: Callable[[], bool],
    *,
    max_events: int | None = None,
) -> list[dict]:
    """Read a filtered history off the GUI thread, including every page."""
    events: list[dict] = []
    query = {**params, "limit": EVENT_PAGE_SIZE}
    while not interrupted():
        result = client.call("events.list", query)
        page = result.get("events", [])
        events.extend(page if max_events is None else page[: max(0, max_events - len(events))])
        if max_events is not None and len(events) >= max_events:
            break
        if len(page) < EVENT_PAGE_SIZE:
            break
        last = page[-1]
        cursor = (int(last["start_ms"]), int(last["id"]))
        previous = (query.get("before_ms"), query.get("before_id"))
        if cursor == previous:
            raise RuntimeError("The events history did not advance to the next page")
        query.update(before_ms=cursor[0], before_id=cursor[1])
    return events


class EventFeedWorker(Worker):
    """Restores current/pending activity, then emits event notices."""

    #: Events going on (dicts), after (re)connecting: replaces what was known.
    started_over = Signal(list)
    pending_over = Signal(list)
    #: New notices (``{seq, phase, event}`` dicts), in order.
    notices = Signal(list)

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
        boot: str | None = None
        seq: int | None = None
        while not self.isInterruptionRequested():
            try:
                client = self._client_factory()
                if seq is None:
                    # Take the cursor before the snapshots: events arriving
                    # during these reads are replayed, never lost in between.
                    cursor = client.call("events.wait", {"timeout_ms": 0})
                    boot, seq = cursor.get("boot"), int(cursor.get("seq") or 0)
                    opened = client.call("events.open")
                    pending = read_events(
                        client,
                        {"kinds": sorted(ACTIVITY_KINDS), "unacknowledged": True},
                        self.isInterruptionRequested,
                    )
                    if self.isInterruptionRequested():
                        return
                    events = opened.get("events") if isinstance(opened, dict) else None
                    self.started_over.emit([e for e in events or [] if isinstance(e, dict)])
                    self.pending_over.emit(pending)
                    continue
                result = client.call(
                    "events.wait",
                    {"after": seq, "boot": boot, "timeout_ms": WAIT_MS},
                    timeout=WAIT_MS / 1000 + 5,
                )
                if not isinstance(result, dict) or result.get("reset"):
                    seq = None  # another backend run: start over
                    continue
                seq = int(result.get("seq") or seq)
                notices = [n for n in result.get("notices") or [] if isinstance(n, dict)]
                if notices:
                    self.notices.emit(notices)
            except Exception as exc:  # backend restarting, or an older one
                logger.debug("Event feed: %s", exc)
                seq = None
                for _ in range(RETRY_MS // 100):
                    if self.isInterruptionRequested():
                        return
                    self.msleep(100)
