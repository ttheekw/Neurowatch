"""The selected event: its saved picture and details, with what to do next.

Pictures come from the page's :class:`EventImageLoader` (saved event images
only; nothing decodes video). Used by the Events page (full) and Playback's
event sidebar (``compact``).
"""

from __future__ import annotations

from PySide6.QtCore import QRectF, QSize, Qt, Signal, SignalInstance
from PySide6.QtGui import (
    QColor,
    QFont,
    QFontMetrics,
    QPainter,
    QPainterPath,
    QPaintEvent,
    QPen,
    QPixmap,
    QShowEvent,
)
from PySide6.QtWidgets import (
    QGridLayout,
    QHBoxLayout,
    QLabel,
    QPushButton,
    QSizePolicy,
    QStackedWidget,
    QVBoxLayout,
    QWidget,
)

from neurowatch.models.events import (
    ACTIVITY_KINDS,
    Event,
    duration_text,
    elapsed_text,
    full_time_text,
    metadata_lines,
)
from neurowatch.theme import active_colors, set_button_variant, set_label_role
from neurowatch.widgets.event_icons import paint_event_icon
from neurowatch.widgets.event_list import EventImageLoader, kind_color, paint_kind_tile

SOURCE_LABELS = {
    "onvif": "Camera (ONVIF)",
    "builtin": "NeuroWatch motion detection",
    "system": "NeuroWatch",
    "user": "You",
}


class _Picture(QWidget):
    """The saved picture at 16:9, with the changed area outlined."""

    retry_requested = Signal()

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setObjectName("eventPicture")
        self.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Preferred)
        self.setMinimumHeight(90)
        self._pixmap: QPixmap | None = None
        self._event: Event | None = None
        self._message = ""
        self._can_retry = False

    def hasHeightForWidth(self) -> bool:  # noqa: N802
        return True

    def heightForWidth(self, width: int) -> int:  # noqa: N802
        return max(90, round(width * 9 / 16))

    def sizeHint(self) -> QSize:
        return QSize(320, 180)

    def show_picture(self, event: Event | None, pixmap: QPixmap | None, message: str = "") -> None:
        self._event, self._pixmap, self._message = event, pixmap, message
        self._can_retry = message.startswith("Picture unavailable")
        self.setCursor(
            Qt.CursorShape.PointingHandCursor if self._can_retry else Qt.CursorShape.ArrowCursor
        )
        self.setToolTip("Click to try again" if self._can_retry else "")
        self.update()

    @property
    def message(self) -> str:
        return self._message

    def mousePressEvent(self, event: object) -> None:
        if self._can_retry:
            self.retry_requested.emit()

    def paintEvent(self, _event: QPaintEvent) -> None:
        colors = active_colors()
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        painter.setRenderHint(QPainter.RenderHint.SmoothPixmapTransform, True)
        width = self.width()
        height = min(self.height(), round(width * 9 / 16))
        frame = QRectF(0, (self.height() - height) / 2, width, height)
        clip = QPainterPath()
        clip.addRoundedRect(frame, 8, 8)
        if self._pixmap is not None and not self._pixmap.isNull():
            painter.fillPath(clip, QColor("#000000"))
            painter.setClipPath(clip)
            size = self._pixmap.size().scaled(
                frame.size().toSize(), Qt.AspectRatioMode.KeepAspectRatio
            )
            target = QRectF(
                frame.center().x() - size.width() / 2,
                frame.center().y() - size.height() / 2,
                size.width(),
                size.height(),
            )
            painter.drawPixmap(target, self._pixmap, QRectF(self._pixmap.rect()))
            if self._event is not None and self._event.bbox is not None:
                x, y, w, h = self._event.bbox
                box = QRectF(
                    target.left() + x * target.width(),
                    target.top() + y * target.height(),
                    w * target.width(),
                    h * target.height(),
                )
                pen = QPen(kind_color(self._event.kind), 2)
                painter.setPen(pen)
                painter.setBrush(Qt.BrushStyle.NoBrush)
                painter.drawRoundedRect(box, 3, 3)
            return
        kind = self._event.kind if self._event else ""
        paint_kind_tile(painter, frame, kind, colors, radius=8)
        if self._message:
            font = QFont(self.font())
            font.setPixelSize(12)
            painter.setFont(font)
            painter.setPen(QColor(colors.text_muted))
            text_rect = frame.adjusted(12, frame.height() * 0.68, -12, -8)
            painter.drawText(
                text_rect,
                int(Qt.AlignmentFlag.AlignHCenter | Qt.AlignmentFlag.AlignTop)
                | int(Qt.TextFlag.TextWordWrap),
                self._message,
            )


