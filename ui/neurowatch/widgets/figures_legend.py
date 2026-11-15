"""Plugin figures and drawing keys. The floating camera overlay uses a slim
inline strip: vector glyph, count and name, with the total alongside the types.
Glyphs carry the same colours as their boxes. Other views keep their figure cards.
Text figures and the drawing key share a separate wrapping row.

Under the picture on the Plugins and Playback pages. On a wall each analysed
camera's tile floats its own over its picture (``floating``, ``place_in``): only
the background is translucent, and clicks pass through to the tile.
"""

from __future__ import annotations

import logging
from pathlib import Path

from PySide6.QtCore import QEvent, QRect, QSize, Qt
from PySide6.QtGui import QColor, QFont, QFontMetrics, QPainter, QPixmap, QResizeEvent
from PySide6.QtWidgets import (
    QBoxLayout,
    QFrame,
    QGridLayout,
    QHBoxLayout,
    QLabel,
    QLayout,
    QLayoutItem,
    QVBoxLayout,
    QWidget,
)

from neurowatch.models.analytics import Figure, Marking
from neurowatch.theme import active_colors, ui_font
from neurowatch.widgets.analytics_paint import BOTTOM_RESERVED, TOP_RESERVED, style_hex
from neurowatch.widgets.count_icons import COUNT_ICON_NAMES, count_icon

logger = logging.getLogger(__name__)

_ICONS = Path(__file__).resolve().parent.parent / "resources" / "analytics-icons"
_ICON_NAMES = COUNT_ICON_NAMES
_ICON_SIZE = 32
_CELL_MIN_WIDTH = 128
#: A floating panel narrower than this (any tile short of a full-size one) is
#: compact: one line per cell (icon, number, colour), smaller type, tighter gaps.
_COMPACT_BELOW = 1100
_COMPACT_ICON_SIZE = 14
_INLINE_ICON_SIZE = 18
_COMPACT_ROW_GAP_X = 10
#: Less room than this over a tile and only the cells are shown.
_ROW_MIN_ROOM = 80
_CELL_GAP = 6
_KEY_SWATCH = 12
_COMPACT_KEY_SWATCH = 9
_ROW_GAP_X = 16
_ROW_GAP_Y = 4
#: Background opacity over the video, in percent; text and icons stay solid.
OVERLAY_OPACITY = 50
#: Entries of the key shown; the rest are counted ("+3 more").
_KEY_LIMIT = 24


def shown_value(value: str) -> str:
    """A count with thousands separated; anything else as it came."""
    return f"{int(value):,}" if value.isdigit() else value


def _icon(name: str, dpr: float, faded: bool = False) -> QPixmap:
    """A type's icon; faded while it has counted nothing."""
    pixmap = QPixmap(str(_ICONS / f"{name}.png")) if name in _ICON_NAMES else QPixmap()
    if pixmap.isNull():
        return pixmap
    size = round(_ICON_SIZE * dpr)
    scaled = pixmap.scaled(
        size,
        size,
        Qt.AspectRatioMode.KeepAspectRatio,
        Qt.TransformationMode.SmoothTransformation,
    )
    if faded:
        solid = scaled
        scaled = QPixmap(solid.size())
        scaled.fill(Qt.GlobalColor.transparent)
        painter = QPainter(scaled)
        painter.setOpacity(0.4)
        painter.drawPixmap(0, 0, solid)
        painter.end()
    scaled.setDevicePixelRatio(dpr)
    return scaled


