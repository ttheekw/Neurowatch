"""Add or edit a bookmark: a note on a moment (or a span) of a camera's
recordings, optionally keeping that footage from every cleanup.

The recorder stores it (``events.bookmark`` / ``events.update_bookmark``);
the call runs on a worker thread, and the dialog closes once the recorder
has saved it.
"""

from __future__ import annotations

import logging
from datetime import datetime
from typing import Any

from PySide6.QtCore import Qt, Signal
from PySide6.QtWidgets import (
    QCheckBox,
    QComboBox,
    QDialog,
    QDialogButtonBox,
    QFormLayout,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QSpinBox,
    QWidget,
)

from neurowatch.ipc.sync_client import SyncIpcClient
from neurowatch.models.events import Event
from neurowatch.theme import (
    apply_dialog_theme,
    set_label_role,
    set_status_tone,
    style_button_box,
)
from neurowatch.utils.threads import Worker
from neurowatch.widgets.form_parts import Segmented
from neurowatch.widgets.window_chrome import install_dialog_chrome

logger = logging.getLogger(__name__)

#: Longest span a bookmark covers (the recorder's limit is a day).
MAX_SPAN_MINUTES = 24 * 60
#: Cameras offered as "every camera": a bookmark of the whole recorder.
EVERY_CAMERA = ""
KEEP_HINT = (
    "Recordings from 30 s before to 30 s after are never deleted by quota, "
    "drive-full or event-retention cleanup until you unmark this bookmark. "
    "They still use disk space."
)


class _SaveWorker(Worker):
    done = Signal(object)  # the saved event dict, or Exception

    def __init__(self, endpoint: str | None, method: str, params: dict[str, Any]) -> None:
        super().__init__()
        self._endpoint, self._method, self._params = endpoint, method, params

    def run(self) -> None:
        try:
            self.done.emit(SyncIpcClient(endpoint=self._endpoint).call(self._method, self._params))
        except Exception as exc:  # shown in the dialog
            self.done.emit(exc)


def bookmark_params(
    camera_id: str | None, at_ms: int, span_minutes: int, note: str, kept: bool
) -> dict[str, Any]:
    """``events.bookmark`` parameters (a span of 0 minutes is one moment)."""
    params: dict[str, Any] = {"camera_id": camera_id or None, "time_ms": at_ms, "kept": kept}
    if span_minutes > 0:
        params["end_ms"] = at_ms + min(span_minutes, MAX_SPAN_MINUTES) * 60_000
    if note.strip():
        params["note"] = note.strip()
    return params


