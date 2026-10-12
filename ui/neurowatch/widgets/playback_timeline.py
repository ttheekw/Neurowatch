"""Playback timeline bar.

Shows:
  - Horizontal ruler with adaptive time labels
  - Green segment blocks for recorded coverage
  - Red playhead line at the current position
  - Mouse-wheel zoom (preserves cursor position), any length of range
  - Sideways scroll (or Shift+wheel) and a scrollbar to pan when zoomed
  - Click/drag to seek

Controls row below the timeline:
  ▶/⏸  ×¼ ×½ ×1 ×2 ×4 (the current speed highlighted)  Fit  — current time label
"""

from __future__ import annotations

import datetime
import logging

from PySide6.QtCore import QRect, QSize, Qt, QTimer, Signal
from PySide6.QtGui import (
    QColor,
    QMouseEvent,
    QPainter,
    QPaintEvent,
    QResizeEvent,
    QWheelEvent,
)
from PySide6.QtWidgets import (
    QButtonGroup,
    QHBoxLayout,
    QLabel,
    QPushButton,
    QScrollBar,
    QSizePolicy,
    QWidget,
)

from neurowatch.theme import mono_font, ui_font

logger = logging.getLogger(__name__)

_BG = QColor(14, 14, 24)
_DISABLED_BTN = (
    "QPushButton:disabled { background: #16162a; color: #50506a; border-color: #26263e; }"
)
_RULER_BG = QColor(20, 20, 34)
_RULER_TEXT = QColor(160, 160, 190)
_SEGMENT_FILL = QColor(46, 228, 112, 190)
_SEGMENT_BORDER = QColor(16, 140, 72, 230)
_PLAYHEAD = QColor(239, 68, 68)  # red
_GRID_LINE = QColor(30, 30, 50)
_ROW_LABEL_BG = QColor(18, 18, 30)
_ROW_LABEL_TEXT = QColor(150, 150, 180)

_RULER_H = 22  # pixels for time ruler
_SCROLLBAR_H = 12
# Ruler spacings, from close up to several days on screen.
_RULER_STEPS_MS = (
    1_000,
    5_000,
    10_000,
    30_000,
    60_000,
    300_000,
    600_000,
    900_000,
    1_800_000,
    3_600_000,
    7_200_000,
    10_800_000,
    21_600_000,
    43_200_000,
    86_400_000,
)
_MIN_TICK_SPACING_PX = 80
_SPEEDS = ((0.25, "×¼"), (0.5, "×½"), (1.0, "×1"), (2.0, "×2"), (4.0, "×4"))


def _local_offset_ms(at_ms: int) -> int:
    """The local time zone's offset from UTC at `at_ms`."""
    try:
        offset = datetime.datetime.fromtimestamp(at_ms / 1000).astimezone().utcoffset()
    except (OverflowError, OSError, ValueError):
        return 0
    return int(offset.total_seconds() * 1000) if offset else 0


