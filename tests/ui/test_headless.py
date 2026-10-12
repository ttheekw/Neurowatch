"""Headless mode: command-line status lines without the GUI."""

from __future__ import annotations

import argparse
import io
from types import SimpleNamespace
from typing import Any

from neurowatch import extensions, headless
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


def _example_extension(said_before: list[str]) -> SimpleNamespace:
    """An extension with every headless hook (see neurowatch.extensions)."""

    def add_monitor_arguments(parser: argparse.ArgumentParser) -> None:
        parser.add_argument("--example", action="store_true")

    def before_monitor(args: argparse.Namespace, _rpc: Any, say: Any) -> int | None:
        said_before.append("before")
        return 2 if args.example else None

    def add_commands(subparsers: Any) -> None:
        subparsers.add_parser("example").add_argument("value")

    def run_command(args: argparse.Namespace, _rpc: Any, say: Any) -> int | None:
        if args.command != "example":
            return None
        say(f"example {args.value}")
        return 0

    return SimpleNamespace(
        commands=("example",),
        status_line=lambda rpc: f"Example: {rpc('example.status', {}, 1.0)}",
        add_monitor_arguments=add_monitor_arguments,
        before_monitor=before_monitor,
        add_commands=add_commands,
        run_command=run_command,
    )


def test_extensions_add_status_lines_monitor_options_and_commands(monkeypatch):
    calls: list[str] = []
    extension = _example_extension(calls)
    monkeypatch.setattr(extensions, "load", lambda: (extension,))
    backend = _monitor_backend([{"a": "streaming"}] + [ConnectionError("gone")] * 3)
    backend.answers["example.status"] = "on"
    monkeypatch.setattr(headless, "client_rpc", lambda _endpoint: backend)

    out = io.StringIO()
    assert headless.main(["status"], out=out) == 0
    assert out.getvalue().splitlines()[-1].endswith("Example: on")

    out = io.StringIO()
    assert headless.main(["example", "42"], out=out) == 0
    assert out.getvalue().strip().endswith("example 42")
    assert extensions.commands() == {"example"}

    # A monitor option can stop before monitoring; otherwise the monitor runs.
    assert headless.main(["monitor", "--example"], out=io.StringIO()) == 2
    assert headless.main(["monitor", "--interval", "0"], out=io.StringIO()) == 1
    assert calls == ["before", "before"]


def test_a_failing_extension_does_not_stop_the_status(monkeypatch):
    def broken(_rpc: Any) -> str:
        raise RuntimeError("broken extension")

    monkeypatch.setattr(extensions, "load", lambda: (SimpleNamespace(status_line=broken),))
    backend = _monitor_backend([{"a": "streaming"}])

    snapshot = headless.poll(backend, {})

    assert snapshot.statuses == {"a": "streaming"} and snapshot.extras == ()
