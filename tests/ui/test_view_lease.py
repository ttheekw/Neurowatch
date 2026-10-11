from __future__ import annotations

import threading

from neurowatch.ipc.view_lease import ViewLeaseWorker


class _Recorder:
    def __init__(self) -> None:
        self.calls: list[tuple[str, dict]] = []
        self.changed = threading.Condition()

    def call(self, method: str, params: dict) -> dict:
        with self.changed:
            self.calls.append((method, params))
            self.changed.notify_all()
        return {}

    def wait_for(self, predicate, timeout: float = 2.0) -> bool:
        with self.changed:
            return self.changed.wait_for(lambda: predicate(self.calls), timeout)


def test_the_lease_follows_the_wall_and_is_released_on_close(qtbot):
    recorder = _Recorder()
    worker = ViewLeaseWorker(None, client_id="ui-test", client_factory=lambda: recorder)
    worker.set_cameras(["cam-b", "cam-a"])
    worker.start()
    try:
        assert recorder.wait_for(
            lambda calls: (
                (
                    "view.update",
                    {
                        "client_id": "ui-test",
                        "camera_ids": ["cam-a", "cam-b"],
                        "ttl_ms": 10_000,
                        "audio_camera_id": None,
                    },
                )
                in calls
            )
        )
        # A change is sent at once, not at the next renewal (3 s).
        worker.set_cameras([])
        assert recorder.wait_for(lambda calls: calls[-1][1].get("camera_ids") == [], timeout=1.0)
    finally:
        worker.stop()
    assert recorder.calls[-1] == ("view.release", {"client_id": "ui-test"})


def test_an_unreachable_backend_does_not_stop_the_lease(qtbot):
    attempts: list[str] = []

    class _Failing:
        def call(self, method: str, params: dict) -> dict:
            attempts.append(method)
            raise ConnectionError("backend down")

    worker = ViewLeaseWorker(None, client_id="ui-test", client_factory=_Failing)
    worker.start()
    qtbot.waitUntil(lambda: "view.update" in attempts, timeout=2000)
    worker.stop()
    assert attempts[-1] == "view.release"


def test_the_lease_names_the_camera_heard_live(qtbot):
    recorder = _Recorder()
    worker = ViewLeaseWorker(None, client_id="ui-test", client_factory=lambda: recorder)
    worker.set_cameras(["cam-a"])
    worker.start()
    try:
        assert recorder.wait_for(lambda calls: bool(calls))
        # Sent at once; a UI that stops renewing goes silent with its lease.
        worker.set_audio_camera("cam-a")
        assert recorder.wait_for(
            lambda calls: calls[-1][1].get("audio_camera_id") == "cam-a", timeout=1.0
        )
        worker.set_audio_camera(None)
        assert recorder.wait_for(
            lambda calls: calls[-1][1].get("audio_camera_id") is None, timeout=1.0
        )
    finally:
        worker.stop()
