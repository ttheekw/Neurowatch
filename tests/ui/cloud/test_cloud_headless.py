"""NeuroWatch Cloud in the headless commands: status line, linking, unlinking."""

from __future__ import annotations

import io
from typing import Any

import pytest

from neurowatch import headless
from neurowatch.cloud import headless as cloud
from neurowatch.cloud.headless import link
from neurowatch.headless import Snapshot, describe_changes

LINKED = {
    "phase": "linked",
    "cloud_configured": True,
    "cloud_reachable": True,
    "cloud_endpoint": "https://cloud.example.test",
    "last_check_in": "now",
}


def _snap(statuses: dict[str, str], status: dict | None = None, recording=None) -> Snapshot:
    return Snapshot(
        statuses=statuses,
        names={"a": "Front Door", "b": "Lobby"},
        recording=recording or {},
        extras=(cloud.cloud_line(status),),
    )


def test_first_poll_prints_the_cloud_line():
    lines = describe_changes(None, _snap({"a": "streaming", "b": "connecting"}, LINKED))

    assert lines == [
        "Cameras: 1 streaming · 1 connecting (of 2)",
        "Cloud: connected",
    ]


def test_recording_and_cloud_changes_are_reported():
    before = _snap({"a": "streaming"}, {"phase": "unlinked", "cloud_configured": True})
    after = _snap({"a": "streaming"}, LINKED, recording={"a": "recording"})

    assert describe_changes(before, after) == [
        "Recording: 1 cameras",
        "Cloud: connected",
    ]


@pytest.mark.parametrize(
    ("status", "text"),
    [
        (None, "status unavailable"),
        ({"phase": "unlinked", "cloud_configured": False}, "not configured for this backend"),
        ({"phase": "unlinked", "cloud_configured": True}, "not linked"),
        ({**LINKED, "cloud_reachable": False}, "Cloud: linked, but cannot"),
        ({**LINKED, "media_publish": {"sessions": {"s": "publishing"}}}, "sending 1 stream"),
        (
            {"phase": "awaiting_local_confirmation", "cloud_configured": True},
            "run the link command again",
        ),
    ],
)
def test_cloud_line_wording(status, text):
    assert text in cloud.cloud_line(status)


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


CODE = "11111111-1111-4111-8111-111111111111.22222222-2222-4222-8222-222222222222.proof-token-1234"


def test_link_links_in_one_step():
    backend = FakeBackend(
        {
            "cloud.status": {"phase": "unlinked", "cloud_configured": True},
            "cloud.begin": lambda params: LINKED,
        }
    )
    said: list[str] = []

    assert link(backend, CODE, "Lab 64", said.append) == 0
    assert backend.calls == ["cloud.status", "cloud.begin"]
    assert said[-1] == "Cloud: connected"


def test_link_finishes_a_link_that_stopped_after_the_claim():
    backend = FakeBackend(
        {
            "cloud.status": {"phase": "awaiting_local_confirmation", "cloud_configured": True},
            "cloud.confirm": LINKED,
        }
    )
    said: list[str] = []

    assert link(backend, CODE, "Lab", said.append) == 0
    assert backend.calls == ["cloud.status", "cloud.confirm"]
    assert said[-1] == "Cloud: connected"


def test_link_rejects_bad_input_before_any_rpc():
    backend = FakeBackend({})
    said: list[str] = []

    assert link(backend, "CODE", "Lab", said.append) == 2
    assert backend.calls == []
    assert "not an enrollment code" in said[0]
    assert "starting 'CODE'" in said[0]
    assert "Create enrollment" in said[0]

    # A real-looking code with a bad ID never has its secret token echoed.
    said.clear()
    assert link(backend, "tenant.enrollment.secret-proof-9999", "Lab", said.append) == 2
    assert "secret-proof-9999" not in said[0]


@pytest.mark.parametrize(
    ("status", "code", "text"),
    [
        ({"phase": "unlinked", "cloud_configured": False}, 1, "NW_CLOUD_ENDPOINT"),
        (LINKED, 0, "already linked"),
    ],
)
def test_link_explains_when_it_cannot_or_need_not_link(status, code, text):
    backend = FakeBackend({"cloud.status": status})
    said: list[str] = []

    assert link(backend, CODE, "Lab", said.append) == code
    assert backend.calls == ["cloud.status"]
    assert text in said[-1]


@pytest.mark.parametrize(
    ("error", "text"),
    [
        (RuntimeError("HTTP 404"), "The web portal did not accept"),
        (RuntimeError("invalid params: this build is not linked to a cloud"), "not available"),
    ],
)
def test_link_failure_is_reported(error, text):
    backend = FakeBackend(
        {
            "cloud.status": {"phase": "unlinked", "cloud_configured": True},
            "cloud.begin": error,
        }
    )
    said: list[str] = []

    assert link(backend, CODE, "Lab", said.append) == 1
    assert said[-1].startswith("Cloud link failed: ") and text in said[-1]


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
            "cloud.status": {"phase": "unlinked", "cloud_configured": True},
            "system.status": {"site_name": "Lab", "video_acceleration": {"kind": "vaapi"}},
            "config.get": {"recording": {"storage_path": "/", "local_all_cameras": True}},
        }
    )