class _Cell(QFrame):
    """One counted type."""

    def __init__(self, parent: QWidget | None = None, *, inline: bool = False) -> None:
        super().__init__(parent)
        self.setObjectName("figureCell")
        self._inline = inline
        self._tight = False
        self._figure: Figure | None = None
        self.icon_color = "#e6eaf2"
        self.icon_shown: tuple[object, ...] | None = None
        self._compact = False
        self._color: str | None = None
        #: A type with no count yet: a compact panel has no room for it.
        self.nothing_counted = False
        row = QHBoxLayout(self)
        row.setContentsMargins(10, 6, 10, 6)
        row.setSpacing(10)
        self._row = row
        self.icon = QLabel()
        self.icon.setFixedSize(_ICON_SIZE, _ICON_SIZE)
        self.icon.setScaledContents(True)
        row.addWidget(self.icon)
        text = QVBoxLayout()
        text.setSpacing(0)
        self._text = text
        self.number = QLabel()
        self.number.setTextFormat(Qt.TextFormat.PlainText)
        self.number.setFont(ui_font(15, QFont.Weight.DemiBold))
        self.number.setObjectName("figureNumber")
        text.addWidget(self.number)
        name_row = QHBoxLayout()
        name_row.setSpacing(5)
        self.swatch = QLabel()
        self.swatch.setFixedSize(10, 10)
        name_row.addWidget(self.swatch)
        self.name = QLabel()
        self.name.setTextFormat(Qt.TextFormat.PlainText)
        self.name.setObjectName("figureName")
        name_row.addWidget(self.name, 1)
        text.addLayout(name_row)
        row.addLayout(text, 1)
        self._density()

    def set_compact(self, compact: bool) -> None:
        """Small over a small tile: the icon and colour say what it counts."""
        if compact == self._compact:
            return
        self._compact = compact
        self._density()
        self._show_color()
        self._show_icon()

    def set_tight(self, tight: bool) -> None:
        """Use less padding before wrapping a small tile's counts onto another row."""
        if tight != self._tight:
            self._tight = tight
            self._density()
            self.updateGeometry()

    def _density(self) -> None:
        compact = self._compact
        side = _COMPACT_ICON_SIZE if compact else _INLINE_ICON_SIZE if self._inline else _ICON_SIZE
        self.icon.setFixedSize(side, side)
        points = 9.5 if compact else 11.5 if self._inline else 15
        self.number.setFont(ui_font(points, QFont.Weight.DemiBold))
        self.name.setFont(ui_font(9 if self._inline else 10))
        self.name.setVisible(not compact)
        self._text.setDirection(
            QBoxLayout.Direction.LeftToRight
            if compact or self._inline
            else QBoxLayout.Direction.TopToBottom
        )
        self._text.setSpacing(6 if self._inline and not compact else 0)
        margins = (
            ((2, 2, 2, 2) if self._tight else (4, 2, 4, 2))
            if compact
            else ((6, 4, 6, 4) if self._inline else (10, 6, 10, 6))
        )
        self._row.setContentsMargins(*margins)
        self._row.setSpacing((3 if self._tight else 4) if compact else 6 if self._inline else 10)

    def labelled_width(self) -> int:
        """The natural inline width, independent of whether compact mode is active."""
        if self._figure is None:
            return 0
        number = QFontMetrics(ui_font(11.5, QFont.Weight.DemiBold))
        name = QFontMetrics(ui_font(9))
        return (
            number.horizontalAdvance(self.number.text())
            + name.horizontalAdvance(self.name.text())
            + _INLINE_ICON_SIZE
            + 12
            + 12
            + 2
        )

    def show_figure(self, figure: Figure) -> None:
        self._figure = figure
        self.number.setText(shown_value(figure.value))
        self.name.setText(figure.label)
        description = f"{figure.label}: {shown_value(figure.value)}"
        self.setToolTip(description)
        self.setAccessibleName(description)
        self._color = style_hex(figure.style)
        if self._color is not None:
            self.swatch.setStyleSheet(f"border: 2px solid {self._color}; border-radius: 3px;")
        self._show_color()
        empty = figure.value == "0"
        self.nothing_counted = empty and figure.icon != "total"
        self._show_icon()
        total = figure.icon == "total"
        if self.property("total") != total or self.property("empty") != empty:
            self.setProperty("total", total)
            self.setProperty("empty", empty)
            self.style().unpolish(self)
            self.style().polish(self)

    def _show_icon(self) -> None:
        figure = self._figure
        if figure is None:
            return
        empty = figure.value == "0"
        dpr = self.devicePixelRatioF()
        self.icon_color = self._color or ("#67e8f9" if figure.icon == "total" else "#cbd5e1")
        key = (figure.icon, empty, self.icon_color, dpr, self.icon.width(), self._inline)
        if key == self.icon_shown:
            return
        self.icon_shown = key
        pixmap = (
            count_icon(figure.icon, self.icon_color, self.icon.width(), dpr, empty)
            if self._inline
            else _icon(figure.icon, dpr, empty)
        )
        self.icon.setPixmap(pixmap)

    def _show_color(self) -> None:
        """Inline glyphs carry the drawing colour; other views retain their swatches."""
        self.swatch.setVisible(self._color is not None and not self._compact and not self._inline)
        outline = self._color if self._compact and not self._inline else None
        self.setStyleSheet(
            f"QFrame#figureCell {{ border: 1px solid {outline}; }}" if outline else ""
        )


