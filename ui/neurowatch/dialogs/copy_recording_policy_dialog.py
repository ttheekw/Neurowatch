"""Review and atomically apply a frozen policy to selected cameras."""

from __future__ import annotations

from typing import Any

from PySide6.QtCore import QSize, Qt, Signal
from PySide6.QtWidgets import (
    QDialog,
    QHBoxLayout,
    QLabel,
    QListWidget,
    QListWidgetItem,
    QPushButton,
    QWidget,
)

from neurowatch.ipc.sync_client import SyncIpcClient
from neurowatch.models.recording_policy import normalized_policy, policy_problem
from neurowatch.theme import apply_dialog_theme, set_button_variant, set_label_role
from neurowatch.utils.threads import Worker, retire
from neurowatch.widgets.window_chrome import install_dialog_chrome, resize_window


class _PolicyWorker(Worker):
    done = Signal(object)

    def __init__(self, endpoint: str | None, method: str, params: dict | None = None) -> None:
        super().__init__()
        self.endpoint, self.method, self.params = endpoint, method, params

    def run(self) -> None:
        try:
            result = SyncIpcClient(self.endpoint).call(self.method, self.params)
        except Exception as error:
            result = error
        self.done.emit(result)


class CopyRecordingPolicyDialog(QDialog):
    copied = Signal(object, object)

    def __init__(
        self, endpoint: str | None, source_id: str, policy: dict, parent: QWidget | None = None
    ) -> None:
        super().__init__(parent)
        self.endpoint, self.source_id = endpoint, source_id
        self.policy = normalized_policy(policy)
        self._worker: _PolicyWorker | None = None
        self._busy = False
        self._applying = False
        self._applied = False
        apply_dialog_theme(self)
        self.setMinimumSize(480, 400)
        layout = install_dialog_chrome(self, "Copy recording policy", resizable=True)
        info = QLabel(
            "Copy the settings currently shown in the camera editor to selected cameras. "
            "This saves those cameras immediately, even if you later cancel the source editor. "
            "Storage paths, quotas, recording enable switches and motion settings "
            "stay as they are. "
            "Manual Start/Stop overrides remain active."
        )
        info.setWordWrap(True)
        layout.addWidget(info)
        p = self.policy
        self.summary = QLabel(
            f"Default: {p['mode']} · {len(p['weekly_rules'])} windows\n"
            f"Event padding: {p['event_pre_sec']} s before / {p['event_post_sec']} s after · "
            f"Non-event history: {p['non_event_keep_hours']} h (+ safety grace)"
        )
        self.summary.setWordWrap(True)
        set_label_role(self.summary, "hint")
        layout.addWidget(self.summary)
        self.targets = QListWidget()
        self.targets.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
        self.targets.setTextElideMode(Qt.TextElideMode.ElideRight)
        self.targets.setAccessibleName("Cameras receiving this policy")
        self.targets.itemChanged.connect(self._selection_changed)
        layout.addWidget(self.targets)
        row = QHBoxLayout()
        self.select_all = QPushButton("Select all")
        self.select_all.clicked.connect(lambda: self._select(True))
        self.clear = QPushButton("Clear")
        self.clear.clicked.connect(lambda: self._select(False))
        row.addWidget(self.select_all)
        row.addWidget(self.clear)
        row.addStretch()
        layout.addLayout(row)
        self.status = QLabel("Loading cameras…")
        self.status.setWordWrap(True)
        layout.addWidget(self.status)
        buttons = QHBoxLayout()
        self.cancel = QPushButton("Cancel")
        self.cancel.clicked.connect(self.reject)
        self.copy = QPushButton("Copy to selected cameras")
        set_button_variant(self.copy, "primary")
        self.copy.clicked.connect(self._copy)
        buttons.addStretch()
        buttons.addWidget(self.cancel)
        buttons.addWidget(self.copy)
        layout.addLayout(buttons)
        self.finished.connect(self._retire)
        resize_window(self, QSize(560, 540))
        self._start("recording.policy_targets")

    def _select(self, checked: bool) -> None:
        for index in range(self.targets.count()):
            self.targets.item(index).setCheckState(
                Qt.CheckState.Checked if checked else Qt.CheckState.Unchecked
            )

    def selected_ids(self) -> list[str]:
        return [
            self.targets.item(i).data(Qt.ItemDataRole.UserRole)
            for i in range(self.targets.count())
            if self.targets.item(i).checkState() == Qt.CheckState.Checked
        ]

    def _selection_changed(self) -> None:
        count = len(self.selected_ids())
        self.copy.setText(
            f"Copy to {count} camera{'s' if count != 1 else ''}"
            if count
            else "Copy to selected cameras"
        )
        self.copy.setEnabled(not self._busy and bool(count) and not self._applied)

    def _start(self, method: str, params: dict | None = None) -> None:
        self._busy = True
        self._selection_changed()
        retire(self._worker)
        worker = _PolicyWorker(self.endpoint, method, params)
        self._worker = worker
        worker.done.connect(self._done)
        worker.finished.connect(worker.deleteLater)
        worker.start()
        retire(worker)

    def _done(self, result: Any) -> None:
        self._busy = False
        applying, self._applying = self._applying, False
        self.cancel.setEnabled(True)
        if isinstance(result, Exception):
            self.status.setText(
                f"Could not {'copy policy' if applying else 'load cameras'}: {result}"
            )
        elif applying:
            self._applied = True
            self.copied.emit(result["copied"], self.policy)
            self.status.setText(f"Policy saved to {len(result['copied'])} cameras.")
            self.cancel.setText("Close")
        else:
            for camera in result:
                if camera["id"] == self.source_id:
                    continue
                details = []
                if not camera["recording_enabled"]:
                    details.append("recording disabled")
                if camera["manual_override"]:
                    details.append("manual override")
                name = camera["name"] + (f" ({', '.join(details)})" if details else "")
                item = QListWidgetItem(name)
                item.setToolTip(name)
                item.setData(Qt.ItemDataRole.UserRole, camera["id"])
                item.setFlags(item.flags() | Qt.ItemFlag.ItemIsUserCheckable)
                item.setCheckState(Qt.CheckState.Unchecked)
                self.targets.addItem(item)
            self.status.setText(
                "Select cameras to replace their recording policy."
                if self.targets.count()
                else "No other cameras are configured."
            )
        self.targets.setEnabled(not self._applied)
        self.select_all.setEnabled(not self._applied)
        self.clear.setEnabled(not self._applied)
        self._selection_changed()

    def _copy(self) -> None:
        if self._busy or self._applied or not self.selected_ids():
            return
        problem = policy_problem(self.policy)
        if problem:
            self.status.setText(problem)
            return
        params = {
            "source_id": self.source_id,
            "target_ids": self.selected_ids(),
            "policy": self.policy,
        }
        self._applying = True
        self.cancel.setEnabled(False)
        self.targets.setEnabled(False)
        self.select_all.setEnabled(False)
        self.clear.setEnabled(False)
        self.status.setText("Saving policy to selected cameras…")
        self._start("recording.copy_policy", params)

    def done(self, result: int) -> None:
        # Keep the result visible until an in-flight write has replied.
        if not self._applying:
            super().done(result)

    def _retire(self) -> None:
        if self._worker is not None:
            try:
                self._worker.done.disconnect(self._done)
            except (RuntimeError, TypeError):
                pass
            retire(self._worker)
            self._worker = None
