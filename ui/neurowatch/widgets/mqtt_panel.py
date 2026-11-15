"""Settings → Home Assistant: send camera events to an MQTT broker, with
Home Assistant discovery.

The panel reads ``mqtt.get`` when created, saves with ``mqtt.set`` (the
recorder reconnects at once) and checks a broker with ``mqtt.test``; every
call runs on a background worker. While shown it reads ``mqtt.status`` every
few seconds, so the connection state follows a save without a reopen.
"""

from __future__ import annotations

import logging
from collections.abc import Callable
from typing import Any

from PySide6.QtCore import QThread, QTimer, Signal
from PySide6.QtWidgets import (
    QCheckBox,
    QFormLayout,
    QFrame,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QPushButton,
    QSpinBox,
    QVBoxLayout,
    QWidget,
)

from neurowatch.ipc.sync_client import SyncIpcClient
from neurowatch.models.mqtt import (
    DEFAULT_PORT,
    DEFAULT_TLS_PORT,
    KEPT_SECRET,
    MqttSettings,
    MqttStatus,
    topics_help,
)
from neurowatch.theme import set_button_variant, set_label_role, set_status_tone
from neurowatch.utils.threads import Worker

logger = logging.getLogger(__name__)

MqttRpc = Callable[[str, dict[str, Any], float], Any]

_TIMEOUT_S = 5.0
_TEST_TIMEOUT_S = 15.0  # the recorder waits up to 8 s for the broker
_STATUS_EVERY_MS = 3000

# Workers outlive a closed Settings dialog; keep them referenced until done.
_LIVE_WORKERS: set[QThread] = set()


class _RpcWorker(Worker):
    result = Signal(str, object)  # method, RPC result or Exception

    def __init__(self, rpc: MqttRpc, method: str, params: dict[str, Any], timeout: float) -> None:
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


def _client_rpc(endpoint: str | None) -> MqttRpc:
    def call(method: str, params: dict[str, Any], timeout: float) -> Any:
        return SyncIpcClient(endpoint=endpoint).call(method, params, timeout=timeout)

    return call


