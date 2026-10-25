"""NeuroWatch Cloud in the tray: its state on the status line, and a problem
when a linked cloud cannot be reached."""

from __future__ import annotations

from typing import Any

import pytest

from neurowatch.cloud.tray import tray_status
from neurowatch.tray import read_status


def _backend(cloud: Any):
    answers: dict[str, Any] = {
        "system.status": {
            "status": "running",
            "cameras": 3,
            "site_name": "Main Gate",
            "video_acceleration": {"label": "VA-API"},
            "version": "0.1.0",
            "uptime_sec": 7500,
        },
        "camera.status_all": {"a": "streaming", "b": "recording_ready", "c": "offline"},
        "recording.status_all": {"a": "recording", "b": "recording", "c": "idle"},
        "cloud.status": cloud,
    }

    def request(method: str, **_kwargs: Any) -> Any:
        return answers.get(method)

    return request


def test_the_tray_shows_the_cloud_state() -> None:
    status = read_status(_backend({"phase": "linked", "cloud_reachable": True}), endpoint="unix:/x")
    assert status.lines() == [
        "Running · Main Gate",
        "Cameras 2/3 online · 2 recording",
        "Cloud: connected · VA-API",
        "⚠ 1 camera offline",
    ]


@pytest.mark.parametrize(
    ("cloud", "problems"),
    [
        (
            {"phase": "linked", "cloud_reachable": False},
            [("Cloud unreachable", "Linked, but can't reach NeuroWatch Cloud")],
        ),
        # Cloud is optional: never linked, or linked and reachable, is fine.
        ({"phase": "unlinked", "cloud_configured": True}, []),
        ({"phase": "linked", "cloud_reachable": True}, []),
        (None, []),
    ],
)
def test_only_an_unreachable_linked_cloud_is_a_problem(cloud, problems) -> None:
    assert list(tray_status(_backend(cloud), "unix:/x").problems) == problems
    labels = [p.label for p in read_status(_backend(cloud), endpoint="unix:/x").problems]
    assert labels == ["1 camera offline", *(label for label, _ in problems)]


def test_a_wrong_computer_clock_is_a_tray_problem_with_the_backends_advice() -> None:
    warning = "This computer's clock is 5 h 30 min ahead of NeuroWatch Cloud."
    cloud = {"phase": "linked", "cloud_reachable": True, "clock_warning": warning}
    assert list(tray_status(_backend(cloud), "unix:/x").problems) == [("Clock is wrong", warning)]
    unreachable = {**cloud, "cloud_reachable": False}
    labels = [label for label, _ in tray_status(_backend(unreachable), "unix:/x").problems]
    assert labels == ["Cloud unreachable", "Clock is wrong"]
