"""A dialog closing while its worker still runs must not crash the app."""

from __future__ import annotations

import os
import subprocess
import sys
from pathlib import Path

from PySide6.QtCore import QThread

from neurowatch.utils import threads


def test_closing_add_camera_with_a_running_worker_does_not_crash() -> None:
    probe = Path(__file__).parents[1] / "helpers" / "closing_dialog_worker_probe.py"
    env = {**os.environ, "QT_QPA_PLATFORM": "offscreen", "PYTHONPATH": os.pathsep.join(sys.path)}
    result = subprocess.run(
        [sys.executable, str(probe)], env=env, capture_output=True, text=True, timeout=60
    )
    output = result.stdout + result.stderr
    assert result.returncode == 0, output
    assert "Destroyed while thread" not in output
    assert "RETIRED 1" in result.stdout and "LEFT 0" in result.stdout, output


class _Sleeper(QThread):
    def run(self) -> None:
        self.msleep(300)


def test_a_retired_worker_is_kept_until_it_finishes(qtbot) -> None:
    worker = _Sleeper()
    worker.start()
    threads.retire(worker)
    assert threads.retired_count() == 1
    del worker
    assert threads.wait_for_retired(3000)
    assert threads.retired_count() == 0
    threads.retire(None)  # nothing to keep
    idle = _Sleeper()
    threads.retire(idle)  # never started: nothing to keep
    assert threads.retired_count() == 0
