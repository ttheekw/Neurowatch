"""Building blocks of a plugin's settings form: a searchable, scrolling
multi-choice list, a grid that wraps its cells to the width it has, and a
setting block (title, control, visible description)."""

from __future__ import annotations

from collections.abc import Mapping, Sequence

from PySide6.QtCore import Qt, QTimer, Signal
from PySide6.QtGui import QResizeEvent, QShowEvent
from PySide6.QtWidgets import (
    QCheckBox,
    QComboBox,
    QDoubleSpinBox,
    QFrame,
    QGridLayout,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QScrollArea,
    QSpinBox,
    QVBoxLayout,
    QWidget,
)

from neurowatch.models.plugins import chosen_first, needs_scroll
from neurowatch.theme import set_label_role


class _FlowBody(QWidget):
    """The check boxes of a :class:`ChoiceList`, in as many columns as fit.
    They are placed when the width is first known and again only when the
    column count changes."""

    def __init__(self, min_column: int) -> None:
        super().__init__()
        self._boxes: list[QCheckBox] = []
        self._min_column = min_column
        self._columns = 0
        self._grid = QGridLayout(self)
        self._grid.setContentsMargins(8, 6, 8, 6)
        self._grid.setHorizontalSpacing(12)
        self._grid.setVerticalSpacing(6)
        self.empty: QLabel | None = None

    def set_boxes(self, boxes: Sequence[QCheckBox]) -> None:
        """The check boxes to show (made after the list first appeared)."""
        self._boxes = list(boxes)
        for box in self._boxes:
            box.setParent(self)
        if self._columns:
            self._place(self._columns)
        elif self.width() > 1:
            self._place(self.columns_for(self.width()))

    def columns_for(self, width: int) -> int:
        return max(1, (width - 16) // self._min_column)

    def resizeEvent(self, event: QResizeEvent) -> None:
        super().resizeEvent(event)
        columns = self.columns_for(self.width())
        if columns != self._columns:
            self._place(columns)

    def _place(self, columns: int) -> None:
        self._columns = columns
        for index, box in enumerate(self._boxes):
            self._grid.addWidget(box, index // columns, index % columns)
        for column in range(columns):
            self._grid.setColumnStretch(column, 1)
        if self.empty is not None:
            self._grid.addWidget(self.empty, len(self._boxes) // columns + 1, 0, 1, columns)
        self._grid.setRowStretch(len(self._boxes) // columns + 2, 1)


class ChoiceList(QWidget):
    """Any number of choices as check boxes in as many columns as fit. More
    than a few: one line with a search box, "Chosen only" and "N of M chosen",
    and a fixed-height scroll area, the chosen ones first. ``boxes`` is keyed
    by choice."""

    changed = Signal()

    #: Height of the scrolling list, and the least width of a column.
    LIST_HEIGHT = 200
    MIN_COLUMN = 160

    def __init__(
        self,
        choices: Sequence[str],
        labels: Mapping[str, str] | None = None,
        chosen: Sequence[str] = (),
        *,
        noun: str = "options",
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        labels = labels or {}
        self.scrolls = needs_scroll(len(choices))
        outer = QVBoxLayout(self)
        outer.setContentsMargins(0, 0, 0, 0)
        outer.setSpacing(6)
        self._choices = list(choices)
        self._labels = dict(labels)
        self._initial = [c for c in self._choices if c in set(chosen)]
        self._boxes: dict[str, QCheckBox] | None = None
        self._body = _FlowBody(self.MIN_COLUMN)
        body = self._body
        if not self.scrolls:
            outer.addWidget(body)
            self._ensure()
            return
        row = QHBoxLayout()
        row.setSpacing(12)
        self.search = QLineEdit()
        self.search.setPlaceholderText(f"Search {noun}")
        self.search.setClearButtonEnabled(True)
        self.search.setAccessibleName(f"Search {noun}")
        self.search.textChanged.connect(self._filter)
        row.addWidget(self.search, 1)
        self.chosen_only = QCheckBox("Chosen only")
        self.chosen_only.toggled.connect(self._filter)
        row.addWidget(self.chosen_only)
        self.summary = QLabel()
        set_label_role(self.summary, "hint")
        row.addWidget(self.summary)
        outer.addLayout(row)
        self.empty = QLabel(f"No {noun} match.")
        set_label_role(self.empty, "hint")
        self.empty.setParent(body)
        self.empty.hide()
        body.empty = self.empty
        scroll = QScrollArea()
        scroll.setWidgetResizable(True)
        scroll.setFrameShape(QFrame.Shape.StyledPanel)
        scroll.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
        scroll.setFixedHeight(self.LIST_HEIGHT)
        scroll.setWidget(body)
        outer.addWidget(scroll)
        self._summarise()

    @property
    def boxes(self) -> dict[str, QCheckBox]:
        """The check boxes by choice; a long list makes them when it first
        appears (or when asked), so opening a form stays quick."""
        return self._ensure()

    def _ensure(self) -> dict[str, QCheckBox]:
        if self._boxes is None:
            picked = set(self._initial)
            boxes: dict[str, QCheckBox] = {}
            for choice in self._choices:
                box = QCheckBox(self._labels.get(choice, choice))
                box.setChecked(choice in picked)
                box.toggled.connect(self._toggled)
                boxes[choice] = box
            self._boxes = boxes
            order = chosen_first(self._choices, self._initial) if self.scrolls else self._choices
            self._body.set_boxes([boxes[c] for c in order])
        return self._boxes

    def showEvent(self, event: QShowEvent) -> None:
        super().showEvent(event)
        if self._boxes is None:
            QTimer.singleShot(0, self._ensure)

    def set_values(self, chosen: Sequence[str]) -> None:
        picked = set(chosen)
        for choice, box in self.boxes.items():
            box.setChecked(choice in picked)

    def values(self) -> list[str]:
        if self._boxes is None:
            return list(self._initial)
        return [choice for choice, box in self._boxes.items() if box.isChecked()]

    def _toggled(self, _on: bool) -> None:
        if self.scrolls:
            self._summarise()
            if self.chosen_only.isChecked():
                self._filter()
        self.changed.emit()

    def _summarise(self) -> None:
        self.summary.setText(f"{len(self.values())} of {len(self._choices)} chosen")

    def _filter(self, *_args: object) -> None:
        needle = self.search.text().strip().casefold()
        only = self.chosen_only.isChecked()
        shown = 0
        for choice, box in self.boxes.items():
            match = not needle or needle in box.text().casefold() or needle in choice.casefold()
            visible = match and (box.isChecked() or not only)
            box.setVisible(visible)
            shown += visible
        self.empty.setVisible(shown == 0)


class FieldGrid(QWidget):
    """Cells laid out in as many columns as fit (at most ``max_columns``, each
    at least ``cell_width`` wide), re-flowed when the width changes. Cells
    hidden with ``hide()`` leave no gap after :meth:`relayout`."""

    def __init__(
        self, cell_width: int = 230, max_columns: int = 3, parent: QWidget | None = None
    ) -> None:
        super().__init__(parent)
        self._cell_width = cell_width
        self._max_columns = max_columns
        self._cells: list[QWidget] = []
        self._columns = 0
        self._grid = QGridLayout(self)
        self._grid.setContentsMargins(0, 0, 0, 0)
        self._grid.setHorizontalSpacing(16)
        self._grid.setVerticalSpacing(10)

    def add(self, cell: QWidget) -> None:
        cell.setParent(self)  # a parentless widget counts as hidden
        self._cells.append(cell)
        self._columns = 0
        self.relayout()

    def columns_for(self, width: int) -> int:
        return max(1, min(self._max_columns, width // self._cell_width))

    def relayout(self) -> None:
        columns = self.columns_for(self.width() if self.width() > 1 else 10_000)
        self._columns = columns
        while self._grid.count():
            self._grid.takeAt(0)
        shown = [cell for cell in self._cells if not cell.isHidden()]
        for index, cell in enumerate(shown):
            self._grid.addWidget(cell, index // columns, index % columns, Qt.AlignmentFlag.AlignTop)
        for column in range(self._max_columns):
            self._grid.setColumnStretch(column, 1 if column < columns else 0)

    def resizeEvent(self, event: object) -> None:
        super().resizeEvent(event)  # type: ignore[arg-type]
        if self.columns_for(self.width()) != self._columns:
            self.relayout()


def setting_block(
    title: str, control: QWidget, description: str = "", *, with_title: bool = True
) -> QWidget:
    """A setting: its title, its control and, in muted wrapped text, its
    description (``with_title`` False for a check box that carries its own)."""
    block = QWidget()
    layout = QVBoxLayout(block)
    layout.setContentsMargins(0, 0, 0, 0)
    layout.setSpacing(4)
    if with_title:
        label = QLabel(title)
        label.setWordWrap(True)
        set_label_role(label, "emphasis")
        layout.addWidget(label)
    if isinstance(control, (QSpinBox, QDoubleSpinBox, QComboBox)):
        control.setMinimumWidth(120)
        layout.addWidget(control, 0, Qt.AlignmentFlag.AlignLeft)
    else:
        layout.addWidget(control)
    if description:
        note = QLabel(description)
        note.setWordWrap(True)
        set_label_role(note, "hint")
        layout.addWidget(note)
    return block