class _FlowLayout(QLayout):
    """Items left to right, onto a new line only when they do not fit."""

    def __init__(self) -> None:
        super().__init__()
        self._items: list[QLayoutItem] = []
        self.gap_x = _ROW_GAP_X
        self.gap_y = _ROW_GAP_Y
        self.setContentsMargins(0, 0, 0, 0)

    def addItem(self, item: QLayoutItem) -> None:  # noqa: N802
        self._items.append(item)

    def count(self) -> int:
        return len(self._items)

    def itemAt(self, index: int) -> QLayoutItem | None:  # noqa: N802
        return self._items[index] if 0 <= index < len(self._items) else None

    def takeAt(self, index: int) -> QLayoutItem | None:  # noqa: N802
        return self._items.pop(index) if 0 <= index < len(self._items) else None

    def expandingDirections(self) -> Qt.Orientation:  # noqa: N802
        return Qt.Orientation(0)

    def hasHeightForWidth(self) -> bool:  # noqa: N802
        return True

    def heightForWidth(self, width: int) -> int:  # noqa: N802
        return self._arrange(QRect(0, 0, width, 0), place=False)

    def setGeometry(self, rect: QRect) -> None:  # noqa: N802
        super().setGeometry(rect)
        self._arrange(rect, place=True)

    def sizeHint(self) -> QSize:
        shown = [item.sizeHint() for item in self._items if not item.isEmpty()]
        width = sum(s.width() for s in shown) + self.gap_x * max(0, len(shown) - 1)
        return QSize(width, max((s.height() for s in shown), default=0))

    def minimumSize(self) -> QSize:  # noqa: N802
        # Never the window's minimum width: an item too wide is cut off.
        shown = [item.sizeHint().height() for item in self._items if not item.isEmpty()]
        return QSize(1, max(shown, default=0))

    def _arrange(self, rect: QRect, *, place: bool) -> int:
        """Lay the items out in ``rect``; the height they take."""
        x, y, line = rect.x(), rect.y(), 0
        for item in self._items:
            if item.isEmpty():
                continue
            size = item.sizeHint()
            width = min(size.width(), max(1, rect.width()))
            if line and x + width > rect.x() + rect.width():
                x, y, line = rect.x(), y + line + self.gap_y, 0
            if place:
                item.setGeometry(QRect(x, y, width, size.height()))
            x += width + self.gap_x
            line = max(line, size.height())
        return y + line - rect.y()


def _rgba(color: str, percent: int) -> str:
    q = QColor(color)
    return f"rgba({q.red()}, {q.green()}, {q.blue()}, {round(percent * 255 / 100)})"


