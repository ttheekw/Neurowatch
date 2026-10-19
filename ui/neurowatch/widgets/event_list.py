"""The event list shared by the Events page and Playback's event sidebar.

Rows are painted by :class:`EventDelegate`: the saved picture (or the kind's
glyph), what happened and where, the time, how long, and a badge for events
still going on or not yet seen. Days get a heading row. Pictures come from
:class:`EventImageLoader` — one background ``events.thumbnail`` request at a
time, newest wish first, for rows actually painted; nothing decodes video.
"""

from __future__ import annotations

import time
from collections import OrderedDict
from collections.abc import Iterable

from PySide6.QtCore import (
    QAbstractListModel,
    QBuffer,
    QByteArray,
    QIODevice,
    QModelIndex,
    QObject,
    QPersistentModelIndex,
    QRect,
    QRectF,
    QSize,
    Qt,
    QTimer,
    Signal,
)
from PySide6.QtGui import (
    QColor,
    QFont,
    QFontMetrics,
    QImage,
    QImageReader,
    QPainter,
    QPainterPath,
    QPixmap,
)
from PySide6.QtWidgets import (
    QAbstractItemView,
    QListView,
    QStyle,
    QStyledItemDelegate,
    QStyleOptionViewItem,
    QWidget,
)

from neurowatch.ipc.sync_client import SyncIpcClient
from neurowatch.models.event_images import decode_saved_image
from neurowatch.models.events import (
    ACTIVITY_KINDS,
    EVENT_COLORS,
    Event,
    clock_text,
    day_key,
    day_text,
    elapsed_text,
)
from neurowatch.theme import ColorTokens, active_colors
from neurowatch.utils.threads import Worker, retire
from neurowatch.widgets.event_icons import paint_event_icon

#: Largest saved picture shown (pixels); larger ones are refused.
MAX_DECODED_PIXELS = 1_048_576
#: Full pictures kept for the detail pane, and small ones kept for rows.
FULL_CACHE = 16
THUMB_CACHE = 240
THUMB_SIZE = QSize(192, 108)
#: Rows waiting for a picture; older wishes are dropped when scrolling fast.
MAX_WAITING = 24

EVENT_ROLE = int(Qt.ItemDataRole.UserRole) + 10
HEADER_ROLE = int(Qt.ItemDataRole.UserRole) + 11


# ----------------------------------------------------------------------
# Saved pictures
# ----------------------------------------------------------------------


class _ImageWorker(Worker):
    done = Signal(object, int, object)  # event id (int64), generation, bytes | None | Exception

    def __init__(self, endpoint: str | None, event_id: int, generation: int) -> None:
        super().__init__()
        self._endpoint, self._event_id, self._generation = endpoint, event_id, generation

    def run(self) -> None:
        try:
            result: object = decode_saved_image(
                SyncIpcClient(endpoint=self._endpoint).call(
                    "events.thumbnail", {"id": self._event_id}
                )
            )
        except Exception as error:
            result = error
        self.done.emit(self._event_id, self._generation, result)


def decode_jpeg(data: bytes) -> QImage | None:
    """A saved picture, or None when it is not a bounded, readable JPEG."""
    buffer = QBuffer()
    buffer.setData(QByteArray(data))
    buffer.open(QIODevice.OpenModeFlag.ReadOnly)
    reader = QImageReader(buffer, b"JPEG")
    size = reader.size()
    if size.width() <= 0 or size.height() <= 0:
        return None
    if size.width() * size.height() > MAX_DECODED_PIXELS:
        return None
    image = reader.read()
    return None if image.isNull() else image


