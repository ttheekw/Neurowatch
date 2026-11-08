"""Motion zones drawn on the camera's picture, with a list to name, add and
remove them. Used where a plugin's settings have a ``nw:zones`` field (the
motion plugin's Watch / Ignore areas).
"""

from __future__ import annotations

import base64
import logging
from typing import Any

from PySide6.QtCore import QObject, Qt, Signal
from PySide6.QtGui import QIcon, QPixmap
from PySide6.QtWidgets import (
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QListWidget,
    QListWidgetItem,
    QPushButton,
    QVBoxLayout,
    QWidget,
)

from neurowatch.ipc.sync_client import SyncIpcClient
from neurowatch.models.motion import valid_motion_zones
from neurowatch.theme import set_button_variant, set_label_role, set_status_tone
from neurowatch.utils.threads import Worker, retire
from neurowatch.widgets.zone_canvas import ZoneCanvas, zone_color

logger = logging.getLogger(__name__)


class _SnapshotWorker(Worker):
    done = Signal(object)  # snapshot.capture result, or Exception

    def __init__(self, endpoint: str | None, camera_id: str) -> None:
        super().__init__()
        self._endpoint = endpoint
        self._camera_id = camera_id

    def run(self) -> None:
        try:
            self.done.emit(
                SyncIpcClient(endpoint=self._endpoint).call(
                    "snapshot.capture", {"camera_id": self._camera_id}
                )
            )
        except Exception as exc:  # shown in the editor, never raised in Qt
            self.done.emit(exc)


class SnapshotSource(QObject):
    """A camera's picture for a drawing editor, taken off the UI thread:
    ``picture`` with it, or ``failed`` with what to tell the user."""

    picture = Signal(QPixmap)
    failed = Signal(str)

    def __init__(self, endpoint: str | None, parent: QObject | None = None) -> None:
        super().__init__(parent)
        self._endpoint = endpoint
        self._worker: _SnapshotWorker | None = None

    @property
    def busy(self) -> bool:
        return self._worker is not None

    def load(self, camera_id: str) -> bool:
        """Ask for ``camera_id``'s picture; False while one is on its way."""
        if self._worker is not None:
            return False
        worker = _SnapshotWorker(self._endpoint, camera_id)
        worker.done.connect(self._done)
        worker.finished.connect(worker.deleteLater)
        self._worker = worker
        worker.start()
        return True

    def stop(self) -> None:
        """Let a running request go (the editor is closing)."""
        worker = self._worker
        if worker is None:
            return
        try:
            worker.done.disconnect(self._done)
        except (RuntimeError, TypeError):
            pass
        retire(worker)
        self._worker = None

    def _done(self, result: object) -> None:
        self._worker = None
        if not isinstance(result, dict):
            logger.info("Camera picture unavailable: %s", result)
            self.failed.emit(
                "The camera's picture is unavailable; you can still draw. "
                "Check the camera connection, then Refresh Picture."
            )
            return
        picture = QPixmap()
        try:
            data = base64.b64decode(result.get("jpeg_b64", ""), validate=True)
        except (ValueError, TypeError):
            data = b""
        if picture.loadFromData(data):
            self.picture.emit(picture)
        else:
            self.failed.emit("The camera did not return a usable picture.")