class _KeyEntry(QWidget):
    """A colour, when it has one, and what it stands for."""

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        row = QHBoxLayout(self)
        row.setContentsMargins(0, 0, 0, 0)
        row.setSpacing(6)
        self.swatch = QLabel()
        self.swatch.setFixedSize(_KEY_SWATCH, _KEY_SWATCH)
        row.addWidget(self.swatch)
        self.name = QLabel()
        self.name.setObjectName("figureName")
        self.name.setTextFormat(Qt.TextFormat.PlainText)  # a plugin's text
        # Never the window's minimum width: a long name is cut off, its
        # tooltip keeps it whole.
        self.name.setMinimumWidth(1)
        row.addWidget(self.name, 1)

    def show_text(self, text: str, marking: Marking | None = None) -> None:
        if marking is None:
            self.swatch.hide()
        else:
            # The same colour as the drawing, also for a style this app has no
            # colour for (analytics_paint falls back to its accent too).
            color = style_hex(marking.style) or style_hex("accent")
            fill = color if marking.filled else "transparent"
            self.swatch.setStyleSheet(
                f"border: 2px solid {color}; border-radius: 3px; background: {fill};"
            )
            self.swatch.show()
        self.name.setText(text)
        self.setToolTip(text)

    def set_compact(self, compact: bool) -> None:
        side = _COMPACT_KEY_SWATCH if compact else _KEY_SWATCH
        self.swatch.setFixedSize(side, side)
        self.name.setFont(ui_font(8 if compact else 10))
        layout = self.layout()
        if layout is not None:
            layout.setSpacing(3 if compact else 6)


