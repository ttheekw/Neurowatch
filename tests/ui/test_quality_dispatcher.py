"""The wall's startup quality requests must be bounded and stay ordered."""

from __future__ import annotations

import threading

from neurowatch.ipc.quality_dispatcher import CameraQualityDispatcher


def test_quality_requests_run_ten_at_a_time(monkeypatch, qtbot):
    release = threading.Event()
    lock = threading.Lock()
    active = 0
    peak = 0
    completed: list[str] = []

    class FakeClient:
        def __init__(self, endpoint: str) -> None:
            assert endpoint == "unix:/test/control.sock"

        def call(self, method: str, params: dict) -> None:
            nonlocal active, peak
            assert method == "camera.set_stream"
            with lock:
                active += 1
                peak = max(peak, active)
            assert release.wait(5)
            with lock:
                completed.append(params["id"])
                active -= 1

    monkeypatch.setattr("neurowatch.ipc.quality_dispatcher.SyncIpcClient", FakeClient)
    dispatcher = CameraQualityDispatcher("unix:/test/control.sock")
    try:
        for index in range(25):
            dispatcher.request(f"cam-{index}", "sub")
        qtbot.waitUntil(lambda: peak == 10, timeout=3000)
        assert len(completed) == 0
        release.set()
        qtbot.waitUntil(lambda: len(completed) == 25, timeout=5000)
        assert peak == 10
    finally:
        release.set()
        dispatcher.shutdown()


def test_latest_quality_waits_for_same_camera_and_supersedes_intermediate_requests(
    monkeypatch, qtbot
):
    first_started = threading.Event()
    release_first = threading.Event()
    calls: list[str] = []

    class FakeClient:
        def __init__(self, endpoint: str) -> None:
            pass

        def call(self, method: str, params: dict) -> None:
            calls.append(params["stream"])
            if len(calls) == 1:
                first_started.set()
                assert release_first.wait(5)

    monkeypatch.setattr("neurowatch.ipc.quality_dispatcher.SyncIpcClient", FakeClient)
    dispatcher = CameraQualityDispatcher("unix:/test/control.sock")
    try:
        dispatcher.request("cam", "sub")
        assert first_started.wait(3)
        dispatcher.request("cam", "sub")
        dispatcher.request("cam", "main")
        assert calls == ["sub"]
        release_first.set()
        qtbot.waitUntil(lambda: calls == ["sub", "main"], timeout=3000)
    finally:
        release_first.set()
        dispatcher.shutdown()


def test_failed_latest_quality_reports_camera_for_retry(monkeypatch, qtbot):
    class FakeClient:
        def __init__(self, endpoint: str) -> None:
            pass

        def call(self, method: str, params: dict) -> None:
            raise RuntimeError("unavailable")

    monkeypatch.setattr("neurowatch.ipc.quality_dispatcher.SyncIpcClient", FakeClient)
    dispatcher = CameraQualityDispatcher("unix:/test/control.sock")
    failures: list[tuple[str, str, object]] = []
    dispatcher.failed.connect(lambda *args: failures.append(args))
    try:
        dispatcher.request("cam", "sub")
        qtbot.waitUntil(lambda: bool(failures), timeout=3000)
        assert failures[0][:2] == ("cam", "sub")
        assert isinstance(failures[0][2], RuntimeError)
    finally:
        dispatcher.shutdown()