NOT_LINKED = "Cloud: not linked (link with: neurowatch cloud link CODE --name NAME)"


def test_the_status_and_the_monitor_show_the_cloud_line(monkeypatch):
    backend = _monitor_backend([{"a": "streaming"}, {"a": "streaming"}])
    monkeypatch.setattr(headless, "client_rpc", lambda _endpoint: backend)
    out = io.StringIO()

    assert headless.main(["cloud", "status"], out=out) == 0
    assert out.getvalue().splitlines()[-1].endswith(NOT_LINKED)
    assert headless.poll(backend, {}).extras == (NOT_LINKED,)


def test_headless_link_failure_keeps_the_node_running(monkeypatch):
    backend = _monitor_backend([{"a": "streaming"}] + [ConnectionError("gone")] * 3)
    backend.answers["cloud.begin"] = RuntimeError("HTTP 404")
    monkeypatch.setattr(headless, "client_rpc", lambda _endpoint: backend)
    out = io.StringIO()

    code = headless.main(["monitor", "--interval", "0", "--link", CODE, "--name", "Lab"], out=out)

    assert code == 1  # the monitor ran on until the backend stopped answering
    assert "cloud.begin" in backend.calls and "camera.status_all" in backend.calls
    assert "Cloud link failed" in out.getvalue()


def test_headless_link_with_a_malformed_code_stops_at_once(monkeypatch):
    backend = FakeBackend({})
    monkeypatch.setattr(headless, "client_rpc", lambda _endpoint: backend)

    assert headless.main(["monitor", "--link", "CODE"], out=io.StringIO()) == 2
    assert backend.calls == []


def test_a_name_needs_a_link(monkeypatch):
    monkeypatch.setattr(headless, "client_rpc", lambda _endpoint: FakeBackend({}))
    out = io.StringIO()

    assert headless.main(["monitor", "--name", "Lab"], out=out) == 2
    assert "--name needs --link" in out.getvalue()


def test_cloud_link_command(monkeypatch):
    backend = FakeBackend(
        {
            "cloud.status": {"phase": "unlinked", "cloud_configured": True},
            "cloud.begin": lambda params: LINKED,
        }
    )
    monkeypatch.setattr(headless, "client_rpc", lambda _endpoint: backend)

    assert headless.main(["cloud", "link", CODE, "--name", "Lab"], out=io.StringIO()) == 0
    assert backend.calls == ["cloud.status", "cloud.begin"]


def test_unlink_needs_confirmation(monkeypatch):
    backend = FakeBackend({"cloud.unlink": {"unlinked": True}})
    monkeypatch.setattr(headless, "client_rpc", lambda _endpoint: backend)
    out = io.StringIO()

    assert headless.main(["cloud", "unlink"], out=out) == 2
    assert backend.calls == []
    assert headless.main(["cloud", "unlink", "--yes"], out=out) == 0
    assert backend.calls == ["cloud.unlink"]


@pytest.mark.parametrize(
    ("status", "text"),
    [
        (
            {
                **LINKED,
                "cloud_reachable": False,
                "cloud_problem": "endpoint_unusable",
                "cloud_endpoint": "http://127.0.0.1:8000",
            },
            "scripts/dev_cloud_headless.sh",
        ),
        (
            {**LINKED, "cloud_reachable": False, "cloud_problem": "endpoint_unusable"},
            "not usable",
        ),
        (
            {**LINKED, "cloud_reachable": False, "cloud_problem": "link_refused"},
            "refused this node",
        ),
    ],
)
def test_unreachable_cloud_says_why_and_how_to_fix_it(status, text):
    assert text in cloud.cloud_line(status)


def test_cloud_is_a_launch_command():
    from neurowatch.app_launch import parse_launch_args

    launch = parse_launch_args(["neurowatch-ui", "cloud", "link", "CODE", "--name", "Lab"])
    assert launch.mode == "command"
    assert launch.rest == ("cloud", "link", "CODE", "--name", "Lab")


WARNING = "This computer's clock is 5 h 30 min ahead of NeuroWatch Cloud."


def test_the_cloud_line_carries_the_clock_warning():
    assert cloud.cloud_line({**LINKED, "clock_warning": WARNING}) == f"Cloud: connected. {WARNING}"


def test_a_failed_link_prints_the_clock_warning_the_attempt_revealed():
    backend = FakeBackend(
        {
            "cloud.status": {
                "phase": "unlinked",
                "cloud_configured": True,
                "clock_warning": WARNING,
            },
            "cloud.begin": RuntimeError(
                "invalid params: unsafe or invalid cloud configuration: the enrollment code "
                "has expired (codes last five minutes)"
            ),
        }
    )
    said: list[str] = []

    assert link(backend, CODE, "Lab", said.append) == 1
    assert said[-2].startswith("Cloud link failed: The enrollment code has expired")
    assert "time zone" in said[-2]
    assert said[-1] == f"Cloud link: {WARNING}"