class EventImageLoader(QObject):
    """Saved event pictures for one page: a bounded cache, one request at a time.

    ``image(id)`` and ``thumbnail(id)`` return what is cached (or None) and,
    with ``request=True``, ask for the picture; ``ready(id)`` says it arrived
    (or that the event has none: ``state(id)`` tells which).
    """

    ready = Signal(object)  # event id

    LOADING, MISSING, FAILED = "loading", "missing", "failed"

    def __init__(self, endpoint: str | None, parent: QObject | None = None) -> None:
        super().__init__(parent)
        self._endpoint = endpoint
        self._generation = 0
        self._worker: _ImageWorker | None = None
        self._waiting: OrderedDict[int, None] = OrderedDict()
        self._full: OrderedDict[int, QPixmap] = OrderedDict()
        self._thumbs: OrderedDict[int, QPixmap] = OrderedDict()
        self._states: dict[int, str] = {}
        self._urgent: int | None = None
        self._in_flight: int | None = None

    @property
    def busy(self) -> bool:
        return self._worker is not None

    def state(self, event_id: int) -> str | None:
        """``loading``, ``missing`` (no saved picture), ``failed``, or None."""
        if event_id in self._full or event_id in self._thumbs:
            return None
        return self._states.get(event_id)

    def image(self, event_id: int, *, request: bool = True) -> QPixmap | None:
        pixmap = self._full.get(event_id)
        if pixmap is not None:
            self._full.move_to_end(event_id)
            return pixmap
        if request:
            self.request(event_id, urgent=True)
        return None

    def thumbnail(self, event_id: int, *, request: bool = True) -> QPixmap | None:
        pixmap = self._thumbs.get(event_id)
        if pixmap is not None:
            self._thumbs.move_to_end(event_id)
            return pixmap
        if request:
            self.request(event_id)
        return None

    def request(self, event_id: int, *, urgent: bool = False) -> None:
        if event_id in self._full or (not urgent and event_id in self._thumbs):
            return
        if self._states.get(event_id) in (self.MISSING, self.FAILED):
            return
        if urgent:
            self._urgent = event_id
        if event_id == self._in_flight:
            return
        self._states[event_id] = self.LOADING
        # Newest wish served first: the rows on screen now, not those
        # scrolled past.
        self._waiting[event_id] = None
        self._waiting.move_to_end(event_id)
        while len(self._waiting) > MAX_WAITING:
            dropped, _ = self._waiting.popitem(last=False)
            if dropped != self._urgent:
                self._states.pop(dropped, None)
        self._next()

    def retry(self, event_id: int) -> None:
        self._states.pop(event_id, None)
        self._full.pop(event_id, None)
        self._thumbs.pop(event_id, None)
        self.request(event_id, urgent=True)

    def _next(self) -> None:
        if self._worker is not None:
            return
        if self._urgent is not None and self._urgent in self._waiting:
            event_id = self._urgent
            del self._waiting[event_id]
        elif self._waiting:
            event_id, _ = self._waiting.popitem(last=True)
        else:
            return
        self._urgent = None if event_id == self._urgent else self._urgent
        worker = _ImageWorker(self._endpoint, event_id, self._generation)
        worker.done.connect(self._on_result)
        worker.finished.connect(self._on_finished)
        worker.finished.connect(worker.deleteLater)
        self._worker = worker
        self._in_flight = event_id
        worker.start()

    def _on_result(self, event_id: int, generation: int, result: object) -> None:
        if generation != self._generation:
            return
        if isinstance(result, bytes):
            image = decode_jpeg(result)
            if image is None:
                self._states[event_id] = self.FAILED
            else:
                self._states.pop(event_id, None)
                self._remember(event_id, image)
        elif result is None:
            self._states[event_id] = self.MISSING
        else:
            self._states[event_id] = self.FAILED
        self.ready.emit(event_id)

    def _remember(self, event_id: int, image: QImage) -> None:
        full = QPixmap.fromImage(image)
        self._full[event_id] = full
        while len(self._full) > FULL_CACHE:
            self._full.popitem(last=False)
        thumb = full.scaled(
            THUMB_SIZE,
            Qt.AspectRatioMode.KeepAspectRatioByExpanding,
            Qt.TransformationMode.SmoothTransformation,
        )
        self._thumbs[event_id] = thumb
        while len(self._thumbs) > THUMB_CACHE:
            self._thumbs.popitem(last=False)

    def _on_finished(self) -> None:
        if self.sender() is not self._worker:
            return
        self._worker = None
        if self._urgent == self._in_flight:
            self._urgent = None
        self._in_flight = None
        self._next()

    def stop(self) -> None:
        """Forget every picture and let a request in flight go."""
        self._generation += 1
        self._waiting.clear()
        self._urgent = self._in_flight = None
        self._states.clear()
        self._full.clear()
        self._thumbs.clear()
        worker, self._worker = self._worker, None
        if worker is not None:
            for signal, slot in (
                (worker.done, self._on_result),
                (worker.finished, self._on_finished),
            ):
                try:
                    signal.disconnect(slot)
                except (RuntimeError, TypeError):
                    pass
            retire(worker)


