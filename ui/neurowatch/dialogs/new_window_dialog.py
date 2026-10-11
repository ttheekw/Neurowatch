"""Pick which wall page moves to a new window.

Each page is a card with a small map of its tiles, so pages with large tiles
are recognisable at a glance. The last page is chosen by default.
"""

from __future__ import annotations

from dataclasses import dataclass

from PySide6.QtCore import QRect, QRectF, QSize, Qt
from PySide6.QtGui import QColor, QFont, QFontMetrics, QMouseEvent, QPainter, QPaintEvent, QPen
from PySide6.QtWidgets import (
    QAbstractButton,
    QButtonGroup,
    QDialog,
    QDialogButtonBox,
    QGridLayout,
    QHBoxLayout,
    QLabel,
    QPushButton,
    QScrollArea,
    QVBoxLayout,
    QWidget,
)

from neurowatch.models.wall_layout import GRID_PRESETS, Slot, grid_label, pack, page_count
from neurowatch.theme import active_colors, apply_dialog_theme, set_label_role, style_button_box
from neurowatch.widgets.window_chrome import install_dialog_chrome

_CARDS_PER_ROW = 4


@dataclass
class PageSummary:
    """One wall page: its tiles as (slot, camera name)."""

    index: int  # 0-based page number
    tiles: list[tuple[Slot, str]]


class _PageCard(QAbstractButton):
    """A checkable card: "Page N", a map of its tiles, its camera count."""

    _MAP_W, _MAP_H = 168, 96

    def __init__(self, page: PageSummary, rows: int, cols: int, parent: QWidget) -> None:
        super().__init__(parent)
        self.page = page
        self._rows, self._cols = rows, cols
        self.setCheckable(True)
        self.setCursor(Qt.CursorShape.PointingHandCursor)
        self.setFocusPolicy(Qt.FocusPolicy.StrongFocus)
        count = len(page.tiles)
        self._caption = f"{count} camera{'s' if count != 1 else ''}"
        self.setAccessibleName(f"Page {page.index + 1}, {self._caption}")
        self.setToolTip(", ".join(name for _slot, name in page.tiles))

    def sizeHint(self) -> QSize:
        return QSize(self._MAP_W + 24, self._MAP_H + 64)

    def mouseDoubleClickEvent(self, event: QMouseEvent) -> None:
        dialog = self.window()
        if event.button() == Qt.MouseButton.LeftButton and isinstance(dialog, QDialog):
            self.setChecked(True)
            dialog.accept()
            return
        super().mouseDoubleClickEvent(event)

    def paintEvent(self, _event: QPaintEvent) -> None:
        colors = active_colors()
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        selected = self.isChecked()
        hovered = self.underMouse()
        frame = QRectF(self.rect()).adjusted(1, 1, -1, -1)
        border = QColor(colors.accent if selected else colors.border_strong)
        if self.hasFocus() and not selected:
            border = QColor(colors.focus_ring)
        fill = QColor(colors.accent_subtle if selected else colors.surface_raised)
        if hovered and not selected:
            fill = QColor(colors.hover)
        painter.setPen(QPen(border, 2 if selected or self.hasFocus() else 1))
        painter.setBrush(fill)
        painter.drawRoundedRect(frame, 8, 8)

        painter.setPen(QColor(colors.text))
        painter.setFont(QFont("Sans", 10, QFont.Weight.DemiBold))
        painter.drawText(
            QRect(12, 8, self.width() - 24, 20),
            Qt.AlignmentFlag.AlignLeft | Qt.AlignmentFlag.AlignVCenter,
            f"Page {self.page.index + 1}",
        )
        painter.setPen(QColor(colors.text_muted))
        painter.setFont(QFont("Sans", 9))
        painter.drawText(
            QRect(12, self.height() - 28, self.width() - 24, 20),
            Qt.AlignmentFlag.AlignLeft | Qt.AlignmentFlag.AlignVCenter,
            self._caption,
        )
        self._paint_map(painter, QRect(12, 32, self.width() - 24, self.height() - 64), selected)
        painter.end()

    def _paint_map(self, painter: QPainter, area: QRect, selected: bool) -> None:
        colors = active_colors()
        cell_w = area.width() / self._cols
        cell_h = area.height() / self._rows
        painter.setPen(Qt.PenStyle.NoPen)
        painter.setBrush(QColor(colors.surface_sunken))
        painter.drawRoundedRect(QRectF(area), 4, 4)
        tile_fill = QColor(colors.accent if selected else colors.border_control)
        tile_fill.setAlpha(150 if selected else 110)
        name_font = QFont("Sans", 7)
        metrics = QFontMetrics(name_font)
        painter.setFont(name_font)
        for (row, col, row_span, col_span), name in self.page.tiles:
            rect = QRectF(
                area.left() + col * cell_w,
                area.top() + row * cell_h,
                col_span * cell_w,
                row_span * cell_h,
            ).adjusted(1.5, 1.5, -1.5, -1.5)
            painter.setPen(Qt.PenStyle.NoPen)
            painter.setBrush(tile_fill)
            painter.drawRoundedRect(rect, 2, 2)
            if rect.width() >= 34 and rect.height() >= 14:
                painter.setPen(QColor(colors.on_accent if selected else colors.text))
                text = metrics.elidedText(name, Qt.TextElideMode.ElideRight, int(rect.width()) - 6)
                painter.drawText(rect, Qt.AlignmentFlag.AlignCenter, text)


