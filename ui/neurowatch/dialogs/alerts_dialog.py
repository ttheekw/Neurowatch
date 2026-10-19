"""Alert Notifications: where alerts go (channels), which events send them
(rules), and what was sent lately. The recorder sends the alerts itself, so
they keep coming while this app is closed; this dialog edits its settings
(``alerts.get`` / ``alerts.set``) on worker threads.
"""

from __future__ import annotations

import copy
import time
from datetime import datetime
from typing import Any

from PySide6.QtCore import QSize, Qt, Signal
from PySide6.QtWidgets import (
    QDialog,
    QHBoxLayout,
    QLabel,
    QListWidget,
    QListWidgetItem,
    QMenu,
    QPushButton,
    QTabWidget,
    QVBoxLayout,
    QWidget,
)

from neurowatch.dialogs.alert_editors import ChannelDialog, RuleDialog
from neurowatch.ipc.sync_client import SyncIpcClient
from neurowatch.models.alerts import (
    CHANNEL_KINDS,
    AlertSettings,
    Channel,
    Rule,
    channel_summary,
    new_id,
    rule_summary,
)
from neurowatch.theme import (
    apply_dialog_theme,
    set_button_variant,
    set_label_role,
    set_status_tone,
    themed_menu,
)
from neurowatch.utils.threads import Worker
from neurowatch.widgets.window_chrome import install_dialog_chrome, resize_window


class _Calls(Worker):
    """Runs a few calls in order; emits each result (or Exception) by name."""

    done = Signal(object)  # {name: result}

    def __init__(self, endpoint: str | None, calls: list[tuple[str, str, dict[str, Any]]]) -> None:
        super().__init__()
        self._endpoint, self._calls = endpoint, calls

    def run(self) -> None:
        results: dict[str, Any] = {}
        client = SyncIpcClient(endpoint=self._endpoint)
        for name, method, params in self._calls:
            try:
                results[name] = client.call(method, params)
            except Exception as exc:
                results[name] = exc
        self.done.emit(results)