# ----------------------------------------------------------------------
# Model
# ----------------------------------------------------------------------


class EventListModel(QAbstractListModel):
    """Events newest first, with a heading row before each day."""

    def __init__(self, parent: QObject | None = None) -> None:
        super().__init__(parent)
        self._events: list[Event] = []
        self._rows: list[Event | str] = []
        self._row_of: dict[int, int] = {}
        self._names: dict[str, str] = {}
        self._now_ms = int(time.time() * 1000)

    # Qt model ---------------------------------------------------------

    def rowCount(self, parent: QModelIndex | QPersistentModelIndex = QModelIndex()) -> int:  # noqa: B008, N802
        return 0 if parent.isValid() else len(self._rows)

    def data(self, index: QModelIndex | QPersistentModelIndex, role: int = 0) -> object:
        if not index.isValid() or not 0 <= index.row() < len(self._rows):
            return None
        row = self._rows[index.row()]
        if isinstance(row, str):
            if role in (HEADER_ROLE, Qt.ItemDataRole.DisplayRole):
                return row
            return None
        if role == EVENT_ROLE:
            return row
        if role == Qt.ItemDataRole.UserRole:
            return row.id
        if role in (Qt.ItemDataRole.DisplayRole, Qt.ItemDataRole.AccessibleTextRole):
            return self.describe(row)
        if role == Qt.ItemDataRole.ToolTipRole:
            return self.describe(row, full=True)
        return None

    def flags(self, index: QModelIndex | QPersistentModelIndex) -> Qt.ItemFlag:
        if not index.isValid():
            return Qt.ItemFlag.NoItemFlags
        if isinstance(self._rows[index.row()], str):
            return Qt.ItemFlag.ItemIsEnabled
        return Qt.ItemFlag.ItemIsEnabled | Qt.ItemFlag.ItemIsSelectable

    # Content ----------------------------------------------------------

    @property
    def events(self) -> list[Event]:
        return list(self._events)

    def camera_name(self, event: Event) -> str:
        if event.camera_id is None:
            return "Recorder"
        return self._names.get(event.camera_id, event.camera_id)

    def describe(self, event: Event, *, full: bool = False) -> str:
        camera = self.camera_name(event)
        text = f"{event.describe(camera)} · {camera} · {clock_text(event.start_ms)}"
        if not full:
            return text
        extra = [event.topic or "", f"Source: {event.source}" if event.source else ""]
        return "\n".join([text, *filter(None, extra)])

    def set_names(self, names: dict[str, str]) -> None:
        self._names = dict(names)
        if self._rows:
            self.dataChanged.emit(self.index(0), self.index(len(self._rows) - 1))

    def set_events(self, events: Iterable[Event]) -> None:
        """Replace the list (sorted newest first here)."""
        self.beginResetModel()
        self._events = sorted(events, key=lambda e: (e.start_ms, e.id), reverse=True)
        self._now_ms = int(time.time() * 1000)
        self._rebuild()
        self.endResetModel()

    def row_of(self, event_id: int) -> int | None:
        return self._row_of.get(event_id)

    def event_at(self, row: int) -> Event | None:
        if 0 <= row < len(self._rows):
            item = self._rows[row]
            return item if isinstance(item, Event) else None
        return None

    def upsert(self, event: Event) -> None:
        """Add or change one event, touching only the rows that change."""
        row = self._row_of.get(event.id)
        if row is not None:
            old = self._rows[row]
            if isinstance(old, Event) and old.start_ms == event.start_ms:
                self._rows[row] = event
                position = self._events.index(old)
                self._events[position] = event
                index = self.index(row)
                self.dataChanged.emit(index, index)
                return
            self.remove(event.id)
        key = (event.start_ms, event.id)
        position = next(
            (i for i, known in enumerate(self._events) if (known.start_ms, known.id) < key),
            len(self._events),
        )
        before = self._events[position - 1] if position > 0 else None
        after = self._events[position] if position < len(self._events) else None
        day = day_key(event.start_ms)
        inserted: list[Event | str] = [event]
        if before is not None and day_key(before.start_ms) == day:
            row = self._row_of[before.id] + 1
        elif after is not None and day_key(after.start_ms) == day:
            # Its day already has a heading: just above the next event.
            row = self._row_of[after.id]
        else:
            row = 0 if before is None else self._row_of[before.id] + 1
            inserted.insert(0, self._heading(event.start_ms))
        self.beginInsertRows(QModelIndex(), row, row + len(inserted) - 1)
        self._events.insert(position, event)
        self._rows[row:row] = inserted
        self._reindex(row)
        self.endInsertRows()

    def remove(self, event_id: int) -> Event | None:
        row = self._row_of.get(event_id)
        if row is None:
            return None
        event = self._rows[row]
        assert isinstance(event, Event)
        first = row
        # Its day heading goes too when it was that day's only event.
        heading_alone = (
            row > 0
            and isinstance(self._rows[row - 1], str)
            and (row + 1 >= len(self._rows) or isinstance(self._rows[row + 1], str))
        )
        if heading_alone:
            first = row - 1
        self.beginRemoveRows(QModelIndex(), first, row)
        del self._rows[first : row + 1]
        self._events.remove(event)
        self._row_of.pop(event_id, None)
        self._reindex(first)
        self.endRemoveRows()
        return event

    def refresh_headings(self) -> None:
        """Day names move on at midnight ("Today" becomes "Yesterday")."""
        self._now_ms = int(time.time() * 1000)
        for row, item in enumerate(self._rows):
            if isinstance(item, str):
                event = self.event_at(row + 1)
                if event is not None:
                    self._rows[row] = self._heading(event.start_ms)
        if self._rows:
            self.dataChanged.emit(self.index(0), self.index(len(self._rows) - 1))

    def _heading(self, ms: int) -> str:
        return day_text(ms, self._now_ms)

    def _rebuild(self) -> None:
        self._rows = []
        last_day: tuple[int, int, int] | None = None
        for event in self._events:
            day = day_key(event.start_ms)
            if day != last_day:
                self._rows.append(self._heading(event.start_ms))
                last_day = day
            self._rows.append(event)
        self._reindex(0)

    def _reindex(self, start: int) -> None:
        if start == 0:
            self._row_of = {}
        for row in range(start, len(self._rows)):
            item = self._rows[row]
            if isinstance(item, Event):
                self._row_of[item.id] = row