def ruler_ticks(view_start_ms: int, view_end_ms: int, track_w: int) -> tuple[int, list[int]]:
    """Ruler spacing and mark times for a view: on local clock boundaries
    (whole hours, local midnight), as close as about 80 px between labels allows."""
    duration = max(1, view_end_ms - view_start_ms)
    max_ticks = max(2, int(track_w // _MIN_TICK_SPACING_PX))
    step = next((s for s in _RULER_STEPS_MS if duration / s <= max_ticks), _RULER_STEPS_MS[-1])
    offset = _local_offset_ms(view_start_ms)
    first = -(-(view_start_ms + offset) // step) * step - offset
    ticks = list(range(first, view_end_ms + 1, step))
    return step, ticks


def tick_label(at_ms: int, step_ms: int) -> str:
    """A ruler label: seconds close up, the date at midnight, else HH:MM."""
    try:
        local = datetime.datetime.fromtimestamp(at_ms / 1000)
    except (OverflowError, OSError, ValueError):
        return _ms_to_hms(at_ms)
    if step_ms < 60_000:
        return local.strftime("%H:%M:%S")
    if local.hour == 0 and local.minute == 0:
        return local.strftime("%b %d")
    return local.strftime("%H:%M")


_MARGIN = 8
_ROW_LABEL_W = 140
_ROW_GAP = 8
_ROW_H = 24


class PlaybackTimeline(QWidget):
    """Full-width timeline — ruler, segment blocks, playhead, zoom/seek."""

    # Use Python-object payloads for epoch-millisecond timestamps to avoid
    # Qt int32 overflow warnings on large absolute values.
    seek_requested = Signal(object)  # ms absolute timestamp
    playhead_changed = Signal(object)  # ms absolute timestamp (UI display sync)
    play_pause_toggled = Signal(bool)  # True = play, False = pause
    speed_changed = Signal(float)

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self._range_start_ms: int = 0
        self._range_end_ms: int = 3_600_000
        self._view_start_ms: int = 0
        self._view_end_ms: int = 3_600_000
        self._playhead_ms: int = 0
        # [{label: str, segments: [{start_ms, end_ms}]}]
        self._segment_rows: list[dict] = []
        self._is_playing: bool = False
        self._speed: float = 1.0
        self._dragging: bool = False
        self._preferred_height: int = 150

        self.setMinimumHeight(80)
        self.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Preferred)
        self.setMouseTracking(True)

        # Build controls row
        ctrl = QWidget(self)
        ctrl_layout = QHBoxLayout(ctrl)
        ctrl_layout.setContentsMargins(4, 2, 4, 2)
        ctrl_layout.setSpacing(4)

        button_style = (
            "QPushButton { background: #1e1e38; color: #a0a0c0; border: 1px solid #3a3a5a;"
            " border-radius: 3px; font-size: 11px; }"
            "QPushButton:hover { background: #2a2a50; }"
            "QPushButton:pressed { background: #3a3a70; }"
            "QPushButton:checked { background: #34346a; color: #ffffff; border-color: #7070b0; }"
            + _DISABLED_BTN
        )

        self._play_btn = QPushButton("▶ Play")
        self._play_btn.setFixedSize(64, 22)
        self._play_btn.setStyleSheet(
            "QPushButton { background: #1a3a1a; color: #6ee86e; border: 1px solid #3a6a3a;"
            " border-radius: 3px; font-size: 11px; }"
            "QPushButton:hover { background: #2a4a2a; }"
            "QPushButton:pressed { background: #3a5a3a; }" + _DISABLED_BTN
        )
        self._play_btn.clicked.connect(self._on_play_pause)
        ctrl_layout.addWidget(self._play_btn)

        # Speeds: the current one stays highlighted; ×1 is normal speed.
        self._speed_group = QButtonGroup(self)
        self._speed_group.setExclusive(True)
        self._speed_buttons: dict[float, QPushButton] = {}
        for rate, label in _SPEEDS:
            btn = QPushButton(label)
            btn.setCheckable(True)
            btn.setFixedSize(40, 22)
            btn.setStyleSheet(button_style)
            btn.setToolTip("Normal speed" if rate == 1.0 else f"Play at {label}")
            btn.setAccessibleName("Normal speed" if rate == 1.0 else f"Speed {label}")
            btn.clicked.connect(lambda _checked=False, r=rate: self._on_speed(r))
            self._speed_group.addButton(btn)
            self._speed_buttons[rate] = btn
            ctrl_layout.addWidget(btn)
        self._speed_buttons[1.0].setChecked(True)

        self._fit_btn = QPushButton("⤢ Fit")
        self._fit_btn.setFixedSize(52, 22)
        self._fit_btn.setStyleSheet(button_style)
        self._fit_btn.setToolTip("Show the whole loaded range (scroll on the timeline to zoom)")
        self._fit_btn.clicked.connect(self.fit_view)
        ctrl_layout.addWidget(self._fit_btn)
        ctrl_layout.addStretch()

        self._time_label = QLabel("--:--:--")
        self._time_label.setStyleSheet("color: #a0a0c0; font-size: 11px;")
        self._time_label.setFont(mono_font(8))
        ctrl_layout.addWidget(self._time_label)

        # Ticker that advances the playhead while playing
        self._ticker = QTimer(self)
        self._ticker.setInterval(250)
        self._ticker.timeout.connect(self._on_tick)

        # Pans the view when zoomed in (in seconds from the range start).
        self._scrollbar = QScrollBar(Qt.Orientation.Horizontal, self)
        self._scrollbar.setFixedHeight(_SCROLLBAR_H)
        self._scrollbar.setAccessibleName("Timeline position")
        self._scrollbar.valueChanged.connect(self._on_scrollbar)
        self._scrollbar.hide()
        self._syncing_scrollbar = False

        # Place controls at the bottom
        self._ctrl = ctrl
        # Nothing to play until recordings are loaded.
        self._set_controls_enabled(False)
        self._update_height()
        self._layout_controls()

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def set_range(self, start_ms: int, end_ms: int) -> None:
        """Set the total loadable time range."""
        self._range_start_ms = start_ms
        self._range_end_ms = max(end_ms, start_ms + 1000)
        self._view_start_ms = start_ms
        self._view_end_ms = self._range_end_ms
        self._playhead_ms = start_ms
        # Recordings load at normal speed.
        self._speed = 1.0
        self._speed_buttons[1.0].setChecked(True)
        self._sync_scrollbar()
        self.update()
        self._update_time_label()
        self.playhead_changed.emit(self._playhead_ms)

    def fit_view(self) -> None:
        """Show the whole loaded range."""
        self._view_start_ms = self._range_start_ms
        self._view_end_ms = self._range_end_ms
        self._sync_scrollbar()
        self.update()

    def set_segments(self, segments: list[dict]) -> None:
        """Backward-compatible single-row segment API."""
        if not segments:
            self.set_camera_rows([])
            return
        self.set_camera_rows([{"label": "All", "segments": segments}])

    def set_camera_rows(self, rows: list[dict]) -> None:
        """Update per-camera rows.

        Each row: {"label": str, "segments": [{"start_ms": int, "end_ms": int}]}
        Overlapping segments within a row are merged to avoid darker overlap blocks.
        """
        normalized_rows: list[dict] = []
        for row in rows:
            label = str(
                row.get("label") or row.get("camera_name") or row.get("camera_id") or "Camera"
            )
            src_segments = row.get("segments") or []
            parsed: list[dict] = []
            for seg in src_segments:
                s = int(seg.get("start_ms", 0))
                raw_end = seg.get("end_ms")
                e = int(raw_end) if raw_end is not None else s + 60_000
                if e <= s:
                    continue
                parsed.append({"start_ms": s, "end_ms": e})
            merged = self._merge_segments(parsed)
            normalized_rows.append({"label": label, "segments": merged})
        self._segment_rows = normalized_rows
        self._set_controls_enabled(bool(normalized_rows))
        self._update_height()
        self.update()

    def set_playhead(self, ts_ms: int) -> None:
        """Move the playhead without emitting seek_requested (external update)."""
        self._playhead_ms = ts_ms
        self._ensure_playhead_visible()
        self.update()
        self._update_time_label()
        self.playhead_changed.emit(self._playhead_ms)

    def _set_controls_enabled(self, enabled: bool) -> None:
        if not enabled and self._is_playing:
            self.stop_playback()
        for button in self._ctrl.findChildren(QPushButton):
            button.setEnabled(enabled)

    def stop_playback(self) -> None:
        """Stop the ticker and update button state."""
        self._is_playing = False
        self._ticker.stop()
        self._play_btn.setText("▶ Play")

    # ------------------------------------------------------------------
    # Qt events
    # ------------------------------------------------------------------

    def resizeEvent(self, event: QResizeEvent) -> None:
        self._layout_controls()
        super().resizeEvent(event)

    def sizeHint(self) -> QSize:
        return QSize(640, self._preferred_height)

    def paintEvent(self, _event: QPaintEvent) -> None:
        painter = QPainter(self)
        w = self.width()
        track_h = self.height() - self._ctrl.height() - _SCROLLBAR_H - 2

        # Background
        painter.fillRect(0, 0, w, track_h, _BG)

        # Ruler strip
        painter.fillRect(0, 0, w, _RULER_H, _RULER_BG)
        self._draw_ruler(painter, w)

        # Segment blocks
        self._draw_segments(painter, w, track_h)

        # Playhead
        self._draw_playhead(painter, w, track_h)

        painter.end()

    def wheelEvent(self, event: QWheelEvent) -> None:
        sideways = event.angleDelta().x()
        delta = event.angleDelta().y()
        if event.modifiers() & Qt.KeyboardModifier.ShiftModifier and not sideways:
            sideways, delta = delta, 0
        if sideways and abs(sideways) >= abs(delta):
            # Sideways scroll moves through time; zoom is unchanged.
            view_dur = self._view_end_ms - self._view_start_ms
            self._pan_to(self._view_start_ms - int(sideways / 120 * 0.15 * view_dur))
            event.accept()
            return
        if delta == 0:
            super().wheelEvent(event)
            return

        track_left, _, track_w = self._track_bounds()
        if track_w <= 0:
            return

        cursor_frac = max(
            0.0,
            min(1.0, (event.position().x() - track_left) / track_w),
        )
        view_dur = self._view_end_ms - self._view_start_ms
        cursor_ms = self._view_start_ms + cursor_frac * view_dur

        factor = 1.4 if delta > 0 else 1.0 / 1.4
        range_total = self._range_end_ms - self._range_start_ms
        new_dur = max(10_000, min(range_total, view_dur / factor))
        new_start = int(cursor_ms - cursor_frac * new_dur)
        new_start = max(self._range_start_ms, min(new_start, self._range_end_ms - int(new_dur)))

        self._view_start_ms = new_start
        self._view_end_ms = new_start + int(new_dur)
        self._sync_scrollbar()
        self.update()
        event.accept()

    def _pan_to(self, start_ms: int) -> None:
        view_dur = self._view_end_ms - self._view_start_ms
        start = max(self._range_start_ms, min(start_ms, self._range_end_ms - view_dur))
        self._view_start_ms = start
        self._view_end_ms = start + view_dur
        self._sync_scrollbar()
        self.update()

    def _on_scrollbar(self, value: int) -> None:
        if not self._syncing_scrollbar:
            self._pan_to(self._range_start_ms + value * 1000)

    def _sync_scrollbar(self) -> None:
        """Show the scrollbar only when zoomed in, matching the view."""
        range_s = (self._range_end_ms - self._range_start_ms) // 1000
        view_s = max(1, (self._view_end_ms - self._view_start_ms) // 1000)
        zoomed = view_s < range_s
        self._syncing_scrollbar = True
        try:
            self._scrollbar.setRange(0, max(0, range_s - view_s))
            self._scrollbar.setPageStep(view_s)
            self._scrollbar.setSingleStep(max(1, view_s // 10))
            self._scrollbar.setValue((self._view_start_ms - self._range_start_ms) // 1000)
        finally:
            self._syncing_scrollbar = False
        self._scrollbar.setVisible(zoomed)

    def mousePressEvent(self, event: QMouseEvent) -> None:
        if event.button() == Qt.MouseButton.LeftButton:
            ts = self._x_to_ms(event.position().x())
            if ts is not None:
                self._dragging = True
                snapped = self._snap_to_segment(ts)
                self._playhead_ms = snapped
                self.update()
                self.playhead_changed.emit(snapped)
        super().mousePressEvent(event)

    def mouseMoveEvent(self, event: QMouseEvent) -> None:
        if self._dragging and event.buttons() & Qt.MouseButton.LeftButton:
            ts = self._x_to_ms(event.position().x())
            if ts is not None:
                snapped = self._snap_to_segment(ts)
                self._playhead_ms = snapped
                self.update()
                self.playhead_changed.emit(snapped)
        super().mouseMoveEvent(event)

    def mouseReleaseEvent(self, event: QMouseEvent) -> None:
        if event.button() == Qt.MouseButton.LeftButton:
            ts = self._x_to_ms(event.position().x())
            if ts is not None:
                snapped = self._snap_to_segment(ts)
                self._playhead_ms = snapped
                self.update()
                self.playhead_changed.emit(snapped)
                self.seek_requested.emit(snapped)
            self._dragging = False
        super().mouseReleaseEvent(event)

    # ------------------------------------------------------------------
    # Drawing helpers
    # ------------------------------------------------------------------

    def _draw_ruler(self, painter: QPainter, w: int) -> None:
        view_dur = max(1, self._view_end_ms - self._view_start_ms)
        track_left, _, track_w = self._track_bounds(w)
        step, ticks = ruler_ticks(self._view_start_ms, self._view_end_ms, track_w)

        painter.setFont(mono_font(8))
        painter.setPen(_RULER_TEXT)

        for tick in ticks:
            x = int(track_left + (tick - self._view_start_ms) / view_dur * track_w)
            painter.drawLine(x, _RULER_H - 6, x, _RULER_H)
            painter.drawText(
                QRect(x + 2, 2, _MIN_TICK_SPACING_PX - 4, _RULER_H - 2),
                Qt.AlignmentFlag.AlignLeft | Qt.AlignmentFlag.AlignVCenter,
                tick_label(tick, step),
            )

    def _draw_segments(self, painter: QPainter, w: int, track_h: int) -> None:
        if not self._segment_rows:
            return

        view_dur = max(1, self._view_end_ms - self._view_start_ms)
        track_left, _, track_w = self._track_bounds(w)
        rows = self._segment_rows
        row_count = max(1, len(rows))
        usable_h = max(1, track_h - _RULER_H - 8)
        row_gap = _ROW_GAP
        if row_count > 1:
            max_gap_total = max(0, usable_h - row_count)
            row_gap = min(row_gap, max_gap_total // (row_count - 1))
        row_h = max(1, int((usable_h - (row_count - 1) * row_gap) / row_count))
        row_y = _RULER_H + 4

        # Label gutter background + separator.
        if track_left > _MARGIN:
            painter.fillRect(
                QRect(_MARGIN, _RULER_H, track_left - _MARGIN - 2, track_h - _RULER_H),
                _ROW_LABEL_BG,
            )
            painter.setPen(_GRID_LINE)
            painter.drawLine(track_left - 2, _RULER_H, track_left - 2, track_h)

        painter.setFont(ui_font(8))
        for row in rows:
            label = str(row.get("label", "Camera"))
            segments = row.get("segments") or []
            if track_left > _MARGIN:
                painter.setPen(_ROW_LABEL_TEXT)
                painter.drawText(
                    QRect(_MARGIN + 4, row_y, track_left - _MARGIN - 8, row_h),
                    Qt.AlignmentFlag.AlignVCenter | Qt.AlignmentFlag.AlignLeft,
                    label,
                )

            # Keep bars visually thick even with many rows.
            seg_h = max(1, row_h - 2)
            seg_y = row_y + max(0, (row_h - seg_h) // 2)
            for seg in segments:
                s = int(seg.get("start_ms", 0))
                e = int(seg.get("end_ms") or s + 60_000)
                s = max(s, self._view_start_ms)
                e = min(e, self._view_end_ms)
                if e <= s:
                    continue
                sx = int(track_left + (s - self._view_start_ms) / view_dur * track_w)
                ex = int(track_left + (e - self._view_start_ms) / view_dur * track_w)
                rw = max(2, ex - sx)
                painter.fillRect(QRect(sx, seg_y, rw, seg_h), _SEGMENT_FILL)
                painter.setPen(_SEGMENT_BORDER)
                if rw > 2 and seg_h > 2:
                    painter.drawRect(QRect(sx, seg_y, rw - 1, seg_h - 1))

            painter.setPen(_GRID_LINE)
            painter.drawLine(track_left, row_y + row_h + 1, w - _MARGIN, row_y + row_h + 1)
            row_y += row_h + row_gap

    def _draw_playhead(self, painter: QPainter, w: int, track_h: int) -> None:
        if self._range_end_ms <= self._range_start_ms:
            return
        view_dur = max(1, self._view_end_ms - self._view_start_ms)
        if self._playhead_ms < self._view_start_ms or self._playhead_ms > self._view_end_ms:
            return
        track_left, _, track_w = self._track_bounds(w)
        x = int(track_left + (self._playhead_ms - self._view_start_ms) / view_dur * track_w)
        painter.setPen(_PLAYHEAD)
        painter.drawLine(x, 0, x, track_h)

    # ------------------------------------------------------------------
    # Coordinate helpers
    # ------------------------------------------------------------------

    def _x_to_ms(self, x: float) -> int | None:
        track_left, _, track_w = self._track_bounds()
        if track_w <= 0:
            return None
        frac = max(0.0, min(1.0, (x - track_left) / track_w))
        view_dur = self._view_end_ms - self._view_start_ms
        return int(self._view_start_ms + frac * view_dur)

    def _ensure_playhead_visible(self) -> None:
        if self._playhead_ms < self._view_start_ms or self._playhead_ms > self._view_end_ms:
            view_dur = self._view_end_ms - self._view_start_ms
            new_start = self._playhead_ms - view_dur // 2
            new_start = max(
                self._range_start_ms,
                min(new_start, self._range_end_ms - view_dur),
            )
            self._view_start_ms = new_start
            self._view_end_ms = new_start + view_dur
            self._sync_scrollbar()

    # ------------------------------------------------------------------
    # Control handlers
    # ------------------------------------------------------------------

    def toggle_play(self) -> None:
        """Play or pause, as the Play button does (the Space key)."""
        if self._play_btn.isEnabled():
            self._on_play_pause()

    def _on_play_pause(self) -> None:
        self._is_playing = not self._is_playing
        if self._is_playing:
            self._play_btn.setText("⏸ Pause")
            self._ticker.start()
        else:
            self._play_btn.setText("▶ Play")
            self._ticker.stop()
        self.play_pause_toggled.emit(self._is_playing)

    def _on_speed(self, rate: float) -> None:
        self._speed = rate
        self._speed_buttons[rate].setChecked(True)
        self.speed_changed.emit(rate)

    def _stop_at_end(self) -> None:
        """The playhead ran out: show Play and pause the video too, so it never
        keeps running behind a timeline that says it stopped."""
        self.stop_playback()
        self.play_pause_toggled.emit(False)

    def _on_tick(self) -> None:
        if not self._is_playing:
            return
        step_ms = int(250 * self._speed)
        new_ts = self._playhead_ms + step_ms

        if new_ts >= self._range_end_ms:
            new_ts = self._range_end_ms
            self._stop_at_end()
            self._playhead_ms = new_ts
            self._ensure_playhead_visible()
            self.update()
            self._update_time_label()
            self.playhead_changed.emit(self._playhead_ms)
            return

        # If we've entered a gap, jump to the next segment start.
        if not self._is_in_any_segment(new_ts):
            next_start = self._find_next_segment_start(new_ts)
            if next_start is None:
                # No more footage — stop playback.
                self._stop_at_end()
                return
            new_ts = next_start
            self._playhead_ms = new_ts
            self._ensure_playhead_visible()
            self.update()
            self._update_time_label()
            self.playhead_changed.emit(self._playhead_ms)
            self.seek_requested.emit(self._playhead_ms)
            return

        self._playhead_ms = new_ts
        self._ensure_playhead_visible()
        self.update()
        self._update_time_label()
        self.playhead_changed.emit(self._playhead_ms)

    def _is_in_any_segment(self, ts_ms: int) -> bool:
        """Return True if ts_ms falls within any segment across all camera rows."""
        for row in self._segment_rows:
            for seg in row.get("segments") or []:
                if int(seg["start_ms"]) <= ts_ms < int(seg["end_ms"]):
                    return True
        return False

    def _find_next_segment_start(self, ts_ms: int) -> int | None:
        """Return the earliest segment start_ms strictly after ts_ms, or None."""
        best: int | None = None
        for row in self._segment_rows:
            for seg in row.get("segments") or []:
                s = int(seg["start_ms"])
                if s > ts_ms and (best is None or s < best):
                    best = s
        return best

    def _snap_to_segment(self, ts_ms: int) -> int:
        """Snap ts_ms to a valid segment position.

        - If ts_ms is already inside a segment, return it unchanged.
        - Otherwise snap to the start of the nearest segment so playback always
          begins at a clean in-point (segment end is not a valid play position).
        - If no segments exist, return ts_ms unchanged.
        """
        if self._is_in_any_segment(ts_ms):
            return ts_ms

        best_start: int | None = None
        best_dist = -1
        for row in self._segment_rows:
            for seg in row.get("segments") or []:
                s = int(seg["start_ms"])
                dist = abs(s - ts_ms)
                if best_start is None or dist < best_dist:
                    best_start = s
                    best_dist = dist

        return best_start if best_start is not None else ts_ms

    def _update_time_label(self) -> None:
        text = _ms_to_hms(self._playhead_ms)
        if self._range_end_ms - self._range_start_ms > 86_400_000 or self._spans_days():
            try:
                day = datetime.datetime.fromtimestamp(self._playhead_ms / 1000).strftime("%b %d")
                text = f"{day} {text}"
            except (OverflowError, OSError, ValueError):
                pass
        self._time_label.setText(text)

    def _spans_days(self) -> bool:
        try:
            first = datetime.datetime.fromtimestamp(self._range_start_ms / 1000).date()
            last = datetime.datetime.fromtimestamp(self._range_end_ms / 1000).date()
        except (OverflowError, OSError, ValueError):
            return False
        return first != last

    def _layout_controls(self) -> None:
        ctrl_h = self._ctrl.sizeHint().height()
        self._ctrl.setGeometry(0, self.height() - ctrl_h, self.width(), ctrl_h)
        left, _, track_w = self._track_bounds()
        self._scrollbar.setGeometry(
            left, self.height() - ctrl_h - _SCROLLBAR_H - 1, track_w, _SCROLLBAR_H
        )
        base_min = _RULER_H + 20 + _SCROLLBAR_H + ctrl_h
        if self.minimumHeight() < base_min:
            self.setMinimumHeight(base_min)

    def _update_height(self) -> None:
        ctrl_h = self._ctrl.sizeHint().height()
        row_count = max(1, len(self._segment_rows))
        track_h = _RULER_H + 8 + row_count * _ROW_H + max(0, row_count - 1) * _ROW_GAP + 8
        self._preferred_height = track_h + _SCROLLBAR_H + 2 + ctrl_h
        self.updateGeometry()
        self._layout_controls()

    def _track_bounds(self, width: int | None = None) -> tuple[int, int, int]:
        w = self.width() if width is None else width
        label_w = _ROW_LABEL_W if self._segment_rows else 0
        left = _MARGIN + label_w
        right = w - _MARGIN
        if right <= left:
            right = left + 1
        return left, right, right - left

    def _merge_segments(self, segments: list[dict]) -> list[dict]:
        if not segments:
            return []
        items = sorted(segments, key=lambda s: int(s.get("start_ms", 0)))
        merged: list[dict] = []
        for seg in items:
            s = int(seg.get("start_ms", 0))
            e = int(seg.get("end_ms", s + 1))
            if e <= s:
                continue
            if not merged:
                merged.append({"start_ms": s, "end_ms": e})
                continue
            last = merged[-1]
            ls = int(last["start_ms"])
            le = int(last["end_ms"])
            if s <= le:
                last["start_ms"] = min(ls, s)
                last["end_ms"] = max(le, e)
            else:
                merged.append({"start_ms": s, "end_ms": e})
        return merged


# ---------------------------------------------------------------------------
# Utilities
# ---------------------------------------------------------------------------


def _ms_to_hms(ms: int) -> str:
    """Format milliseconds as HH:MM:SS.

    Uses local wall-clock formatting for epoch-like timestamps when possible.
    Falls back safely for out-of-range values to avoid paint-event exceptions.
    """
    total_sec = ms // 1000
    hours = total_sec // 3600
    minutes = (total_sec % 3600) // 60
    seconds = total_sec % 60
    if ms >= 86_400_000:  # >= 24h — likely epoch-like value
        try:
            # datetime supports years up to 9999. Keep UI robust if DB data is malformed.
            if ms <= 253_402_300_799_000:
                dt = datetime.datetime.fromtimestamp(ms / 1000.0)
                return dt.strftime("%H:%M:%S")
        except (OverflowError, OSError, ValueError):
            pass
        # Safe fallback: normalize to time-of-day without datetime conversion.
        day_sec = total_sec % 86_400
        h = day_sec // 3600
        m = (day_sec % 3600) // 60
        s = day_sec % 60
        return f"{h:02d}:{m:02d}:{s:02d}"
    return f"{hours:02d}:{minutes:02d}:{seconds:02d}"