class _KindChip(QWidget):
    """The kind's glyph and name in its colour."""

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self._kind = ""
        self._text = ""
        self.setFixedHeight(24)

    def set_kind(self, kind: str, text: str) -> None:
        self._kind, self._text = kind, text
        font = QFont(self.font())
        font.setPixelSize(11)
        font.setWeight(QFont.Weight.Bold)
        self.setFixedWidth(QFontMetrics(font).horizontalAdvance(text.upper()) + 40)
        self.update()

    def paintEvent(self, _event: QPaintEvent) -> None:
        colors = active_colors()
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        color = kind_color(self._kind)
        back = QColor(colors.surface)
        tint = QColor(
            round(color.red() * 0.18 + back.red() * 0.82),
            round(color.green() * 0.18 + back.green() * 0.82),
            round(color.blue() * 0.18 + back.blue() * 0.82),
        )
        rect = QRectF(self.rect())
        painter.setPen(Qt.PenStyle.NoPen)
        painter.setBrush(tint)
        painter.drawRoundedRect(rect, 12, 12)
        paint_event_icon(painter, QRectF(8, 4, 16, 16), self._kind, color)
        font = QFont(self.font())
        font.setPixelSize(11)
        font.setWeight(QFont.Weight.Bold)
        painter.setFont(font)
        painter.setPen(color)
        painter.drawText(
            rect.adjusted(30, 0, -8, 0),
            int(Qt.AlignmentFlag.AlignVCenter | Qt.AlignmentFlag.AlignLeft),
            self._text.upper(),
        )


