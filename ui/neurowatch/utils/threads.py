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