class FiguresLegend(QFrame):
    """Hidden until it has figures or a key to show."""

    def __init__(self, parent: QWidget | None = None, *, floating: bool = False) -> None:
        super().__init__(parent)
        self.setObjectName("figuresLegend")
        self._floating = floating
        self._area = QRect()
        self._compact = False
        self._figures_now: object = None
        self._laid_out: list[_Cell] = []
        self.apply_theme()
        self._cells: list[_Cell] = []
        self._columns = 0
        outer = QVBoxLayout(self)
        outer.setContentsMargins(*((10, 5, 10, 5) if floating else (10, 8, 10, 8)))
        outer.setSpacing(6)
        self._title = QLabel()
        self._title.setObjectName("figureName")
        self._title.setTextFormat(Qt.TextFormat.PlainText)
        self._title.hide()
        outer.addWidget(self._title)
        self._grid = QGridLayout()
        self._grid.setSpacing(4 if floating else _CELL_GAP)
        outer.addLayout(self._grid)
        # One row: the figures told as text, then the key.
        self._text_figures: tuple[Figure, ...] = ()
        self._markings: tuple[Marking, ...] = ()
        self._row_shown: object = None
        self._figures_shown = False
        self._text_entries: list[_KeyEntry] = []
        self._key_entries: list[_KeyEntry] = []
        self._row = _FlowLayout()
        outer.addLayout(self._row)
        self.hide()
        if floating:
            self.setAttribute(Qt.WidgetAttribute.WA_TransparentForMouseEvents)

    def apply_theme(self, theme_name: str | None = None) -> None:
        if self._floating:
            # Over video, whatever the app's theme: light text on a dark wash.
            c = active_colors("dark")
            self.setStyleSheet(
                f"""
                QFrame#figuresLegend {{ background: {_rgba("#0c1624", OVERLAY_OPACITY)};
                    border: 1px solid {_rgba("#ffffff", 12)}; border-radius: 10px; }}
                QFrame#figuresLegend QWidget {{ background: transparent; border: none; }}
                QFrame#figuresLegend QFrame#figureCell {{
                    background: transparent; border: none; border-radius: 6px;
                }}
                QFrame#figuresLegend QFrame#figureCell[total="true"] {{
                    background: {_rgba("#67e8f9", 10)};
                    border-left: 1px solid {_rgba("#67e8f9", 28)};
                }}
                QLabel#figureNumber {{ color: {c.text}; background: transparent; }}
                QLabel#figureName {{ color: {c.text}; background: transparent; }}
                """
            )
            return
        c = active_colors(theme_name)
        self.setStyleSheet(
            f"""
            QFrame#figuresLegend {{ background: {c.surface}; border-top: 1px solid {c.border}; }}
            QFrame#figureCell {{ background: {c.surface_raised}; border-radius: 8px; }}
            QFrame#figureCell[total="true"] {{ border: 1px solid {c.border_control}; }}
            QLabel#figureNumber {{ color: {c.text}; background: transparent; }}
            QLabel#figureName {{ color: {c.text_muted}; background: transparent; }}
            """
        )

    def set_figures(self, figures: tuple[Figure, ...] | list[Figure], title: str = "") -> None:
        """What to show, and whose it is when that is not obvious."""
        if (tuple(figures), title) == self._figures_now:
            return  # called as often as drawings arrive
        self._figures_now = (tuple(figures), title)
        cells = [figure for figure in figures if figure.icon]
        while len(self._cells) < len(cells):
            cell = _Cell(self, inline=self._floating)
            cell.set_compact(self._compact)
            cell.show()
            self._cells.append(cell)
        while len(self._cells) > len(cells):
            cell = self._cells.pop()
            self._grid.removeWidget(cell)
            cell.setParent(None)
            cell.deleteLater()
            self._columns = 0
        for cell, figure in zip(self._cells, cells, strict=True):
            cell.show_figure(figure)
        self._title.setText(title)
        self._title.setVisible(bool(title) and bool(figures))
        self._text_figures = tuple(figure for figure in figures if not figure.icon)
        self._show_row()
        self._reflow()
        self._figures_shown = bool(figures)
        self._show_if_needed()

    def set_markings(self, markings: tuple[Marking, ...]) -> None:
        """The key to what is drawn on the picture. Drawings arrive many times
        a second: nothing happens unless the key changed."""
        if markings == self._markings:
            return
        self._markings = markings
        self._show_row()
        self._show_if_needed()

    def _show_row(self) -> None:
        """A figure and an entry of the key about the same thing are one entry:
        its colour, name and number."""
        by_label = {marking.label: marking for marking in self._markings}
        told: list[tuple[str, Marking | None]] = [
            (
                f"{figure.label}: {figure.value}",
                by_label.get(figure.label)
                or (Marking(figure.label, figure.style) if figure.style else None),
            )
            for figure in self._text_figures
        ]
        # Told once: a plugin may report a lane and its line under one name.
        told = list(dict.fromkeys(told))
        labels = {figure.label for figure in self._text_figures}
        rest = [marking for marking in self._markings if marking.label not in labels]
        hidden = max(0, len(rest) - _KEY_LIMIT)
        key: list[tuple[str, Marking | None]] = [(m.label, m) for m in rest[:_KEY_LIMIT]]
        if hidden:
            key.append((f"+{hidden} more", None))
        if (told, key) == self._row_shown:
            return
        self._row_shown = (told, key)
        self._fill(self._text_entries, told)
        self._fill(self._key_entries, key)
        # Text first: added later, a new one would land after the key.
        for entry in (*self._text_entries, *self._key_entries):
            self._row.removeWidget(entry)
        for entry in (*self._text_entries, *self._key_entries):
            self._row.addWidget(entry)
        self._row.invalidate()

    def _fill(self, entries: list[_KeyEntry], wanted: list[tuple[str, Marking | None]]) -> None:
        while len(entries) < len(wanted):
            entry = _KeyEntry(self)
            if self._compact:
                entry.set_compact(True)
            entry.show()
            entries.append(entry)
        while len(entries) > len(wanted):
            entry = entries.pop()
            self._row.removeWidget(entry)
            entry.setParent(None)
            entry.deleteLater()
        for entry, (text, marking) in zip(entries, wanted, strict=True):
            entry.show_text(text, marking)

    def key(self) -> list[str]:
        """What the key shows, for tests."""
        return [entry.name.text() for entry in self._key_entries]

    def _show_if_needed(self) -> None:
        if self._floating:
            self._place()
        else:
            self.setVisible(self._figures_shown or bool(self._markings))

    def place_in(self, area: QRect) -> None:
        """Float over ``area``, a tile's picture."""
        if area != self._area:
            self._area = area
            self._place()

    def _place(self) -> None:
        """Along the bottom of the area, above the tile's own time and rate
        strip and below its name, as tall as what it shows. Too little room:
        first without the row of text, then not at all. This decides by the
        area alone, so a layout pass cannot flip it back and forth."""
        area = self._area
        if not self._floating:
            return
        if area.isEmpty() or not (self._figures_shown or self._markings):
            self.hide()
            return
        available = area.width() - 20
        labelled = sum(cell.labelled_width() for cell in self._cells)
        labelled += max(0, len(self._cells) - 1) * 4
        self._set_compact(area.width() < _COMPACT_BELOW or labelled > available)
        room = area.height() - TOP_RESERVED - BOTTOM_RESERVED
        row_shown = room >= _ROW_MIN_ROOM or not self._cells
        for entry in (*self._text_entries, *self._key_entries):
            entry.setVisible(row_shown)
        top = area.bottom() + 1 - BOTTOM_RESERVED
        self.setGeometry(area.x(), top - max(1, self.height()), area.width(), max(1, self.height()))
        self._reflow()
        layout = self.layout()
        height = layout.totalHeightForWidth(area.width()) if layout is not None else 0
        if height > room:
            self.hide()
            return
        self.setGeometry(area.x(), top - height, area.width(), height)
        self.show()
        self.raise_()

    def _set_compact(self, compact: bool) -> None:
        if compact != self._compact:
            self._compact = compact
            self._columns = 0
            for cell in self._cells:
                cell.set_compact(compact)
            for entry in (*self._text_entries, *self._key_entries):
                entry.set_compact(compact)
            outer = self.layout()
            if outer is not None:
                outer.setContentsMargins(*((6, 3, 6, 3) if compact else (10, 5, 10, 5)))
                outer.setSpacing(2 if compact else 6)
            self._grid.setSpacing(2 if compact else 4 if self._floating else _CELL_GAP)
            self._row.gap_x = _COMPACT_ROW_GAP_X if compact else _ROW_GAP_X
            self._row.gap_y = 1 if compact else _ROW_GAP_Y

    def event(self, event: QEvent) -> bool:
        handled = super().event(event)
        if event.type() == QEvent.Type.LayoutRequest:
            self._place()  # a cell or an entry changed size
        return handled

    def summary(self) -> list[tuple[str, str]]:
        """What the cells show, ``(name, number)``."""
        return [(cell.name.text(), cell.number.text()) for cell in self._cells]

    def other_text(self) -> str:
        return "   ·   ".join(entry.name.text() for entry in self._text_entries)

    def _reflow(self) -> None:
        shown = [c for c in self._cells if not (self._compact and c.nothing_counted)]
        layout = self.layout()
        margins = layout.contentsMargins() if layout is not None else None
        available = self.width() - (margins.left() + margins.right() if margins else 20)
        if self._floating:
            # Calculate normal spacing without toggling it off and on on each
            # layout pass (which would keep posting LayoutRequest events).
            normal = sum(c.sizeHint().width() + (5 if c._tight else 0) for c in shown)
            tight = self._compact and normal + max(0, len(shown) - 1) * 2 > available
            for cell in self._cells:
                cell.set_tight(tight)
        least = _CELL_MIN_WIDTH
        if self._compact:
            # As wide as the longest count, so big numbers are never cut off.
            least = max((cell.sizeHint().width() for cell in shown), default=0)
        gap = self._grid.horizontalSpacing()
        columns = max(1, (available + gap) // (least + gap))
        columns = min(columns, max(1, len(shown)))
        if self._floating and shown:
            # Natural widths share one row whenever they fit. A large count
            # must not force every other category to reserve that same width.
            columns = len(shown)
            widths = [cell.sizeHint().width() for cell in shown]
            while columns > 1:
                needed = sum(max(widths[c::columns]) for c in range(columns))
                if needed + gap * (columns - 1) <= available:
                    break
                columns -= 1
        if columns == self._columns and shown == self._laid_out:
            return
        self._columns = columns
        self._laid_out = shown
        for cell in self._cells:
            self._grid.removeWidget(cell)
            cell.setVisible(cell in shown)
        for column in range(self._grid.columnCount()):
            self._grid.setColumnStretch(column, 0)
        for index, cell in enumerate(shown):
            self._grid.addWidget(cell, index // columns, index % columns)
        if self._compact or self._floating:
            self._grid.setColumnStretch(columns, 1)  # packed left, each its own size
        else:
            for column in range(columns):
                self._grid.setColumnStretch(column, 1)

    def resizeEvent(self, event: QResizeEvent) -> None:
        super().resizeEvent(event)
        self._reflow()