class BookmarkDialog(QDialog):
    """New bookmark at ``at_ms`` on one of ``cameras`` (id, name), or, with
    ``event``, that bookmark's note and kept footage."""

    saved = Signal(object)  # Event

    def __init__(
        self,
        endpoint: str | None,
        cameras: list[tuple[str, str]],
        at_ms: int,
        parent: QWidget | None = None,
        *,
        event: Event | None = None,
        allow_span: bool = True,
        allow_every_camera: bool = False,
    ) -> None:
        super().__init__(parent)
        self._endpoint = endpoint
        self._event = event
        self._at_ms = event.start_ms if event is not None else at_ms
        self._worker: _SaveWorker | None = None
        editing = event is not None
        self.setWindowTitle("Edit Bookmark" if editing else "Add Bookmark")
        self.setMinimumWidth(440)
        apply_dialog_theme(self)
        layout = install_dialog_chrome(self, "Edit bookmark" if editing else "Add bookmark")
        layout.setSpacing(12)

        when = QLabel(datetime.fromtimestamp(self._at_ms / 1000).strftime("%a %d %b %Y, %H:%M:%S"))
        set_label_role(when, "emphasis")
        form = QFormLayout()
        form.setHorizontalSpacing(14)
        form.setVerticalSpacing(10)
        form.addRow("Time", when)

        self._camera = QComboBox()
        self._camera.setAccessibleName("Bookmark camera")
        for camera_id, name in cameras:
            self._camera.addItem(name, camera_id)
        if allow_every_camera and len(cameras) > 1:
            self._camera.addItem("Every camera", EVERY_CAMERA)
        if event is not None:
            name = next((n for i, n in cameras if i == event.camera_id), "Every camera")
            camera_label = QLabel(name)
            form.addRow("Camera", camera_label)
        elif self._camera.count() > 1:
            form.addRow("Camera", self._camera)
        elif cameras:
            form.addRow("Camera", QLabel(cameras[0][1]))

        self._span = Segmented([("This moment", 0), ("A span", 1)])
        self._span.setAccessibleName("What the bookmark covers")
        self._minutes = QSpinBox()
        self._minutes.setRange(1, MAX_SPAN_MINUTES)
        self._minutes.setValue(5)
        self._minutes.setSuffix(" min")
        self._minutes.setAccessibleName("Span length in minutes")
        self._minutes.setEnabled(False)
        self._span.changed.connect(lambda value: self._minutes.setEnabled(value == 1))
        if allow_span and not editing:
            covers = QHBoxLayout()
            covers.setSpacing(8)
            covers.addWidget(self._span)
            covers.addWidget(self._minutes)
            covers.addStretch(1)
            form.addRow("Covers", covers)

        self._note = QLineEdit()
        self._note.setMaxLength(500)
        self._note.setPlaceholderText("What happened here (optional)")
        self._note.setAccessibleName("Bookmark note")
        if event is not None and event.detail:
            self._note.setText(event.detail)
        form.addRow("Note", self._note)
        layout.addLayout(form)

        self._keep = QCheckBox("Keep this footage")
        self._keep.setAccessibleName("Keep the footage this bookmark covers")
        self._keep.setChecked(bool(event and event.kept))
        layout.addWidget(self._keep)
        hint = QLabel(KEEP_HINT)
        hint.setWordWrap(True)
        set_label_role(hint, "hint")
        layout.addWidget(hint)

        self._error = QLabel()
        self._error.setWordWrap(True)
        set_status_tone(self._error, "error")
        self._error.hide()
        layout.addWidget(self._error)

        self._buttons = QDialogButtonBox(
            QDialogButtonBox.StandardButton.Save | QDialogButtonBox.StandardButton.Cancel
        )
        self._save = self._buttons.button(QDialogButtonBox.StandardButton.Save)
        self._save.setText("Save Bookmark" if not editing else "Save Changes")
        self._buttons.accepted.connect(self._submit)
        self._buttons.rejected.connect(self.reject)
        style_button_box(self._buttons)
        layout.addWidget(self._buttons)
        self._note.setFocus(Qt.FocusReason.OtherFocusReason)

    # ------------------------------------------------------------------

    def request(self) -> tuple[str, dict[str, Any]]:
        """The call that saves what the dialog shows."""
        note, kept = self._note.text(), self._keep.isChecked()
        if self._event is not None:
            return "events.update_bookmark", {
                "id": self._event.id,
                "note": note.strip() or None,
                "kept": kept,
            }
        span = self._minutes.value() if self._span.value() == 1 else 0
        camera = self._camera.currentData()
        return "events.bookmark", bookmark_params(
            camera if camera != EVERY_CAMERA else None, self._at_ms, span, note, kept
        )

    def _submit(self) -> None:
        if self._worker is not None:
            return
        method, params = self.request()
        self._save.setEnabled(False)
        self._error.hide()
        worker = _SaveWorker(self._endpoint, method, params)
        self._worker = worker
        worker.done.connect(self._finished)
        worker.finished.connect(worker.deleteLater)
        worker.start()

    def _finished(self, result: object) -> None:
        self._worker = None
        self._save.setEnabled(True)
        event = Event.from_dict(result) if isinstance(result, dict) else None
        if event is None:
            logger.warning("Bookmark not saved: %s", result)
            self._error.setText(f"The bookmark was not saved: {result}")
            self._error.show()
            return
        self.saved.emit(event)
        self.accept()

    def done(self, result: int) -> None:
        if self._worker is not None and self._worker.isRunning():
            self._worker.wait(3000)
        super().done(result)
