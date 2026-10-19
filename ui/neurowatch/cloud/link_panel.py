"""Settings → Cloud: link this recorder to NeuroWatch Cloud, or unlink it.

Sends the one-time enrollment code from the web portal to ``cloud.begin``,
which links to NeuroWatch Cloud in one step. A link
whose claim was accepted but never finished is completed with
``cloud.confirm``. All RPCs run on a background ``QThread`` because linking
waits on the network. Linking never affects local capture or recording.
"""

from __future__ import annotations

import logging
from collections.abc import Callable
from typing import Any

from PySide6.QtCore import QThread, Signal
from PySide6.QtWidgets import (
    QFormLayout,
    QFrame,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QPushButton,
    QVBoxLayout,
    QWidget,
)

from neurowatch.cloud.cloud_link import (
    CloudLinkStatus,
    begin_params,
    operator_error,
    parse_enrollment_code,
)
from neurowatch.dialogs.message import ask
from neurowatch.ipc.sync_client import SyncIpcClient
from neurowatch.theme import set_button_variant, set_label_role, set_status_tone
from neurowatch.utils.threads import Worker

logger = logging.getLogger(__name__)

CloudRpc = Callable[[str, dict[str, Any], float], Any]

_STATUS_TIMEOUT_S = 5.0
_BEGIN_TIMEOUT_S = 45.0  # backend allows 10 s connect + 20 s request
_LINK_TIMEOUT_S = 2 * _BEGIN_TIMEOUT_S  # cloud.begin also confirms the claim
_SUCCESS = "Success — linked to NeuroWatch Cloud."

# Workers outlive a closed Settings dialog (a link may still be waiting on
# the network); keep them referenced until they finish.
_LIVE_WORKERS: set[QThread] = set()


class _CloudRpcWorker(Worker):
    result = Signal(str, object)  # method, RPC result or Exception

    def __init__(self, rpc: CloudRpc, method: str, params: dict[str, Any], timeout: float) -> None:
        super().__init__()
        self._rpc = rpc
        self._method = method
        self._params = params
        self._timeout = timeout

    def run(self) -> None:
        try:
            self.result.emit(self._method, self._rpc(self._method, self._params, self._timeout))
        except Exception as exc:  # reported in the panel, never raised in Qt
            self.result.emit(self._method, exc)


def _client_rpc(endpoint: str | None) -> CloudRpc:
    def call(method: str, params: dict[str, Any], timeout: float) -> Any:
        return SyncIpcClient(endpoint=endpoint).call(method, params, timeout=timeout)

    return call


