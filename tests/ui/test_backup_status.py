"""Cloud Backup's status bar symbol and tray problems, from system.status."""

from __future__ import annotations

from neurowatch.models.backup_status import (
    GREEN,
    GREY,
    RED,
    backup_notice,
    backup_problems,
    backup_summary,
)
from neurowatch.tray import read_status


def _system(
    state: str, failing: list[dict[str, str]] | None = None, cameras: int = 2
) -> dict[str, object]:
    return {
        "status": "running",
        "cameras": cameras,
        "backup": {"state": state, "cameras": cameras, "failing": failing or []},
    }


def test_the_symbol_is_green_while_backing_up_red_when_failing_and_hidden_without_a_plan() -> None:
    ok = backup_notice(backup_summary(_system("ok")))
    assert (ok.text, ok.color) == ("Backup", GREEN)
    assert ok.tooltip == "Cloud Backup: backing up 2 cameras to the cloud."
    failing = backup_notice(
        backup_summary(
            _system(
                "failing",
                [{"id": "c1", "name": "Gate", "why": "Not connected to the backup server."}],
            )
        )
    )
    assert (failing.text, failing.color) == ("Backup failing", RED)
    assert "Gate: Not connected to the backup server." in failing.tooltip
    two = backup_notice(
        backup_summary(
            _system(
                "failing",
                [
                    {"id": "c1", "name": "Gate", "why": "x"},
                    {"id": "c2", "name": "Yard", "why": "y"},
                ],
            )
        )
    )
    assert two.text == "Backup failing (2)"
    assert backup_notice(backup_summary(_system("starting"))).color == GREY
    assert backup_notice(backup_summary(_system("off"))).text == ""
    # An older recorder reports no backup: no symbol, never an error.
    for system in ({"status": "running"}, None, {"backup": "x"}, {"backup": {"state": "odd"}}):
        assert backup_notice(backup_summary(system)).text == ""


def test_the_indicator_follows_each_poll(qtbot) -> None:  # type: ignore[no-untyped-def]
    from neurowatch.widgets.backup_notice import BackupIndicator

    indicator = BackupIndicator()
    qtbot.addWidget(indicator)
    assert indicator.text() == "", "no symbol before the first poll"
    indicator.show()
    indicator.set_status(_system("ok"))
    assert (
        indicator.isVisible() and indicator.text() == "Backup" and GREEN in indicator.styleSheet()
    )
    assert indicator.mark() == "cloud"
    indicator.set_status(_system("failing", [{"id": "c1", "name": "Gate", "why": "x"}]))
    assert indicator.text() == "Backup failing" and RED in indicator.styleSheet()
    assert indicator.accessibleDescription().startswith("Cloud Backup is not backing up Gate")
    indicator.set_status(None)
    assert not indicator.isVisible()
    indicator.grab()  # the cloud mark paints


def test_the_tray_names_each_camera_whose_backup_fails() -> None:
    summary = backup_summary(
        _system("failing", [{"id": "c1", "name": "Gate", "why": "Not connected."}])
    )
    assert backup_problems(summary) == [
        ("Cloud Backup failing: Gate", "Cloud Backup failing on Gate: Not connected.")
    ]

    def request(method: str, **_: object) -> object:
        return {
            "system.status": _system(
                "failing", [{"id": "c1", "name": "Gate", "why": "Not connected."}]
            ),
            "camera.status_all": {"c1": "streaming", "c2": "streaming"},
            "recording.status_all": {},
            "stats.get_performance": {},
        }.get(method)

    status = read_status(request, endpoint="unix:/nonexistent")
    assert "Cloud Backup failing: Gate" in [problem.label for problem in status.problems]


def test_a_wall_tile_shows_its_cameras_backup_symbol_beside_the_name(qtbot) -> None:  # type: ignore[no-untyped-def]
    import numpy as np

    from neurowatch.models.backup_status import backup_tile_marks
    from neurowatch.models.camera import CameraConfig
    from neurowatch.widgets.video_wall import VideoWallWidget

    system = {
        "backup": {
            "state": "failing",
            "cameras": 2,
            "failing": [],
            "states": {
                "c1": {"state": "ok", "why": None},
                "c2": {"state": "failing", "why": "Not connected to the backup server."},
            },
        }
    }
    marks = backup_tile_marks(system)
    assert marks == {
        "c1": ("ok", "Cloud Backup: backing up to the cloud"),
        "c2": ("failing", "Cloud Backup failing: Not connected to the backup server."),
    }
    assert backup_tile_marks({"backup": {"state": "ok"}}) == {}, (
        "an older recorder: no tile symbols"
    )

    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall.resize(640, 360)
    wall.set_grid(1, 2)
    wall.set_cameras(
        [
            CameraConfig(id="c1", name="Door", url="rtsp://x/1"),
            CameraConfig(id="c2", name="Gate", url="rtsp://x/2"),
        ]
    )
    wall.show()
    wall.set_backup(marks)
    door, gate = wall._cells[0], wall._cells[1]
    assert (door.backup, gate.backup) == ("ok", "failing")
    assert "Cloud Backup failing: Not connected" in gate.toolTip()
    frame = np.full((180, 320, 3), 40, dtype=np.uint8).tobytes()
    for cell, colour in ((door, "green"), (gate, "red")):
        cell.update_frame(frame, 320, 180, "rgb")
        image = cell.grab().toImage()
        box = cell.backup_marker_rect()
        assert not box.isNull()
        pixel = image.pixelColor(box.left() + 2, box.center().y())
        assert (
            (pixel.green() > pixel.red()) if colour == "green" else (pixel.red() > pixel.green())
        ), colour
    # Beside a plugin's marker, it comes after it.
    door.set_analytics("Analysed by Motion detection", True)
    door.grab()
    assert door.backup_marker_rect().left() > door.analytics_marker_rect().right()
    wall.set_backup({})
    assert door.backup is None and door.backup_marker_rect().isNull()
