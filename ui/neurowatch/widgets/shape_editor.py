"""Everything a plugin draws on the picture, edited on one snapshot: pick what to
change, then click. Points can also be moved with the arrow keys and removed with
Delete, so dragging is never the only way. Where a plugin's settings have more
than zones (vehicle counting: lanes and detection areas); a plugin with only zones keeps
the simpler ``ZoneEditor``. The portal's ``PluginDrawingEditor.vue`` is its twin.
"""

from __future__ import annotations

from typing import Any

from PySide6.QtCore import QPointF, QRectF, QSize, Qt, Signal
from PySide6.QtGui import (
    QColor,
    QFont,
    QFontMetricsF,
    QIcon,
    QKeyEvent,
    QMouseEvent,
    QPainter,
    QPen,
    QPixmap,
    QPolygonF,
)
from PySide6.QtWidgets import (
    QFormLayout,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QListWidget,
    QListWidgetItem,
    QPushButton,
    QSizePolicy,
    QVBoxLayout,
    QWidget,
)

from neurowatch.models.lanes import Lane
from neurowatch.models.plugins import FormField
from neurowatch.models.shapes import DEFAULT_MAX_POINTS, Item, ShapeSet, issue, lane_summary
from neurowatch.theme import set_button_variant, set_label_role, set_status_tone
from neurowatch.widgets.figures_legend import _FlowLayout
from neurowatch.widgets.form_parts import Segmented
from neurowatch.widgets.zone_canvas import SnapshotCanvas
from neurowatch.widgets.zone_editor import SnapshotSource

#: As the plugin draws them on the video.
LANE_COLOR = "#22c55e"
LINE_COLOR = "#f59e0b"
WATCHED_COLOR = "#94a3b8"
IGNORED_COLOR = "#ef4444"
LINE, AREA = "line", "area"
_SINGULAR = {"lanes": "Lane", "lines": "Line", "areas": "Area", "zones": "Area"}
#: One arrow-key press, as a share of the picture; with Shift, five.
_NUDGE = 0.01
#: The column beside the picture. Fixed, so the picture stays put between tabs.
_SIDE_WIDTH = 320
_LIST_ROW_HEIGHT = 44


def _swatch(color: str, kind: str) -> QPixmap:
    """A key's mark: a line, a line with its arrow, or an area (filled if ``solid``)."""
    picture = QPixmap(22, 14)
    picture.fill(Qt.GlobalColor.transparent)
    painter = QPainter(picture)
    painter.setRenderHint(QPainter.RenderHint.Antialiasing)
    pen = QPen(QColor(color), 2.5)
    painter.setPen(pen)
    if kind in ("line", "arrow"):
        y = 4 if kind == "arrow" else 7
        painter.drawLine(QPointF(1, y), QPointF(21, y))
        if kind == "arrow":
            painter.drawLine(QPointF(11, y), QPointF(11, 13))
    else:
        fill = QColor(color)
        fill.setAlpha(120 if kind == "solid" else 40)
        painter.setBrush(fill)
        painter.setPen(QPen(QColor(color), 1.5))
        painter.drawRect(QRectF(2, 2, 18, 10))
    painter.end()
    return picture


def _key_entries(fields: list[FormField]) -> list[tuple[str, str, str]]:
    """What the picture's colours mean, for the kinds this plugin draws."""
    entries: list[tuple[str, str, str]] = []
    for field in fields:
        if field.kind == "lanes":
            entries += [("Crossing line", LINE_COLOR, "line"), ("Lane area", LANE_COLOR, "area")]
        elif field.kind == "lines":
            entries.append((field.title, LINE_COLOR, "arrow"))
        elif field.kind == "areas":
            entries.append((field.title, LINE_COLOR, "area"))
        elif field.kind == "zones":
            entries += [("Watched", WATCHED_COLOR, "area"), ("Ignored", IGNORED_COLOR, "solid")]
    return entries


def _mean(points: list[QPointF]) -> QPointF:
    return QPointF(
        sum(p.x() for p in points) / len(points), sum(p.y() for p in points) / len(points)
    )


