"""Frame readers must finish before their Qt owners are destroyed."""
from __future__ import annotations

import os
import subprocess
import sys
import threading
import time
from pathlib import Path

import pytest

from neurowatch.ipc.frame_reader import FrameReaderThread


def test_stop_before_run_is_not_overwritten(qtbot, monkeypatch):
    entered = threading.Event()
    release = threading.Event()

    class DelayedReader(FrameReaderThread):
        def run(self):
            entered.set()
            release.wait(2)
            super().run()

    monkeypatch.setattr("neurowatch.ipc.frame_reader.SHM_PREFIX", "/nonexistent/nw-test-")
    reader = DelayedReader("stop-race")
    reader.start()
    assert entered.wait(2)
    timer = threading.Timer(0.05, release.set)
    timer.start()
    try:
        reader.stop()
        assert not reader.isRunning()
    finally:
        release.set()
        reader.stop()
        timer.join()


def test_stop_while_waiting_for_missing_shared_memory(qtbot, monkeypatch):
    monkeypatch.setattr("neurowatch.ipc.frame_reader.SHM_PREFIX", "/nonexistent/nw-test-")
    reader = FrameReaderThread("backend-offline")
    reader.start()
    qtbot.waitUntil(reader.isRunning)
    reader.stop()
    assert not reader.isRunning()


def test_missing_shm_retry_sleep_is_interruptible(qtbot, monkeypatch):
    waiting = threading.Event()
    reader = FrameReaderThread("interruptible-stop")
    original_wait = reader._stop_event.wait

    def wait(timeout):
        waiting.set()
        return original_wait(timeout)

    monkeypatch.setattr(reader._stop_event, "wait", wait)
    monkeypatch.setattr("neurowatch.ipc.frame_reader.SHM_PREFIX", "/nonexistent/nw-test-")
    reader.start()
    try:
        assert waiting.wait(2)
        started = time.monotonic()
        reader.stop()
        assert time.monotonic() - started < 0.3
    finally:
        reader.stop()


@pytest.mark.parametrize("exit_mode", ["SIGINT", "SIGTERM", "quit"])
def test_application_exit_joins_threads(exit_mode):
    probe = Path(__file__).parents[1] / "helpers" / "ui_shutdown_probe.py"
    result = subprocess.run(
        [sys.executable, str(probe), exit_mode],
        env={**os.environ, "QT_QPA_PLATFORM": "offscreen"},
        capture_output=True, text=True, timeout=10,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert "JOINED 16 READERS" in result.stdout
    assert "KeyboardInterrupt" not in result.stderr
    assert "QThread: Destroyed" not in result.stderr


def test_main_window_close_joins_live_readers_after_backend_loss(qtbot, monkeypatch):
    from neurowatch.ipc.sync_client import BackendOfflineError
    from neurowatch.main import MainWindow

    class FakeClient:
        _endpoint = "unix:/tmp/test-main-shutdown.sock"
        online = True

        def is_online(self):
            return True

        def call(self, method, params=None):
            if not self.online:
                raise BackendOfflineError("simulated backend crash")
            if method == "config.get":
                return {"wall": {"rows": 2, "cols": 3}}
            if method == "camera.list":
                return [
                    {"id": f"test-close-{i}", "name": f"Camera {i}", "url": "/missing.avi"}
                    for i in range(5)
                ]
            return {}

    monkeypatch.setattr("neurowatch.main.SyncIpcClient", FakeClient)
    monkeypatch.setattr("neurowatch.ipc.frame_reader.SHM_PREFIX", "/nonexistent/nw-test-")
    window = MainWindow()
    qtbot.addWidget(window)
    readers = list(window._wall._readers.values())
    assert len(readers) == 5
    FakeClient.online = False
    window.close()
    assert window._wall._readers == {}
    assert all(not reader.isRunning() for reader in readers)
    assert not window._poll_worker.isRunning()


def test_page_change_never_composites_the_wall(qtbot, monkeypatch):
    from neurowatch.main import MainWindow

    class Client:
        _endpoint = "unix:/nonexistent/test-wall-effect.sock"

        def is_online(self):
            return True

        def call(self, method, params=None):
            if method == "config.get":
                return {"wall": {"rows": 1, "cols": 1}}
            if method == "camera.list":
                return [{"id": f"fade-{i}", "name": str(i), "url": "/missing.avi"}
                        for i in range(2)]
            return {}

    monkeypatch.setattr("neurowatch.main.SyncIpcClient", Client)
    window = MainWindow()
    qtbot.addWidget(window)
    try:
        assert window._wall.graphicsEffect() is None
        window._change_page(1)
        assert window._wall.page == 1
        assert window._wall.graphicsEffect() is None
    finally:
        window.close()