class CloudLinkPanel(QWidget):
    """The link status, the enrollment code and recorder name, and Link /
    Unlink. Reads ``cloud.status`` when created."""

    def __init__(
        self,
        parent: QWidget | None = None,
        *,
        endpoint: str | None = None,
        rpc: CloudRpc | None = None,
    ) -> None:
        super().__init__(parent)
        self._rpc: CloudRpc = rpc or _client_rpc(endpoint)
        self._status = CloudLinkStatus(phase="unknown")
        self._busy = False

        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(10)

        intro = QLabel(
            "Watch your cameras and review recordings from anywhere. Linking is "
            "optional: local viewing, recording and playback keep working without it, "
            "and unlinking only turns cloud services off."
        )
        intro.setObjectName("dialogSubtitle")
        intro.setWordWrap(True)
        layout.addWidget(intro)

        # Where the link stands now.
        status_card = QFrame()
        status_card.setObjectName("settingsCard")
        status_layout = QVBoxLayout(status_card)
        status_layout.setContentsMargins(14, 12, 14, 12)
        status_layout.setSpacing(4)
        self.status_label = QLabel("Checking link status…")
        self.status_label.setWordWrap(True)
        set_label_role(self.status_label, "emphasis")
        status_layout.addWidget(self.status_label)
        self.cloud_label = QLabel("")
        self.cloud_label.setWordWrap(True)
        set_label_role(self.cloud_label, "hint")
        status_layout.addWidget(self.cloud_label)
        layout.addWidget(status_card)

        # The code and the recorder's name.
        form = QFormLayout()
        form.setContentsMargins(0, 6, 0, 0)
        form.setHorizontalSpacing(14)
        form.setVerticalSpacing(8)
        form.setRowWrapPolicy(QFormLayout.RowWrapPolicy.WrapAllRows)
        self.code_edit = QLineEdit()
        self.code_edit.setPlaceholderText("Paste the one-time code")
        self.code_edit.setAccessibleName("Enrollment code")
        form.addRow("Enrollment code", self.code_edit)
        code_hint = QLabel(
            "In the web portal: Recorders → Create enrollment code. "
            "It is shown once and expires after a few minutes."
        )
        code_hint.setWordWrap(True)
        set_label_role(code_hint, "hint")
        form.addRow(code_hint)
        self.name_edit = QLineEdit()
        self.name_edit.setPlaceholderText("e.g. Front office")
        self.name_edit.setMaxLength(64)
        self.name_edit.setAccessibleName("Recorder name")
        form.addRow("Recorder name", self.name_edit)
        layout.addLayout(form)

        self.error_label = QLabel("")
        self.error_label.setWordWrap(True)
        set_status_tone(self.error_label, "error")
        layout.addWidget(self.error_label)

        buttons = QHBoxLayout()
        buttons.setSpacing(8)
        self.unlink_button = QPushButton("Unlink")
        self.unlink_button.setToolTip("Stop cloud services for this computer")
        set_button_variant(self.unlink_button, "danger")
        buttons.addWidget(self.unlink_button)
        buttons.addStretch(1)
        self.link_button = QPushButton("Link")
        set_button_variant(self.link_button, "primary")
        buttons.addWidget(self.link_button)
        layout.addLayout(buttons)
        layout.addStretch(1)

        self.link_button.clicked.connect(self._on_link)
        self.unlink_button.clicked.connect(self._on_unlink)

        self._update_controls()
        self._run("cloud.status", {}, _STATUS_TIMEOUT_S)

    # -- actions ----------------------------------------------------------

    def _on_link(self) -> None:
        if self._status.can_confirm:
            # The claim was accepted earlier; finishing needs no new code.
            self._run("cloud.confirm", {}, _BEGIN_TIMEOUT_S)
            return
        try:
            params = begin_params(
                parse_enrollment_code(self.code_edit.text()), self.name_edit.text()
            )
        except ValueError as exc:
            self.error_label.setText(str(exc))
            return
        self.status_label.setText("Linking…")
        self._run("cloud.begin", params, _LINK_TIMEOUT_S)

    def _on_unlink(self) -> None:
        if ask(
            self,
            "Unlink from NeuroWatch Cloud",
            "Stop cloud services for this computer? Local cameras, recordings and "
            "playback are not affected.",
            yes_text="Unlink",
            no_text="Cancel",
            destructive=True,
        ):
            self._run("cloud.unlink", {}, _STATUS_TIMEOUT_S)

    # -- background RPC ---------------------------------------------------

    def _run(self, method: str, params: dict[str, Any], timeout: float) -> None:
        if self._busy:
            return
        self._busy = True
        self.error_label.setText("")
        self._update_controls()
        worker = _CloudRpcWorker(self._rpc, method, params, timeout)
        worker.result.connect(self._on_result)
        worker.finished.connect(lambda: _LIVE_WORKERS.discard(worker))
        worker.finished.connect(worker.deleteLater)
        _LIVE_WORKERS.add(worker)
        worker.start()

    def _on_result(self, method: str, result: object) -> None:
        self._busy = False
        if isinstance(result, Exception):
            logger.warning("Cloud link %s failed: %s", method, result)
            self.error_label.setText(operator_error(f"{method} failed: {result}"))
            self.status_label.setText(self._status.summary)
            self._update_controls()
            return
        if method == "cloud.unlink":
            self._run("cloud.status", {}, _STATUS_TIMEOUT_S)
            return
        if isinstance(result, dict):
            self._status = CloudLinkStatus.from_result(result)
            if method == "cloud.begin":
                self.code_edit.clear()  # the transfer proof is single-use
        linked_now = method in ("cloud.begin", "cloud.confirm") and self._status.phase == "linked"
        self.status_label.setText(_SUCCESS if linked_now else self._status.summary)
        self.cloud_label.setText(self._status.cloud_description)
        self._update_controls()

    def _update_controls(self) -> None:
        idle = not self._busy
        self.link_button.setEnabled(idle and (self._status.can_begin or self._status.can_confirm))
        self.code_edit.setEnabled(idle and self._status.can_begin)
        self.name_edit.setEnabled(idle and self._status.can_begin)
        self.unlink_button.setEnabled(idle and self._status.can_unlink)
