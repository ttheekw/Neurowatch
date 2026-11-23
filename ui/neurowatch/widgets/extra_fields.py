"""Inputs for the extra fields of one area or one class of a plugin's settings
(:class:`~neurowatch.models.plugins.ExtraField`): a number, a check box, a
multi-choice list or a row of numbers. Used by the zone editor and the
classes picker."""

from __future__ import annotations

from typing import Any

from PySide6.QtCore import Signal
from PySide6.QtWidgets import (
    QCheckBox,
    QDoubleSpinBox,
    QHBoxLayout,
    QSpinBox,
    QVBoxLayout,
    QWidget,
)

from neurowatch.models.plugins import ExtraField
from neurowatch.widgets.choice_list import ChoiceList


def number_spin(
    item: ExtraField, *, low: float | None = None, high: float | None = None
) -> QWidget:
    """A spin box for an integer or number field, within its bounds."""
    low = item.minimum if low is None else low
    high = item.maximum if high is None else high
    if item.kind == "int":
        spin = QSpinBox()
        spin.setRange(
            int(low) if low is not None else -1_000_000,
            int(high) if high is not None else 1_000_000,
        )
        return spin
    double = QDoubleSpinBox()
    double.setDecimals(2)
    double.setRange(
        float(low) if low is not None else -1e6, float(high) if high is not None else 1e6
    )
    return double


class _Choices(QWidget):
    """A multi-choice field: a few check boxes, or with many options a
    searchable scrolling list (:class:`ChoiceList`)."""

    changed = Signal()

    def __init__(self, item: ExtraField, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        self.list = ChoiceList(item.choices, item.choice_labels, noun=item.title.lower())
        self.list.changed.connect(self.changed)
        self.boxes = self.list.boxes
        layout.addWidget(self.list)


class _Numbers(QWidget):
    changed = Signal()

    def __init__(self, item: ExtraField, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        row = QHBoxLayout(self)
        row.setContentsMargins(0, 0, 0, 0)
        row.setSpacing(6)
        self.spins: list[QSpinBox | QDoubleSpinBox] = []
        for index in range(item.length):
            spin = number_spin(item)
            assert isinstance(spin, (QSpinBox, QDoubleSpinBox))
            spin.setAccessibleName(f"{item.title} {index + 1}")
            spin.valueChanged.connect(lambda _v: self.changed.emit())
            self.spins.append(spin)
            row.addWidget(spin)
        row.addStretch(1)


def make_input(item: ExtraField) -> QWidget:
    """The input for ``item`` (no value set yet; see :func:`set_value`)."""
    if item.kind == "bool":
        box = QCheckBox(item.title)
        return box
    if item.kind == "many":
        return _Choices(item)
    if item.kind == "numbers":
        return _Numbers(item)
    spin = number_spin(item)
    spin.setAccessibleName(item.title)
    return spin


def set_value(widget: QWidget, item: ExtraField, value: object) -> None:
    """Show ``value`` (the default where it is missing or does not fit)."""
    shown: Any = item.value_of(value)
    if isinstance(widget, QCheckBox):
        widget.setChecked(bool(shown))
    elif isinstance(widget, _Choices):
        for choice, box in widget.boxes.items():
            box.setChecked(choice in shown)
    elif isinstance(widget, _Numbers):
        for spin, number in zip(widget.spins, shown, strict=True):
            spin.setValue(number)
    elif isinstance(widget, QSpinBox):
        widget.setValue(int(shown))
    elif isinstance(widget, QDoubleSpinBox):
        widget.setValue(float(shown))


def get_value(widget: QWidget) -> Any:
    if isinstance(widget, QCheckBox):
        return widget.isChecked()
    if isinstance(widget, _Choices):
        return [choice for choice, box in widget.boxes.items() if box.isChecked()]
    if isinstance(widget, _Numbers):
        return [round(spin.value(), 3) for spin in widget.spins]
    if isinstance(widget, QSpinBox):
        return widget.value()
    if isinstance(widget, QDoubleSpinBox):
        return round(widget.value(), 3)
    return None


def connect_changed(widget: QWidget, slot: Any) -> None:
    """Call ``slot()`` when the user changes the input."""
    if isinstance(widget, QCheckBox):
        widget.toggled.connect(lambda _on: slot())
    elif isinstance(widget, (_Choices, _Numbers)):
        widget.changed.connect(slot)
    elif isinstance(widget, (QSpinBox, QDoubleSpinBox)):
        widget.valueChanged.connect(lambda _v: slot())
