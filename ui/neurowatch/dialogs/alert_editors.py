"""Editors for one alert channel and one alert rule (see ``alerts_dialog``)."""

from __future__ import annotations

import copy
from typing import Any

from PySide6.QtCore import QTime, Signal
from PySide6.QtWidgets import (
    QButtonGroup,
    QCheckBox,
    QComboBox,
    QDialog,
    QDialogButtonBox,
    QFormLayout,
    QGridLayout,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QPushButton,
    QRadioButton,
    QScrollArea,
    QTimeEdit,
    QVBoxLayout,
    QWidget,
)

from neurowatch.ipc.sync_client import SyncIpcClient
from neurowatch.models.alerts import (
    CHANNEL_KINDS,
    DAY_NAMES,
    DEFAULT_NTFY_SERVER,
    KEPT_SECRET,
    KIND_CHOICES,
    SECRETS,
    Channel,
    Rule,
    Window,
    channel_problem,
    rule_problem,
)
from neurowatch.theme import (
    apply_dialog_theme,
    set_button_variant,
    set_label_role,
    set_status_tone,
    style_button_box,
)
from neurowatch.utils.threads import Worker
from neurowatch.widgets.window_chrome import install_dialog_chrome

QUIET_CHOICES: tuple[tuple[str, int], ...] = (
    ("Every event", 0),
    ("30 seconds", 30),
    ("1 minute", 60),
    ("5 minutes", 300),
    ("15 minutes", 900),
    ("1 hour", 3600),
)

#: (field, label, placeholder, help) per channel kind.
CHANNEL_FIELDS: dict[str, tuple[tuple[str, str, str], ...]] = {
    "webhook": (
        ("url", "Address", "https://example.com/hooks/neurowatch"),
        ("secret", "Secret (optional)", "Sent as the X-NeuroWatch-Token header"),
    ),
    "ntfy": (
        ("server", "Server", DEFAULT_NTFY_SERVER),
        ("topic", "Topic", "a hard-to-guess name, e.g. home-cams-7f3k"),
        ("token", "Access token (optional)", "for a protected topic"),
    ),
    "telegram": (
        ("bot_token", "Bot token", "123456789:AA… from @BotFather"),
        ("chat_id", "Chat id", "e.g. -1001234567890 or @yourchannel"),
    ),
}
CHANNEL_HELP: dict[str, str] = {
    "desktop": "Shows a notification on this computer while NeuroWatch "
    "(or its tray icon) runs here.",
    "ntfy": "Install the ntfy app on your phone and subscribe to the same topic. Anyone who "
    "knows a topic on ntfy.sh can read it: choose one that is hard to guess.",
    "telegram": "Create a bot with @BotFather, add it to your chat or channel, then send it a "
    "message. The chat id is shown by bots such as @userinfobot.",
    "webhook": "NeuroWatch POSTs each alert as JSON (title, message, rule, event). Home "
    "Assistant, Node-RED and similar tools can act on it.",
}


class _Call(Worker):
    done = Signal(object)

    def __init__(self, endpoint: str | None, method: str, params: dict[str, Any]) -> None:
        super().__init__()
        self._endpoint, self._method, self._params = endpoint, method, params

    def run(self) -> None:
        try:
            self.done.emit(SyncIpcClient(endpoint=self._endpoint).call(self._method, self._params))
        except Exception as exc:
            self.done.emit(exc)


