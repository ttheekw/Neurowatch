"""Playback's event sidebar: the events of the loaded recording range,
filtered, with the selected one's picture and details."""

from __future__ import annotations

from PySide6.QtCore import Qt, QTimer, Signal
from PySide6.QtGui import QShowEvent
from PySide6.QtWidgets import (
    QCheckBox,
    QComboBox,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QPushButton,
    QSplitter,
    QVBoxLayout,
    QWidget,
)

from neurowatch.models.events import EVENT_LABELS, Event
from neurowatch.theme import active_colors, set_button_variant, set_label_role
from neurowatch.widgets.event_list import EventImageLoader, EventListView
from neurowatch.widgets.event_preview import EventPreview

#: Rows listed at a time; "Show More" adds as many again.
PAGE_ROWS = 500
#: Seconds of recording shown before an event when it is reviewed.
REVIEW_LEAD_MS = 3_000


class PlaybackEventsPane(QWidget):
    seek_requested = Signal(str, object)  # epoch milliseconds exceed Qt int32
    export_requested = Signal(object)

    def __init__(self, endpoint: str | None, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setObjectName("playbackEvents")
        self.setAttribute(Qt.WidgetAttribute.WA_StyledBackground, True)
        self.setMinimumWidth(0)
        self._events: dict[int, Event] = {}
        self._names: dict[str, str] = {}
        self._zones: dict[str, dict[str, str]] = {}
        self._limit = PAGE_ROWS
        self._live_dirty = False
        self._live_refresh = QTimer(self)
        self._live_refresh.setSingleShot(True)
        self._live_refresh.setInterval(100)
        self._live_refresh.timeout.connect(self._refresh_live)

        layout = QVBoxLayout(self)
        layout.setContentsMargins(12, 12, 12, 12)
        layout.setSpacing(8)
        heading_row = QHBoxLayout()
        heading = QLabel("Events")
        set_label_role(heading, "sectionTitle")
        heading_row.addWidget(heading)
        heading_row.addStretch(1)
        self._count = QLabel("")
        set_label_role(self._count, "muted")
        heading_row.addWidget(self._count)
        layout.addLayout(heading_row)
        self._search = QLineEdit()
        self._search.setPlaceholderText("Search these events…")
        self._search.setAccessibleName("Search events in the loaded playback range")
        self._search.setClearButtonEnabled(True)
        self._search.setMaxLength(200)
        layout.addWidget(self._search)
        filters = QHBoxLayout()
        filters.setSpacing(6)
        self._camera = QComboBox()
        self._camera.addItem("All cameras", "")
        self._camera.setAccessibleName("Playback event camera")
        self._kind = QComboBox()
        self._kind.addItem("All types", "")
        for kind, name in EVENT_LABELS.items():
            if kind not in ("disk_low", "storage_missing", "backend_started"):
                self._kind.addItem(name, kind)
        self._kind.setAccessibleName("Playback event type")
        for field in (self._camera, self._kind):
            field.setMinimumWidth(0)
            field.setSizeAdjustPolicy(
                QComboBox.SizeAdjustPolicy.AdjustToMinimumContentsLengthWithIcon
            )
            field.setMinimumContentsLength(6)
            filters.addWidget(field, 1)
        layout.addLayout(filters)
        self._new_only = QCheckBox("New only")
        self._new_only.setToolTip("Only events not yet marked as seen")
        self._new_only.setAccessibleName("Only events not yet seen")
        layout.addWidget(self._new_only)

        self._images = EventImageLoader(endpoint, self)
        self._list = EventListView(self._images, compact=True)
        self._list.setAccessibleName("Playback events; double-click or press Enter to review")
        self._list.selection_changed.connect(self._selection_changed)
        self._list.activated_event.connect(lambda _event: self.review_selected())
        list_box = QWidget()
        list_box.setObjectName("eventListBox")
        list_layout = QVBoxLayout(list_box)
        list_layout.setContentsMargins(0, 0, 0, 0)
        list_layout.setSpacing(6)
        list_layout.addWidget(self._list, 1)
        more_row = QHBoxLayout()
        self._message = QLabel("Load recordings to review their events.")
        self._message.setWordWrap(True)
        set_label_role(self._message, "muted")
        more_row.addWidget(self._message, 1)
        self._older = QPushButton("Show More")
        set_button_variant(self._older, "ghost")
        self._older.clicked.connect(self.show_older)
        self._older.hide()
        more_row.addWidget(self._older)
        list_layout.addLayout(more_row)

        self._preview = EventPreview(self._images, compact=True, play_text="Review")
        self._preview.hide()
        self._preview.play_requested.connect(lambda _event: self.review_selected())
        self._preview.export_requested.connect(lambda _event: self.export_selected())
        self._split = QSplitter(Qt.Orientation.Vertical)
        self._split.setChildrenCollapsible(False)
        self._split.setHandleWidth(10)
        self._split.addWidget(list_box)
        self._split.addWidget(self._preview)
        self._split.setStretchFactor(0, 1)
        self._split.setStretchFactor(1, 0)
        layout.addWidget(self._split, 1)

        self._search.textChanged.connect(self._filter_changed)
        self._new_only.toggled.connect(self._filter_changed)
        for field in (self._camera, self._kind):
            field.currentIndexChanged.connect(self._filter_changed)

    def apply_theme(self, theme_name: str | None = None) -> None:
        self._list.apply_theme(active_colors(theme_name))

    def selected_event(self) -> Event | None:
        return self._list.selected_event()

    def set_zone_names(self, names: dict[str, dict[str, str]]) -> None:
        self._zones = names

    def set_events(self, events: list[Event], names: dict[str, str] | None = None) -> None:
        self._events = {event.id: event for event in events}
        if names is not None:
            self._names = dict(names)
            self._list.events_model.set_names(self._names)
            chosen = self._camera.currentData()
            self._camera.blockSignals(True)
            self._camera.clear()
            self._camera.addItem("All cameras", "")
            for id_, name in names.items():
                self._camera.addItem(name, id_)
            self._camera.setCurrentIndex(max(0, self._camera.findData(chosen)))
            self._camera.blockSignals(False)
        self._render()

    def update_event(self, event: Event) -> None:
        self._events[event.id] = event
        self._live_dirty = True
        if self.isVisible() and not self._live_refresh.isActive():
            self._live_refresh.start()

    def _refresh_live(self) -> None:
        if self._live_dirty and self.isVisible():
            self._render()

    def showEvent(self, event: QShowEvent) -> None:
        super().showEvent(event)
        if self._live_dirty:
            self._render()

    def set_status(self, message: str) -> None:
        self._message.setText(message)

    def _matches(self, event: Event) -> bool:
        camera, kind = self._camera.currentData(), self._kind.currentData()
        if (camera and event.camera_id != camera) or (kind and event.kind != kind):
            return False
        if self._new_only.isChecked() and event.acknowledged:
            return False
        query = self._search.text().strip().casefold()
        return not query or any(
            query in field.casefold()
            for field in (
                self._names.get(event.camera_id or "", "Recorder"),
                event.label,
                event.detail or "",
                event.topic or "",
                event.source,
            )
        )

    def _filter_changed(self) -> None:
        self._limit = PAGE_ROWS
        self._render()

    def _render(self) -> None:
        self._live_refresh.stop()
        self._live_dirty = False
        events = sorted(
            (event for event in self._events.values() if self._matches(event)),
            key=lambda event: (event.start_ms, event.id),
            reverse=True,
        )
        shown = events[: self._limit]
        selected = self.selected_event()
        self._list.set_events(shown, keep=selected.id if selected else None)
        total = len(self._events)
        self._count.setText(
            f"{len(events):,}" if len(events) == total else f"{len(events):,} of {total:,}"
        )
        if not total:
            self._message.setText("No events in this recording range.")
        elif not events:
            self._message.setText("No events match these filters.")
        elif len(events) > len(shown):
            self._message.setText(f"Showing the newest {len(shown):,}.")
        else:
            self._message.setText("")
        self._message.setVisible(bool(self._message.text()))
        self._older.setVisible(len(events) > len(shown))
        self._selection_changed(self.selected_event())

    def _selection_changed(self, event: Event | None) -> None:
        self._preview.set_zone_names(self._zones.get(event.camera_id or "", {}) if event else {})
        self._preview.set_event(
            event, self._names.get(event.camera_id or "", "Recorder") if event else ""
        )

    def review_selected(self) -> None:
        event = self.selected_event()
        if event is not None and event.camera_id is not None:
            self.seek_requested.emit(event.camera_id, max(0, event.start_ms - REVIEW_LEAD_MS))

    def export_selected(self) -> None:
        event = self.selected_event()
        if event is not None and event.camera_id is not None:
            self.export_requested.emit(event)

    def show_older(self) -> None:
        self._limit += PAGE_ROWS
        self._render()

    def stop(self) -> None:
        self._live_refresh.stop()
        self._live_dirty = False
        self._events.clear()
        self._list.set_events([])
        self._preview.stop()
        self._images.stop()
        self._limit = PAGE_ROWS
        self._count.setText("")
        self._message.setText("Load recordings to review their events.")
        self._message.show()
        self._older.hide()
