"""The Events tab: what happened (motion, camera analytics, cameras offline,
recordings failing), newest first by day, kept current as events arrive.

Lists come from ``events.list`` on a worker thread; new events arrive from
the window's ``EventFeedWorker`` through :meth:`EventsView.add_notices`.
Double-clicking an event of a camera (or **Play Recording**) asks the window
to open that moment in Playback.
"""

from __future__ import annotations

import logging
from typing import Any

from PySide6.QtCore import QDate, QDateTime, Qt, QTimer, Signal
from PySide6.QtGui import QResizeEvent, QShowEvent
from PySide6.QtWidgets import (
    QComboBox,
    QDateEdit,
    QGridLayout,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QPushButton,
    QSplitter,
    QStackedWidget,
    QVBoxLayout,
    QWidget,
)

from neurowatch.ipc.sync_client import SyncIpcClient
from neurowatch.models.camera import CameraConfig
from neurowatch.models.events import (
    ACTIVITY_KINDS,
    EVENT_LABELS,
    SYSTEM_KINDS,
    Event,
    zone_names,
)
from neurowatch.theme import (
    active_colors,
    events_stylesheet,
    set_button_variant,
    set_label_role,
    set_status_tone,
)
from neurowatch.utils.threads import Worker, retire
from neurowatch.widgets.event_list import EventImageLoader, EventListView
from neurowatch.widgets.event_preview import EventPreview
from neurowatch.widgets.wheel_guard import guard_wheel

logger = logging.getLogger(__name__)

#: Events listed at a time; "Load Older" adds as many again.
PAGE_SIZE = 200
#: Most events kept while new ones arrive (the oldest drop off; Load Older
#: reads them again). Each new event costs one row, never a redraw.
MAX_ROWS = 1000
#: Seconds of recording shown before an event when it is played.
PLAY_LEAD_MS = 5_000
#: Below this width the filters wrap and the details go under the list.
NARROW_WIDTH = 1100

#: Filter choices: (label, kinds sent to events.list; empty: all).
KIND_FILTERS: tuple[tuple[str, tuple[str, ...]], ...] = (
    ("All events", ()),
    ("Motion and analytics", (*sorted(ACTIVITY_KINDS), "tamper", "digital_input", "camera_event")),
    ("Camera and recording problems", tuple(sorted(SYSTEM_KINDS - {"backend_started"}))),
    ("Bookmarks", ("bookmark",)),
    *((text, (kind,)) for kind, text in EVENT_LABELS.items() if kind != "bookmark"),
)


def _as_dict(event: Event) -> dict[str, Any]:
    """An event as the recorder sends it (for ``add_notices``)."""
    raw: dict[str, Any] = {
        "id": event.id,
        "kind": event.kind,
        "camera_id": event.camera_id,
        "start_ms": event.start_ms,
        "end_ms": event.end_ms,
        "source": event.source,
        "acknowledged": event.acknowledged,
        "kept": event.kept,
    }
    for name in ("topic", "detail", "score"):
        if getattr(event, name) is not None:
            raw[name] = getattr(event, name)
    if event.zones:
        raw["zones"] = list(event.zones)
    if event.bbox is not None:
        raw["bbox"] = list(event.bbox)
    if event.has_image:
        raw["thumbnail"] = "saved"
    return raw


class _CallWorker(Worker):
    done = Signal(int, object)  # generation, result or Exception

    def __init__(
        self, endpoint: str | None, method: str, params: dict[str, Any], generation: int
    ) -> None:
        super().__init__()
        self._endpoint = endpoint
        self._method = method
        self._params = params
        self._generation = generation

    def run(self) -> None:
        try:
            result = SyncIpcClient(endpoint=self._endpoint).call(self._method, self._params)
        except Exception as exc:
            result = exc
        self.done.emit(self._generation, result)


class _ListWorker(_CallWorker):
    def __init__(self, endpoint: str | None, params: dict[str, Any], generation: int) -> None:
        super().__init__(endpoint, "events.list", params, generation)