class ChannelDialog(QDialog):
    """Name, address and options of one channel, with a test send."""

    def __init__(
        self, endpoint: str | None, channel: Channel, parent: QWidget | None = None
    ) -> None:
        super().__init__(parent)
        self._endpoint = endpoint
        self.channel = copy.deepcopy(channel)
        self._worker: _Call | None = None
        kind_name = next(
            (name for kind, name, _ in CHANNEL_KINDS if kind == channel.kind), channel.kind
        )
        self.setWindowTitle("Alert Channel")
        self.setMinimumWidth(480)
        apply_dialog_theme(self)
        layout = install_dialog_chrome(self, f"{kind_name} channel")
        layout.setSpacing(12)
        help_text = QLabel(CHANNEL_HELP.get(channel.kind, ""))
        help_text.setWordWrap(True)
        set_label_role(help_text, "hint")
        layout.addWidget(help_text)

        form = QFormLayout()
        form.setHorizontalSpacing(14)
        form.setVerticalSpacing(10)
        self._name = QLineEdit(channel.name)
        self._name.setMaxLength(60)
        self._name.setPlaceholderText("e.g. My phone")
        self._name.setAccessibleName("Channel name")
        form.addRow("Name", self._name)
        self._fields: dict[str, QLineEdit] = {}
        for name, label, placeholder in CHANNEL_FIELDS.get(channel.kind, ()):
            edit = QLineEdit(channel.fields.get(name, ""))
            edit.setPlaceholderText(placeholder)
            edit.setAccessibleName(label)
            if name in SECRETS:
                edit.setEchoMode(QLineEdit.EchoMode.Password)
                if edit.text() == KEPT_SECRET:
                    edit.setPlaceholderText("Saved — leave as is to keep it")
            self._fields[name] = edit
            form.addRow(label, edit)
        layout.addLayout(form)

        self._picture = QCheckBox("Send the event's picture too")
        self._picture.setChecked(channel.attach_picture)
        self._picture.setVisible(channel.kind != "desktop")
        layout.addWidget(self._picture)
        self._enabled = QCheckBox("Send alerts to this channel")
        self._enabled.setChecked(channel.enabled)
        layout.addWidget(self._enabled)

        test_row = QHBoxLayout()
        self._test = QPushButton("Send Test")
        set_button_variant(self._test, "secondary")
        self._test.clicked.connect(self._send_test)
        test_row.addWidget(self._test)
        self._status = QLabel()
        self._status.setWordWrap(True)
        test_row.addWidget(self._status, 1)
        layout.addLayout(test_row)

        buttons = QDialogButtonBox(
            QDialogButtonBox.StandardButton.Save | QDialogButtonBox.StandardButton.Cancel
        )
        buttons.accepted.connect(self._accept)
        buttons.rejected.connect(self.reject)
        style_button_box(buttons)
        layout.addWidget(buttons)

    def _read(self) -> Channel:
        channel = copy.deepcopy(self.channel)
        channel.name = self._name.text().strip()
        channel.fields = {name: edit.text().strip() for name, edit in self._fields.items()}
        channel.attach_picture = self._picture.isChecked()
        channel.enabled = self._enabled.isChecked()
        return channel

    def _show(self, text: str, tone: str) -> None:
        self._status.setText(text)
        set_status_tone(self._status, tone)

    def _send_test(self) -> None:
        channel = self._read()
        problem = channel_problem(channel)
        if problem:
            self._show(problem, "error")
            return
        if self._worker is not None:
            return
        self._test.setEnabled(False)
        self._show("Sending…", "info")
        worker = _Call(self._endpoint, "alerts.test", {"channel": channel.to_dict()})
        self._worker = worker
        worker.done.connect(self._tested)
        worker.finished.connect(worker.deleteLater)
        worker.start()

    def _tested(self, result: object) -> None:
        self._worker = None
        self._test.setEnabled(True)
        if isinstance(result, dict) and result.get("ok"):
            self._show("Sent. Check that it arrived.", "success")
        elif isinstance(result, dict):
            self._show(f"Not sent: {result.get('error') or 'unknown error'}", "error")
        else:
            self._show(f"Not sent: {result}", "error")

    def _accept(self) -> None:
        channel = self._read()
        problem = channel_problem(channel)
        if problem:
            self._show(problem, "error")
            return
        self.channel = channel
        self.accept()

    def done(self, result: int) -> None:
        if self._worker is not None and self._worker.isRunning():
            self._worker.wait(15000)
        super().done(result)


