"""Modeless ONVIF controls: leased hold movement and camera-stored presets."""

from __future__ import annotations

import uuid
from typing import Any

from PySide6.QtCore import QEvent, QObject, Qt, Signal
from PySide6.QtGui import QCloseEvent, QHideEvent, QKeyEvent
from PySide6.QtWidgets import (
    QComboBox,
    QDialog,
    QGridLayout,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QPushButton,
    QSpinBox,
    QWidget,
)

from neurowatch.dialogs.message import ask
from neurowatch.ipc.ptz_worker import PtzWorker
from neurowatch.theme import apply_dialog_theme, set_button_variant, set_label_role
from neurowatch.utils.threads import retire
from neurowatch.widgets.window_chrome import install_dialog_chrome


class PtzDialog(QDialog):
    closed = Signal()

    def __init__(
        self, endpoint: str | None, camera_id: str, camera_name: str, parent: QWidget | None = None
    ) -> None:
        super().__init__(parent)
        self.setAttribute(Qt.WidgetAttribute.WA_DeleteOnClose)
        self.setMinimumWidth(350)
        self._caps: dict[str, Any] = {}
        self._keys: set[int] = set()
        self._stopped = False
        self._holding = False
        self._busy = False
        self._last_error = ""
        apply_dialog_theme(self)
        layout = install_dialog_chrome(self, f"PTZ · {camera_name}")
        hint = QLabel(
            "Hold a direction or zoom button to move. Arrow keys and + / - work here. "
            "Release or leave this window to stop."
        )
        hint.setWordWrap(True)
        set_label_role(hint, "hint")
        layout.addWidget(hint)
        controls = QHBoxLayout()
        self.speed = QSpinBox()
        self.speed.setRange(10, 100)
        self.speed.setSingleStep(10)
        self.speed.setValue(50)
        self.speed.setSuffix(" %")
        self.speed.setAccessibleName("PTZ speed")
        controls.addWidget(QLabel("Speed:"))
        controls.addWidget(self.speed)
        self.refresh = QPushButton("Detect / refresh")
        self.refresh.clicked.connect(self._load)
        controls.addStretch()
        controls.addWidget(self.refresh)
        layout.addLayout(controls)
        pad = QGridLayout()
        self.directions: list[QPushButton] = []
        for row, col, text, pan, tilt in (
            (0, 0, "↖", -1, 1),
            (0, 1, "↑", 0, 1),
            (0, 2, "↗", 1, 1),
            (1, 0, "←", -1, 0),
            (1, 2, "→", 1, 0),
            (2, 0, "↙", -1, -1),
            (2, 1, "↓", 0, -1),
            (2, 2, "↘", 1, -1),
        ):
            button = QPushButton(text)
            button.setMinimumSize(58, 38)
            button.setAccessibleName(f"PTZ pan {pan}, tilt {tilt}")
            button.pressed.connect(lambda x=pan, y=tilt: self._move(x, y, 0))
            button.released.connect(self._halt)
            pad.addWidget(button, row, col)
            self.directions.append(button)
        self.stop = QPushButton("Stop")
        set_button_variant(self.stop, "danger")
        self.stop.clicked.connect(self._emergency_stop)
        pad.addWidget(self.stop, 1, 1)
        self.zoom_buttons: list[QPushButton] = []
        for row, text, zoom in ((0, "Zoom +", 1), (2, "Zoom -", -1)):
            button = QPushButton(text)
            button.pressed.connect(lambda z=zoom: self._move(0, 0, z))
            button.released.connect(self._halt)
            pad.addWidget(button, row, 3)
            self.zoom_buttons.append(button)
        self.home = QPushButton("Home")
        self.home.clicked.connect(lambda: self._action("ptz.home"))
        pad.addWidget(self.home, 1, 3)
        layout.addLayout(pad)
        for button in [*self.directions, self.stop, *self.zoom_buttons, self.home]:
            button.installEventFilter(self)
        self.presets = QComboBox()
        self.presets.setAccessibleName("Camera presets")
        self.presets.currentIndexChanged.connect(self._preset_selected)
        layout.addWidget(self.presets)
        preset_row = QHBoxLayout()
        self.go = QPushButton("Go to preset")
        self.go.clicked.connect(
            lambda: self._action("ptz.goto_preset", {"token": self.presets.currentData()})
        )
        self.delete = QPushButton("Delete preset")
        set_button_variant(self.delete, "danger")
        self.delete.clicked.connect(self._delete)
        preset_row.addWidget(self.go)
        preset_row.addWidget(self.delete)
        layout.addLayout(preset_row)
        self.name = QLineEdit()
        self.name.setMaxLength(64)
        self.name.setPlaceholderText("Preset name")
        self.name.setAccessibleName("Preset name")
        layout.addWidget(self.name)
        save_row = QHBoxLayout()
        self.save_new = QPushButton("Save new preset")
        self.save_new.clicked.connect(
            lambda: self._action("ptz.save_preset", {"name": self.name.text()})
        )
        self.update_preset = QPushButton("Update selected")
        self.update_preset.clicked.connect(
            lambda: self._action(
                "ptz.save_preset", {"name": self.name.text(), "token": self.presets.currentData()}
            )
        )
        save_row.addWidget(self.save_new)
        save_row.addWidget(self.update_preset)
        layout.addLayout(save_row)
        self.status = QLabel("Detecting camera controls…")
        self.status.setWordWrap(True)
        set_label_role(self.status, "hint")
        layout.addWidget(self.status)
        for button in self.findChildren(QPushButton):
            button.setAutoDefault(False)
        self._worker = PtzWorker(endpoint, camera_id, str(uuid.uuid4()))
        self._worker.result.connect(self._result)
        self._worker.finished.connect(self._worker.deleteLater)
        self._worker.command("ptz.capabilities")
        self.finished.connect(self._stop_worker)
        self._worker.start()
        retire(self._worker)
        self._controls()

    def _load(self) -> None:
        self._halt()
        self._worker.command("ptz.capabilities", {"refresh": True})
        self._last_error = ""
        self.status.setText("Detecting camera controls…")

    def _controls(self) -> None:
        for button in self.directions:
            button.setEnabled(bool(self._caps.get("pan_tilt")) and not self._busy)
        for button in self.zoom_buttons:
            button.setEnabled(bool(self._caps.get("zoom")) and not self._busy)
        self.home.setEnabled(bool(self._caps.get("home")) and not self._busy)
        has_preset = self.presets.currentData() is not None
        for button in (self.go, self.delete, self.update_preset):
            button.setEnabled(has_preset and not self._busy)
        self.save_new.setEnabled(bool(self._caps.get("presets")) and not self._busy)

    def _preset_selected(self) -> None:
        self.name.setText(
            self.presets.currentText() if self.presets.currentData() is not None else ""
        )
        self._controls()

    def _move(self, pan: float, tilt: float, zoom: float) -> None:
        if self._busy or not (
            ((pan or tilt) and self._caps.get("pan_tilt")) or (zoom and self._caps.get("zoom"))
        ):
            return
        self._holding = True
        speed = self.speed.value() / 100
        self._worker.move(pan * speed, tilt * speed, zoom * speed)
        self._last_error = ""
        self.status.setText("Moving · release to stop")

    def _emergency_stop(self) -> None:
        self._halt()
        self._last_error = ""
        if not self._stopped:
            self._worker.halt(force=True)

    def _halt(self) -> None:
        self._keys.clear()
        self._holding = False
        if self._stopped:
            return
        self._busy = False
        self._controls()
        self._worker.halt()

    def _action(self, method: str, params: dict | None = None) -> None:
        if self._busy:
            return
        if method == "ptz.save_preset" and not self.name.text().strip():
            self.status.setText("Enter a preset name first.")
            return
        self._halt()
        self._last_error = ""
        self._busy = self._worker.command(method, params)
        self.status.setText("Waiting for the camera…")
        self._controls()

    def _delete(self) -> None:
        if self.presets.currentData() is not None and ask(
            self,
            "Delete preset",
            f"Delete '{self.presets.currentText()}' from this camera?",
            yes_text="Delete",
            no_text="Cancel",
            destructive=True,
        ):
            self._action("ptz.delete_preset", {"token": self.presets.currentData()})

    def _result(self, method: str, value: Any) -> None:
        if self._stopped:
            return
        if method != "ptz.move" and method != "ptz.stop":
            self._busy = False
        if isinstance(value, Exception):
            self._last_error = str(value)
            self.status.setText(self._last_error)
            if method == "ptz.capabilities":
                self._caps.clear()
        elif method == "ptz.capabilities":
            self._caps = value
            self.status.setText("Hold arrows to move. Presets are saved on the camera.")
            if value.get("presets"):
                self._worker.command("ptz.presets")
        elif method == "ptz.presets":
            selected = self.presets.currentData()
            self.presets.blockSignals(True)
            self.presets.clear()
            for preset in value:
                self.presets.addItem(preset["name"], preset["token"])
            index = self.presets.findData(selected)
            if index >= 0:
                self.presets.setCurrentIndex(index)
            self.presets.blockSignals(False)
            self._preset_selected()
        elif method in ("ptz.save_preset", "ptz.delete_preset"):
            self.status.setText(
                "Preset saved." if method == "ptz.save_preset" else "Preset deleted."
            )
            self._worker.command("ptz.presets")
        elif method == "ptz.stop":
            if not self._busy and not self._last_error:
                self.status.setText("Stopped")
        elif method != "ptz.move":
            self.status.setText("Camera command accepted.")
        self._controls()

    def eventFilter(self, watched: QObject, event: QEvent) -> bool:  # noqa: N802
        if self._stopped:
            return super().eventFilter(watched, event)
        if event.type() == QEvent.Type.FocusOut and (self._holding or self._keys):
            self._halt()
        if isinstance(event, QKeyEvent) and event.key() in (
            Qt.Key.Key_Up,
            Qt.Key.Key_Down,
            Qt.Key.Key_Left,
            Qt.Key.Key_Right,
            Qt.Key.Key_Plus,
            Qt.Key.Key_Minus,
            Qt.Key.Key_Equal,
            Qt.Key.Key_Space,
        ):
            if event.type() == QEvent.Type.KeyPress:
                self.keyPressEvent(event)
                return True
            if event.type() == QEvent.Type.KeyRelease:
                self.keyReleaseEvent(event)
                return True
        return super().eventFilter(watched, event)

    def event(self, event: QEvent) -> bool:
        if event.type() == QEvent.Type.WindowDeactivate and hasattr(self, "_worker"):
            self._halt()
        return super().event(event)

    def hideEvent(self, event: QHideEvent) -> None:
        self._halt()
        super().hideEvent(event)

    def keyPressEvent(self, event: QKeyEvent) -> None:
        if event.key() in (
            Qt.Key.Key_Up,
            Qt.Key.Key_Down,
            Qt.Key.Key_Left,
            Qt.Key.Key_Right,
            Qt.Key.Key_Plus,
            Qt.Key.Key_Minus,
            Qt.Key.Key_Equal,
        ):
            if not event.isAutoRepeat() and not isinstance(
                self.focusWidget(), (QLineEdit, QComboBox, QSpinBox)
            ):
                self._keys.add(event.key())
                self._key_move()
            event.accept()
        elif event.key() == Qt.Key.Key_Space:
            self._emergency_stop()
        else:
            super().keyPressEvent(event)

    def keyReleaseEvent(self, event: QKeyEvent) -> None:
        if event.key() in self._keys and not event.isAutoRepeat():
            self._keys.discard(event.key())
            self._key_move()
        else:
            super().keyReleaseEvent(event)

    def _key_move(self) -> None:
        pan = int(Qt.Key.Key_Right in self._keys) - int(Qt.Key.Key_Left in self._keys)
        tilt = int(Qt.Key.Key_Up in self._keys) - int(Qt.Key.Key_Down in self._keys)
        zoom = int(bool(self._keys & {Qt.Key.Key_Plus, Qt.Key.Key_Equal})) - int(
            Qt.Key.Key_Minus in self._keys
        )
        if pan or tilt or zoom:
            self._move(pan, tilt, zoom)
        else:
            self._halt()

    def _stop_worker(self) -> None:
        if not self._stopped:
            self._stopped = True
            self._worker.result.disconnect(self._result)
            self._worker.shutdown()
            retire(self._worker)
            self.closed.emit()

    def closeEvent(self, event: QCloseEvent) -> None:
        self._stop_worker()
        super().closeEvent(event)