class AlertsDialog(QDialog):
    def __init__(
        self,
        endpoint: str | None,
        cameras: list[tuple[str, str]],
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        self._endpoint = endpoint
        self._cameras = cameras
        self._settings = AlertSettings()
        self._saved = AlertSettings()
        self._workers: set[_Calls] = set()
        self.setWindowTitle("Alert Notifications")
        self.setMinimumSize(620, 520)
        apply_dialog_theme(self)
        layout = install_dialog_chrome(self, "Alert notifications", resizable=True)
        layout.setSpacing(10)
        intro = QLabel(
            "Send events to your phone, a chat or this computer. The recorder sends them, "
            "so alerts keep coming while this window is closed."
        )
        intro.setWordWrap(True)
        set_label_role(intro, "hint")
        layout.addWidget(intro)

        self._tabs = QTabWidget()
        layout.addWidget(self._tabs, 1)

        # Rules first: they are what users change most.
        rules_page = QWidget()
        rules_layout = QVBoxLayout(rules_page)
        self._rules = QListWidget()
        self._rules.setAccessibleName("Alert rules")
        self._rules.setWordWrap(True)
        self._rules.itemDoubleClicked.connect(lambda _item: self._edit_rule())
        self._rules.itemChanged.connect(self._rule_toggled)
        self._rules.currentRowChanged.connect(lambda _row: self._sync_buttons())
        rules_layout.addWidget(self._rules, 1)
        self._rules_empty = QLabel()
        self._rules_empty.setWordWrap(True)
        set_label_role(self._rules_empty, "hint")
        rules_layout.addWidget(self._rules_empty)
        row = QHBoxLayout()
        self._add_rule = QPushButton("Add Rule…")
        set_button_variant(self._add_rule, "primary")
        self._add_rule.clicked.connect(self._new_rule)
        self._edit_rule_btn = QPushButton("Edit…")
        self._edit_rule_btn.clicked.connect(self._edit_rule)
        self._remove_rule = QPushButton("Remove")
        set_button_variant(self._remove_rule, "ghost")
        self._remove_rule.clicked.connect(self._delete_rule)
        for button in (self._add_rule, self._edit_rule_btn, self._remove_rule):
            row.addWidget(button)
        row.addStretch(1)
        rules_layout.addLayout(row)
        self._tabs.addTab(rules_page, "Rules")

        channels_page = QWidget()
        channels_layout = QVBoxLayout(channels_page)
        self._channels = QListWidget()
        self._channels.setAccessibleName("Alert channels")
        self._channels.itemDoubleClicked.connect(lambda _item: self._edit_channel())
        self._channels.currentRowChanged.connect(lambda _row: self._sync_buttons())
        channels_layout.addWidget(self._channels, 1)
        row = QHBoxLayout()
        self._add_channel = QPushButton("Add Channel ▾")
        set_button_variant(self._add_channel, "primary")
        menu: QMenu = themed_menu(self._add_channel)
        for kind, name, explanation in CHANNEL_KINDS:
            action = menu.addAction(name, lambda k=kind: self._new_channel(k))
            action.setToolTip(explanation)
        menu.setToolTipsVisible(True)
        self._add_channel.setMenu(menu)
        self._edit_channel_btn = QPushButton("Edit…")
        self._edit_channel_btn.clicked.connect(self._edit_channel)
        self._remove_channel = QPushButton("Remove")
        set_button_variant(self._remove_channel, "ghost")
        self._remove_channel.clicked.connect(self._delete_channel)
        for button in (self._add_channel, self._edit_channel_btn, self._remove_channel):
            row.addWidget(button)
        row.addStretch(1)
        channels_layout.addLayout(row)
        self._tabs.addTab(channels_page, "Channels")

        history_page = QWidget()
        history_layout = QVBoxLayout(history_page)
        self._history = QListWidget()
        self._history.setAccessibleName("Recent alerts")
        self._history.setSelectionMode(QListWidget.SelectionMode.NoSelection)
        history_layout.addWidget(self._history, 1)
        refresh = QPushButton("Refresh")
        set_button_variant(refresh, "ghost")
        refresh.clicked.connect(self._load_history)
        history_layout.addWidget(refresh, 0, Qt.AlignmentFlag.AlignLeft)
        self._tabs.addTab(history_page, "Recently sent")

        bottom = QHBoxLayout()
        self._status = QLabel("Loading…")
        self._status.setWordWrap(True)
        bottom.addWidget(self._status, 1)
        self._cancel = QPushButton("Close")
        self._cancel.clicked.connect(self.reject)
        bottom.addWidget(self._cancel)
        self._save = QPushButton("Save Changes")
        set_button_variant(self._save, "primary")
        self._save.clicked.connect(self._save_settings)
        bottom.addWidget(self._save)
        layout.addLayout(bottom)
        self._sync_buttons()
        resize_window(self, QSize(720, 600))
        self._run(
            [("settings", "alerts.get", {}), ("history", "alerts.history", {"limit": 50})],
            self._loaded,
        )

    # -- workers -------------------------------------------------------

    def _run(self, calls: list[tuple[str, str, dict[str, Any]]], done: Any) -> None:
        worker = _Calls(self._endpoint, calls)
        self._workers.add(worker)
        worker.done.connect(done)
        worker.finished.connect(lambda: self._workers.discard(worker))
        worker.finished.connect(worker.deleteLater)
        worker.start()

    def done(self, result: int) -> None:
        for worker in list(self._workers):
            worker.wait(5000)
        super().done(result)

    def reject(self) -> None:
        if self.dirty:
            from neurowatch.dialogs.confirm import confirm_action

            if not confirm_action(
                self,
                "Unsaved changes",
                "Close without saving your alert changes?",
                confirm_text="Discard Changes",
                destructive=True,
            ):
                return
        super().reject()

    @property
    def dirty(self) -> bool:
        return self._settings.to_dict() != self._saved.to_dict()

    def _note(self, text: str, tone: str = "muted") -> None:
        self._status.setText(text)
        set_status_tone(self._status, tone)

    def _loaded(self, results: dict[str, Any]) -> None:
        settings = results.get("settings")
        if isinstance(settings, Exception):
            self._note(f"Could not read the alert settings: {settings}", "error")
            self._save.setEnabled(False)
            return
        self._settings = AlertSettings.from_dict(settings)
        self._saved = copy.deepcopy(self._settings)
        self._note("")
        self._render()
        self._show_history(results.get("history"))

    def _load_history(self) -> None:
        self._run(
            [("history", "alerts.history", {"limit": 50})],
            lambda r: self._show_history(r.get("history")),
        )

    def _show_history(self, result: Any) -> None:
        self._history.clear()
        deliveries = result.get("deliveries", []) if isinstance(result, dict) else []
        if not deliveries:
            self._history.addItem("Nothing sent since the recorder started.")
            return
        for delivery in deliveries:
            when = datetime.fromtimestamp(int(delivery.get("at_ms", 0)) / 1000).strftime(
                "%d %b %H:%M:%S"
            )
            outcome = (
                "Sent" if delivery.get("ok") else f"Not sent: {delivery.get('error') or 'error'}"
            )
            test = " (test)" if delivery.get("test") else ""
            item = QListWidgetItem(
                f"{when} · {delivery.get('channel_name', '')}{test}"
                f" · {delivery.get('title', '')}\n{outcome}"
            )
            if not delivery.get("ok"):
                item.setForeground(Qt.GlobalColor.red)
            self._history.addItem(item)

    def _save_settings(self) -> None:
        self._save.setEnabled(False)
        self._note("Saving…", "info")
        self._run([("saved", "alerts.set", self._settings.to_dict())], self._after_save)

    def _after_save(self, results: dict[str, Any]) -> None:
        self._save.setEnabled(True)
        saved = results.get("saved")
        if isinstance(saved, Exception):
            self._note(f"Not saved: {saved}", "error")
            return
        self._settings = AlertSettings.from_dict(saved)
        self._saved = copy.deepcopy(self._settings)
        self._render()
        self._note(f"Saved at {time.strftime('%H:%M:%S')}.", "success")

    # -- lists ---------------------------------------------------------

    def _names(self) -> tuple[dict[str, str], dict[str, str]]:
        return dict(self._cameras), {c.id: c.name for c in self._settings.channels}

    def _render(self) -> None:
        cameras, channels = self._names()
        self._rules.blockSignals(True)
        self._rules.clear()
        for rule in self._settings.rules:
            item = QListWidgetItem(f"{rule.name}\n{rule_summary(rule, cameras, channels)}")
            item.setFlags(item.flags() | Qt.ItemFlag.ItemIsUserCheckable)
            item.setCheckState(Qt.CheckState.Checked if rule.enabled else Qt.CheckState.Unchecked)
            item.setToolTip("Tick to turn the rule on; double-click to edit")
            self._rules.addItem(item)
        self._rules.blockSignals(False)
        self._channels.clear()
        for channel in self._settings.channels:
            self._channels.addItem(f"{channel.name}\n{channel_summary(channel)}")
        if not self._settings.channels:
            self._rules_empty.setText(
                "Start by adding a channel (Channels tab): where alerts should go."
            )
        elif not self._settings.rules:
            self._rules_empty.setText(
                "Add a rule: which events send an alert, when, and to which channels."
            )
        else:
            self._rules_empty.setText("")
        self._rules_empty.setVisible(bool(self._rules_empty.text()))
        self._sync_buttons()

    def _sync_buttons(self) -> None:
        has_rule = 0 <= self._rules.currentRow() < len(self._settings.rules)
        has_channel = 0 <= self._channels.currentRow() < len(self._settings.channels)
        self._add_rule.setEnabled(bool(self._settings.channels))
        self._edit_rule_btn.setEnabled(has_rule)
        self._remove_rule.setEnabled(has_rule)
        self._edit_channel_btn.setEnabled(has_channel)
        self._remove_channel.setEnabled(has_channel)

    def _rule_toggled(self, item: QListWidgetItem) -> None:
        row = self._rules.row(item)
        if 0 <= row < len(self._settings.rules):
            self._settings.rules[row].enabled = item.checkState() == Qt.CheckState.Checked

    # -- channels ------------------------------------------------------

    def _new_channel(self, kind: str) -> None:
        name = next(n for k, n, _ in CHANNEL_KINDS if k == kind)
        taken = {c.id for c in self._settings.channels}
        channel = Channel(id=new_id(name, taken), name=name if kind == "desktop" else "", kind=kind)
        dialog = ChannelDialog(self._endpoint, channel, self)
        if dialog.exec() == QDialog.DialogCode.Accepted:
            self._settings.channels.append(dialog.channel)
            self._render()
            self._channels.setCurrentRow(len(self._settings.channels) - 1)
        dialog.deleteLater()

    def _edit_channel(self) -> None:
        row = self._channels.currentRow()
        if not 0 <= row < len(self._settings.channels):
            return
        dialog = ChannelDialog(self._endpoint, self._settings.channels[row], self)
        if dialog.exec() == QDialog.DialogCode.Accepted:
            self._settings.channels[row] = dialog.channel
            self._render()
            self._channels.setCurrentRow(row)
        dialog.deleteLater()

    def _delete_channel(self) -> None:
        row = self._channels.currentRow()
        if not 0 <= row < len(self._settings.channels):
            return
        orphaned = self._settings.remove_channel(self._settings.channels[row].id)
        if orphaned:
            self._note(
                "These rules now send nowhere; edit or remove them: " + ", ".join(orphaned),
                "warning",
            )
        self._render()

    # -- rules ---------------------------------------------------------

    def _new_rule(self) -> None:
        taken = {r.id for r in self._settings.rules}
        rule = Rule(
            id=new_id("rule", taken),
            name="",
            kinds=["motion"],
            channels=[self._settings.channels[0].id] if len(self._settings.channels) == 1 else [],
        )
        dialog = RuleDialog(rule, self._cameras, self._settings.channels, self)
        if dialog.exec() == QDialog.DialogCode.Accepted:
            rule = dialog.rule
            rule.id = new_id(rule.name or "rule", taken)
            self._settings.rules.append(rule)
            self._render()
            self._rules.setCurrentRow(len(self._settings.rules) - 1)
        dialog.deleteLater()

    def _edit_rule(self) -> None:
        row = self._rules.currentRow()
        if not 0 <= row < len(self._settings.rules):
            return
        dialog = RuleDialog(self._settings.rules[row], self._cameras, self._settings.channels, self)
        if dialog.exec() == QDialog.DialogCode.Accepted:
            self._settings.rules[row] = dialog.rule
            self._render()
            self._rules.setCurrentRow(row)
        dialog.deleteLater()

    def _delete_rule(self) -> None:
        row = self._rules.currentRow()
        if 0 <= row < len(self._settings.rules):
            del self._settings.rules[row]
            self._render()
