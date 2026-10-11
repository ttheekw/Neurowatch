"""Recording drive: listed like the web portal, applied when Settings is saved."""

from __future__ import annotations

from typing import Any

import pytest

from neurowatch.dialogs import settings_dialog
from neurowatch.dialogs.drive_picker import (
    Drive,
    DrivePickerDialog,
    StorageScan,
    format_bytes,
)
from neurowatch.dialogs.settings_dialog import SettingsDialog
from neurowatch.ipc.sync_client import IpcError

ROOT = Drive("/", "/dev/nvme0n1p2", "ext4", 500_000_000_000, 200_000_000_000, True, True)
BIG = Drive(
    "/media/nw/Big Disk", "/dev/sdb1", "ext4", 2_000_000_000_000, 1_500_000_000_000, True, False
)
USB = Drive("/mnt/usb", "/dev/sdc1", "exfat", 64_000_000_000, 60_000_000_000, False, False)
SCAN = StorageScan(drives=(ROOT, BIG, USB), storage_path="/home/nw/Videos/neurowatch")


@pytest.mark.parametrize(
    ("count", "shown"),
    [
        (0, "0 B"),
        (64_000_000_000, "64 GB"),
        (1_500_000_000_000, "1.5 TB"),
        (2_000_000_000_000, "2 TB"),
    ],
)
def test_sizes_read_like_the_label_on_a_drive(count: int, shown: str) -> None:
    assert format_bytes(count) == shown


def test_a_path_belongs_to_the_longest_mount_it_lies_under() -> None:
    assert SCAN.drive_of("/media/nw/Big Disk/nw-storage") == BIG
    assert SCAN.drive_of("/media/nw/Big Diskette") == ROOT
    assert SCAN.drive_of("/home/nw/Videos") == ROOT
    assert SCAN.folder_on(ROOT) == "/nw-storage"
    assert SCAN.folder_on(BIG) == "/media/nw/Big Disk/nw-storage"
    assert StorageScan.from_json(
        {"drives": [{"mount_point": "/x", "total_bytes": 1}], "storage_path": "/x/y"}
    ).drive_of("/x/y") == Drive("/x", "", "", 1, 0, False, False)


def test_small_drives_are_hidden_until_asked(qtbot) -> None:
    picker = DrivePickerDialog(SCAN, SCAN.storage_path)
    qtbot.addWidget(picker)
    assert [row.drive for row in picker.rows()] == [ROOT, BIG]
    assert not picker._empty.isHidden() and "1 smaller drive hidden" in picker._empty.text()

    picker._show_all.setChecked(True)
    assert [row.drive for row in picker.rows() if not row.isHidden()] == [ROOT, BIG, USB]


def test_choosing_a_drive_closes_the_picker_with_it(qtbot) -> None:
    picker = DrivePickerDialog(SCAN, "/media/nw/Big Disk/nw-storage")
    qtbot.addWidget(picker)
    rows = {row.drive: row for row in picker.rows()}
    # The drive whose folder is already the pending path cannot be chosen again.
    assert not rows[BIG].button.isEnabled() and rows[BIG].button.text() == "Chosen"
    assert rows[ROOT].button.isEnabled()

    rows[ROOT].button.click()
    assert picker.result() == DrivePickerDialog.DialogCode.Accepted
    assert picker.chosen_drive() == ROOT


def test_settings_shows_the_drive_and_hands_the_pick_to_save(qtbot, monkeypatch) -> None:
    started: list[str] = []
    monkeypatch.setattr(SettingsDialog, "_scan_drives", lambda self: started.append("scan"))
    dlg = SettingsDialog(
        {"recording": {"storage_path": SCAN.storage_path}}, ipc_endpoint="unix:/tmp/nw.sock"
    )
    qtbot.addWidget(dlg)
    assert started == ["scan"] and not dlg._choose_drive_btn.isEnabled()

    dlg._on_drives(SCAN)
    assert dlg._choose_drive_btn.isEnabled()
    assert dlg._drive_label.text() == "/ · 200 GB free of 500 GB"

    class _Picker:
        def __init__(self, *_args: Any) -> None:
            pass

        def exec(self) -> object:
            return DrivePickerDialog.DialogCode.Accepted

        def chosen_drive(self) -> Drive:
            return BIG

    monkeypatch.setattr(settings_dialog, "DrivePickerDialog", _Picker)
    dlg._choose_drive()
    assert dlg.get_config()["recording"]["storage_path"] == "/media/nw/Big Disk/nw-storage"
    assert dlg._drive_label.text() == "/media/nw/Big Disk · 1.5 TB free of 2 TB"
    assert dlg.selected_drive() == "/media/nw/Big Disk"

    # Typing another path drops the drive pick: that path is saved as typed.
    dlg._rec_storage.setText("/srv/recordings")
    assert dlg.selected_drive() is None


def test_settings_without_a_backend_has_no_drive_row(qtbot) -> None:
    dlg = SettingsDialog({})
    qtbot.addWidget(dlg)
    assert dlg._scan_worker is None
    assert dlg._drive_label.parentWidget().isHidden()
    assert dlg.selected_drive() is None