class _WindowRow(QWidget):
    """One time window: days and from–to."""

    removed = Signal(object)

    def __init__(self, window: Window, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        row = QHBoxLayout(self)
        row.setContentsMargins(0, 0, 0, 0)
        row.setSpacing(4)
        self.days: list[QCheckBox] = []
        for index, name in enumerate(DAY_NAMES):
            box = QCheckBox(name)
            box.setChecked(index in window.days)
            self.days.append(box)
            row.addWidget(box)
        row.addSpacing(8)
        self.start = QTimeEdit(QTime.fromString(window.start, "HH:mm"))
        self.start.setDisplayFormat("HH:mm")
        self.start.setAccessibleName("From")
        self.end = QTimeEdit(QTime.fromString(window.end, "HH:mm"))
        self.end.setDisplayFormat("HH:mm")
        self.end.setAccessibleName("Until")
        row.addWidget(self.start)
        row.addWidget(QLabel("–"))
        row.addWidget(self.end)
        remove = QPushButton("Remove")
        set_button_variant(remove, "ghost")
        remove.clicked.connect(lambda: self.removed.emit(self))
        row.addWidget(remove)
        row.addStretch(1)

    def value(self) -> Window:
        return Window(
            [i for i, box in enumerate(self.days) if box.isChecked()],
            self.start.time().toString("HH:mm"),
            self.end.time().toString("HH:mm"),
        )


class RuleDialog(QDialog):
    """Which events, cameras and hours a rule covers, and where it sends."""

    def __init__(
        self,
        rule: Rule,
        cameras: list[tuple[str, str]],
        channels: list[Channel],
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        self.rule = copy.deepcopy(rule)
        self.setWindowTitle("Alert Rule")
        self.setMinimumSize(560, 520)
        apply_dialog_theme(self)
        outer = install_dialog_chrome(self, "Alert rule", resizable=True)
        scroll = QScrollArea()
        scroll.setWidgetResizable(True)
        scroll.setFrameShape(QScrollArea.Shape.NoFrame)
        body = QWidget()
        layout = QVBoxLayout(body)
        layout.setContentsMargins(0, 0, 8, 0)
        layout.setSpacing(12)
        scroll.setWidget(body)
        outer.addWidget(scroll, 1)

        form = QFormLayout()
        self._name = QLineEdit(rule.name)
        self._name.setMaxLength(60)
        self._name.setPlaceholderText("e.g. Motion at night")
        self._name.setAccessibleName("Rule name")
        form.addRow("Name", self._name)
        layout.addLayout(form)
        self._enabled = QCheckBox("This rule is on")
        self._enabled.setChecked(rule.enabled)
        layout.addWidget(self._enabled)

        layout.addWidget(self._heading("When these happen"))
        grid = QGridLayout()
        self._kinds: dict[str, QCheckBox] = {}
        for index, (kind, label) in enumerate(KIND_CHOICES):
            box = QCheckBox(label)
            box.setChecked(kind in rule.kinds)
            self._kinds[kind] = box
            grid.addWidget(box, index // 2, index % 2)
        layout.addLayout(grid)

        layout.addWidget(self._heading("On these cameras"))
        self._all_cameras = QRadioButton("Every camera (and recorder-wide events)")
        self._some_cameras = QRadioButton("Only these cameras")
        group = QButtonGroup(self)
        group.addButton(self._all_cameras)
        group.addButton(self._some_cameras)
        layout.addWidget(self._all_cameras)
        layout.addWidget(self._some_cameras)
        self._cameras: dict[str, QCheckBox] = {}
        camera_grid = QGridLayout()
        camera_grid.setContentsMargins(24, 0, 0, 0)
        for index, (camera_id, name) in enumerate(cameras):
            box = QCheckBox(name)
            box.setChecked(camera_id in rule.cameras)
            self._cameras[camera_id] = box
            camera_grid.addWidget(box, index // 3, index % 3)
        layout.addLayout(camera_grid)
        (self._some_cameras if rule.cameras else self._all_cameras).setChecked(True)
        self._some_cameras.toggled.connect(self._sync_cameras)
        self._sync_cameras()

        layout.addWidget(self._heading("At these times (the recorder's clock)"))
        self._always = QRadioButton("Any time")
        self._hours = QRadioButton("Only during these hours")
        times = QButtonGroup(self)
        times.addButton(self._always)
        times.addButton(self._hours)
        layout.addWidget(self._always)
        layout.addWidget(self._hours)
        self._windows_box = QVBoxLayout()
        self._windows_box.setContentsMargins(24, 0, 0, 0)
        self._window_rows: list[_WindowRow] = []
        layout.addLayout(self._windows_box)
        self._add_window = QPushButton("Add Time Window")
        set_button_variant(self._add_window, "ghost")
        self._add_window.clicked.connect(
            lambda: self._add_window_row(Window([0, 1, 2, 3, 4, 5, 6], "22:00", "06:00"))
        )
        add_row = QHBoxLayout()
        add_row.setContentsMargins(24, 0, 0, 0)
        add_row.addWidget(self._add_window)
        add_row.addStretch(1)
        layout.addLayout(add_row)
        for window in rule.schedule:
            self._add_window_row(window)
        (self._hours if rule.schedule else self._always).setChecked(True)
        self._hours.toggled.connect(self._sync_hours)
        self._sync_hours()

        layout.addWidget(self._heading("Then send to"))
        self._channels: dict[str, QCheckBox] = {}
        for channel in channels:
            box = QCheckBox(channel.name + ("" if channel.enabled else " (off)"))
            box.setChecked(channel.id in rule.channels)
            self._channels[channel.id] = box
            layout.addWidget(box)
        quiet = QHBoxLayout()
        quiet.addWidget(QLabel("After an alert, stay quiet for"))
        self._quiet = QComboBox()
        self._quiet.setAccessibleName("Quiet time after an alert")
        for label, seconds in QUIET_CHOICES:
            self._quiet.addItem(label, seconds)
        if self._quiet.findData(rule.cooldown_sec) < 0:
            self._quiet.addItem(f"{rule.cooldown_sec} seconds", rule.cooldown_sec)
        self._quiet.setCurrentIndex(self._quiet.findData(rule.cooldown_sec))
        quiet.addWidget(self._quiet)
        quiet.addStretch(1)
        layout.addLayout(quiet)
        hint = QLabel("Per camera: a busy camera sends one alert, then waits this long.")
        set_label_role(hint, "hint")
        layout.addWidget(hint)
        layout.addStretch(1)

        self._error = QLabel()
        self._error.setWordWrap(True)
        set_status_tone(self._error, "error")
        self._error.hide()
        outer.addWidget(self._error)
        buttons = QDialogButtonBox(
            QDialogButtonBox.StandardButton.Save | QDialogButtonBox.StandardButton.Cancel
        )
        buttons.accepted.connect(self._accept)
        buttons.rejected.connect(self.reject)
        style_button_box(buttons)
        outer.addWidget(buttons)

    @staticmethod
    def _heading(text: str) -> QLabel:
        label = QLabel(text)
        set_label_role(label, "sectionLabel")
        return label

    def _sync_cameras(self) -> None:
        for box in self._cameras.values():
            box.setEnabled(self._some_cameras.isChecked())

    def _sync_hours(self) -> None:
        on = self._hours.isChecked()
        self._add_window.setEnabled(on)
        for row in self._window_rows:
            row.setEnabled(on)
        if on and not self._window_rows:
            self._add_window_row(Window([0, 1, 2, 3, 4, 5, 6], "22:00", "06:00"))

    def _add_window_row(self, window: Window) -> None:
        row = _WindowRow(window)
        row.removed.connect(self._remove_window_row)
        self._window_rows.append(row)
        self._windows_box.addWidget(row)

    def _remove_window_row(self, row: _WindowRow) -> None:
        self._window_rows.remove(row)
        row.setParent(None)
        row.deleteLater()

    def read(self) -> Rule:
        rule = copy.deepcopy(self.rule)
        rule.name = self._name.text().strip()
        rule.enabled = self._enabled.isChecked()
        rule.kinds = [kind for kind, box in self._kinds.items() if box.isChecked()]
        rule.cameras = (
            [cid for cid, box in self._cameras.items() if box.isChecked()]
            if self._some_cameras.isChecked()
            else []
        )
        rule.schedule = (
            [row.value() for row in self._window_rows] if self._hours.isChecked() else []
        )
        rule.channels = [cid for cid, box in self._channels.items() if box.isChecked()]
        rule.cooldown_sec = int(self._quiet.currentData())
        return rule

    def _accept(self) -> None:
        rule = self.read()
        problem = rule_problem(rule)
        if problem is None and self._some_cameras.isChecked() and not rule.cameras:
            problem = "Choose at least one camera, or Every camera."
        if problem:
            self._error.setText(problem)
            self._error.show()
            return
        self.rule = rule
        self.accept()
