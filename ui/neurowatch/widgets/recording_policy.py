"""When a camera records: the default mode, a painted weekly schedule with
exact time windows, and how event footage is kept. No backend calls."""

from __future__ import annotations

from copy import deepcopy
from typing import Any

from PySide6.QtCore import QTime, Signal
from PySide6.QtGui import QAction, QColor, QPainter, QPaintEvent
from PySide6.QtWidgets import (
    QComboBox,
    QGridLayout,
    QHBoxLayout,
    QHeaderView,
    QLabel,
    QMenu,
    QPushButton,
    QSpinBox,
    QTableWidget,
    QTimeEdit,
    QVBoxLayout,
    QWidget,
)

from neurowatch.models.recording_policy import (
    DAYS,
    normalized_policy,
    paint_windows,
    policy_problem,
    weekly_slots,
)
from neurowatch.theme import set_button_variant, set_label_role
from neurowatch.widgets.form_parts import Segmented, settings_card
from neurowatch.widgets.recording_schedule import RecordingSchedule, mode_color

MODE_LABELS = (("Continuous", "continuous"), ("Events only", "events"), ("Off", "off"))
MODE_HELP = {
    "continuous": "Records all the time.",
    "events": "Records all the time, then keeps only footage around events (with the "
    "padding below). Footage without events is trimmed later.",
    "off": "Does not record, except in scheduled windows.",
}


def mode_picker(value: str) -> QComboBox:
    combo = QComboBox()
    for label, key in MODE_LABELS:
        combo.addItem(label, key)
    combo.setCurrentIndex(max(0, combo.findData(value)))
    return combo


class _Swatch(QWidget):
    """A small colour square for the legend."""

    def __init__(self, mode: str | None, default: bool = False) -> None:
        super().__init__()
        self._mode, self._default = mode, default
        self.setFixedSize(12, 12)

    def set_mode(self, mode: str) -> None:
        self._mode = mode
        self.update()

    def paintEvent(self, _event: QPaintEvent) -> None:
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing)
        color = QColor(mode_color(self._mode or "continuous"))
        if self._default:
            color.setAlpha(80)
        painter.setPen(QColor(0, 0, 0, 0))
        painter.setBrush(color)
        painter.drawRoundedRect(self.rect().adjusted(0, 0, -1, -1), 3, 3)