def test_a_failed_scan_says_why(qtbot, monkeypatch) -> None:
    monkeypatch.setattr(SettingsDialog, "_scan_drives", lambda self: None)
    dlg = SettingsDialog({}, ipc_endpoint="unix:/tmp/nw.sock")
    qtbot.addWidget(dlg)
    dlg._on_drives(IpcError(-32601, "Method not found"))
    assert dlg._drive_label.text() == "Drives could not be listed: Method not found"
    assert not dlg._choose_drive_btn.isEnabled()


class _Backend:
    def __init__(self, select_error: str | None = None) -> None:
        self.calls: list[tuple[str, Any]] = []
        self._select_error = select_error

    def client(self, endpoint: str | None = None) -> _Backend:
        return self

    def call(self, method: str, params: Any = None) -> Any:
        self.calls.append((method, params))
        if method == "storage.select":
            if self._select_error:
                raise IpcError(-32603, self._select_error)
            return {"storage_path": f"{params['mount_point']}/nw-storage"}
        if method == "config.get":
            return {"refreshed": True}
        return {"ok": True}


def _save(monkeypatch, backend: _Backend, config: dict, drive: str | None) -> tuple[list, list]:
    from neurowatch import main

    monkeypatch.setattr(main, "SyncIpcClient", backend.client)
    worker = main._SettingsSaveWorker("unix:/tmp/nw.sock", config, drive)
    failures: list[str] = []
    results: list[object] = []
    worker.drive_failed.connect(failures.append)
    worker.result.connect(results.append)
    worker.run()
    return failures, results


def test_saving_prepares_the_chosen_drive_first(monkeypatch) -> None:
    backend = _Backend()
    config = {"site_name": "A", "recording": {"storage_path": "/media/d/nw-storage"}}
    failures, results = _save(monkeypatch, backend, config, "/media/d")

    assert [method for method, _ in backend.calls] == ["storage.select", "config.set", "config.get"]
    assert backend.calls[0][1] == {"mount_point": "/media/d"}
    assert backend.calls[1][1]["recording"]["storage_path"] == "/media/d/nw-storage"
    assert failures == [] and results == [{"refreshed": True}]


def test_a_drive_that_cannot_be_used_leaves_the_location_but_saves_the_rest(monkeypatch) -> None:
    backend = _Backend(select_error="cannot create /media/d/nw-storage: permission denied")
    config = {
        "site_name": "A",
        "recording": {"storage_path": "/media/d/nw-storage", "segment_minutes": 5},
    }
    failures, results = _save(monkeypatch, backend, config, "/media/d")

    saved = backend.calls[1][1]
    assert "storage_path" not in saved["recording"] and saved["recording"]["segment_minutes"] == 5
    assert saved["site_name"] == "A"
    assert failures == ["cannot create /media/d/nw-storage: permission denied"]
    assert results == [{"refreshed": True}]
    # The dialog's config is not changed by the save.
    assert config["recording"]["storage_path"] == "/media/d/nw-storage"


def test_saving_without_a_drive_pick_does_not_touch_drives(monkeypatch) -> None:
    backend = _Backend()
    _save(monkeypatch, backend, {"recording": {"storage_path": "/srv/rec"}}, None)
    assert [method for method, _ in backend.calls] == ["config.set", "config.get"]


def test_disk_threshold_is_shown_in_drive_size_and_saved(qtbot, monkeypatch) -> None:
    monkeypatch.setattr(SettingsDialog, "_scan_drives", lambda self: None)
    dlg = SettingsDialog(
        {"recording": {"storage_path": SCAN.storage_path, "disk_threshold_percent": 80}},
        ipc_endpoint="unix:/tmp/nw.sock",
    )
    qtbot.addWidget(dlg)
    assert dlg._rec_cleanup.isChecked() and dlg._rec_threshold.value() == 80
    assert "80% full" in dlg._threshold_hint.text()

    dlg._on_drives(SCAN)
    assert dlg._threshold_hint.text() == (
        "The oldest recordings are deleted once 400 GB of 500 GB is used on /."
    )
    assert dlg.get_config()["recording"]["disk_threshold_percent"] == 80

    dlg._rec_cleanup.setChecked(False)
    assert not dlg._rec_threshold.isEnabled()
    assert dlg.get_config()["recording"]["disk_threshold_percent"] == 0


def test_disk_threshold_defaults_on_and_off_stays_off(qtbot) -> None:
    default = SettingsDialog({})
    qtbot.addWidget(default)
    assert default._rec_cleanup.isChecked()
    assert default.get_config()["recording"]["disk_threshold_percent"] == 90

    off = SettingsDialog({"recording": {"disk_threshold_percent": 0}})
    qtbot.addWidget(off)
    assert not off._rec_cleanup.isChecked()
    assert off.get_config()["recording"]["disk_threshold_percent"] == 0
