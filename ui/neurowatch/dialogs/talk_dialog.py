"""Talk through a camera's speaker (two-way audio): hold the button, or the
Space bar, while you speak. The recorder sends this computer's microphone
to the camera over its ONVIF audio backchannel (``talk.*``)."""

from __future__ import annotations

import uuid

from PySide6.QtCore import QEvent, Qt, Signal
from PySide6.QtGui import QCloseEvent, QKeyEvent
from PySide6.QtWidgets import QCheckBox, QDialog, QLabel, QPushButton, QWidget

from neurowatch.ipc.talk_worker import TalkWorker
from neurowatch.theme import apply_dialog_theme, set_button_variant, set_label_role, set_status_tone
from neurowatch.widgets.window_chrome import install_dialog_chrome


class TalkDialog(QDialog):
    closed = Signal()

    def __init__(
        self,
        endpoint: str | None,
        camera_id: str,
        camera_name: str,
        parent: QWidget | None = None,
        worker: TalkWorker | None = None,
    ) -> None:
        super().__init__(parent)
        self.setWindowTitle(f"Talk · {camera_name}")
        self.setMinimumWidth(380)
        apply_dialog_theme(self)
        layout = install_dialog_chrome(self, f"Talk through {camera_name}")
        layout.setSpacing(12)
        hint = QLabel(
            "Hold the button (or the Space bar) while you speak; release to stop. "
            "The camera plays your voice on its speaker."
        )
        hint.setWordWrap(True)
        set_label_role(hint, "hint")
        layout.addWidget(hint)
        self.button = QPushButton("Hold to Talk")
        self.button.setMinimumHeight(64)
        self.button.setAccessibleName("Hold to talk")
        set_button_variant(self.button, "primary")
        self.button.pressed.connect(self._press)
        self.button.released.connect(self._release)
        layout.addWidget(self.button)
        self.tone = QCheckBox("Send a test tone instead of the microphone")
        self.tone.setToolTip("Check the camera's speaker without speaking")
        layout.addWidget(self.tone)
        self.status = QLabel("Ready.")
        self.status.setWordWrap(True)
        layout.addWidget(self.status)
        self._worker = worker or TalkWorker(endpoint, camera_id, f"desktop-{uuid.uuid4().hex[:12]}")
        self._worker.state.connect(self._show)
        self._worker.start()

    def _press(self) -> None:
        self._worker.press("tone" if self.tone.isChecked() else "microphone")

    def _release(self) -> None:
        self._worker.release()

    def _show(self, state: str, detail: str) -> None:
        if state == "connecting":
            self.status.setText("Connecting to the camera's speaker…")
            set_status_tone(self.status, "info")
        elif state == "talking":
            self.status.setText(f"Talking ({detail}). Release to stop." if detail else "Talking…")
            set_status_tone(self.status, "success")
            self.button.setText("Talking… release to stop")
        elif state == "stopped":
            self.status.setText("Ready.")
            set_status_tone(self.status, "muted")
            self.button.setText("Hold to Talk")
        else:
            self.status.setText(f"Could not talk: {detail}")
            set_status_tone(self.status, "error")
            self.button.setText("Hold to Talk")

    def keyPressEvent(self, event: QKeyEvent) -> None:
        if event.key() == Qt.Key.Key_Space and not event.isAutoRepeat():
            self.button.setDown(True)
            self._press()
            return
        super().keyPressEvent(event)

    def keyReleaseEvent(self, event: QKeyEvent) -> None:
        if event.key() == Qt.Key.Key_Space and not event.isAutoRepeat():
            self.button.setDown(False)
            self._release()
            return
        super().keyReleaseEvent(event)

    def changeEvent(self, event: QEvent) -> None:
        # Leaving the window while held must not leave the microphone open.
        if event.type() == QEvent.Type.ActivationChange and not self.isActiveWindow():
            self.button.setDown(False)
            self._release()
        super().changeEvent(event)

    def closeEvent(self, event: QCloseEvent) -> None:
        self._worker.shutdown()
        self._worker.wait(5000)
        self.closed.emit()
        super().closeEvent(event)