class ZoneEditor(QWidget):
    """The zone canvas and its list. ``zones()`` is what to save; the
    camera's picture is taken with ``load_snapshot`` (off the UI thread)."""

    changed = Signal()

    def __init__(
        self,
        zones: list[dict[str, Any]],
        *,
        endpoint: str | None = None,
        camera_id: str | None = None,
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        self._camera_id = camera_id
        self._snapshots = SnapshotSource(endpoint, self)
        self._snapshots.picture.connect(self._on_picture)
        self._snapshots.failed.connect(self._on_no_picture)

        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(8)
        editor = QHBoxLayout()
        editor.setSpacing(12)
        picture_column = QVBoxLayout()
        picture_column.setSpacing(8)
        self.canvas = ZoneCanvas()
        self.canvas.set_zones(zones)
        self.canvas.changed.connect(self._refresh)
        picture_column.addWidget(self.canvas)
        self.status = QLabel("" if camera_id else "Choose a camera to see its picture here.")
        self.status.setWordWrap(True)
        set_label_role(self.status, "hint")
        picture_column.addWidget(self.status)
        self.snapshot_button = QPushButton("Refresh Picture")
        self.snapshot_button.setToolTip("Take a new picture from the camera to draw on")
        set_button_variant(self.snapshot_button, "secondary")
        self.snapshot_button.clicked.connect(self.load_snapshot)
        self.snapshot_button.setEnabled(bool(camera_id))
        picture_column.addWidget(self.snapshot_button, 0, Qt.AlignmentFlag.AlignLeft)
        editor.addLayout(picture_column, 3)

        zone_column = QVBoxLayout()
        zone_column.setSpacing(8)
        zones_label = QLabel("Zones")
        set_label_role(zones_label, "sectionLabel")
        zone_column.addWidget(zones_label)
        self.picker = QListWidget()
        self.picker.setObjectName("zoneList")
        self.picker.setAccessibleName("Motion zones")
        self.picker.setMinimumWidth(180)
        self.picker.currentRowChanged.connect(self._select)
        zone_column.addWidget(self.picker, 1)
        self.name = QLineEdit()
        self.name.setMaxLength(64)
        self.name.setPlaceholderText("Zone name")
        self.name.setAccessibleName("Zone name")
        self.name.textEdited.connect(self._rename)
        zone_column.addWidget(self.name)
        add_row = QHBoxLayout()
        add_row.setSpacing(6)
        for label, kind, tip in (
            ("+ Watch", "include", "Add an area where movement counts"),
            ("+ Ignore", "exclude", "Add an area where movement never counts"),
        ):
            button = QPushButton(label)
            button.setToolTip(tip)
            set_button_variant(button, "secondary")
            button.clicked.connect(lambda _checked=False, kind=kind: self.add_zone(kind))
            add_row.addWidget(button)
        zone_column.addLayout(add_row)
        self.remove_button = QPushButton("Remove Zone")
        set_button_variant(self.remove_button, "ghost")
        self.remove_button.clicked.connect(self._remove)
        zone_column.addWidget(self.remove_button)
        editor.addLayout(zone_column, 1)
        layout.addLayout(editor)
        hint = QLabel(
            "Without zones the whole picture is watched; ignored areas always win. Click "
            "the picture to add points (three or more make a zone), drag a point to move "
            "it, right-click a point to remove it. Check the zones again after the camera "
            "is moved or re-aimed."
        )
        hint.setWordWrap(True)
        set_label_role(hint, "hint")
        layout.addWidget(hint)
        self._refresh()

    def zones(self) -> list[dict[str, Any]]:
        return self.canvas.zones()

    def valid(self) -> bool:
        return valid_motion_zones(self.zones())

    def show_problem(self, text: str) -> None:
        set_status_tone(self.status, "warning")
        self.status.setText(text)

    def set_camera(self, camera_id: str | None) -> None:
        """Draw on another camera's picture (a new binding's camera choice)."""
        if camera_id == self._camera_id:
            return
        self._camera_id = camera_id
        self.canvas.set_snapshot(QPixmap())
        self.canvas.set_placeholder(
            "Loading the camera picture…" if camera_id else "No camera chosen"
        )
        self.snapshot_button.setEnabled(bool(camera_id))
        self.status.setText("" if camera_id else "Choose a camera to see its picture here.")

    def add_zone(self, kind: str) -> None:
        if self.canvas.add_zone(kind):
            self.name.setFocus()
            self.name.selectAll()

    def load_snapshot(self) -> None:
        if not self._camera_id or self._snapshots.busy:
            return
        self.snapshot_button.setEnabled(False)
        set_status_tone(self.status, "muted")
        self.status.setText("Taking a picture from the camera…")
        self.canvas.set_placeholder("Loading the camera picture…")
        self._snapshots.load(self._camera_id)

    def stop(self) -> None:
        """Let a running snapshot request go (the dialog is closing)."""
        self._snapshots.stop()

    def _on_picture(self, picture: QPixmap) -> None:
        self.snapshot_button.setEnabled(bool(self._camera_id))
        set_status_tone(self.status, "muted")
        self.canvas.set_snapshot(picture)
        self.status.setText("")

    def _on_no_picture(self, text: str) -> None:
        self.snapshot_button.setEnabled(bool(self._camera_id))
        set_status_tone(self.status, "muted")
        self.canvas.set_placeholder("No camera picture")
        self.status.setText(text)

    def _refresh(self) -> None:
        zones = self.canvas.zones()
        selected = self.canvas.selected
        self.picker.blockSignals(True)
        self.picker.clear()
        for zone in zones:
            points = len(zone.get("points", []))
            kind = "Ignore" if zone.get("kind") == "exclude" else "Watch"
            text = str(zone.get("name") or zone["id"])
            item = QListWidgetItem(f"{text}\n{kind}" + ("" if points >= 3 else " · add points"))
            swatch = QPixmap(12, 12)
            swatch.fill(zone_color(zone))
            item.setIcon(QIcon(swatch))
            self.picker.addItem(item)
        if not zones:
            item = QListWidgetItem("Whole picture\nNo zones yet")
            item.setFlags(Qt.ItemFlag.NoItemFlags)
            self.picker.addItem(item)
        else:
            self.picker.setCurrentRow(selected)
        self.picker.blockSignals(False)
        self._select(selected)
        self.changed.emit()

    def _select(self, index: int) -> None:
        zones = self.canvas.zones()
        if not 0 <= index < len(zones):
            index = 0
        self.canvas.selected = index
        name = str(zones[index].get("name", "")) if zones else ""
        if self.name.text() != name:
            self.name.setText(name)
        self.name.setEnabled(bool(zones))
        self.remove_button.setEnabled(bool(zones))
        self.canvas.update()

    def _rename(self, name: str) -> None:
        zones = self.canvas.zones()
        if zones:
            zones[self.canvas.selected]["name"] = name
            self.canvas.set_zones(zones)
            self._refresh()

    def _remove(self) -> None:
        zones = self.canvas.zones()
        if zones:
            zones.pop(self.canvas.selected)
            self.canvas.set_zones(zones)
            self._refresh()