class MqttPanel(QWidget):
    """The connection state, the broker form, Test Connection and Save."""

    def __init__(
        self,
        parent: QWidget | None = None,
        *,
        endpoint: str | None = None,
        rpc: MqttRpc | None = None,
    ) -> None:
        super().__init__(parent)
        self._rpc: MqttRpc = rpc or _client_rpc(endpoint)
        self._busy = False
        self._loaded = False

        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(10)

        intro = QLabel(
            "Send camera events to Home Assistant, or to anything else that speaks MQTT. "
            "With Home Assistant each camera appears by itself, with motion, people, "
            "vehicles and animals, whether it is online, and its latest event picture."
        )
        intro.setObjectName("dialogSubtitle")
        intro.setWordWrap(True)
        layout.addWidget(intro)

        status_card = QFrame()
        status_card.setObjectName("settingsCard")
        status_layout = QVBoxLayout(status_card)
        status_layout.setContentsMargins(14, 12, 14, 12)
        self.status_label = QLabel("Reading the MQTT settings…")
        self.status_label.setWordWrap(True)
        set_status_tone(self.status_label, "muted")
        status_layout.addWidget(self.status_label)
        layout.addWidget(status_card)

        card = QFrame()
        card.setObjectName("settingsCard")
        form = QFormLayout(card)
        form.setContentsMargins(14, 12, 14, 12)
        form.setSpacing(8)
        self.enabled_box = QCheckBox("Send events to an MQTT broker")
        form.addRow(self.enabled_box)
        self.host_edit = QLineEdit()
        self.host_edit.setPlaceholderText("homeassistant.local or 192.168.1.10")
        form.addRow("Broker:", self.host_edit)
        self.port_box = QSpinBox()
        self.port_box.setRange(1, 65535)
        self.port_box.setValue(DEFAULT_PORT)
        form.addRow("Port:", self.port_box)
        self.tls_box = QCheckBox("Encrypted connection (TLS)")
        self.tls_box.toggled.connect(self._on_tls_toggled)
        form.addRow(self.tls_box)
        self.username_edit = QLineEdit()
        self.username_edit.setPlaceholderText("Optional")
        form.addRow("User name:", self.username_edit)
        self.password_edit = QLineEdit()
        self.password_edit.setEchoMode(QLineEdit.EchoMode.Password)
        self.password_edit.setPlaceholderText("Optional")
        form.addRow("Password:", self.password_edit)
        self.prefix_edit = QLineEdit()
        self.prefix_edit.setToolTip(
            "Every topic starts with this. Give each recorder on the same broker its own."
        )
        self.prefix_edit.textChanged.connect(self._update_help)
        form.addRow("Topic prefix:", self.prefix_edit)
        self.discovery_box = QCheckBox(
            "Add cameras to Home Assistant automatically (MQTT discovery)"
        )
        form.addRow(self.discovery_box)
        self.discovery_edit = QLineEdit()
        self.discovery_edit.setToolTip(
            "Home Assistant's discovery prefix; homeassistant unless you changed it."
        )
        form.addRow("Discovery prefix:", self.discovery_edit)
        layout.addWidget(card)

        self.help_label = QLabel()
        self.help_label.setWordWrap(True)
        set_label_role(self.help_label, "hint")
        layout.addWidget(self.help_label)

        self.error_label = QLabel()
        self.error_label.setWordWrap(True)
        set_status_tone(self.error_label, "error")
        layout.addWidget(self.error_label)

        buttons = QHBoxLayout()
        buttons.addStretch(1)
        self.test_button = QPushButton("Test Connection")
        set_button_variant(self.test_button, "secondary")
        self.test_button.clicked.connect(self._on_test)
        buttons.addWidget(self.test_button)
        self.save_button = QPushButton("Save")
        set_button_variant(self.save_button, "primary")
        self.save_button.clicked.connect(self._on_save)
        buttons.addWidget(self.save_button)
        layout.addLayout(buttons)

        self.enabled_box.toggled.connect(self._update_controls)
        self.host_edit.textChanged.connect(self._update_controls)
        self.discovery_box.toggled.connect(self._update_controls)
        self._timer = QTimer(self)
        self._timer.setInterval(_STATUS_EVERY_MS)
        self._timer.timeout.connect(self._refresh_status)
        self._update_help()
        self._update_controls()
        self._run("mqtt.get", {}, _TIMEOUT_S)

    # -- the form ---------------------------------------------------------

    def settings(self) -> MqttSettings:
        return MqttSettings(
            enabled=self.enabled_box.isChecked(),
            host=self.host_edit.text().strip(),
            port=self.port_box.value(),
            tls=self.tls_box.isChecked(),
            username=self.username_edit.text(),
            password=self.password_edit.text(),
            topic_prefix=self.prefix_edit.text().strip(),
            discovery=self.discovery_box.isChecked(),
            discovery_prefix=self.discovery_edit.text().strip(),
        )

    def _show(self, settings: MqttSettings) -> None:
        self.enabled_box.setChecked(settings.enabled)
        self.host_edit.setText(settings.host)
        self.tls_box.blockSignals(True)
        self.tls_box.setChecked(settings.tls)
        self.tls_box.blockSignals(False)
        self.port_box.setValue(settings.port)
        self.username_edit.setText(settings.username)
        self.password_edit.setText(settings.password)
        self.password_edit.setPlaceholderText(
            "Saved (type to change)" if settings.password == KEPT_SECRET else "Optional"
        )
        self.prefix_edit.setText(settings.topic_prefix)
        self.discovery_box.setChecked(settings.discovery)
        self.discovery_edit.setText(settings.discovery_prefix)

    def _on_tls_toggled(self, tls: bool) -> None:
        # Follow the usual port unless one of the user's own was entered.
        if tls and self.port_box.value() == DEFAULT_PORT:
            self.port_box.setValue(DEFAULT_TLS_PORT)
        elif not tls and self.port_box.value() == DEFAULT_TLS_PORT:
            self.port_box.setValue(DEFAULT_PORT)

    def _update_help(self) -> None:
        self.help_label.setText("Topics: " + topics_help(self.prefix_edit.text().strip()))

    def _update_controls(self) -> None:
        idle = self._loaded and not self._busy
        for widget in (
            self.enabled_box,
            self.host_edit,
            self.port_box,
            self.tls_box,
            self.username_edit,
            self.password_edit,
            self.prefix_edit,
            self.discovery_box,
        ):
            widget.setEnabled(idle)
        self.discovery_edit.setEnabled(idle and self.discovery_box.isChecked())
        self.save_button.setEnabled(idle)
        self.test_button.setEnabled(idle and bool(self.host_edit.text().strip()))

    def _show_status(self, status: MqttStatus) -> None:
        self.status_label.setText(status.text)
        set_status_tone(self.status_label, status.tone)

    # -- actions ------------------------------------------------------------

    def _on_save(self) -> None:
        settings = self.settings()
        problem = settings.problem()
        if problem:
            self.error_label.setText(problem)
            return
        self._run("mqtt.set", settings.to_json(), _TIMEOUT_S)

    def _on_test(self) -> None:
        settings = self.settings()
        problem = settings.problem() if settings.host else "Enter the broker's address."
        if problem:
            self.error_label.setText(problem)
            return
        self.status_label.setText(f"Testing {settings.host}:{settings.port}…")
        set_status_tone(self.status_label, "info")
        self._run("mqtt.test", settings.to_json(), _TEST_TIMEOUT_S)

    def _refresh_status(self) -> None:
        if not self._busy and self.isVisible():
            self._run("mqtt.status", {}, _TIMEOUT_S, quiet=True)

    # -- background RPC -------------------------------------------------------

    def _run(
        self, method: str, params: dict[str, Any], timeout: float, *, quiet: bool = False
    ) -> None:
        if self._busy:
            return
        self._busy = True
        if not quiet:
            self.error_label.setText("")
            self._update_controls()
        worker = _RpcWorker(self._rpc, method, params, timeout)
        worker.result.connect(self._on_result)
        worker.finished.connect(lambda: _LIVE_WORKERS.discard(worker))
        worker.finished.connect(worker.deleteLater)
        _LIVE_WORKERS.add(worker)
        worker.start()

    def _on_result(self, method: str, result: object) -> None:
        self._busy = False
        if isinstance(result, Exception):
            logger.warning("%s failed: %s", method, result)
            if method != "mqtt.status":
                self.error_label.setText(str(result))
            if method == "mqtt.get":
                self.status_label.setText("Could not read the MQTT settings from the recorder.")
                set_status_tone(self.status_label, "error")
        elif method in ("mqtt.get", "mqtt.set") and isinstance(result, dict):
            self._loaded = True
            self._show(MqttSettings.from_json(result.get("config")))
            self._show_status(MqttStatus.from_json(result.get("status")))
            if method == "mqtt.set":
                self.error_label.setText("")
            self._timer.start()
        elif method == "mqtt.status":
            self._show_status(MqttStatus.from_json(result))
        elif method == "mqtt.test" and isinstance(result, dict):
            if result.get("ok"):
                self.status_label.setText(
                    "The broker accepted the connection. Save to start sending."
                )
                set_status_tone(self.status_label, "success")
            else:
                self.status_label.setText(
                    f"Test failed: {result.get('error') or 'unknown problem'}."
                )
                set_status_tone(self.status_label, "error")
        self._update_controls()
