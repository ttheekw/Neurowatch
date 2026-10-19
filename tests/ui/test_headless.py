"""Headless mode: command-line status lines without the GUI."""

from __future__ import annotations

import io
from typing import Any

from neurowatch import headless
from neurowatch.headless import Snapshot, describe_changes, monitor


def _snap(statuses: dict[str, str], extras=(), recording=None) -> Snapshot:
    return Snapshot(
        statuses=statuses,
        names={"a": "Front Door", "b": "Lobby"},
        recording=recording or {},
        extras=tuple(extras),
    )


def test_first_poll_prints_a_summary_not_one_line_per_camera():
    lines = describe_changes(None, _snap({"a": "streaming", "b": "connecting"}, ["Example: on"]))

    assert lines == ["Cameras: 1 streaming · 1 connecting (of 2)", "Example: on"]


def test_camera_transitions_use_names_and_hide_recording_ready():
    before = _snap({"a": "connecting", "b": "streaming"})
    after = _snap({"a": "recording_ready", "b": "streaming"})

    assert describe_changes(before, after) == [
        "Camera Front Door: connecting → streaming",
        "Cameras: 2 streaming (of 2)",
    ]
    assert describe_changes(after, after) == []


def test_many_changes_collapse_into_one_line():
    before = Snapshot(statuses={str(i): "connecting" for i in range(40)})
    after = Snapshot(statuses={str(i): "streaming" for i in range(40)})

    assert describe_changes(before, after) == [
        "40 cameras changed state",
        "Cameras: 40 streaming (of 40)",
    ]


def test_recording_and_extension_changes_are_reported():
    before = _snap({"a": "streaming"}, ["Example: off", "Other: same"])
    after = _snap({"a": "streaming"}, ["Example: on", "Other: same"], recording={"a": "recording"})

    assert describe_changes(before, after) == ["Recording: 1 cameras", "Example: on"]


def test_the_periodic_status_includes_the_extension_lines():
    line = headless.status_line(_snap({"a": "streaming"}, ["Example: on"]), free_gb=12.5)

    assert line == "Status: 1/1 cameras streaming · recording 0 · 12.5 GB free · example on"


class FakeBackend:
    """Answers RPCs from a dict; a callable value is called with the params."""

    def __init__(self, answers: dict[str, Any]) -> None:
        self.answers = answers
        self.calls: list[str] = []

    def __call__(self, method: str, params: dict[str, Any], timeout: float) -> Any:
        self.calls.append(method)
        answer = self.answers[method]
        if isinstance(answer, Exception):
            raise answer
        return answer(params) if callable(answer) else answer


def _monitor_backend(statuses_seq: list[Any]) -> FakeBackend:
    sequence = iter(statuses_seq)

    def statuses(_params):
        value = next(sequence)
        if isinstance(value, Exception):
            raise value
        return value

    return FakeBackend(
        {
            "camera.status_all": statuses,
            "camera.list": [{"id": "a", "name": "Front Door"}],
            "recording.status_all": {},
            "system.status": {"site_name": "Lab", "video_acceleration": {"kind": "vaapi"}},
            "config.get": {"recording": {"storage_path": "/", "local_all_cameras": True}},
        }
    )


def test_monitor_reports_readiness_changes_and_a_periodic_summary():
    backend = _monitor_backend([{"a": "connecting"}, {"a": "streaming"}, {"a": "streaming"}])
    said: list[str] = []
    now = [0.0]

    def sleep(_seconds: float) -> None:
        now[0] += 40  # each poll interval "takes" 40 s

    code = monitor(
        backend,
        said.append,
        interval=0,
        heartbeat=60,
        sleep=sleep,
        clock=lambda: now[0],
        max_polls=3,
    )

    assert code == 0
    assert said[0] == "Backend ready: Lab · decoder VA-API hardware"
    # Where recordings go, and how much room is left, is visible up front.
    assert said[1].startswith("Recording: all cameras to / (") and "GB free" in said[1]
    assert "Camera Front Door: connecting → streaming" in said
    assert said[-1].startswith("Status: 1/1 cameras streaming · recording 0 · ")
    assert "GB free" in said[-1]


