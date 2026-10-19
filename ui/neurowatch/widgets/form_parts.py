"""Building blocks for settings pages: titled cards, segmented choices and
option cards. Styled by the dialog theme (``settingsCard``, ``segmented``,
``optionCard``)."""

from __future__ import annotations

from collections.abc import Iterable

from PySide6.QtCore import Qt, Signal
from PySide6.QtGui import QMouseEvent
from PySide6.QtWidgets import (
    QButtonGroup,
    QCheckBox,
    QFrame,
    QHBoxLayout,
    QLabel,
    QPushButton,
    QVBoxLayout,
    QWidget,
)

from neurowatch.theme import set_label_role


def settings_card(
    title: str, subtitle: str = "", parent: QWidget | None = None
) -> tuple[QFrame, QVBoxLayout, QHBoxLayout]:
    """A titled card: returns it, its body layout and its heading row (for
    a button beside the title)."""
    card = QFrame(parent)
    card.setObjectName("settingsCard")
    layout = QVBoxLayout(card)
    layout.setContentsMargins(16, 14, 16, 16)
    layout.setSpacing(10)
    heading = QHBoxLayout()
    heading.setSpacing(8)
    titles = QVBoxLayout()
    titles.setSpacing(2)
    label = QLabel(title)
    label.setObjectName("cardTitle")
    titles.addWidget(label)
    if subtitle:
        hint = QLabel(subtitle)
        hint.setWordWrap(True)
        set_label_role(hint, "hint")
        titles.addWidget(hint)
    heading.addLayout(titles, 1)
    layout.addLayout(heading)
    return card, layout, heading


class Segmented(QWidget):
    """One choice of a few, as joined buttons."""

    changed = Signal(object)

    def __init__(
        self, options: Iterable[tuple[str, object]], parent: QWidget | None = None
    ) -> None:
        super().__init__(parent)
        self.setObjectName("segmented")
        layout = QHBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(0)
        self._group = QButtonGroup(self)
        self._group.setExclusive(True)
        self._values: list[object] = []
        options = list(options)
        for index, (text, value) in enumerate(options):
            button = QPushButton(text)
            button.setCheckable(True)
            button.setFocusPolicy(Qt.FocusPolicy.TabFocus)
            button.setProperty(
                "segment",
                "only"
                if len(options) == 1
                else "first"
                if index == 0
                else "last"
                if index == len(options) - 1
                else "middle",
            )
            self._group.addButton(button, index)
            self._values.append(value)
            layout.addWidget(button)
        layout.addStretch(1)
        self._group.idToggled.connect(self._toggled)
        if self._values:
            self.set_value(self._values[0])

    def buttons(self) -> list[QPushButton]:
        return [b for b in self._group.buttons() if isinstance(b, QPushButton)]

    def value(self) -> object:
        index = self._group.checkedId()
        return self._values[index] if 0 <= index < len(self._values) else None

    def set_value(self, value: object) -> None:
        if value in self._values:
            button = self._group.button(self._values.index(value))
            if button is not None:
                button.setChecked(True)

    def _toggled(self, index: int, checked: bool) -> None:
        if checked:
            self.changed.emit(self._values[index])


class OptionCard(QFrame):
    """A checkable card: a checkbox with a description and a status line.
    Clicking anywhere on the card toggles it."""

    def __init__(self, text: str, description: str, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setObjectName("optionCard")
        self.setCursor(Qt.CursorShape.PointingHandCursor)
        layout = QVBoxLayout(self)
        layout.setContentsMargins(14, 12, 14, 12)
        layout.setSpacing(6)
        self.checkbox = QCheckBox(text)
        layout.addWidget(self.checkbox)
        self.description = QLabel(description)
        self.description.setWordWrap(True)
        set_label_role(self.description, "hint")
        layout.addWidget(self.description)
        self.status = QLabel()
        self.status.setWordWrap(True)
        self.status.hide()
        layout.addWidget(self.status)
        layout.addStretch(1)
        self.checkbox.toggled.connect(self._sync)
        self._sync(self.checkbox.isChecked())

    def set_status(self, text: str, tone: str = "muted") -> None:
        self.status.setText(text)
        self.status.setProperty("tone", tone)
        self.status.style().unpolish(self.status)
        self.status.style().polish(self.status)
        self.status.setVisible(bool(text))

    def mouseReleaseEvent(self, event: QMouseEvent) -> None:
        if event.button() == Qt.MouseButton.LeftButton and self.isEnabled():
            self.checkbox.toggle()
        super().mouseReleaseEvent(event)

    def _sync(self, checked: bool) -> None:
        self.setProperty("checked", "true" if checked else "false")
        self.style().unpolish(self)
        self.style().polish(self)
