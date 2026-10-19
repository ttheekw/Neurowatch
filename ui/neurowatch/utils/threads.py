"""Letting go of a worker thread that may still be running.

A ``QThread`` destroyed while it runs aborts the whole application ("QThread:
Destroyed while thread is still running"). A dialog that closes while one of
its workers still waits on the backend or the network (a camera search, a
stream discovery, a connection test) waits a moment, then must not drop the
last reference to it: ``retire`` keeps the worker until it finishes by itself.
"""

from __future__ import annotations

import logging
import time

from PySide6.QtCore import QThread

logger = logging.getLogger(__name__)

_retired: set[QThread] = set()
_alive: set[QThread] = set()


class Worker(QThread):
    """A ``QThread`` that keeps itself alive while it runs.

    A worker's result signal is emitted from its thread just before ``run``
    returns, and the slot that receives it (on the GUI thread) often drops
    the last reference (``self._worker = None``). When the GUI thread gets
    there first, Python destroyed a running thread and Qt aborted the app
    ("QThread: Destroyed while thread '' is still running"; seen in the
    camera dialog while the backend restarted). A ``Worker`` holds itself from
    ``start`` until its thread has ended, so letting go of it is always safe.
    """

    def start(self, priority: QThread.Priority = QThread.Priority.InheritPriority) -> None:
        if self not in _alive:
            _alive.add(self)
            self.finished.connect(self._release)
        super().start(priority)

    def _release(self) -> None:
        # `finished` comes just before the thread ends: wait for that moment.
        self.wait()
        _alive.discard(self)
        try:
            self.finished.disconnect(self._release)
        except (RuntimeError, TypeError):
            pass


def alive_count() -> int:
    """Workers started and not yet ended (for tests)."""
    return len(_alive)


def retire(worker: QThread | None) -> None:
    """Keep ``worker`` alive until it finishes; the caller may forget it."""
    if worker is None:
        return
    try:
        running = worker.isRunning()
    except RuntimeError:  # its C++ object is already gone
        return
    if not running:
        return
    _retired.add(worker)
    worker.finished.connect(lambda w=worker: _retired.discard(w))
    logger.debug("Worker %s still running; kept until it finishes", type(worker).__name__)


def retired_count() -> int:
    """Workers still running after their owners let them go."""
    return sum(1 for worker in list(_retired) if _running(worker))


def wait_for_retired(timeout_ms: int) -> bool:
    """At exit: give retired workers up to ``timeout_ms`` in total to finish.
    True when none is left running."""
    deadline = time.monotonic() + timeout_ms / 1000
    for worker in list(_retired):
        left = int((deadline - time.monotonic()) * 1000)
        if left > 0 and _running(worker):
            worker.wait(left)
    for worker in list(_retired):
        if not _running(worker):
            _retired.discard(worker)
    if _retired:
        logger.warning("%d background task(s) still running at exit", len(_retired))
    return not _retired


def _running(worker: QThread) -> bool:
    try:
        return worker.isRunning()
    except RuntimeError:
        return False
