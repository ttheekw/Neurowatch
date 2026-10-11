"""Recording drive picker: the recorder's drives, as the web portal lists them.

The backend scans its mounted drives (``storage.list``); choosing one records
to an ``nw-storage`` folder on it (``storage.select``, applied when Settings
is saved). Drives smaller than the recommended size are hidden unless asked.
"""

from __future__ import annotations

import os
from dataclasses import dataclass

from PySide6.QtCore import Qt, QThread, Signal
from PySide6.QtWidgets import (
    QCheckBox,
    QDialog,
    QDialogButtonBox,
    QFrame,
    QHBoxLayout,
    QLabel,
    QPushButton,
    QVBoxLayout,
    QWidget,
)

from neurowatch.ipc.sync_client import SyncIpcClient
from neurowatch.theme import (
    apply_dialog_theme,
    set_button_variant,
    set_label_role,
    set_status_tone,
    style_button_box,
)
from neurowatch.widgets.window_chrome import install_dialog_chrome

DEFAULT_FOLDER = "nw-storage"
DEFAULT_RECOMMENDED_MIN_BYTES = 128_000_000_000


@dataclass(frozen=True)
class Drive:
    mount_point: str
    device: str
    filesystem: str
    total_bytes: int
    available_bytes: int
    #: At least the recommended size for recordings.
    recommended: bool
    #: The backend records to this drive now.
    selected: bool

    @classmethod
    def from_json(cls, value: dict) -> Drive:
        return cls(
            mount_point=str(value.get("mount_point", "")),
            device=str(value.get("device", "")),
            filesystem=str(value.get("filesystem", "")),
            total_bytes=int(value.get("total_bytes", 0)),
            available_bytes=int(value.get("available_bytes", 0)),
            recommended=bool(value.get("recommended", False)),
            selected=bool(value.get("selected", False)),
        )


@dataclass(frozen=True)
class StorageScan:
    """A ``storage.list`` answer."""

    drives: tuple[Drive, ...]
    storage_path: str
    recommended_min_bytes: int = DEFAULT_RECOMMENDED_MIN_BYTES
    folder: str = DEFAULT_FOLDER

    @classmethod
    def from_json(cls, value: dict) -> StorageScan:
        drives = tuple(Drive.from_json(d) for d in value.get("drives", []) if isinstance(d, dict))
        return cls(
            drives=drives,
            storage_path=str(value.get("storage_path", "")),
            recommended_min_bytes=int(
                value.get("recommended_min_bytes", DEFAULT_RECOMMENDED_MIN_BYTES)
            ),
            folder=str(value.get("folder", DEFAULT_FOLDER)),
        )

    def folder_on(self, drive: Drive) -> str:
        """The recording folder a drive gets when chosen."""
        return f"{drive.mount_point.rstrip('/')}/{self.folder}"

    def drive_of(self, path: str) -> Drive | None:
        """The drive holding ``path``: the longest mount point it lies under."""
        if not path:
            return None
        full = os.path.normpath(os.path.expanduser(path))
        holding = [d for d in self.drives if _under(full, d.mount_point)]
        return max(holding, key=lambda d: len(d.mount_point), default=None)


def _under(path: str, mount_point: str) -> bool:
    mount = mount_point.rstrip("/")
    return path == mount or path.startswith(f"{mount}/")


def format_bytes(count: int) -> str:
    """Decimal units, like the size printed on a drive (as the web portal)."""
    units = ("B", "KB", "MB", "GB", "TB", "PB")
    value = float(count)
    unit = 0
    while value >= 1000 and unit < len(units) - 1:
        value /= 1000
        unit += 1
    shown = str(round(value)) if value >= 10 else f"{value:.1f}".removesuffix(".0")
    return f"{shown} {units[unit]}"


def describe_space(drive: Drive) -> str:
    return f"{format_bytes(drive.available_bytes)} free of {format_bytes(drive.total_bytes)}"


class DriveScanWorker(QThread):
    """Asks the backend for its drives (``storage.list``, every drive)."""

    done = Signal(object)  # StorageScan, or Exception

    def __init__(self, endpoint: str) -> None:
        super().__init__()
        self._endpoint = endpoint

    def run(self) -> None:
        try:
            result = SyncIpcClient(endpoint=self._endpoint).call("storage.list", {"all": True})
            if not isinstance(result, dict):
                raise ValueError("the backend sent no drive list")
            self.done.emit(StorageScan.from_json(result))
        except Exception as exc:
            self.done.emit(exc)