# ----------------------------------------------------------------------
# Painting
# ----------------------------------------------------------------------


def _mix(color: QColor, background: QColor, amount: float) -> QColor:
    """``amount`` of ``color`` over ``background`` (an opaque tint)."""
    return QColor(
        round(color.red() * amount + background.red() * (1 - amount)),
        round(color.green() * amount + background.green() * (1 - amount)),
        round(color.blue() * amount + background.blue() * (1 - amount)),
    )


def kind_color(kind: str) -> QColor:
    return QColor(EVENT_COLORS.get(kind, "#64748b"))


def paint_badge(
    painter: QPainter, right: float, center_y: float, text: str, color: QColor, back: QColor
) -> float:
    """A rounded label ending at ``right``; returns its left edge."""
    font = QFont(painter.font())
    font.setPixelSize(10)
    font.setWeight(QFont.Weight.DemiBold)
    metrics = QFontMetrics(font)
    width = metrics.horizontalAdvance(text) + 14
    rect = QRectF(right - width, center_y - 9, width, 18)
    painter.save()
    painter.setPen(Qt.PenStyle.NoPen)
    painter.setBrush(_mix(color, back, 0.18))
    painter.drawRoundedRect(rect, 9, 9)
    painter.setPen(color)
    painter.setFont(font)
    painter.drawText(rect, Qt.AlignmentFlag.AlignCenter, text)
    painter.restore()
    return rect.left()


