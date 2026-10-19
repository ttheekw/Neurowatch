"""The archive tier in Settings: its settings round trip and its status."""

from __future__ import annotations

from neurowatch.dialogs.settings_dialog import SettingsDialog
from neurowatch.models.archive import ArchiveSettings, ArchiveStatus, format_size


def test_settings_round_trip_and_tolerate_older_or_odd_values() -> None:
    raw = {
        "enabled": True,
        "path": " /mnt/nas/archive ",
        "after_days": 14,
        "quota_gb": 500,
        "hours": {"start": "22:00", "end": "06:00"},
        "max_mbps": None,
    }
    settings = ArchiveSettings.from_json(raw)
    assert settings.hours == ("22:00", "06:00") and settings.max_mbps is None
    assert settings.to_json() == {**raw, "path": "/mnt/nas/archive"}
    # A recorder without the archive sends nothing; junk falls back to defaults.
    assert ArchiveSettings.from_json(None) == ArchiveSettings()
    odd = ArchiveSettings.from_json({"after_days": "x", "quota_gb": True, "hours": {"start": ""}})
    assert (odd.after_days, odd.quota_gb, odd.hours) == (7, None, None)


def test_status_reads_as_sentences() -> None:
    status = ArchiveStatus.from_json(
        {
            "state": "idle",
            "archived_files": 300,
            "archived_bytes": 3 * 1024**3,
            "pending_files": 2,
            "pending_bytes": 1536 * 1024**2,
            "last_error": "a different file is already at /nas/x.mkv",
            "missing_locations": [{"path": "/mnt/nas", "reason": "/mnt/nas is not mounted"}],
        }
    )
    assert status.summary() == (
        "Up to date. 300 recordings (3.0 GB) in the archive · 2 (1.5 GB) waiting to move."
        " Last problem: a different file is already at /nas/x.mkv."
    )
    assert status.missing_text() == "/mnt/nas: /mnt/nas is not mounted"
    waiting = ArchiveStatus.from_json(
        {"state": "waiting", "detail": "archiving runs from 22:00 to 06:00"}
    )
    assert waiting.summary() == "Waiting: archiving runs from 22:00 to 06:00."
    assert ArchiveStatus.from_json({"state": "off", "pending_files": 9}).summary() == (
        "Archiving is off."
    )
    assert format_size(512) == "512 B"


def test_the_dialog_edits_the_archive_and_shows_its_status(qtbot, monkeypatch) -> None:
    monkeypatch.setattr(SettingsDialog, "_scan_drives", lambda self: None)
    started: list[str] = []
    monkeypatch.setattr(SettingsDialog, "_read_archive_status", lambda self: started.append("s"))
    dlg = SettingsDialog(
        {
            "recording": {
                "storage_path": "/rec",
                "archive": {"enabled": True, "path": "/nas", "after_days": 30, "quota_gb": 200},
            }
        },
        ipc_endpoint="unix:/tmp/nw.sock",
    )
    qtbot.addWidget(dlg)
    assert started == ["s"]
    assert dlg._archive_on.isChecked() and dlg._archive_path.text() == "/nas"
    assert dlg._archive_after.value() == 30
    assert dlg._archive_quota_on.isChecked() and dlg._archive_quota.value() == 200
    assert not dlg._archive_hours_on.isChecked() and not dlg._archive_start.isEnabled()
    assert not dlg._archive_speed_on.isChecked() and not dlg._archive_speed.isEnabled()

    dlg._archive_hours_on.setChecked(True)
    dlg._archive_speed_on.setChecked(True)
    dlg._archive_speed.setValue(50)
    dlg._archive_quota_on.setChecked(False)
    archive = dlg.get_config()["recording"]["archive"]
    assert archive == {
        "enabled": True,
        "path": "/nas",
        "after_days": 30,
        "quota_gb": None,
        "hours": {"start": "22:00", "end": "06:00"},
        "max_mbps": 50,
    }

    assert dlg._archive_status.isHidden()
    dlg._on_archive_status(
        ArchiveStatus(
            state="missing",
            detail="/nas is not mounted: connect its drive or network share",
            missing_locations=(),
        )
    )
    assert not dlg._archive_status.isHidden()
    assert dlg._archive_status.text().startswith("The archive folder is not available: /nas")
    assert dlg._missing_locations.isHidden()
    dlg._on_archive_status(RuntimeError("Method not found"))  # an older recorder
    assert "not available" in dlg._archive_status.text()


def test_archive_fields_follow_the_switch(qtbot, monkeypatch) -> None:
    monkeypatch.setattr(SettingsDialog, "_scan_drives", lambda self: None)
    dlg = SettingsDialog({})
    qtbot.addWidget(dlg)
    assert dlg._archive_worker is None, "no recorder to ask"
    assert not dlg._archive_on.isChecked() and not dlg._archive_path.isEnabled()
    dlg._archive_on.setChecked(True)
    assert dlg._archive_path.isEnabled()
    assert dlg.get_config()["recording"]["archive"]["after_days"] == 7


def test_a_missing_location_can_be_recorded_to_again_after_confirming(qtbot, monkeypatch) -> None:
    from neurowatch.dialogs import settings_dialog
    from neurowatch.models.archive import MissingLocation

    monkeypatch.setattr(SettingsDialog, "_scan_drives", lambda self: None)
    reads: list[str] = []
    monkeypatch.setattr(SettingsDialog, "_read_archive_status", lambda self: reads.append("s"))
    calls: list[str] = []

    from PySide6.QtCore import QObject, Signal

    class FakeWorker(QObject):
        done = Signal(object)
        finished = Signal()

        def __init__(self, endpoint: str, path: str) -> None:
            super().__init__()
            calls.append(path)

        def start(self) -> None:
            pass

    monkeypatch.setattr(settings_dialog, "_UseLocationWorker", FakeWorker)
    answers = [False, True]
    asked: list[str] = []

    def fake_ask(parent, title: str, text: str, **kw) -> bool:
        asked.append(text)
        return answers.pop(0)

    monkeypatch.setattr(settings_dialog, "ask", fake_ask)
    errors: list[str] = []
    monkeypatch.setattr(
        settings_dialog, "show_message", lambda parent, title, text, **kw: errors.append(text)
    )
    dlg = SettingsDialog({"recording": {"storage_path": "/rec"}}, ipc_endpoint="unix:/tmp/nw.sock")
    qtbot.addWidget(dlg)
    assert dlg._missing_actions.isHidden()

    reason = "/rec is not there: is its drive or network share connected and mounted?"
    dlg._on_archive_status(ArchiveStatus(missing_locations=(MissingLocation("/rec", reason),)))
    assert not dlg._missing_locations.isHidden() and not dlg._missing_actions.isHidden()
    buttons = dlg._missing_actions.findChildren(settings_dialog.QPushButton)
    assert [b.text() for b in buttons] == ["Record Here Anyway: /rec…"]

    buttons[0].click()  # declined: nothing is asked of the recorder
    assert calls == [] and "marker file" in asked[0]
    buttons[0].click()
    assert calls == ["/rec"] and dlg._use_worker is not None

    dlg._on_location_used(None)  # done: the status is read again, no message
    assert dlg._use_worker is None and errors == [] and reads[-1] == "s"
    dlg._on_location_used(RuntimeError("/rec is not mounted: connect its drive"))
    assert errors == ["/rec is not mounted: connect its drive"]

    dlg._on_archive_status(ArchiveStatus())  # back: no buttons left
    assert dlg._missing_actions.isHidden()
    assert dlg._missing_actions.findChildren(settings_dialog.QPushButton) == []