class _DriveRow(QFrame):
    """One drive: where it is mounted, its space, and a button to use it."""

    chosen = Signal(object)  # Drive

    def __init__(self, drive: Drive, *, in_use: bool, pending: bool) -> None:
        super().__init__()
        self.drive = drive
        self.setObjectName("driveRow")
        self.setProperty("current", pending)
        layout = QHBoxLayout(self)
        layout.setContentsMargins(12, 8, 10, 8)
        layout.setSpacing(10)

        text = QVBoxLayout()
        text.setSpacing(2)
        mount = QLabel(drive.mount_point)
        mount.setTextInteractionFlags(Qt.TextInteractionFlag.TextSelectableByMouse)
        set_label_role(mount, "emphasis")
        text.addWidget(mount)
        detail = QLabel(f"{describe_space(drive)} · {drive.filesystem} · {drive.device}")
        set_label_role(detail, "hint")
        text.addWidget(detail)
        layout.addLayout(text, stretch=1)

        if in_use:
            badge = QLabel("In use")
            set_status_tone(badge, "success")
            layout.addWidget(badge)
        self.button = QPushButton("Chosen" if pending else "Use this drive")
        self.button.setEnabled(not pending)
        self.button.setAccessibleName(f"Record to {drive.mount_point}")
        set_button_variant(self.button, "secondary")
        self.button.clicked.connect(lambda: self.chosen.emit(self.drive))
        layout.addWidget(self.button)


class DrivePickerDialog(QDialog):
    """Pick the drive recordings go to, from a ``storage.list`` scan."""

    def __init__(self, scan: StorageScan, pending_path: str, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setWindowTitle("Recording Drive")
        self.setMinimumWidth(520)
        apply_dialog_theme(self)
        self._scan = scan
        self._pending_path = pending_path
        self._chosen: Drive | None = None

        layout = install_dialog_chrome(self, "Recording drive")
        intro = QLabel(
            f"Now recording to {scan.storage_path}. Choosing a drive records to a "
            f"{scan.folder} folder on it when you save Settings; recordings made "
            "so far stay where they are."
        )
        intro.setObjectName("dialogSubtitle")
        intro.setWordWrap(True)
        layout.addWidget(intro)

        self._show_all = QCheckBox("Show all drives")
        self._show_all.setToolTip(
            f"Also list drives smaller than {format_bytes(scan.recommended_min_bytes)}"
        )
        self._show_all.toggled.connect(self._fill)
        layout.addWidget(self._show_all)

        self._rows_box = QWidget()
        self._rows = QVBoxLayout(self._rows_box)
        self._rows.setContentsMargins(0, 0, 0, 0)
        self._rows.setSpacing(6)
        layout.addWidget(self._rows_box)
        self._empty = QLabel()
        self._empty.setWordWrap(True)
        set_label_role(self._empty, "hint")
        layout.addWidget(self._empty)

        btns = QDialogButtonBox(QDialogButtonBox.StandardButton.Cancel)
        btns.rejected.connect(self.reject)
        style_button_box(btns)
        layout.addWidget(btns)
        self._fill()

    def visible_drives(self) -> list[Drive]:
        show_all = self._show_all.isChecked()
        pending = self._scan.drive_of(self._pending_path)
        return [
            d for d in self._scan.drives if show_all or d.recommended or d.selected or d == pending
        ]

    def rows(self) -> list[_DriveRow]:
        return self._rows_box.findChildren(_DriveRow)

    def chosen_drive(self) -> Drive | None:
        return self._chosen

    def _fill(self) -> None:
        for row in self.rows():
            row.setParent(None)
            row.deleteLater()
        pending = os.path.normpath(os.path.expanduser(self._pending_path))
        drives = self.visible_drives()
        for drive in drives:
            row = _DriveRow(
                drive,
                in_use=drive.selected,
                pending=os.path.normpath(self._scan.folder_on(drive)) == pending,
            )
            row.chosen.connect(self._choose)
            self._rows.addWidget(row)
        hidden = len(self._scan.drives) - len(drives)
        if not drives:
            self._empty.setText(
                "No suitable drives found. Turn on “Show all drives” to see smaller ones."
                if hidden
                else "No drives found."
            )
        elif hidden:
            self._empty.setText(
                f"{hidden} smaller drive{'s' if hidden != 1 else ''} hidden "
                f"(under {format_bytes(self._scan.recommended_min_bytes)})."
            )
        self._empty.setVisible(not drives or hidden > 0)

    def _choose(self, drive: Drive) -> None:
        self._chosen = drive
        self.accept()