def paint_kind_tile(
    painter: QPainter, rect: QRectF, kind: str, colors: ColorTokens, radius: float = 6
) -> None:
    """The stand-in for a saved picture: the kind's glyph on a tinted tile."""
    color = kind_color(kind)
    painter.save()
    painter.setPen(Qt.PenStyle.NoPen)
    painter.setBrush(_mix(color, QColor(colors.surface_sunken), 0.16))
    painter.drawRoundedRect(rect, radius, radius)
    side = min(rect.width(), rect.height()) * 0.5
    icon = QRectF(rect.center().x() - side / 2, rect.center().y() - side / 2, side, side)
    paint_event_icon(painter, icon, kind, color)
    painter.restore()


class EventDelegate(QStyledItemDelegate):
    """One event: picture, what and where, when and how long, status."""

    def __init__(
        self,
        model: EventListModel,
        loader: EventImageLoader,
        *,
        compact: bool = False,
        parent: QObject | None = None,
    ) -> None:
        super().__init__(parent)
        self._model = model
        self._loader = loader
        self._compact = compact
        self.colors = active_colors()

    @property
    def row_height(self) -> int:
        return 62 if self._compact else 72

    @property
    def thumb_size(self) -> QSize:
        return QSize(80, 45) if self._compact else QSize(96, 54)

    def sizeHint(
        self, option: QStyleOptionViewItem, index: QModelIndex | QPersistentModelIndex
    ) -> QSize:
        if index.data(HEADER_ROLE) is not None and index.data(EVENT_ROLE) is None:
            return QSize(option.rect.width(), 34)
        return QSize(option.rect.width(), self.row_height)

    def paint(
        self,
        painter: QPainter,
        option: QStyleOptionViewItem,
        index: QModelIndex | QPersistentModelIndex,
    ) -> None:
        event = index.data(EVENT_ROLE)
        painter.save()
        painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        if isinstance(event, Event):
            self._paint_event(painter, option, event)
        else:
            self._paint_heading(painter, option.rect, str(index.data(HEADER_ROLE) or ""))
        painter.restore()

    def _paint_heading(self, painter: QPainter, rect: QRect, text: str) -> None:
        c = self.colors
        font = QFont(painter.font())
        font.setPixelSize(11)
        font.setWeight(QFont.Weight.Bold)
        font.setLetterSpacing(QFont.SpacingType.PercentageSpacing, 106)
        painter.setFont(font)
        painter.setPen(QColor(c.text_muted))
        text_rect = rect.adjusted(14, 10, -14, -4)
        painter.drawText(
            text_rect, int(Qt.AlignmentFlag.AlignLeft | Qt.AlignmentFlag.AlignVCenter), text.upper()
        )
        width = QFontMetrics(font).horizontalAdvance(text.upper())
        line_y = text_rect.center().y() + 1
        left = text_rect.left() + width + 10
        if left < text_rect.right():
            painter.setPen(QColor(c.border))
            painter.drawLine(left, line_y, text_rect.right(), line_y)

    def _paint_event(self, painter: QPainter, option: QStyleOptionViewItem, event: Event) -> None:
        c = self.colors
        rect = QRectF(option.rect).adjusted(6, 2, -6, -2)
        selected = bool(option.state & QStyle.StateFlag.State_Selected)
        hovered = bool(option.state & QStyle.StateFlag.State_MouseOver)
        back = QColor(c.surface)
        if selected or hovered:
            back = QColor(c.accent_subtle if selected else c.hover)
            painter.setPen(Qt.PenStyle.NoPen)
            painter.setBrush(back)
            painter.drawRoundedRect(rect, 8, 8)
        if selected:
            painter.setPen(Qt.PenStyle.NoPen)
            painter.setBrush(QColor(c.accent))
            painter.drawRoundedRect(
                QRectF(rect.left(), rect.top() + 10, 3, rect.height() - 20), 1.5, 1.5
            )

        # Picture, or the kind's glyph.
        size = self.thumb_size
        thumb = QRectF(
            rect.left() + 10, rect.center().y() - size.height() / 2, size.width(), size.height()
        )
        pixmap = self._loader.thumbnail(event.id) if event.has_image else None
        if pixmap is not None and not pixmap.isNull():
            clip = QPainterPath()
            clip.addRoundedRect(thumb, 6, 6)
            painter.save()
            painter.setClipPath(clip)
            source = QRectF(pixmap.rect())
            scale = max(thumb.width() / source.width(), thumb.height() / source.height())
            crop_w, crop_h = thumb.width() / scale, thumb.height() / scale
            painter.drawPixmap(
                thumb,
                pixmap,
                QRectF(
                    source.center().x() - crop_w / 2,
                    source.center().y() - crop_h / 2,
                    crop_w,
                    crop_h,
                ),
            )
            painter.restore()
            # The kind, in a corner chip on the picture.
            chip = QRectF(thumb.left() + 4, thumb.bottom() - 20, 16, 16)
            painter.setPen(Qt.PenStyle.NoPen)
            painter.setBrush(QColor(0, 0, 0, 170))
            painter.drawRoundedRect(chip, 4, 4)
            paint_event_icon(
                painter, chip.adjusted(2, 2, -2, -2), event.kind, kind_color(event.kind)
            )
        else:
            paint_kind_tile(painter, thumb, event.kind, c)

        # Right column: time, then the status badge.
        left = thumb.right() + 12
        right = rect.right() - 12
        time_font = QFont(painter.font())
        time_font.setPixelSize(12)
        time_text = clock_text(event.start_ms)
        time_width = QFontMetrics(time_font).horizontalAdvance(time_text)
        top_line = QRectF(left, rect.top() + (12 if self._compact else 15), right - left, 20)
        bottom_line = QRectF(left, top_line.bottom() + 2, right - left, 18)
        painter.setFont(time_font)
        painter.setPen(QColor(c.text_muted))
        painter.drawText(
            QRectF(right - time_width, top_line.top(), time_width, top_line.height()),
            int(Qt.AlignmentFlag.AlignRight | Qt.AlignmentFlag.AlignVCenter),
            time_text,
        )
        badge_left = right
        if event.going_on:
            badge_left = paint_badge(
                painter, right, bottom_line.center().y(), "ONGOING", QColor(c.warning), back
            )
        unseen = not event.acknowledged and event.kind in ACTIVITY_KINDS

        # Title: what happened; subtitle: camera · how long.
        camera = self._model.camera_name(event)
        title_font = QFont(painter.font())
        title_font.setPixelSize(13)
        title_font.setWeight(QFont.Weight.DemiBold)
        painter.setFont(title_font)
        painter.setPen(QColor(c.text))
        title_width = top_line.width() - time_width - 10
        if unseen:
            # Not yet seen: a dot before the title.
            painter.setPen(Qt.PenStyle.NoPen)
            painter.setBrush(QColor(c.info))
            painter.drawEllipse(QRectF(left, top_line.center().y() - 3.5, 7, 7))
            left += 13
            title_width -= 13
            painter.setPen(QColor(c.text))
        title = QFontMetrics(title_font).elidedText(
            event.describe(camera), Qt.TextElideMode.ElideRight, int(max(0, title_width))
        )
        painter.drawText(
            QRectF(left, top_line.top(), title_width, top_line.height()),
            int(Qt.AlignmentFlag.AlignLeft | Qt.AlignmentFlag.AlignVCenter),
            title,
        )
        sub_font = QFont(painter.font())
        sub_font.setPixelSize(12)
        sub_font.setWeight(QFont.Weight.Normal)
        painter.setFont(sub_font)
        painter.setPen(QColor(c.text_muted))
        left = thumb.right() + 12
        parts = [camera]
        lasted = elapsed_text(event)
        if lasted:
            parts.append(f"for {lasted}" if event.going_on else lasted)
        sub_width = badge_left - left - 8
        subtitle = QFontMetrics(sub_font).elidedText(
            " · ".join(parts), Qt.TextElideMode.ElideRight, int(max(0, sub_width))
        )
        painter.drawText(
            QRectF(left, bottom_line.top(), sub_width, bottom_line.height()),
            int(Qt.AlignmentFlag.AlignLeft | Qt.AlignmentFlag.AlignVCenter),
            subtitle,
        )