class RecordingPolicyEditor(QWidget):
    copy_requested = Signal()

    def __init__(self, original: dict | None = None, mode_host: QVBoxLayout | None = None) -> None:
        """``mode_host``: where the default-mode choice goes (a card of the
        caller's); without one, the editor shows it in a card of its own."""
        super().__init__()
        self._original = deepcopy(original or {})
        policy = normalized_policy(self._original)
        self._syncing = True
        self._history: list[list[dict]] = []
        self._initial = deepcopy(policy)
        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(14)

        # ── Default mode ─────────────────────────────────────────────
        if mode_host is None:
            card, mode_host, _ = settings_card(
                "When to record", "What the camera does outside any scheduled window."
            )
            layout.addWidget(card)
        mode_row = QHBoxLayout()
        mode_row.setSpacing(10)
        mode_label = QLabel("Default mode")
        mode_row.addWidget(mode_label)
        self.mode = Segmented(MODE_LABELS)
        self.mode.setAccessibleName("Default recording mode")
        self.mode.setToolTip("What the camera does outside any scheduled window")
        self.mode.set_value(policy["mode"])
        mode_row.addWidget(self.mode, 1)
        mode_host.addLayout(mode_row)
        self._mode_help = QLabel()
        self._mode_help.setWordWrap(True)
        set_label_role(self._mode_help, "hint")
        mode_host.addWidget(self._mode_help)

        # ── Weekly schedule ──────────────────────────────────────────
        card, body, heading = settings_card(
            "Weekly schedule",
            "Optional. Drag across the grid to paint a mode over days and hours "
            "(recorder's local time, daylight saving included).",
        )
        self.copy_button = QPushButton("Copy to Cameras…")
        self.copy_button.setToolTip("Use this camera's recording settings on other cameras")
        set_button_variant(self.copy_button, "secondary")
        self.copy_button.clicked.connect(self.copy_requested)
        heading.addWidget(self.copy_button)
        tools = QHBoxLayout()
        tools.setSpacing(8)
        paint_label = QLabel("Paint")
        set_label_role(paint_label, "hint")
        tools.addWidget(paint_label)
        self.brush = Segmented((*MODE_LABELS, ("Erase", None)))
        self.brush.setAccessibleName("Schedule paint mode")
        self.brush.set_value("events")
        for button in self.brush.buttons():
            if button.text() == "Erase":
                button.setToolTip("Paint the default mode back")
        tools.addWidget(self.brush)
        tools.addStretch(1)
        self.resolution = QComboBox()
        self.resolution.addItem("Hour steps", 60)
        self.resolution.addItem("15-minute steps", 15)
        self.resolution.setAccessibleName("Schedule paint interval")
        tools.addWidget(self.resolution)
        self.undo_button = QPushButton("Undo")
        set_button_variant(self.undo_button, "ghost")
        self.undo_button.setEnabled(False)
        self.undo_button.clicked.connect(self.undo)
        tools.addWidget(self.undo_button)
        body.addLayout(tools)
        self.grid = RecordingSchedule()
        self.grid.painted.connect(self.paint)
        body.addWidget(self.grid)
        legend = QHBoxLayout()
        legend.setSpacing(6)
        for text, mode in MODE_LABELS:
            legend.addWidget(_Swatch(mode))
            label = QLabel(text)
            set_label_role(label, "hint")
            legend.addWidget(label)
            legend.addSpacing(10)
        self._default_swatch = _Swatch(policy["mode"], default=True)
        legend.addWidget(self._default_swatch)
        default_label = QLabel("Faint: default mode")
        set_label_role(default_label, "hint")
        legend.addWidget(default_label)
        legend.addStretch(1)
        body.addLayout(legend)
        self.problem = QLabel()
        self.problem.setWordWrap(True)
        set_label_role(self.problem, "hint")
        body.addWidget(self.problem)
        self.exact = QPushButton("Exact Times…")
        self.exact.setCheckable(True)
        self.exact.setToolTip(
            "Edit the windows to the minute. An end before the start continues "
            "overnight; equal times mean the whole day. Windows must not overlap."
        )
        set_button_variant(self.exact, "ghost")
        exact_row = QHBoxLayout()
        exact_row.addWidget(self.exact)
        exact_row.addStretch(1)
        body.addLayout(exact_row)
        self.table = QTableWidget(0, 5)
        self.table.setHorizontalHeaderLabels(["Days", "Start", "End", "Mode", ""])
        self.table.verticalHeader().hide()
        self.table.horizontalHeader().setSectionResizeMode(0, QHeaderView.ResizeMode.Stretch)
        for col, width in ((1, 84), (2, 84), (3, 140), (4, 84)):
            self.table.setColumnWidth(col, width)
        self.table.setMinimumHeight(130)
        self.table.setMaximumHeight(230)
        body.addWidget(self.table)
        self._rows: list[dict] = []
        for rule in policy["weekly_rules"]:
            self.add_rule(rule)
        self.add = QPushButton("Add Window")
        set_button_variant(self.add, "secondary")
        self.add.clicked.connect(lambda: self.add_rule())
        add_row = QHBoxLayout()
        add_row.addWidget(self.add)
        add_row.addStretch(1)
        body.addLayout(add_row)
        self.add.setEnabled(len(self._rows) < 64)
        self.table.hide()
        self.add.hide()
        self.exact.toggled.connect(self.table.setVisible)
        self.exact.toggled.connect(self.add.setVisible)
        layout.addWidget(card)

        # ── Event footage ────────────────────────────────────────────
        card, body, _ = settings_card(
            "Event footage",
            "Used by Events only: how much is kept around each event, and for how long "
            "other footage stays.",
        )
        details = QGridLayout()
        details.setHorizontalSpacing(14)
        details.setVerticalSpacing(8)
        details.setColumnStretch(2, 1)
        self._numbers: dict[str, QSpinBox] = {}
        for row, (key, name, limit, suffix) in enumerate(
            (
                ("event_pre_sec", "Keep before each event", 3600, " s"),
                ("event_post_sec", "Keep after each event", 3600, " s"),
                ("non_event_keep_hours", "Keep other footage for", 87600, " h"),
            )
        ):
            spin = QSpinBox()
            spin.setRange(0, limit)
            spin.setSuffix(suffix)
            spin.setValue(policy[key])
            spin.setMinimumWidth(170)
            if key == "non_event_keep_hours":
                spin.setSpecialValueText("Trim soon (5-minute grace)")
            details.addWidget(QLabel(name), row, 0)
            details.addWidget(spin, row, 1)
            self._numbers[key] = spin
        body.addLayout(details)
        info = QLabel(
            "Footage is kept as whole segments. Cleanup pauses while detection is "
            "unavailable, and storage quota and drive limits still apply."
        )
        info.setWordWrap(True)
        set_label_role(info, "hint")
        body.addWidget(info)
        layout.addWidget(card)

        self.brush.changed.connect(lambda _value: self._brush_changed())
        self.resolution.currentIndexChanged.connect(self._brush_changed)
        self.mode.changed.connect(lambda _value: self._rules_changed())
        self._brush_changed()
        self._syncing = False
        self._rules_changed()

    def _brush_changed(self) -> None:
        self.grid.brush = self.brush.value()  # type: ignore[assignment]
        self.grid.step = self.resolution.currentData()
        day, minute = self.grid._cursor
        self.grid._cursor = (day, minute // self.grid.step * self.grid.step)
        self.grid.update()

    def _rules_changed(self) -> None:
        if self._syncing:
            return
        mode = str(self.mode.value())
        self._mode_help.setText(MODE_HELP.get(mode, ""))
        self._default_swatch.set_mode(mode)
        policy = normalized_policy(self.policy())
        problem = policy_problem(policy)
        self.problem.setText(problem or "")
        self.problem.setVisible(bool(problem))
        self.grid.setEnabled(problem is None)
        if problem is None:
            self.grid.set_schedule(weekly_slots(policy["weekly_rules"]), policy["mode"])

    def _replace_rules(self, rules: list[dict]) -> None:
        self._syncing = True
        self._rows.clear()
        self.table.setRowCount(0)
        for rule in rules:
            self.add_rule(rule)
        self.add.setEnabled(len(self._rows) < 64)
        self._syncing = False
        self._rules_changed()

    def paint(self, first_day: int, last_day: int, start: int, end: int, mode: str | None) -> None:
        rules = self.policy().get("weekly_rules", [])
        try:
            changed = paint_windows(rules, first_day, last_day, start, end, mode)
        except ValueError as error:
            self.problem.setText(str(error))
            self.problem.show()
            return
        if weekly_slots(changed) == weekly_slots(rules):
            return
        self._history = [*self._history, deepcopy(rules)][-20:]
        self.undo_button.setEnabled(True)
        self._replace_rules(changed)

    def undo(self) -> None:
        if self._history:
            self._replace_rules(self._history.pop())
        self.undo_button.setEnabled(bool(self._history))

    def add_rule(self, value: dict | None = None) -> None:
        if len(self._rows) >= 64:
            return
        rule = deepcopy(
            value
            or {"days": [0, 1, 2, 3, 4], "start": "08:00", "end": "18:00", "mode": "continuous"}
        )
        days = QPushButton()
        days.setAccessibleName("Schedule days")
        menu = QMenu(days)
        actions: list[QAction] = []
        for i, name in enumerate(DAYS):
            action = menu.addAction(name)
            action.setCheckable(True)
            action.setChecked(i in rule["days"])
            actions.append(action)
        days.setMenu(menu)

        def show_days() -> None:
            days.setText(
                ", ".join(
                    name for name, action in zip(DAYS, actions, strict=True) if action.isChecked()
                )
                or "Choose days…"
            )

        for action in actions:
            action.toggled.connect(show_days)
        show_days()
        start, end = QTimeEdit(), QTimeEdit()
        for widget, key in ((start, "start"), (end, "end")):
            widget.setDisplayFormat("HH:mm")
            widget.setTime(QTime.fromString(rule[key], "HH:mm"))
            widget.setAccessibleName(f"Schedule {key} time")
        mode = mode_picker(rule["mode"])
        mode.setAccessibleName("Schedule recording mode")
        remove = QPushButton("Remove")
        set_button_variant(remove, "ghost")
        row: dict[str, Any] = {
            "days": actions,
            "start": start,
            "end": end,
            "mode": mode,
            "widgets": [days, start, end, mode, remove],
        }
        index = self.table.rowCount()
        self.table.insertRow(index)
        self.table.setRowHeight(index, 38)
        for column, widget in enumerate(row["widgets"]):
            self.table.setCellWidget(index, column, widget)
        self._rows.append(row)

        def remove_row() -> None:
            index = self._rows.index(row)
            self._rows.pop(index)
            self.table.removeRow(index)
            self.add.setEnabled(True)
            self._rules_changed()

        for action in actions:
            action.toggled.connect(self._rules_changed)
        start.timeChanged.connect(self._rules_changed)
        end.timeChanged.connect(self._rules_changed)
        mode.currentIndexChanged.connect(self._rules_changed)
        remove.clicked.connect(remove_row)
        if hasattr(self, "add"):
            self.add.setEnabled(len(self._rows) < 64)

        self._rules_changed()

    def policy(self) -> dict:
        result = {
            "mode": self.mode.value(),
            **{key: spin.value() for key, spin in self._numbers.items()},
            "weekly_rules": [
                {
                    "days": [i for i, action in enumerate(row["days"]) if action.isChecked()],
                    "start": row["start"].time().toString("HH:mm"),
                    "end": row["end"].time().toString("HH:mm"),
                    "mode": row["mode"].currentData(),
                }
                for row in self._rows
            ],
        }
        return deepcopy(self._original if result == self._initial else result)