class EventPreview(QWidget):
    """Details of the selected event, or a hint to select one."""

    play_requested = Signal(object)
    export_requested = Signal(object)
    acknowledge_requested = Signal(object)
    #: A bookmark: change its note or kept footage, or delete it.
    edit_requested = Signal(object)
    delete_requested = Signal(object)

    def __init__(
        self,
        loader: EventImageLoader,
        *,
        compact: bool = False,
        play_text: str = "Play Recording",
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        self.setObjectName("eventPreview")
        self.setAttribute(Qt.WidgetAttribute.WA_StyledBackground, True)
        self._loader = loader
        self._compact = compact
        self._event: Event | None = None
        self._camera = ""
        self._zone_names: dict[str, str] = {}
        loader.ready.connect(self._picture_ready)

        outer = QVBoxLayout(self)
        margin = 0 if compact else 18
        outer.setContentsMargins(margin, margin, margin, margin)
        self._stack = QStackedWidget()
        outer.addWidget(self._stack)

        # Nothing selected.
        empty = QWidget()
        empty.setObjectName("eventPreviewPage")
        empty_layout = QVBoxLayout(empty)
        empty_layout.addStretch(1)
        empty_title = QLabel("No event selected")
        empty_title.setObjectName("eventPreviewEmptyTitle")
        empty_title.setAlignment(Qt.AlignmentFlag.AlignCenter)
        empty_layout.addWidget(empty_title)
        empty_help = QLabel("Select an event to see its picture and details.")
        empty_help.setAlignment(Qt.AlignmentFlag.AlignCenter)
        empty_help.setWordWrap(True)
        set_label_role(empty_help, "muted")
        empty_layout.addWidget(empty_help)
        empty_layout.addStretch(1)
        self._stack.addWidget(empty)

        # An event.
        content = QWidget()
        content.setObjectName("eventPreviewPage")
        layout = QVBoxLayout(content)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(8 if compact else 12)
        self._picture = _Picture()
        if compact:
            self._picture.setMaximumHeight(150)
        self._picture.retry_requested.connect(self.retry)
        layout.addWidget(self._picture)
        chips = QHBoxLayout()
        chips.setSpacing(8)
        self._chip = _KindChip()
        chips.addWidget(self._chip)
        self._state = QLabel()
        self._state.setObjectName("eventPreviewState")
        chips.addWidget(self._state)
        chips.addStretch(1)
        layout.addLayout(chips)
        self._title = QLabel()
        self._title.setObjectName("eventPreviewTitle")
        self._title.setWordWrap(True)
        self._title.setTextFormat(Qt.TextFormat.PlainText)
        layout.addWidget(self._title)
        self._facts = QGridLayout()
        self._facts.setHorizontalSpacing(14)
        self._facts.setVerticalSpacing(6)
        self._facts.setColumnStretch(1, 1)
        layout.addLayout(self._facts)
        self._fact_widgets: list[QLabel] = []
        layout.addStretch(1)
        actions = QHBoxLayout()
        actions.setSpacing(8)
        self._play = QPushButton(play_text)
        set_button_variant(self._play, "primary")
        self._play.clicked.connect(lambda: self._emit(self.play_requested))
        actions.addWidget(self._play, 1)
        self._export = QPushButton("Export Clip…")
        self._export.setToolTip("Prepare a clip of this event; review the trim before saving")
        set_button_variant(self._export, "secondary")
        self._export.clicked.connect(lambda: self._emit(self.export_requested))
        actions.addWidget(self._export, 1)
        layout.addLayout(actions)
        self._ack = QPushButton("Mark as Seen")
        self._ack.setToolTip("Clear this event's alert on the camera tile")
        set_button_variant(self._ack, "ghost")
        self._ack.clicked.connect(lambda: self._emit(self.acknowledge_requested))
        layout.addWidget(self._ack)
        self._ack.setVisible(False)
        self._bookmark_actions = QWidget()
        bookmark_row = QHBoxLayout(self._bookmark_actions)
        bookmark_row.setContentsMargins(0, 0, 0, 0)
        bookmark_row.setSpacing(8)
        self._edit = QPushButton("Edit Bookmark…")
        self._edit.setToolTip("Change the note, or whether this footage is kept")
        set_button_variant(self._edit, "ghost")
        self._edit.clicked.connect(lambda: self._emit(self.edit_requested))
        bookmark_row.addWidget(self._edit, 1)
        self._delete = QPushButton("Delete")
        self._delete.setToolTip("Delete this bookmark (its recordings stay)")
        set_button_variant(self._delete, "ghost")
        self._delete.clicked.connect(lambda: self._emit(self.delete_requested))
        bookmark_row.addWidget(self._delete)
        layout.addWidget(self._bookmark_actions)
        self._bookmark_actions.setVisible(False)
        self._stack.addWidget(content)
        self._stack.setCurrentIndex(0)

    # ------------------------------------------------------------------

    @property
    def event_id(self) -> int | None:
        return self._event.id if self._event else None

    @property
    def current_event(self) -> Event | None:
        return self._event

    @property
    def picture_message(self) -> str:
        return self._picture.message

    def details_text(self) -> str:
        """Everything the pane says about the event, as plain text."""
        lines = [self._title.text()]
        for index in range(0, len(self._fact_widgets), 2):
            lines.append(
                f"{self._fact_widgets[index].text()}: {self._fact_widgets[index + 1].text()}"
            )
        return "\n".join(lines)

    def set_zone_names(self, names: dict[str, str]) -> None:
        self._zone_names = dict(names)

    def set_event(self, event: Event | None, camera_name: str = "Recorder") -> None:
        previous = self.event_id
        self._event, self._camera = event, camera_name
        if event is None:
            self._stack.setCurrentIndex(0)
            self._picture.show_picture(None, None)
            if self._compact:
                # A sidebar gives the list all its room until one is chosen.
                self.hide()
            return
        self._stack.setCurrentIndex(1)
        if self._compact and self.isHidden():
            self.show()
        self._chip.set_kind(event.kind, event.label)
        if event.going_on:
            self._state.setText("Ongoing")
            self._state.setProperty("tone", "warning")
        elif not event.acknowledged and event.kind in ACTIVITY_KINDS:
            self._state.setText("New")
            self._state.setProperty("tone", "info")
        else:
            self._state.setText("")
            self._state.setProperty("tone", "")
        self._state.style().unpolish(self._state)
        self._state.style().polish(self._state)
        self._state.setVisible(bool(self._state.text()))
        description = event.describe(camera_name)
        self._title.setText(
            f"{description} · {camera_name}" if event.camera_id is not None else description
        )
        self._set_facts(event)
        has_camera = event.camera_id is not None
        self._play.setEnabled(has_camera)
        self._export.setEnabled(has_camera)
        self._ack.setVisible(
            not self._compact and not event.acknowledged and event.kind in ACTIVITY_KINDS
        )
        self._bookmark_actions.setVisible(not self._compact and event.kind == "bookmark")
        if previous != event.id or self._picture.message:
            self._show_picture()

    def _set_facts(self, event: Event) -> None:
        for widget in self._fact_widgets:
            self._facts.removeWidget(widget)
            widget.deleteLater()
        self._fact_widgets = []
        facts: list[tuple[str, str]] = [("Started", full_time_text(event.start_ms))]
        if event.going_on:
            facts.append(("Duration", f"Ongoing for {elapsed_text(event)}"))
        elif event.end_ms is not None and event.end_ms != event.start_ms:
            facts.append(("Duration", duration_text(event)))
            if not self._compact:
                facts.append(("Ended", full_time_text(event.end_ms)))
        else:
            facts.append(("Duration", "Instant"))
        if not self._compact and event.source:
            facts.append(("Source", SOURCE_LABELS.get(event.source, event.source)))
        if event.kind == "bookmark":
            facts.append(
                (
                    "Footage",
                    "Kept: cleanup never deletes it"
                    if event.kept
                    else "Normal: deleted with older recordings",
                )
            )
        for line in metadata_lines(event, self._zone_names):
            name, _, value = line.partition(": ")
            if name.startswith("Changed area"):
                continue  # outlined on the picture
            facts.append((name.replace("Peak ", "").capitalize(), value))
        if not self._compact and event.topic and event.kind == "camera_event":
            facts.append(("Topic", event.topic))
        for row, (name, value) in enumerate(facts):
            label = QLabel(name)
            set_label_role(label, "muted")
            text = QLabel(value)
            text.setTextFormat(Qt.TextFormat.PlainText)
            text.setWordWrap(True)
            text.setTextInteractionFlags(Qt.TextInteractionFlag.TextSelectableByMouse)
            self._facts.addWidget(label, row, 0, Qt.AlignmentFlag.AlignTop)
            self._facts.addWidget(text, row, 1)
            self._fact_widgets += [label, text]

    def _show_picture(self) -> None:
        event = self._event
        if event is None:
            return
        if not event.has_image:
            self._picture.show_picture(
                event,
                None,
                "No picture saved for this event" if event.camera_id is not None else "",
            )
            return
        pixmap = self._loader.image(event.id, request=self.isVisible())
        if pixmap is not None:
            self._picture.show_picture(event, pixmap)
            return
        state = self._loader.state(event.id)
        if state == EventImageLoader.MISSING:
            self._picture.show_picture(event, None, "No picture saved for this event")
        elif state == EventImageLoader.FAILED:
            self._picture.show_picture(event, None, "Picture unavailable — click to try again")
        else:
            # Meanwhile, the row's small picture when there is one.
            thumb = self._loader.thumbnail(event.id, request=False)
            self._picture.show_picture(event, thumb, "" if thumb else "Loading picture…")

    def _picture_ready(self, event_id: int) -> None:
        if event_id == self.event_id:
            self._show_picture()

    def retry(self) -> None:
        if self._event is not None:
            self._loader.retry(self._event.id)
            self._show_picture()

    def showEvent(self, event: QShowEvent) -> None:
        super().showEvent(event)
        if self._event is not None:
            self._show_picture()

    def _emit(self, signal: SignalInstance) -> None:
        if self._event is not None:
            signal.emit(self._event)

    def stop(self) -> None:
        self.set_event(None)