class EventListView(QListView):
    """A list of events (newest first) with day headings and pictures."""

    #: The selected event, or None.
    selection_changed = Signal(object)
    #: Double-click / Enter on an event.
    activated_event = Signal(object)

    def __init__(
        self,
        loader: EventImageLoader,
        *,
        compact: bool = False,
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        self.setObjectName("eventList")
        self.events_model = EventListModel(self)
        self.setModel(self.events_model)
        self._loader = loader
        self.delegate = EventDelegate(self.events_model, loader, compact=compact, parent=self)
        self.setItemDelegate(self.delegate)
        self.setSelectionMode(QAbstractItemView.SelectionMode.SingleSelection)
        self.setSelectionBehavior(QAbstractItemView.SelectionBehavior.SelectRows)
        self.setEditTriggers(QAbstractItemView.EditTrigger.NoEditTriggers)
        self.setVerticalScrollMode(QAbstractItemView.ScrollMode.ScrollPerPixel)
        self.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
        self.setMouseTracking(True)
        self.setUniformItemSizes(False)
        self.setSpacing(0)
        self.setMinimumSize(0, 80)
        self.viewport().setAttribute(Qt.WidgetAttribute.WA_Hover, True)
        loader.ready.connect(self._picture_ready)
        self.selectionModel().currentChanged.connect(self._current_changed)
        self.activated.connect(self._on_activated)
        self.doubleClicked.connect(self._on_activated)
        # Ongoing events show how long they have lasted; day names roll over.
        self._clock = QTimer(self)
        self._clock.setInterval(5_000)
        self._clock.timeout.connect(self._tick)
        self._clock.start()
        self._day = day_key(int(time.time() * 1000))

    def apply_theme(self, colors: ColorTokens) -> None:
        self.delegate.colors = colors
        self.viewport().update()

    # Selection --------------------------------------------------------

    def selected_event(self) -> Event | None:
        index = self.currentIndex()
        if not index.isValid() or not self.selectionModel().isSelected(index):
            return None
        event = index.data(EVENT_ROLE)
        return event if isinstance(event, Event) else None

    def select_event(self, event_id: int | None) -> bool:
        if event_id is None:
            self.clearSelection()
            self.setCurrentIndex(QModelIndex())
            return False
        row = self.events_model.row_of(event_id)
        if row is None:
            return False
        index = self.events_model.index(row)
        self.setCurrentIndex(index)
        self.scrollTo(index)
        return True

    def _current_changed(self, current: QModelIndex, _previous: QModelIndex) -> None:
        event = current.data(EVENT_ROLE) if current.isValid() else None
        self.selection_changed.emit(event if isinstance(event, Event) else None)

    def _on_activated(self, index: QModelIndex) -> None:
        event = index.data(EVENT_ROLE)
        if isinstance(event, Event):
            self.activated_event.emit(event)

    # Content ----------------------------------------------------------

    def set_events(self, events: Iterable[Event], *, keep: int | None = None) -> None:
        """Replace the list, keeping the selected event when it is still there."""
        keep = keep if keep is not None else getattr(self.selected_event(), "id", None)
        self.selectionModel().blockSignals(True)
        try:
            self.events_model.set_events(events)
            found = keep is not None and self.select_event(keep)
        finally:
            self.selectionModel().blockSignals(False)
        if keep is not None and not found:
            self.selection_changed.emit(None)
        elif found:
            self.selection_changed.emit(self.selected_event())

    def _picture_ready(self, event_id: int) -> None:
        row = self.events_model.row_of(event_id)
        if row is not None:
            self.update(self.events_model.index(row))

    def _tick(self) -> None:
        if not self.isVisible():
            return
        today = day_key(int(time.time() * 1000))
        if today != self._day:
            self._day = today
            self.events_model.refresh_headings()
            return
        if any(event.going_on for event in self.events_model.events[:200]):
            self.viewport().update()