def test_monitor_exits_when_the_backend_stops_answering():
    backend = _monitor_backend([{"a": "streaming"}] + [ConnectionError("gone")] * 3)
    said: list[str] = []

    code = monitor(backend, said.append, interval=0, sleep=lambda _s: None, max_polls=10)

    assert code == 1
    assert said[-1].startswith("Backend stopped responding")


def test_service_monitor_waits_for_a_restarting_backend():
    # The background service restarts its backend (a crash, a dev rebuild):
    # the monitor says so and carries on, announcing it again when it is back.
    backend = _monitor_backend(
        [{"a": "streaming"}] + [ConnectionError("restarting")] * 4 + [{"a": "streaming"}]
    )
    said: list[str] = []

    code = monitor(
        backend, said.append, interval=0, sleep=lambda _s: None, max_polls=6, persistent=True
    )

    assert code == 0
    assert said.count("Backend not responding; waiting for the service to bring it back") == 1
    assert [line for line in said if line.startswith("Backend ready")] == [
        "Backend ready: Lab · decoder VA-API hardware"
    ] * 2


def test_headless_starts_the_service_in_headless_mode_and_outlives_ctrl_c(monkeypatch):
    from neurowatch.service import manager

    calls: list[tuple[bool, str]] = []

    class Controller:
        def __init__(self, **_kwargs: Any) -> None:
            pass

        def status(self):
            return {"status": "running", "headless": False}  # the desktop's service

        def ensure_running(self, *, autostart: bool, mode: str) -> bool:
            calls.append((autostart, mode))
            return True

        def describe(self):
            return {
                "manager": "systemd user service",
                "starts_at_boot": True,
                "starts_at_login": True,
            }

    def interrupted(*_args: Any, **_kwargs: Any) -> int:
        raise KeyboardInterrupt

    monkeypatch.setattr(manager, "ServiceController", Controller)
    monkeypatch.setattr(headless, "monitor", interrupted)
    monkeypatch.setattr(headless, "client_rpc", lambda _endpoint: FakeBackend({}))
    out = io.StringIO()

    assert headless.main(["monitor", "--service"], out=out) == 0
    text = out.getvalue()
    assert calls == [(True, "headless")], "registered to start at boot by default"
    assert "Restarting the NeuroWatch service in headless mode" in text
    assert "it starts at boot" in text
    assert "NeuroWatch keeps running in the background" in text
    assert text.splitlines()[0].endswith(headless.NO_WARRANTY), "the notice comes first"
    assert "ABSOLUTELY NO WARRANTY" in headless._parser().format_help()

    calls.clear()
    headless.main(["monitor", "--service", "--no-autostart"], out=io.StringIO())
    assert calls == [(False, "headless")]


def test_the_cloud_adds_its_status_line_link_options_and_command(monkeypatch):
    # Two status reads (status, cloud status), then the recorder is gone.
    backend = _monitor_backend([{"a": "streaming"}] * 2 + [ConnectionError("gone")] * 3)
    backend.answers["cloud.status"] = {"phase": "unlinked", "cloud_configured": True}
    monkeypatch.setattr(headless, "client_rpc", lambda _endpoint: backend)

    out = io.StringIO()
    assert headless.main(["status"], out=out) == 0
    assert "Cloud: not linked" in out.getvalue().splitlines()[-1]

    out = io.StringIO()
    assert headless.main(["cloud", "status"], out=out) == 0
    assert "Cloud: not linked" in out.getvalue()

    # --name without --link stops before monitoring; otherwise the monitor runs.
    assert headless.main(["monitor", "--name", "Lab"], out=io.StringIO()) == 2
    assert headless.main(["monitor", "--interval", "0"], out=io.StringIO()) == 1


def test_a_failing_cloud_line_does_not_stop_the_status(monkeypatch):
    from neurowatch.cloud import headless as cloud_cli

    def broken(_rpc: Any) -> str:
        raise RuntimeError("broken cloud line")

    monkeypatch.setattr(cloud_cli, "status_line", broken)
    backend = _monitor_backend([{"a": "streaming"}])

    snapshot = headless.poll(backend, {})

    assert snapshot.statuses == {"a": "streaming"} and snapshot.extras == ()