@dataclass
class WindowSummary:
    """An open extra window, as a destination."""

    number: int
    pages: int
    cameras: int


_NEW_WINDOW = 0  # destination id of "New window" (open windows use their number)


class _DestinationCard(QAbstractButton):
    """A checkable card naming where the page goes."""

    def __init__(self, glyph: str, title: str, caption: str, parent: QWidget) -> None:
        super().__init__(parent)
        self._glyph, self._title, self._caption = glyph, title, caption
        self.setCheckable(True)
        self.setCursor(Qt.CursorShape.PointingHandCursor)
        self.setFocusPolicy(Qt.FocusPolicy.StrongFocus)
        self.setAccessibleName(f"{title}, {caption}")
        self.setText(title)

    def sizeHint(self) -> QSize:
        return QSize(_PageCard._MAP_W + 44, 58)

    def mouseDoubleClickEvent(self, event: QMouseEvent) -> None:
        dialog = self.window()
        if event.button() == Qt.MouseButton.LeftButton and isinstance(dialog, QDialog):
            self.setChecked(True)
            dialog.accept()
            return
        super().mouseDoubleClickEvent(event)

    def paintEvent(self, _event: QPaintEvent) -> None:
        colors = active_colors()
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        selected = self.isChecked()
        frame = QRectF(self.rect()).adjusted(1, 1, -1, -1)
        border = QColor(colors.accent if selected else colors.border_strong)
        if self.hasFocus() and not selected:
            border = QColor(colors.focus_ring)
        fill = QColor(colors.accent_subtle if selected else colors.surface_raised)
        if self.underMouse() and not selected:
            fill = QColor(colors.hover)
        painter.setPen(QPen(border, 2 if selected or self.hasFocus() else 1))
        painter.setBrush(fill)
        painter.drawRoundedRect(frame, 8, 8)

        badge = QRectF(12, (self.height() - 30) / 2, 30, 30)
        painter.setPen(Qt.PenStyle.NoPen)
        painter.setBrush(QColor(colors.accent if selected else colors.surface_sunken))
        painter.drawRoundedRect(badge, 8, 8)
        painter.setPen(QColor(colors.on_accent if selected else colors.text_muted))
        painter.setFont(QFont("Sans", 11, QFont.Weight.Bold))
        painter.drawText(badge, Qt.AlignmentFlag.AlignCenter, self._glyph)

        text_x = int(badge.right()) + 10
        width = self.width() - text_x - 10
        painter.setPen(QColor(colors.text))
        painter.setFont(QFont("Sans", 10, QFont.Weight.DemiBold))
        painter.drawText(
            QRect(text_x, 9, width, 20),
            Qt.AlignmentFlag.AlignLeft | Qt.AlignmentFlag.AlignVCenter,
            self._title,
        )
        painter.setPen(QColor(colors.text_muted))
        small = QFont("Sans", 9)
        painter.setFont(small)
        caption = QFontMetrics(small).elidedText(self._caption, Qt.TextElideMode.ElideRight, width)
        painter.drawText(
            QRect(text_x, 29, width, 18),
            Qt.AlignmentFlag.AlignLeft | Qt.AlignmentFlag.AlignVCenter,
            caption,
        )
        painter.end()