class ShapeCanvas(SnapshotCanvas):
    """All the shapes over the camera's picture; the chosen one's points edit."""

    changed = Signal()

    def __init__(self, shapes: ShapeSet, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.shapes = shapes
        self.field: FormField | None = shapes.fields[0] if shapes.fields else None
        self.item: Item | None = None
        self.mode = LINE
        self._picture = QPixmap()
        self._dragging: int | None = None
        self._hover: int | None = None
        #: The point the keys move: the one last touched.
        self.active_point: int | None = None
        self.placeholder = "Loading the camera picture…"
        self.setMouseTracking(True)
        self.setCursor(Qt.CursorShape.CrossCursor)
        self.setFocusPolicy(Qt.FocusPolicy.StrongFocus)
        self.setAccessibleName("Shapes on the camera picture")
        self.setToolTip(
            "Click to add a point, drag a point to move it, right-click a point to remove it. "
            "The arrow keys move the last point touched, Delete removes it."
        )

    # ---- The picture --------------------------------------------------------
    def set_snapshot(self, picture: QPixmap) -> None:
        self._picture = picture
        self.update()

    def has_snapshot(self) -> bool:
        return not self._picture.isNull()

    def set_placeholder(self, text: str) -> None:
        self.placeholder = text
        self.update()

    def _placeholder_text(self) -> str:
        return self.placeholder

    def image_rect(self) -> QRectF:
        if self._picture.isNull():
            return super().image_rect()
        scale = min(self.width() / self._picture.width(), self.height() / self._picture.height())
        width, height = self._picture.width() * scale, self._picture.height() * scale
        return QRectF((self.width() - width) / 2, (self.height() - height) / 2, width, height)

    def image_point(self, point: QPointF) -> list[float]:
        rect = self.image_rect()
        return [
            round(max(0.0, min(1.0, (point.x() - rect.left()) / max(1.0, rect.width()))), 4),
            round(max(0.0, min(1.0, (point.y() - rect.top()) / max(1.0, rect.height()))), 4),
        ]

    def _at(self, rect: QRectF, point: list[float]) -> QPointF:
        return QPointF(rect.left() + point[0] * rect.width(), rect.top() + point[1] * rect.height())

    # ---- What is being edited -----------------------------------------------
    def editing_line(self) -> bool:
        kind = self.field.kind if self.field is not None else ""
        return kind == "lines" or (kind == "lanes" and self.mode == LINE)

    def target(self) -> list[list[float]] | None:
        item = self.item
        if item is None:
            return None
        if isinstance(item, Lane):
            return item.line if self.mode == LINE else item.area
        points: list[list[float]] = item["points"]
        return points

    def max_points(self) -> int:
        if self.editing_line():
            return 2
        return (self.field.max_points if self.field else 0) or DEFAULT_MAX_POINTS

    def _point_at(self, position: QPointF) -> int | None:
        target = self.target()
        if not target:
            return None
        rect = self.image_rect()
        return next(
            (
                i
                for i, point in enumerate(target)
                if (self._at(rect, point) - position).manhattanLength() < 12
            ),
            None,
        )

    def mousePressEvent(self, event: QMouseEvent) -> None:
        target = self.target()
        if not self.isEnabled() or target is None:
            return
        if not self.image_rect().contains(event.position()):
            return
        closest = self._point_at(event.position())
        if event.button() == Qt.MouseButton.RightButton:
            if closest is not None:
                target.pop(closest)
                self.active_point = None
                self.changed.emit()
                self.update()
        elif event.button() == Qt.MouseButton.LeftButton:
            if closest is not None:
                self._dragging = closest
                self.active_point = closest
                return
            if len(target) >= self.max_points():
                return
            target.append(self.image_point(event.position()))
            self.active_point = len(target) - 1
            self.changed.emit()
            self.update()

    def mouseMoveEvent(self, event: QMouseEvent) -> None:
        target = self.target()
        if self._dragging is None or target is None:
            hover = self._point_at(event.position())
            if hover != self._hover:
                self._hover = hover
                self.setCursor(
                    Qt.CursorShape.SizeAllCursor
                    if hover is not None
                    else Qt.CursorShape.CrossCursor
                )
                self.update()
            return
        target[self._dragging] = self.image_point(event.position())
        self.changed.emit()
        self.update()

    def mouseReleaseEvent(self, event: QMouseEvent) -> None:
        self._dragging = None

    def keyPressEvent(self, event: QKeyEvent) -> None:
        target = self.target()
        if not target:
            super().keyPressEvent(event)
            return
        index = self.active_point if self.active_point is not None else len(target) - 1
        if not 0 <= index < len(target):
            index = len(target) - 1
        if event.key() in (Qt.Key.Key_Delete, Qt.Key.Key_Backspace):
            target.pop(index)
            self.active_point = None
        else:
            step = {
                Qt.Key.Key_Left: (-1, 0),
                Qt.Key.Key_Right: (1, 0),
                Qt.Key.Key_Up: (0, -1),
                Qt.Key.Key_Down: (0, 1),
            }.get(Qt.Key(event.key()))
            if step is None:
                super().keyPressEvent(event)
                return
            size = _NUDGE * (5 if event.modifiers() & Qt.KeyboardModifier.ShiftModifier else 1)
            point = target[index]
            target[index] = [
                round(min(1.0, max(0.0, point[0] + step[0] * size)), 4),
                round(min(1.0, max(0.0, point[1] + step[1] * size)), 4),
            ]
            self.active_point = index
        event.accept()
        self.changed.emit()
        self.update()

    # ---- Painting -----------------------------------------------------------
    def _paint_background(self, painter: QPainter, rect: QRectF) -> bool:
        if self._picture.isNull():
            return False
        painter.drawPixmap(rect, self._picture, QRectF(self._picture.rect()))
        return True

    def _paint_overlay(self, painter: QPainter, rect: QRectF) -> None:
        # The chosen one last: over the others, and its points on top.
        for field in self.shapes.fields:
            painter.setOpacity(1.0 if field is self.field else 0.55)
            for item in self.shapes.items(field):
                if item is not self.item:
                    self._paint_item(painter, rect, field, item, False)
        painter.setOpacity(1.0)
        if self.item is not None and self.field is not None:
            self._paint_item(painter, rect, self.field, self.item, True)
            target = self.target() or []
            color = QColor(LINE_COLOR if self.editing_line() else LANE_COLOR)
            for index, point in enumerate(target):
                at = self._at(rect, point)
                active = index == self.active_point and self.hasFocus()
                painter.setPen(QPen(QColor("#2563eb") if active else color, 3 if active else 2))
                painter.setBrush(QColor("#ffffff"))
                radius = 6.0 if index == self._hover or active else 4.5
                painter.drawEllipse(at, radius, radius)

    def _paint_item(
        self, painter: QPainter, rect: QRectF, field: FormField, item: Item, chosen: bool
    ) -> None:
        name = str(item.name if isinstance(item, Lane) else item.get("name") or "").strip()
        if isinstance(item, Lane):
            self._paint_area(painter, rect, item.area, QColor(LANE_COLOR), chosen, False)
            line = [self._at(rect, p) for p in item.line]
            area = [self._at(rect, p) for p in item.area]
            if len(line) == 2:
                painter.setPen(QPen(QColor(LINE_COLOR), 4 if chosen else 2.5))
                painter.drawLine(line[0], line[1])
            elif line:
                painter.setPen(QPen(QColor(LINE_COLOR), 2))
                painter.drawPoint(line[0])
            spot = line if len(line) == 2 or not area else area
            if spot:
                self._label(painter, _mean(spot), name or "Unnamed lane", chosen)
            return
        points = [self._at(rect, p) for p in item["points"]]
        if not points:
            return
        if field.kind == "lines":
            painter.setPen(QPen(QColor(LINE_COLOR), 4 if chosen else 2.5))
            painter.setBrush(Qt.BrushStyle.NoBrush)
            if len(points) == 2:
                painter.drawLine(points[0], points[1])
                self._paint_arrow(painter, points[0], points[1])
            self._label(painter, _mean(points), name or "Unnamed line", chosen)
            return
        ignored = field.kind == "zones" and item.get("kind") == "exclude"
        color = IGNORED_COLOR if ignored else WATCHED_COLOR if field.kind == "zones" else LINE_COLOR
        self._paint_area(painter, rect, item["points"], QColor(color), chosen, ignored)
        self._label(painter, _mean(points), name or "Unnamed area", chosen)

    def _paint_area(
        self,
        painter: QPainter,
        rect: QRectF,
        points: list[list[float]],
        color: QColor,
        chosen: bool,
        filled: bool,
    ) -> None:
        area = [self._at(rect, p) for p in points]
        if not area:
            return
        fill = QColor(color)
        fill.setAlpha((90 if filled else 50) if chosen else (70 if filled else 25))
        painter.setPen(QPen(color, 2.5 if chosen else 1.5))
        painter.setBrush(fill)
        if len(area) >= 3:
            painter.drawPolygon(QPolygonF(area))
        else:
            painter.drawPolyline(QPolygonF(area))
        painter.setBrush(Qt.BrushStyle.NoBrush)

    def _paint_arrow(self, painter: QPainter, a: QPointF, b: QPointF) -> None:
        """Towards the line's In side (its right), as the plugin counts it."""
        along = b - a
        length = max(1.0, (along.x() ** 2 + along.y() ** 2) ** 0.5)
        middle = _mean([a, b])
        painter.drawLine(
            middle, middle + QPointF(-along.y() / length * 20, along.x() / length * 20)
        )

    def _label(self, painter: QPainter, at: QPointF, text: str, chosen: bool) -> None:
        font = QFont(self.font())
        font.setPixelSize(13)
        font.setWeight(QFont.Weight.DemiBold)
        painter.setFont(font)
        metrics = QFontMetricsF(font)
        text = metrics.elidedText(text, Qt.TextElideMode.ElideRight, 180)
        box = QRectF(0, 0, metrics.horizontalAdvance(text) + 10, metrics.height() + 4)
        box.moveCenter(QPointF(at.x(), at.y() - box.height()))
        painter.setPen(Qt.PenStyle.NoPen)
        painter.setBrush(QColor(0, 0, 0, 200 if chosen else 130))
        painter.drawRoundedRect(box, 4, 4)
        painter.setPen(QColor("#ffffff"))
        painter.drawText(box, Qt.AlignmentFlag.AlignCenter, text)


class ShapeEditor(QWidget):
    """The canvas and what chooses what it edits. Read ``values()`` to save;
    ``problem()`` says what is missing."""

    changed = Signal()

    def __init__(
        self,
        fields: list[FormField],
        config: dict[str, Any],
        *,
        endpoint: str | None = None,
        camera_id: str | None = None,
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        self._camera_id = camera_id
        self.shapes = ShapeSet(fields, config)
        self._snapshots = SnapshotSource(endpoint, self)
        self._snapshots.picture.connect(self._on_picture)
        self._snapshots.failed.connect(self._on_no_picture)

        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(8)
        self.tabs = Segmented([(f.title, f.key) for f in self.shapes.fields])
        self.tabs.setAccessibleName("What to draw")
        self.tabs.changed.connect(self._on_tab)
        layout.addWidget(self.tabs)
        self.description = QLabel()
        self.description.setWordWrap(True)
        set_label_role(self.description, "hint")
        layout.addWidget(self.description)

        editor = QHBoxLayout()
        editor.setSpacing(12)
        picture_column = QVBoxLayout()
        picture_column.setSpacing(8)
        self.canvas = ShapeCanvas(self.shapes)
        self.canvas.changed.connect(self._refresh)
        picture_column.addWidget(self.canvas)
        self.key = _FlowLayout()
        self.key.gap_x, self.key.gap_y = 14, 4
        for text, color, kind in _key_entries(self.shapes.fields):
            entry = QWidget()
            entry.setObjectName("shapePart")
            row = QHBoxLayout(entry)
            row.setContentsMargins(0, 0, 0, 0)
            row.setSpacing(5)
            mark = QLabel()
            mark.setPixmap(_swatch(color, kind))
            row.addWidget(mark)
            caption = QLabel(text)
            set_label_role(caption, "hint")
            row.addWidget(caption)
            self.key.addWidget(entry)
        picture_column.addLayout(self.key)
        self.hint = QLabel()
        self.hint.setWordWrap(True)
        set_label_role(self.hint, "hint")
        picture_column.addWidget(self.hint)
        self.status = QLabel("" if camera_id else "Choose a camera to see its picture here.")
        self.status.setWordWrap(True)
        set_label_role(self.status, "hint")
        picture_column.addWidget(self.status)
        self.snapshot_button = QPushButton("Refresh Picture")
        self.snapshot_button.setToolTip("Take a new picture from the camera to draw on")
        set_button_variant(self.snapshot_button, "secondary")
        self.snapshot_button.clicked.connect(self.load_snapshot)
        self.snapshot_button.setEnabled(bool(camera_id))
        picture_column.addWidget(self.snapshot_button, 0, Qt.AlignmentFlag.AlignLeft)
        editor.addLayout(picture_column, 1)

        side_panel = QWidget()
        side_panel.setObjectName("shapePart")
        side_panel.setFixedWidth(_SIDE_WIDTH)
        side_panel.setSizePolicy(QSizePolicy.Policy.Fixed, QSizePolicy.Policy.Preferred)
        side = QVBoxLayout(side_panel)
        side.setContentsMargins(0, 0, 0, 0)
        side.setSpacing(8)
        self.picker = QListWidget()
        self.picker.setObjectName("laneList")
        self.picker.setFixedHeight(3 * _LIST_ROW_HEIGHT + 10)
        self.picker.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
        self.picker.setTextElideMode(Qt.TextElideMode.ElideRight)
        self.picker.setWordWrap(False)
        self.picker.currentRowChanged.connect(self._select)
        side.addWidget(self.picker)
        adding = QHBoxLayout()
        adding.setSpacing(6)
        self.add_button = QPushButton()
        set_button_variant(self.add_button, "secondary")
        self.add_button.clicked.connect(lambda: self.add())
        adding.addWidget(self.add_button)
        self.add_ignored = QPushButton("+ Ignore an Area")
        set_button_variant(self.add_ignored, "secondary")
        self.add_ignored.clicked.connect(lambda: self.add("exclude"))
        adding.addWidget(self.add_ignored)
        side.addLayout(adding)

        self.details = QFormLayout()
        self.details.setSpacing(6)
        self.details.setFieldGrowthPolicy(QFormLayout.FieldGrowthPolicy.AllNonFixedFieldsGrow)
        self.name = QLineEdit()
        self.name.setMaxLength(64)
        self.name.setAccessibleName("Name")
        self.name.textEdited.connect(self._rename)
        self.details.addRow("Name", self.name)
        self.in_label = QLineEdit()
        self.in_label.setMaxLength(32)
        self.in_label.setAccessibleName("Crossing towards the arrow")
        self.in_label.textEdited.connect(lambda text: self._set_text("in_label", text))
        self.details.addRow("Towards the arrow", self.in_label)
        self.out_label = QLineEdit()
        self.out_label.setMaxLength(32)
        self.out_label.setAccessibleName("Crossing the other way")
        self.out_label.textEdited.connect(lambda text: self._set_text("out_label", text))
        self.details.addRow("The other way", self.out_label)
        self.zone_kind = Segmented([("Watch", "include"), ("Ignore", "exclude")])
        self.zone_kind.setAccessibleName("What happens in this area")
        self.zone_kind.changed.connect(self._set_kind)
        self.details.addRow("In this area", self.zone_kind)
        side.addLayout(self.details)
        # Under the form, at full width: it does not fit beside a label.
        self.mode = Segmented([("Crossing line", LINE), ("Area (optional)", AREA)])
        for button, (color, kind) in zip(
            self.mode.buttons(), ((LINE_COLOR, "line"), (LANE_COLOR, "area")), strict=True
        ):
            button.setIcon(QIcon(_swatch(color, kind)))
            button.setIconSize(QSize(22, 14))
        self.mode.setAccessibleName("What to draw for this lane")
        self.mode.changed.connect(self._set_mode)
        self.mode_box = QWidget()
        self.mode_box.setObjectName("shapePart")
        mode_layout = QVBoxLayout(self.mode_box)
        mode_layout.setContentsMargins(0, 0, 0, 0)
        mode_layout.setSpacing(4)
        draw_label = QLabel("Draw on the picture")
        mode_layout.addWidget(draw_label)
        mode_layout.addWidget(self.mode)
        side.addWidget(self.mode_box)
        row = QHBoxLayout()
        row.setSpacing(6)
        self.clear_button = QPushButton("Redraw Line")
        self.clear_button.setToolTip("Start this line or area again")
        set_button_variant(self.clear_button, "secondary")
        self.clear_button.clicked.connect(self.clear_points)
        row.addWidget(self.clear_button)
        self.remove_button = QPushButton("Remove")
        set_button_variant(self.remove_button, "secondary")
        self.remove_button.clicked.connect(self.remove)
        row.addWidget(self.remove_button)
        side.addLayout(row)
        side.addStretch(1)
        editor.addWidget(side_panel, 0, Qt.AlignmentFlag.AlignTop)
        layout.addLayout(editor)
        self._on_tab(self.tabs.value())

    def set_values(self, config: dict[str, Any]) -> None:
        """Show these saved shapes instead (a new binding's camera choice)."""
        self.shapes = ShapeSet(self.shapes.fields, config)
        self.canvas.shapes = self.shapes
        self._on_tab(self.tabs.value())

    # ---- What to save ---------------------------------------------------------
    def values(self) -> dict[str, Any]:
        return self.shapes.values()

    def problem(self) -> str | None:
        return self.shapes.problem()

    def show_problem(self, text: str) -> None:
        set_status_tone(self.status, "warning")
        self.status.setText(text)

    @property
    def field(self) -> FormField | None:
        return self.canvas.field

    def _on_tab(self, key: object) -> None:
        field = next((f for f in self.shapes.fields if f.key == key), None)
        self.canvas.field = field
        self.canvas.active_point = None
        items = self.shapes.items(field) if field is not None else []
        self.canvas.item = items[0] if items else None
        self.mode.set_value(LINE)
        self._refresh()

    # ---- Editing ----------------------------------------------------------------
    def add(self, kind: str = "include") -> None:
        field = self.field
        if field is None:
            return
        item = self.shapes.add(field, kind)
        if item is None:
            return
        self.canvas.item = item
        self.canvas.active_point = None
        self.mode.set_value(LINE)
        self._refresh()
        self.name.setFocus()
        self.name.selectAll()

    def _select(self, index: int) -> None:
        field = self.field
        items = self.shapes.items(field) if field is not None else []
        if 0 <= index < len(items):
            self.canvas.item = items[index]
            self.canvas.active_point = None
            self.mode.set_value(LINE)
            self._refresh()

    def _set_mode(self, mode: object) -> None:
        self.canvas.mode = str(mode)
        self.canvas.active_point = None
        self._refresh()

    def _rename(self, name: str) -> None:
        item = self.canvas.item
        if isinstance(item, Lane):
            item.name = name
        elif item is not None:
            item["name"] = name
        self._refresh(keep_text=True)

    def _set_text(self, key: str, text: str) -> None:
        if isinstance(self.canvas.item, dict):
            self.canvas.item[key] = text

    def _set_kind(self, kind: object) -> None:
        if isinstance(self.canvas.item, dict) and self.field and self.field.kind == "zones":
            self.canvas.item["kind"] = str(kind)
            self._refresh(keep_text=True)

    def clear_points(self) -> None:
        target = self.canvas.target()
        if target is not None:
            target.clear()
            self.canvas.active_point = None
            self._refresh()

    def remove(self) -> None:
        field, item = self.field, self.canvas.item
        if field is None or item is None:
            return
        items = self.shapes.items(field)
        index = next(i for i, other in enumerate(items) if other is item)
        del items[index]
        self.canvas.item = items[min(index, len(items) - 1)] if items else None
        self.canvas.active_point = None
        self._refresh()

    def _refresh(self, keep_text: bool = False) -> None:
        field = self.field
        items = self.shapes.items(field) if field is not None else []
        kind = field.kind if field is not None else ""
        self.description.setText(field.description if field is not None else "")
        self.description.setVisible(bool(self.description.text()))
        word = _SINGULAR.get(kind, "Shape")
        self.add_button.setText("+ Watch an Area" if kind == "zones" else f"+ Add {word}")
        self.add_ignored.setVisible(kind == "zones")
        can_add = field is not None and self.shapes.can_add(field)
        self.add_button.setEnabled(can_add)
        self.add_ignored.setEnabled(can_add)
        self.remove_button.setText(f"Remove {word}")
        self.picker.setAccessibleName(field.title if field is not None else "Shapes")
        self.picker.blockSignals(True)
        self.picker.clear()
        for item in items:
            self.picker.addItem(self._row(field, item))
        if not items:
            empty = QListWidgetItem(
                "None: the whole picture is watched"
                if kind == "zones"
                else f"No {field.title.lower() if field else 'shapes'} yet"
            )
            empty.setFlags(Qt.ItemFlag.NoItemFlags)
            self.picker.addItem(empty)
        else:
            current = next((i for i, o in enumerate(items) if o is self.canvas.item), -1)
            self.picker.setCurrentRow(current)
        self.picker.blockSignals(False)
        item = self.canvas.item
        has_item = item is not None
        if not keep_text:
            self.name.setText(self._name_of(item))
            self.in_label.setText(
                str(item.get("in_label") or "In") if isinstance(item, dict) else ""
            )
            self.out_label.setText(
                str(item.get("out_label") or "Out") if isinstance(item, dict) else ""
            )
        if isinstance(item, dict) and kind == "zones":
            self.zone_kind.blockSignals(True)
            self.zone_kind.set_value("exclude" if item.get("kind") == "exclude" else "include")
            self.zone_kind.blockSignals(False)
        self.mode_box.setVisible(kind == "lanes" and has_item)
        for widget, shown in (
            (self.in_label, kind == "lines"),
            (self.out_label, kind == "lines"),
            (self.zone_kind, kind == "zones"),
        ):
            self.details.setRowVisible(widget, shown and has_item)
        self.details.setRowVisible(self.name, has_item)
        self.clear_button.setText("Redraw Line" if self.canvas.editing_line() else "Clear Corners")
        self.clear_button.setEnabled(has_item)
        self.remove_button.setEnabled(has_item)
        self.clear_button.setVisible(has_item)
        self.remove_button.setVisible(has_item)
        self.hint.setText(self._hint(item))
        if self.status.text() and self.problem() is None:
            set_status_tone(self.status, "muted")
            self.status.setText("")
        self.canvas.update()
        self.changed.emit()

    def _name_of(self, item: Item | None) -> str:
        if isinstance(item, Lane):
            return item.name
        return str(item.get("name") or "") if item is not None else ""

    def _row(self, field: FormField | None, item: Item) -> QListWidgetItem:
        assert field is not None
        missing = issue(field, item)
        name = self._name_of(item).strip() or f"Unnamed {_SINGULAR[field.kind].lower()}"
        ignored = field.kind == "zones" and item.get("kind") == "exclude"  # type: ignore[union-attr]
        color = IGNORED_COLOR if ignored else LANE_COLOR
        if field.kind == "zones" and not ignored:
            color = WATCHED_COLOR
        if field.kind in ("lines", "areas"):
            color = LINE_COLOR
        detail = missing or (lane_summary(item) if isinstance(item, Lane) else "")
        row = QListWidgetItem(f"{name}\n{detail}" if detail else name)
        row.setSizeHint(QSize(0, _LIST_ROW_HEIGHT))
        row.setToolTip(name)
        swatch = QPixmap(12, 12)
        swatch.fill(QColor(LINE_COLOR if missing else color))
        row.setIcon(QIcon(swatch))
        return row

    def _hint(self, item: Item | None) -> str:
        if item is None:
            word = _SINGULAR.get(self.field.kind if self.field else "", "shape").lower()
            return f"Add a {word}, then click the picture to draw it."
        moving = (
            "Drag a point to move it, right-click to remove it; "
            "or use the arrow keys and Delete on the last point touched."
        )
        target = self.canvas.target() or []
        if self.canvas.editing_line():
            if len(target) == 0:
                return "Click the picture where the line starts."
            if len(target) == 1:
                return "Click where the line ends."
            return moving
        if self.field is not None and self.field.kind == "lanes" and not target:
            return "Optional: click to outline the lane on the video."
        if len(target) < 3:
            return f"Click to add corners: {3 - len(target)} more needed."
        return f"Click to add corners. {moving}"

    # ---- The camera's picture ---------------------------------------------------
    def set_camera(self, camera_id: str | None) -> None:
        if camera_id == self._camera_id:
            return
        self._camera_id = camera_id
        self.canvas.set_snapshot(QPixmap())
        self.canvas.set_placeholder(
            "Loading the camera picture…" if camera_id else "No camera chosen"
        )
        self.snapshot_button.setEnabled(bool(camera_id))
        self.status.setText("" if camera_id else "Choose a camera to see its picture here.")

    def load_snapshot(self) -> None:
        if not self._camera_id or self._snapshots.busy:
            return
        self.snapshot_button.setEnabled(False)
        set_status_tone(self.status, "muted")
        self.status.setText("Taking a picture from the camera…")
        self.canvas.set_placeholder("Loading the camera picture…")
        self._snapshots.load(self._camera_id)

    def stop(self) -> None:
        self._snapshots.stop()

    def _on_picture(self, picture: QPixmap) -> None:
        self.snapshot_button.setEnabled(bool(self._camera_id))
        set_status_tone(self.status, "muted")
        self.canvas.set_snapshot(picture)
        self.status.setText("")

    def _on_no_picture(self, text: str) -> None:
        self.snapshot_button.setEnabled(bool(self._camera_id))
        set_status_tone(self.status, "muted")
        self.canvas.set_placeholder("No camera picture")
        self.status.setText(text)