class EventsView(QWidget):
    """Top-level widget shown when the Events tab is active."""

    #: (camera id, time in ms): open this moment in Playback.
    play_requested = Signal(str, object)  # epoch milliseconds exceed Qt int32
    export_requested = Signal(object)

    def __init__(
        self,
        endpoint: str | None,
        cameras: list[CameraConfig],
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        self.setObjectName("eventsPage")
        self.setStyleSheet(events_stylesheet())
        self._colors = active_colors()
        self._endpoint = endpoint
        self._names: dict[str, str] = {}
        self._zones: dict[str, dict[str, str]] = {}
        self._events: list[Event] = []
        self._worker: _ListWorker | None = None
        self._ack_workers: set[_CallWorker] = set()
        self._generation = 0
        self._loading = False
        self._live_during_load: dict[int, Event] = {}
        self._appending = False
        self._has_more = False
        self._cursor: tuple[int, int] | None = None
        # Rows kept: MAX_ROWS, or more after Load Older.
        self._limit = MAX_ROWS
        # Changes arrived while the tab was hidden: redraw when it shows.
        self._dirty = False
        self._narrow: bool | None = None

        root = QVBoxLayout(self)
        root.setContentsMargins(24, 18, 24, 18)
        root.setSpacing(14)

        # ── Heading ──────────────────────────────────────────────────
        bar = QHBoxLayout()
        heading = QVBoxLayout()
        heading.setSpacing(2)
        title = QLabel("Events")
        set_label_role(title, "pageTitle")
        heading.addWidget(title)
        subtitle = QLabel("Motion, camera alerts and recording problems, newest first.")
        subtitle.setWordWrap(True)
        set_label_role(subtitle, "muted")
        heading.addWidget(subtitle)
        bar.addLayout(heading, 1)
        self._count = QLabel("")
        self._count.setObjectName("eventsCount")
        bar.addWidget(self._count, 0, Qt.AlignmentFlag.AlignBottom)
        self._refresh_btn = QPushButton("Refresh")
        self._refresh_btn.setToolTip("Read the event history again")
        set_button_variant(self._refresh_btn, "secondary")
        self._refresh_btn.clicked.connect(self.reload)
        bar.addWidget(self._refresh_btn, 0, Qt.AlignmentFlag.AlignBottom)
        root.addLayout(bar)

        # ── Filters ──────────────────────────────────────────────────
        filters = QWidget()
        filters.setObjectName("eventsFilters")
        filter_layout = QVBoxLayout(filters)
        filter_layout.setContentsMargins(0, 0, 0, 0)
        filter_layout.setSpacing(8)
        self._filter_grid = QGridLayout()
        self._filter_grid.setContentsMargins(0, 0, 0, 0)
        self._filter_grid.setHorizontalSpacing(8)
        self._filter_grid.setVerticalSpacing(8)
        self._search = QLineEdit()
        self._search.setObjectName("eventsSearch")
        self._search.setPlaceholderText("Search cameras, events and details…")
        self._search.setAccessibleName("Search event history")
        self._search.setToolTip("Searches all retained events, not just those loaded")
        self._search.setMaxLength(200)
        self._search.setClearButtonEnabled(True)
        self._camera_filter = QComboBox()
        self._camera_filter.setAccessibleName("Filter by camera")
        self._kind_filter = QComboBox()
        self._kind_filter.setAccessibleName("Filter by event type")
        for text, kinds in KIND_FILTERS:
            self._kind_filter.addItem(text, kinds)
        self._state_filter = QComboBox()
        self._state_filter.setAccessibleName("Filter by event status")
        for text, value in (("Any status", None), ("Ongoing", True), ("Finished", False)):
            self._state_filter.addItem(text, value)
        self._range_filter = QComboBox()
        self._range_filter.setAccessibleName("Filter by date range")
        for text, period in (
            ("Any time", "all"),
            ("Today", "today"),
            ("Last 24 hours", "24h"),
            ("Last 7 days", "7d"),
            ("Last 30 days", "30d"),
            ("Custom dates…", "custom"),
        ):
            self._range_filter.addItem(text, period)
        self._combos = (
            self._camera_filter,
            self._kind_filter,
            self._state_filter,
            self._range_filter,
        )
        for field in self._combos:
            field.setMinimumWidth(0)
            field.setSizeAdjustPolicy(
                QComboBox.SizeAdjustPolicy.AdjustToMinimumContentsLengthWithIcon
            )
            field.setMinimumContentsLength(10)
        self._clear_btn = QPushButton("Clear")
        self._clear_btn.setToolTip("Clear the search and every filter")
        set_button_variant(self._clear_btn, "ghost")
        self._clear_btn.clicked.connect(self._clear_filters)
        self._filter_columns = 0
        self._layout_filters(wide=True)
        filter_layout.addLayout(self._filter_grid)

        self._custom_range = QWidget()
        custom_layout = QHBoxLayout(self._custom_range)
        custom_layout.setContentsMargins(0, 0, 0, 0)
        custom_layout.setSpacing(8)
        self._from_date = QDateEdit(QDate.currentDate().addDays(-6))
        self._to_date = QDateEdit(QDate.currentDate())
        for name, date_field in (("From", self._from_date), ("Through", self._to_date)):
            date_field.setCalendarPopup(True)
            date_field.setDisplayFormat("dd MMM yyyy")
            date_field.setAccessibleName(f"{name} date")
            label = QLabel(name)
            label.setBuddy(date_field)
            set_label_role(label, "muted")
            custom_layout.addWidget(label)
            custom_layout.addWidget(date_field)
        custom_layout.addStretch(1)
        self._custom_range.hide()
        filter_layout.addWidget(self._custom_range)
        root.addWidget(filters)

        # ── List and details ─────────────────────────────────────────
        self._images = EventImageLoader(endpoint, self)
        history = QWidget()
        history.setObjectName("eventsHistory")
        history_layout = QVBoxLayout(history)
        history_layout.setContentsMargins(1, 6, 1, 1)
        history_layout.setSpacing(0)
        self._list = EventListView(self._images)
        self._list.setAccessibleName("Event history; double-click or press Enter to play")
        self._list.selection_changed.connect(self._selection_changed)
        self._list.activated_event.connect(lambda _event: self._play_selected())
        self._stack = QStackedWidget()
        self._stack.addWidget(self._list)
        empty = QWidget()
        empty.setObjectName("eventsEmpty")
        empty_layout = QVBoxLayout(empty)
        empty_layout.setContentsMargins(30, 30, 30, 30)
        empty_layout.addStretch(1)
        self._empty_title = QLabel("No events yet")
        self._empty_title.setObjectName("eventsEmptyTitle")
        self._empty_title.setAlignment(Qt.AlignmentFlag.AlignCenter)
        empty_layout.addWidget(self._empty_title)
        self._empty_help = QLabel()
        set_label_role(self._empty_help, "muted")
        self._empty_help.setAlignment(Qt.AlignmentFlag.AlignCenter)
        self._empty_help.setWordWrap(True)
        empty_layout.addWidget(self._empty_help)
        empty_layout.addStretch(1)
        self._stack.addWidget(empty)
        history_layout.addWidget(self._stack, 1)
        footer = QWidget()
        self._footer = footer
        footer.setObjectName("eventsFooter")
        footer_layout = QHBoxLayout(footer)
        footer_layout.setContentsMargins(14, 8, 10, 8)
        self._status = QLabel()
        self._status.setWordWrap(True)
        footer_layout.addWidget(self._status, stretch=1)
        self._older_btn = QPushButton("Load Older")
        set_button_variant(self._older_btn, "secondary")
        self._older_btn.clicked.connect(self._load_older)
        self._older_btn.hide()
        footer_layout.addWidget(self._older_btn)
        history_layout.addWidget(footer)
        footer.hide()

        self._preview = EventPreview(self._images)
        self._preview.play_requested.connect(lambda _event: self._play_selected())
        self._preview.export_requested.connect(lambda _event: self._export_selected())
        self._preview.acknowledge_requested.connect(self._acknowledge)
        self._preview.edit_requested.connect(self._edit_bookmark)
        self._preview.delete_requested.connect(self._delete_bookmark)
        self._history_splitter = QSplitter(Qt.Orientation.Horizontal)
        self._history_splitter.setChildrenCollapsible(False)
        self._history_splitter.setHandleWidth(12)
        self._history_splitter.addWidget(history)
        self._history_splitter.addWidget(self._preview)
        self._history_splitter.setStretchFactor(0, 3)
        self._history_splitter.setStretchFactor(1, 2)
        self._history_splitter.setSizes([760, 420])
        root.addWidget(self._history_splitter, 1)

        self._search_timer = QTimer(self)
        self._search_timer.setSingleShot(True)
        self._search_timer.setInterval(300)
        self._search_timer.timeout.connect(self.reload)
        self._search.textChanged.connect(self._filters_changed)
        self._search.returnPressed.connect(self.reload)
        for field in self._combos:
            field.currentIndexChanged.connect(self._filters_changed)
        self._from_date.dateChanged.connect(lambda: self._date_changed(True))
        self._to_date.dateChanged.connect(lambda: self._date_changed(False))
        guard_wheel(self, *self._combos, self._from_date, self._to_date)
        self.set_cameras(cameras)

    # ------------------------------------------------------------------
    # Cameras and filters
    # ------------------------------------------------------------------

    def apply_theme(self, theme_name: str | None = None) -> None:
        """Apply startup/settings theme changes without resetting the list."""
        self._colors = active_colors(theme_name)
        self._list.apply_theme(self._colors)
        stylesheet = events_stylesheet(theme_name)
        if self.styleSheet() != stylesheet:
            self.setStyleSheet(stylesheet)
        self._preview.update()

    def _layout_filters(self, *, wide: bool) -> None:
        """One row when wide: search, then the filters; else search above."""
        columns = 6 if wide else 5
        if columns == self._filter_columns:
            return
        for widget in (self._search, *self._combos, self._clear_btn):
            self._filter_grid.removeWidget(widget)
        for column in range(6):
            self._filter_grid.setColumnStretch(column, 0)
        if wide:
            self._filter_grid.addWidget(self._search, 0, 0)
            self._filter_grid.setColumnStretch(0, 3)
            for column, combo in enumerate(self._combos, start=1):
                self._filter_grid.addWidget(combo, 0, column)
                self._filter_grid.setColumnStretch(column, 1)
            self._filter_grid.addWidget(self._clear_btn, 0, 5)
        else:
            self._filter_grid.addWidget(self._search, 0, 0, 1, 4)
            self._filter_grid.addWidget(self._clear_btn, 0, 4)
            for column, combo in enumerate(self._combos):
                self._filter_grid.addWidget(combo, 1, column, 1, 2 if column == 3 else 1)
                self._filter_grid.setColumnStretch(column, 1)
        self._filter_columns = columns

    def resizeEvent(self, event: QResizeEvent) -> None:
        super().resizeEvent(event)
        narrow = self.width() < NARROW_WIDTH
        if narrow == self._narrow:
            return
        self._narrow = narrow
        self._layout_filters(wide=not narrow)
        self._history_splitter.setOrientation(
            Qt.Orientation.Vertical if narrow else Qt.Orientation.Horizontal
        )
        self._history_splitter.setSizes([600, 380] if narrow else [760, 420])

    def _filters_changed(self) -> None:
        self.stop()
        self._has_more = False
        self._custom_range.setVisible(self._range_filter.currentData() == "custom")
        self._clear_btn.setEnabled(self._has_filters())
        self._older_btn.hide()
        self._search_timer.start()

    def _date_changed(self, from_changed: bool) -> None:
        # Keep an inclusive, valid range without a confusing validation error.
        if self._from_date.date() > self._to_date.date():
            other = self._to_date if from_changed else self._from_date
            other.blockSignals(True)
            other.setDate(self._from_date.date() if from_changed else self._to_date.date())
            other.blockSignals(False)
        self._filters_changed()

    def _clear_filters(self) -> None:
        self._search.blockSignals(True)
        self._search.clear()
        self._search.blockSignals(False)
        for field in self._combos:
            field.blockSignals(True)
            field.setCurrentIndex(0)
            field.blockSignals(False)
        self._filters_changed()

    def _has_filters(self) -> bool:
        return bool(self._search.text().strip()) or any(
            field.currentIndex() > 0 for field in self._combos
        )

    def set_cameras(self, cameras: list[CameraConfig]) -> None:
        self._names = {c.id: c.name or c.id for c in cameras}
        self._zones = {c.id: zone_names(c.motion) for c in cameras}
        self._list.events_model.set_names(self._names)
        chosen = self._camera_filter.currentData()
        self._camera_filter.blockSignals(True)
        self._camera_filter.clear()
        self._camera_filter.addItem("All cameras", None)
        for cam in cameras:
            self._camera_filter.addItem(cam.name or cam.id, cam.id)
        index = self._camera_filter.findData(chosen) if chosen else 0
        self._camera_filter.setCurrentIndex(max(index, 0))
        self._camera_filter.blockSignals(False)
        self._render()

    def _filter_params(self) -> dict[str, Any]:
        params: dict[str, Any] = {"limit": PAGE_SIZE}
        camera = self._camera_filter.currentData()
        if isinstance(camera, str):
            params["camera_ids"] = [camera]
        kinds = self._kind_filter.currentData()
        if kinds:
            params["kinds"] = list(kinds)
        query = self._search.text().strip()
        if query:
            params["search"] = query
        ongoing = self._state_filter.currentData()
        if isinstance(ongoing, bool):
            params["ongoing"] = ongoing
        range_ = self._range_filter.currentData()
        now = QDateTime.currentDateTime()
        if range_ == "today":
            params["from_ms"] = QDate.currentDate().startOfDay().toMSecsSinceEpoch()
        elif range_ in ("24h", "7d", "30d"):
            params["from_ms"] = now.addSecs(
                -86400 * {"24h": 1, "7d": 7, "30d": 30}[range_]
            ).toMSecsSinceEpoch()
        elif range_ == "custom":
            params["from_ms"] = self._from_date.date().startOfDay().toMSecsSinceEpoch()
            params["to_ms"] = self._to_date.date().addDays(1).startOfDay().toMSecsSinceEpoch() - 1
        return params

    def _matches(self, event: Event) -> bool:
        camera = self._camera_filter.currentData()
        kinds = self._kind_filter.currentData()
        if (isinstance(camera, str) and event.camera_id != camera) or (
            kinds and event.kind not in kinds
        ):
            return False
        ongoing = self._state_filter.currentData()
        if isinstance(ongoing, bool) and event.going_on != ongoing:
            return False
        if self._range_filter.currentIndex() > 0:
            params = self._filter_params()
            if event.start_ms > params.get("to_ms", event.start_ms):
                return False
            if event.end_ms is not None and event.end_ms < params.get("from_ms", event.end_ms):
                return False
        query = self._search.text().strip().lower()
        if not query:
            return True
        # Same fields and phrase semantics as events.list; match each field separately.
        return any(
            query in text.lower()
            for text in (
                self._names.get(event.camera_id or "", ""),
                event.camera_id or "",
                event.kind.replace("_", " "),
                event.label,
                event.detail or "",
                event.topic or "",
                event.source,
            )
        )

    # ------------------------------------------------------------------
    # Loading
    # ------------------------------------------------------------------

    def reload(self) -> None:
        self._search_timer.stop()
        self._load(self._filter_params(), append=False)

    def _load_older(self) -> None:
        if (
            self._loading
            or self._search_timer.isActive()
            or (not self._cursor and not self._events)
        ):
            return
        cursor = self._cursor or (self._events[-1].start_ms, self._events[-1].id)
        params = self._filter_params()
        params.update(before_ms=cursor[0], before_id=cursor[1])
        self._load(params, append=True)

    def _load(self, params: dict[str, Any], *, append: bool) -> None:
        self.stop()
        self._appending = append
        self._loading = True
        self._live_during_load.clear()
        self._refresh_btn.setEnabled(False)
        self._older_btn.setEnabled(False)
        set_status_tone(self._status, "info")
        self._status.setText("Loading events…")
        self._status.show()
        self._footer.show()
        if not self._list.events_model.rowCount():
            self._empty_title.setText("Loading events…")
            self._empty_help.setText("Reading the event history from the recorder.")
        worker = _ListWorker(self._endpoint, params, self._generation)
        worker.done.connect(self._on_result)
        self._worker = worker
        worker.start()

    def _on_result(self, generation: int, result: object) -> None:
        if generation == self._generation:
            self._on_loaded(result)

    def _on_loaded(self, result: object) -> None:
        retire(self._worker)
        self._worker = None
        self._loading = False
        self._refresh_btn.setEnabled(True)
        self._older_btn.setEnabled(True)
        if isinstance(result, Exception) or not isinstance(result, dict):
            set_status_tone(self._status, "error")
            self._status.setText(f"Events could not be read: {result}")
            self._status.show()
            self._footer.show()
            if not self._list.events_model.rowCount():
                self._empty_title.setText("Could not load events")
                self._empty_help.setText(
                    "Check the recorder connection, then click Refresh to try again."
                )
                self._stack.setCurrentIndex(1)
            return
        loaded = [Event.from_dict(e) for e in result.get("events") or [] if isinstance(e, dict)]
        events = [e for e in loaded if e is not None]
        merged = {e.id: e for e in self._events} if self._appending else {}
        merged.update((e.id, e) for e in events)
        # Explicit history pages may expand the limit; a burst of live notices
        # during a slow read may not silently expand it.
        self._limit = max(MAX_ROWS, len(merged))
        merged.update(self._live_during_load)
        self._live_during_load.clear()
        self._events = sorted(
            (e for e in merged.values() if self._matches(e)),
            key=lambda e: (e.start_ms, e.id),
            reverse=True,
        )
        if events:
            last = events[-1]
            self._cursor = (last.start_ms, last.id)
        elif not self._appending:
            self._cursor = None
        overflow = len(self._events) > self._limit
        if overflow:
            self._events = self._events[: self._limit]
            last = self._events[-1]
            self._cursor = (last.start_ms, last.id)
        self._has_more = len(events) >= PAGE_SIZE or overflow
        self._render()

    def stop(self) -> None:
        """Let go of a list still loading (closing, or a new filter)."""
        self._generation += 1
        self._loading = False
        self._search_timer.stop()
        self._refresh_btn.setEnabled(True)
        self._older_btn.setEnabled(True)
        worker = self._worker
        if worker is not None:
            try:
                worker.done.disconnect(self._on_result)
            except (RuntimeError, TypeError):
                pass
            retire(worker)
            self._worker = None

    def shutdown(self) -> None:
        """The window closes: stop loading and drop saved pictures."""
        self.stop()
        self._images.stop()
        self._preview.stop()
        for worker in list(self._ack_workers):
            retire(worker)
        self._ack_workers.clear()

    # ------------------------------------------------------------------
    # Live updates
    # ------------------------------------------------------------------

    def add_notices(self, notices: list[dict]) -> None:
        """New events and changes, as the event feed delivers them. Shown, a
        change touches only its row; hidden, the tab redraws when it shows."""
        shown = self.isVisible()
        model = self._list.events_model
        changed = False
        for notice in notices:
            raw = notice.get("event")
            event = Event.from_dict(raw) if isinstance(raw, dict) else None
            if event is None:
                continue
            if self._loading:
                self._live_during_load[event.id] = event
            index = next((i for i, known in enumerate(self._events) if known.id == event.id), None)
            if not self._matches(event):
                if index is not None:
                    self._events.pop(index)
                    if shown:
                        model.remove(event.id)
                    changed = True
                continue
            changed = True
            if index is not None:
                self._events[index] = event
            else:
                key = (event.start_ms, event.id)
                index = next(
                    (i for i, known in enumerate(self._events) if (known.start_ms, known.id) < key),
                    len(self._events),
                )
                self._events.insert(index, event)
            if shown:
                model.upsert(event)
            if len(self._events) > self._limit:
                dropped = self._events.pop()
                self._has_more = True
                last = self._events[-1]
                self._cursor = (last.start_ms, last.id)
                if shown:
                    model.remove(dropped.id)
        if not changed:
            return
        if shown:
            self._update_footer()
            selected = self._preview.current_event
            if selected is not None:
                current = next((e for e in self._events if e.id == selected.id), None)
                if current is not None and current != selected:
                    self._show_details(current)
        else:
            self._dirty = True

    def showEvent(self, event: QShowEvent) -> None:
        super().showEvent(event)
        if self._dirty:
            self._render()

    # ------------------------------------------------------------------
    # List
    # ------------------------------------------------------------------

    def _render(self) -> None:
        """Redraw every row (a new list or filter, or changes made while hidden)."""
        self._dirty = False
        self._list.set_events([e for e in self._events if self._matches(e)])
        self._update_footer()

    def _update_footer(self) -> None:
        set_status_tone(self._status, "muted")
        count = len(self._list.events_model.events)
        filtered = self._has_filters()
        self._clear_btn.setEnabled(filtered)
        self._stack.setCurrentIndex(0 if count else 1)
        going_on = sum(1 for e in self._list.events_model.events if e.going_on)
        self._count.setText(
            (f"{count:,} event{'s' if count != 1 else ''}" if count else "")
            + (f" · {going_on} ongoing" if going_on else "")
        )
        self._count.setVisible(bool(count))
        if not count:
            self._empty_title.setText("No matching events" if filtered else "No events yet")
            self._empty_help.setText(
                "Try a different search or widen the filters."
                if filtered
                else "Motion, camera alerts and recording problems will appear here.\n"
                "Turn on motion events in Edit Camera → Motion."
            )
            self._status.setText("")
        else:
            self._status.setText("Older events are not loaded yet." if self._has_more else "")
        if self._loading:
            set_status_tone(self._status, "info")
            self._status.setText("Loading events…")
        self._older_btn.setVisible(self._has_more)
        self._status.setVisible(bool(self._status.text()) and (bool(count) or self._loading))
        self._footer.setVisible(self._status.isVisibleTo(self) or self._has_more)

    def _selected(self) -> Event | None:
        return self._list.selected_event()

    def _selection_changed(self, event: Event | None) -> None:
        self._show_details(event)

    def _show_details(self, event: Event | None) -> None:
        self._preview.set_zone_names(self._zones.get(event.camera_id or "", {}) if event else {})
        self._preview.set_event(
            event, self._names.get(event.camera_id or "", "Recorder") if event else ""
        )

    def _play_selected(self) -> None:
        event = self._selected()
        if event is None or event.camera_id is None:
            return
        self.play_requested.emit(event.camera_id, max(0, event.start_ms - PLAY_LEAD_MS))

    def _export_selected(self) -> None:
        event = self._selected()
        if event is not None and event.camera_id is not None:
            self.export_requested.emit(event)

    def _edit_bookmark(self, event: Event) -> None:
        from neurowatch.dialogs.bookmark_dialog import BookmarkDialog

        cameras = (
            [(event.camera_id, self._names.get(event.camera_id, event.camera_id))]
            if event.camera_id
            else []
        )
        dialog = BookmarkDialog(self._endpoint, cameras, event.start_ms, self, event=event)
        # The recorder's change notice updates the row; show it at once too.
        dialog.saved.connect(lambda saved: self.add_notices([{"event": _as_dict(saved)}]))
        dialog.exec()
        dialog.deleteLater()

    def _delete_bookmark(self, event: Event) -> None:
        from neurowatch.dialogs.confirm import confirm_action

        if not confirm_action(
            self,
            "Delete bookmark",
            "Delete this bookmark?",
            confirm_text="Delete Bookmark",
            destructive=True,
            informative_text="Its recordings stay"
            + (", but cleanup may delete them again." if event.kept else "."),
        ):
            return
        worker = _CallWorker(self._endpoint, "events.delete", {"ids": [event.id]}, self._generation)

        def done(_generation: int, result: object) -> None:
            if isinstance(result, Exception):
                set_status_tone(self._status, "error")
                self._status.setText(f"Could not delete the bookmark: {result}")
                self._status.show()
                self._footer.show()
                return
            self._events = [known for known in self._events if known.id != event.id]
            self._list.events_model.remove(event.id)
            if self._preview.event_id == event.id:
                self._show_details(None)
            self._update_footer()

        worker.done.connect(done)
        worker.finished.connect(lambda: self._ack_workers.discard(worker))
        worker.finished.connect(worker.deleteLater)
        self._ack_workers.add(worker)
        worker.start()

    def _acknowledge(self, event: Event) -> None:
        """Mark one event as seen; the event feed brings the change back."""
        worker = _CallWorker(
            self._endpoint, "events.acknowledge", {"ids": [event.id]}, self._generation
        )

        def done(_generation: int, result: object) -> None:
            if isinstance(result, Exception):
                set_status_tone(self._status, "error")
                self._status.setText(f"Could not mark the event as seen: {result}")
                self._status.show()
                self._footer.show()

        worker.done.connect(done)
        worker.finished.connect(lambda: self._ack_workers.discard(worker))
        worker.finished.connect(worker.deleteLater)
        self._ack_workers.add(worker)
        worker.start()