# ── Events ─────────────────────────────────────────────────────────────────


def _notice(seq: int, phase: str, kind: str, camera: str | None = "a", **extra: Any) -> dict:
    return {
        "seq": seq,
        "phase": phase,
        "event": {"id": seq, "kind": kind, "camera_id": camera, "start_ms": 0, **extra},
    }


def test_event_lines_name_the_camera_and_skip_uninteresting_ends():
    names = {"a": "Front Door"}
    line = headless.event_line
    assert line({"kind": "motion", "camera_id": "a"}, names) == "Motion · Front Door"
    assert line({"kind": "motion", "camera_id": "a"}, names, "end") is None
    assert line({"kind": "camera_offline", "camera_id": "a"}, names, "end") == (
        "Camera back online · Front Door"
    )
    # The health monitor's detail is the camera's name: said once.
    assert line({"kind": "camera_offline", "camera_id": "a", "detail": "Front Door"}, names) == (
        "Camera offline · Front Door"
    )
    assert line(
        {"kind": "disk_low", "camera_id": None, "detail": "1.5 GB free on /rec"}, names
    ) == ("Recording drive almost full · 1.5 GB free on /rec")
    assert line({"kind": "motion"}, names, "update") is None


def test_event_feed_starts_from_now_and_follows_the_cursor():
    calls: list[dict] = []
    answers = iter(
        [
            {"boot": "b1", "seq": 5, "reset": False, "notices": []},
            {"boot": "b1", "seq": 6, "reset": False, "notices": [_notice(6, "start", "motion")]},
            {"boot": "b2", "seq": 1, "reset": True, "notices": []},
        ]
    )

    def wait(params: dict) -> dict:
        calls.append(params)
        return next(answers)

    feed = headless.EventFeed(FakeBackend({"events.wait": wait}))
    assert feed.poll() == []
    assert [n["seq"] for n in feed.poll(1000)] == [6]
    assert calls[1] == {"timeout_ms": 1000, "after": 5, "boot": "b1"}
    assert feed.poll() == []  # a restarted backend: start over, quietly
    assert feed._seq == 1


def test_monitor_prints_new_events():
    backend = _monitor_backend([{"a": "streaming"}, {"a": "streaming"}])
    answers = iter(
        [
            {"boot": "b", "seq": 0, "notices": []},
            {
                "boot": "b",
                "seq": 2,
                "notices": [_notice(1, "start", "motion"), _notice(2, "end", "motion")],
            },
        ]
    )
    backend.answers["events.wait"] = lambda _params: next(answers)
    said: list[str] = []

    monitor(backend, said.append, interval=0, sleep=lambda _s: None, max_polls=2)

    assert said.count("Motion · Front Door") == 1


def test_events_command_lists_oldest_first_and_filters_by_camera():
    seen: list[dict] = []

    def listed(params: dict) -> dict:
        seen.append(params)
        return {
            "events": [
                {"kind": "motion", "camera_id": "a", "start_ms": 2_000, "end_ms": 5_000},
                {"kind": "camera_offline", "camera_id": "a", "start_ms": 1_000, "end_ms": None},
            ]
        }

    backend = FakeBackend(
        {"camera.list": [{"id": "a", "name": "Front Door"}], "events.list": listed}
    )
    said: list[str] = []
    assert headless.print_events(backend, said.append, camera="Front Door", limit=5) == 0
    assert seen == [{"limit": 5, "camera_ids": ["a"]}]
    assert said[0].endswith("Camera offline · Front Door  (going on)")
    assert said[1].endswith("Motion · Front Door  (3 s)")
    assert headless.print_events(backend, said.append, camera="Nope") == 2


def test_events_is_a_launcher_command():
    from neurowatch.app_launch import parse_launch_args

    launch = parse_launch_args(["neurowatch-ui", "events", "--follow"])
    assert (launch.mode, launch.rest) == ("command", ("events", "--follow"))