def _plural(count: int, word: str) -> str:
    return f"{count} {word}{'' if count == 1 else 's'}"


class NewWindowDialog(QDialog):
    """Asks which page of the wall to move, and to which window: a new one,
    or (when some are open) one of the open windows."""

    def __init__(
        self,
        pages: list[PageSummary],
        rows: int,
        cols: int,
        parent: QWidget | None = None,
        *,
        windows: list[WindowSummary] | None = None,
        destination: int | None = None,
    ) -> None:
        super().__init__(parent)
        windows = windows or []
        self.setWindowTitle("New Window")
        apply_dialog_theme(self)

        heading = "Move a page to a window" if windows else "Move a page to a new window"
        layout = install_dialog_chrome(self, heading)
        subtitle = QLabel(
            "Its cameras leave this window. A window you can move to another monitor "
            "and maximize shows them; with several pages it cycles through them. "
            "Close that window to bring them back."
        )
        subtitle.setObjectName("dialogSubtitle")
        subtitle.setWordWrap(True)
        layout.addWidget(subtitle)

        if windows:
            layout.addWidget(self._section("Page to move"))
        cards = QWidget()
        grid = QGridLayout(cards)
        grid.setContentsMargins(0, 4, 0, 4)
        grid.setSpacing(10)
        self._group = QButtonGroup(self)
        self._group.setExclusive(True)
        self._cards: list[_PageCard] = []
        for i, page in enumerate(pages):
            card = _PageCard(page, rows, cols, cards)
            self._group.addButton(card, page.index)
            grid.addWidget(card, i // _CARDS_PER_ROW, i % _CARDS_PER_ROW)
            self._cards.append(card)
        if self._cards:
            self._cards[-1].setChecked(True)  # the last page by default

        scroll = QScrollArea()
        scroll.setWidget(cards)
        scroll.setWidgetResizable(True)
        scroll.setFrameShape(QScrollArea.Shape.NoFrame)
        scroll.setStyleSheet(
            "QScrollArea, QScrollArea > QWidget > QWidget { background: transparent; }"
        )
        scroll.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
        card_h = _PageCard.sizeHint(self._cards[0]).height() if self._cards else 160
        visible_rows = min(2, max(1, (len(pages) + _CARDS_PER_ROW - 1) // _CARDS_PER_ROW))
        scroll.setMinimumHeight(visible_rows * (card_h + 10) + 8)
        per_row = min(max(len(pages), len(windows) + 1 if windows else 1), _CARDS_PER_ROW)
        scroll.setMinimumWidth(per_row * (_PageCard._MAP_W + 34) + 8)
        layout.addWidget(scroll, stretch=1)

        # Where to: shown only when a window is open (else: a new window).
        self._destinations = QButtonGroup(self)
        self._destinations.setExclusive(True)
        self._destination_cards: list[_DestinationCard] = []
        if windows:
            layout.addWidget(self._section("Move to"))
            row = QHBoxLayout()
            row.setSpacing(10)
            choices = [(_NEW_WINDOW, "＋", "New window", "Another monitor if any")]
            choices += [
                (
                    w.number,
                    "⧉",
                    f"Window {w.number}",
                    f"{_plural(w.pages, 'page')} · {_plural(w.cameras, 'camera')}",
                )
                for w in windows
            ]
            for number, glyph, title, caption in choices:
                target = _DestinationCard(glyph, title, caption, self)
                self._destinations.addButton(target, number)
                self._destination_cards.append(target)
                row.addWidget(target)
            row.addStretch(1)
            layout.addLayout(row)
            preferred = destination if destination in [w.number for w in windows] else None
            chosen = self._destinations.button(_NEW_WINDOW if preferred is None else preferred)
            chosen.setChecked(True)
            self._destinations.idToggled.connect(self._sync_ok_text)

        # The new window's grid (an open window keeps its own).
        self._pages = {page.index: page for page in pages}
        self._grid_box = QWidget(self)
        grid_section = QVBoxLayout(self._grid_box)
        grid_section.setContentsMargins(0, 4, 0, 0)
        grid_section.setSpacing(6)
        grid_section.addWidget(self._section("Window grid"))
        chips = QHBoxLayout()
        chips.setSpacing(6)
        self._grids = QButtonGroup(self)
        self._grids.setExclusive(True)
        self._grid_choices = [(r, c) for _label, r, c in GRID_PRESETS]
        if (rows, cols) not in self._grid_choices:
            self._grid_choices.insert(0, (rows, cols))  # this wall's custom grid
        for index, (r, c) in enumerate(self._grid_choices):
            chip = QPushButton(grid_label(r, c), self._grid_box)
            chip.setObjectName("gridChoice")
            chip.setCheckable(True)
            chip.setCursor(Qt.CursorShape.PointingHandCursor)
            chip.setToolTip(f"{_plural(c, 'column')} × {_plural(r, 'row')}")
            chip.setChecked((r, c) == (rows, cols))  # this wall's grid by default
            self._grids.addButton(chip, index)
            chips.addWidget(chip)
        chips.addStretch(1)
        grid_section.addLayout(chips)
        self._grid_hint = QLabel(self._grid_box)
        set_label_role(self._grid_hint, "hint")
        grid_section.addWidget(self._grid_hint)
        layout.addWidget(self._grid_box)
        self._grids.idToggled.connect(self._sync_grid_hint)
        self._group.idToggled.connect(self._sync_grid_hint)
        self._sync_grid_hint()

        self._buttons = QDialogButtonBox(
            QDialogButtonBox.StandardButton.Ok | QDialogButtonBox.StandardButton.Cancel
        )
        self._buttons.accepted.connect(self.accept)
        self._buttons.rejected.connect(self.reject)
        style_button_box(self._buttons)
        layout.addWidget(self._buttons)
        self._sync_ok_text()
        if self._cards:
            self._cards[-1].setFocus()

    @staticmethod
    def _section(text: str) -> QLabel:
        label = QLabel(text)
        set_label_role(label, "sectionLabel")
        return label

    def _sync_ok_text(self, *_args: object) -> None:
        window = self.selected_window
        ok = self._buttons.button(QDialogButtonBox.StandardButton.Ok)
        ok.setText("Open Window" if window is None else f"Add to Window {window}")
        self._grid_box.setVisible(window is None)

    def _sync_grid_hint(self, *_args: object) -> None:
        """How the chosen page fits the chosen grid."""
        page = self._pages.get(self.selected_page if self.selected_page is not None else -1)
        rows, cols = self.selected_grid
        if page is None:
            self._grid_hint.setText("")
            return
        ids = [str(i) for i in range(len(page.tiles))]
        spans = {str(i): (slot[2], slot[3]) for i, (slot, _name) in enumerate(page.tiles)}
        count = page_count(pack(ids, spans, rows, cols))
        cameras = _plural(len(page.tiles), "camera")
        where = f"{_plural(count, 'page')} at {grid_label(rows, cols)}"
        cycle = " — the window cycles through them" if count > 1 else ""
        self._grid_hint.setText(f"Page {page.index + 1}: {cameras} on {where}{cycle}.")

    @property
    def selected_grid(self) -> tuple[int, int]:
        """(rows, cols) for a new window."""
        index = self._grids.checkedId()
        return self._grid_choices[index] if index >= 0 else self._grid_choices[0]

    @property
    def selected_page(self) -> int | None:
        """0-based page to move, or None when nothing is selected."""
        page = self._group.checkedId()
        return None if page < 0 else page

    @property
    def selected_window(self) -> int | None:
        """Number of the open window to move the page to; None: a new one."""
        number = self._destinations.checkedId()
        return None if number in (-1, _NEW_WINDOW) else number
