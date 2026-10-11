"""Multi-camera playback view.

Layout:
  QHBoxLayout
    Left panel (260 px):
      - Camera checklist with search
      - From / To date-time pickers
      - Load Recordings button
      - Stop All button
    Right area (QVBoxLayout):
      - PlaybackGrid  (QGridLayout of CellWidget)
      - PlaybackTimeline (fixed height 140 px)
"""

from __future__ import annotations

import logging
import math
import time
from collections.abc import Callable

from PySide6.QtCore import QDateTime, Qt, QThread, QTimer, Signal
from PySide6.QtGui import QCloseEvent, QKeySequence, QShortcut, QShowEvent
from PySide6.QtWidgets import (
    QCalendarWidget,
    QDateTimeEdit,
    QDialog,
    QDialogButtonBox,
    QGridLayout,
    QGroupBox,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QListWidget,
    QListWidgetItem,
    QPushButton,
    QSizePolicy,
    QSplitter,
    QVBoxLayout,
    QWidget,
)

from neurowatch.ipc.frame_reader import FrameReaderThread
from neurowatch.ipc.sync_client import SyncIpcClient
from neurowatch.models.camera import CameraConfig
from neurowatch.theme import (
    apply_dialog_theme,
    datetime_input_stylesheet,
    icon_path,
    style_button_box,
    style_calendar,
)
from neurowatch.widgets.cell_widget import AUDIO_AVAILABLE, AUDIO_NONE, AUDIO_ON, CellWidget
from neurowatch.widgets.playback_timeline import PlaybackTimeline
from neurowatch.widgets.wheel_guard import guard_wheel

logger = logging.getLogger(__name__)


# ---------------------------------------------------------------------------
# Background IPC worker (same pattern as main.py _IpcCallWorker)
# ---------------------------------------------------------------------------


#: How the time range reads (and its precision: whole minutes).
_RANGE_FORMAT = "yyyy-MM-dd HH:mm"


def _to_minute(value: QDateTime) -> QDateTime:
    """``value`` without seconds and milliseconds."""
    time = value.time()
    return QDateTime(value.date(), time.addSecs(-time.second()).addMSecs(-time.msec()))


def _default_range() -> tuple[QDateTime, QDateTime]:
    end = _to_minute(QDateTime.currentDateTime())
    return end.addSecs(-3600), end


#: How often open sessions are asked whether their recording has audio (a
#: later segment may have it when the first had none).
AUDIO_STATUS_INTERVAL_MS = 3000
#: The first check, once the sessions have opened their first segment.
AUDIO_FIRST_CHECK_MS = 800


class _IpcWorker(QThread):
    """Run a batch of IPC calls in a background thread, emit results."""

    done = Signal(object)  # list[result] or Exception

    def __init__(
        self, calls: list[tuple[str, dict]], endpoint: str, *, tolerant: bool = False
    ) -> None:
        super().__init__()
        self._calls = calls
        self._endpoint = endpoint
        # Tolerant: a failed call (e.g. a session already closed) is logged and
        # the rest still run, so one camera cannot keep the others playing.
        self._tolerant = tolerant

    def run(self) -> None:
        try:
            client = SyncIpcClient(endpoint=self._endpoint)
            results: list[object] = []
            for method, params in self._calls:
                try:
                    results.append(client.call(method, params))
                except Exception as exc:
                    if not self._tolerant:
                        raise
                    logger.warning("Playback control %s failed: %s", method, exc)
                    results.append(exc)
            self.done.emit(results)
        except Exception as exc:
            self.done.emit(exc)


# ---------------------------------------------------------------------------
# Calendar popup helper
# ---------------------------------------------------------------------------


class _CalendarDialog(QDialog):
    def __init__(self, initial: QDateTime, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setWindowTitle("Pick date/time")
        apply_dialog_theme(self)
        layout = QVBoxLayout(self)
        layout.setContentsMargins(16, 16, 16, 14)
        layout.setSpacing(12)
        self._cal = QCalendarWidget()
        self._cal.setObjectName("playbackCalendar")
        self._cal.setAccessibleName("Playback date")
        style_calendar(self._cal)
        self._cal.setSelectedDate(initial.date())
        layout.addWidget(self._cal)
        buttons = QDialogButtonBox(
            QDialogButtonBox.StandardButton.Ok | QDialogButtonBox.StandardButton.Cancel
        )
        buttons.button(QDialogButtonBox.StandardButton.Ok).setText("Select Date")
        buttons.accepted.connect(self.accept)
        buttons.rejected.connect(self.reject)
        style_button_box(buttons)
        layout.addWidget(buttons)

    def selected_date(self) -> QDateTime:
        dt = QDateTime()
        dt.setDate(self._cal.selectedDate())
        return dt


# ---------------------------------------------------------------------------
# PlaybackView
# ---------------------------------------------------------------------------


class PlaybackView(QWidget):
    """Top-level widget shown when the Playback tab is active."""

    def __init__(
        self,
        ipc: SyncIpcClient,
        cameras: list[CameraConfig],
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        self._ipc = ipc
        self._cameras: list[CameraConfig] = list(cameras)

        # Active playback state
        self._sessions: dict[str, dict] = {}  # cam_id → {session_id, shm_key, start_ts, end_ts}
        self._readers: dict[str, FrameReaderThread] = {}  # session_id → thread
        self._cells: dict[str, CellWidget] = {}  # session_id → cell
        self._active_playhead_ms: int | None = None
        # session_id → (window start, frames in window): the shown frame rate.
        self._frame_counts: dict[str, tuple[float, int]] = {}

        self._worker: _IpcWorker | None = None
        self._bg_workers: set[_IpcWorker] = set()
        # Seek, speed and play/pause reach the backend in the order they were
        # made: one control worker at a time, the rest wait here. Parallel
        # workers could deliver Pause before an earlier Play.
        self._control_queue: list[tuple[str, dict]] = []
        self._control_worker: _IpcWorker | None = None

        # Audio: one session heard at a time. Which recordings have audio is
        # read from the backend (`playback.status`) while sessions are open.
        self._setting_range = False
        self._audio_session: str | None = None
        self._has_audio: set[str] = set()  # session ids
        self._audio_chosen = False  # the user picked (or muted): no auto pick
        self._audio_worker: _IpcWorker | None = None
        self._audio_timer = QTimer(self)
        self._audio_timer.setInterval(AUDIO_STATUS_INTERVAL_MS)
        self._audio_timer.timeout.connect(self._poll_audio)

        self._build_ui()

    # ------------------------------------------------------------------
    # Build UI
    # ------------------------------------------------------------------

    def _build_ui(self) -> None:
        root_layout = QHBoxLayout(self)
        root_layout.setContentsMargins(0, 0, 0, 0)
        root_layout.setSpacing(0)

        # ── Left panel ──────────────────────────────────────────────────
        left = QWidget()
        left.setFixedWidth(260)
        left.setStyleSheet("background-color: #0e0e1a; border-right: 1px solid #1e1e36;")
        left_layout = QVBoxLayout(left)
        left_layout.setContentsMargins(8, 8, 8, 8)
        left_layout.setSpacing(6)

        # Search box
        self._search = QLineEdit()
        self._search.setPlaceholderText("Search cameras…")
        self._search.setStyleSheet(
            "background: #1a1a2e; color: #c8c8dc; border: 1px solid #2a2a4e;"
            " border-radius: 3px; padding: 4px;"
        )
        self._search.textChanged.connect(self._filter_camera_list)
        left_layout.addWidget(self._search)

        # Camera checklist
        self._cam_list = QListWidget()
        # Style the check boxes explicitly: the default indicator is nearly
        # invisible on this dark list.
        self._cam_list.setStyleSheet(
            "QListWidget { background: #0e0e1a; color: #c8c8dc; border: none; }"
            "QListWidget::item { padding: 3px 2px; }"
            "QListWidget::indicator { width: 14px; height: 14px; border: 1px solid #68758e;"
            " border-radius: 3px; background: #121824; }"
            "QListWidget::indicator:hover { border-color: #9aa5b8; }"
            "QListWidget::indicator:checked { background: #2563eb; border-color: #2563eb;"
            f' image: url("{icon_path("check.svg")}"); }}'
        )
        self._cam_list.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Expanding)
        left_layout.addWidget(self._cam_list, stretch=1)

        # Date-time range
        dt_group = QGroupBox("Time range")
        dt_group.setStyleSheet(
            "QGroupBox { color: #808098; border: 1px solid #2a2a4e; margin-top: 8px; }"
            "QGroupBox::title { subcontrol-origin: margin; padding: 0 4px; }"
        )
        dt_layout = QVBoxLayout(dt_group)
        dt_layout.setSpacing(4)

        one_hour_ago, now = _default_range()

        self._from_dt = QDateTimeEdit(one_hour_ago)
        self._from_dt.setCalendarPopup(True)
        self._from_dt.setDisplayFormat("yyyy-MM-dd HH:mm")
        self._from_dt.setStyleSheet(self._dt_style())
        self._from_dt.setAccessibleName("Playback range start")
        style_calendar(self._from_dt.calendarWidget())
        from_row = QHBoxLayout()
        from_row.addWidget(QLabel("From:"))
        from_row.addWidget(self._from_dt)
        dt_layout.addLayout(from_row)

        self._to_dt = QDateTimeEdit(now)
        self._to_dt.setCalendarPopup(True)
        self._to_dt.setDisplayFormat("yyyy-MM-dd HH:mm")
        self._to_dt.setStyleSheet(self._dt_style())
        self._to_dt.setAccessibleName("Playback range end")
        style_calendar(self._to_dt.calendarWidget())
        to_row = QHBoxLayout()
        to_row.addWidget(QLabel("To:"))
        to_row.addWidget(self._to_dt)
        dt_layout.addLayout(to_row)
        # The scroll wheel over these fields changed them while scrolling the
        # panel (the end could move before the start unnoticed): only a field
        # clicked into takes the wheel.
        self._wheel_guard = guard_wheel(self, self._from_dt, self._to_dt)
        self._range_edited = False
        for field in (self._from_dt, self._to_dt):
            field.dateTimeChanged.connect(self._on_range_edited)

        left_layout.addWidget(dt_group)

        # Load button
        self._load_btn = QPushButton("▶ Load Recordings")
        self._load_btn.setStyleSheet(
            "QPushButton { background: #1a3a1a; color: #6ee86e; border: 1px solid #3a6a3a;"
            " border-radius: 4px; padding: 6px; font-weight: bold; }"
            "QPushButton:hover { background: #2a4a2a; }"
            "QPushButton:disabled { background: #1a1a2a; color: #505060; }"
        )
        self._load_btn.clicked.connect(self._on_load)
        # A clicked button keeps the keyboard focus, and Space clicks a focused
        # button: Space (play/pause) reloaded the recordings. Space is
        # play/pause here instead (see the shortcut below).
        self._load_btn.setFocusPolicy(Qt.FocusPolicy.TabFocus)
        left_layout.addWidget(self._load_btn)

        # Stop All button
        self._stop_btn = QPushButton("■ Stop All")
        self._stop_btn.setFocusPolicy(Qt.FocusPolicy.TabFocus)
        self._stop_btn.setEnabled(False)
        self._stop_btn.setStyleSheet(
            "QPushButton { background: #3a1a1a; color: #e86e6e; border: 1px solid #6a3a3a;"
            " border-radius: 4px; padding: 6px; font-weight: bold; }"
            "QPushButton:hover { background: #4a2a2a; }"
            "QPushButton:disabled { background: #1a1a2a; color: #505060; }"
        )
        self._stop_btn.clicked.connect(self._stop_all)
        left_layout.addWidget(self._stop_btn)

        root_layout.addWidget(left)

        # ── Right area ──────────────────────────────────────────────────
        right = QWidget()
        right.setStyleSheet("background-color: #12121c;")
        right_layout = QVBoxLayout(right)
        right_layout.setContentsMargins(0, 0, 0, 0)
        right_layout.setSpacing(0)

        self._right_splitter = QSplitter(Qt.Orientation.Vertical)
        self._right_splitter.setChildrenCollapsible(False)
        self._right_splitter.setHandleWidth(8)
        self._right_splitter.setStyleSheet(
            "QSplitter::handle { background: #1e1e32; }"
            "QSplitter::handle:hover { background: #2a2a48; }"
        )

        # Grid container
        self._grid_container = QWidget()
        self._grid_container.setStyleSheet("background-color: #12121c;")
        self._grid_layout = QGridLayout(self._grid_container)
        self._grid_layout.setSpacing(2)
        self._grid_layout.setContentsMargins(2, 2, 2, 2)
        self._right_splitter.addWidget(self._grid_container)

        # Placeholder label shown when no cameras loaded
        self._placeholder = QLabel("Select cameras and a time range, then click  ▶ Load Recordings")
        self._placeholder.setAlignment(Qt.AlignmentFlag.AlignCenter)
        self._placeholder.setStyleSheet("color: #505070; font-size: 13px;")
        self._grid_layout.addWidget(self._placeholder, 0, 0)

        # Timeline
        self._timeline = PlaybackTimeline()
        self._timeline.seek_requested.connect(self._on_seek_all)
        self._timeline.playhead_changed.connect(self._on_playhead_changed)
        self._timeline.play_pause_toggled.connect(self._on_play_pause)
        # Space plays and pauses anywhere in this view (a text field keeps it
        # for typing: Qt gives the focused editor the key first).
        self._space = QShortcut(QKeySequence(Qt.Key.Key_Space), self)
        self._space.setContext(Qt.ShortcutContext.WidgetWithChildrenShortcut)
        self._space.activated.connect(self._toggle_play)
        self._timeline.speed_changed.connect(self._on_speed_all)
        self._right_splitter.addWidget(self._timeline)
        self._right_splitter.setStretchFactor(0, 4)
        self._right_splitter.setStretchFactor(1, 2)
        # Keep timeline compact by default; user can drag handle up/down.
        self._right_splitter.setSizes([820, 140])
        right_layout.addWidget(self._right_splitter)

        root_layout.addWidget(right, stretch=1)

        self._populate_camera_list()

    def _dt_style(self) -> str:
        return datetime_input_stylesheet()

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def refresh_camera_list(self, cameras: list[CameraConfig]) -> None:
        self._cameras = list(cameras)
        self._populate_camera_list()

    # ------------------------------------------------------------------
    # Camera checklist
    # ------------------------------------------------------------------

    def _populate_camera_list(self) -> None:
        self._cam_list.clear()
        for cam in self._cameras:
            item = QListWidgetItem(cam.name)
            item.setData(Qt.ItemDataRole.UserRole, cam.id)
            item.setCheckState(Qt.CheckState.Unchecked)
            self._cam_list.addItem(item)

    def _filter_camera_list(self, text: str) -> None:
        q = text.lower()
        for i in range(self._cam_list.count()):
            item = self._cam_list.item(i)
            if item:
                item.setHidden(q not in item.text().lower())

    def _checked_camera_ids(self) -> list[str]:
        ids = []
        for i in range(self._cam_list.count()):
            item = self._cam_list.item(i)
            if item and item.checkState() == Qt.CheckState.Checked:
                ids.append(item.data(Qt.ItemDataRole.UserRole))
        return ids

    # ------------------------------------------------------------------
    # Load flow
    # ------------------------------------------------------------------

    def _ipc_endpoint(self) -> str:
        """Reuse the endpoint already proven to work during app startup."""
        return str(getattr(self._ipc, "endpoint", getattr(self._ipc, "_endpoint", "")))

    def _start_worker(
        self,
        calls: list[tuple[str, dict]],
        done_handler: Callable[[object], None] | None = None,
        *,
        primary: bool = False,
    ) -> None:
        worker = _IpcWorker(calls, self._ipc_endpoint())
        if done_handler is not None:
            worker.done.connect(done_handler)
        self._bg_workers.add(worker)

        def _cleanup(w: _IpcWorker = worker) -> None:
            self._bg_workers.discard(w)
            if self._worker is w:
                self._worker = None
            w.deleteLater()

        worker.finished.connect(_cleanup)
        if primary:
            self._worker = worker
        worker.start()

    def _queue_controls(self, calls: list[tuple[str, dict]]) -> None:
        """Send playback controls after every earlier one, in order."""
        self._control_queue.extend(calls)
        self._drain_controls()

    def _drain_controls(self) -> None:
        if self._control_worker is not None or not self._control_queue:
            return
        calls, self._control_queue = self._control_queue, []
        worker = _IpcWorker(calls, self._ipc_endpoint(), tolerant=True)
        self._control_worker = worker
        self._bg_workers.add(worker)

        def _finished(w: _IpcWorker = worker) -> None:
            self._bg_workers.discard(w)
            if self._control_worker is w:
                self._control_worker = None
            w.deleteLater()
            self._drain_controls()

        worker.finished.connect(_finished)
        worker.start()

    def _on_load(self) -> None:
        cam_ids = self._checked_camera_ids()
        if not cam_ids:
            self._show_placeholder("Select at least one camera before loading recordings.")
            return

        # Whole minutes, as the fields show them: hidden seconds made two times
        # that read the same compare as different (or the other way round).
        start = _to_minute(self._from_dt.dateTime())
        end = _to_minute(self._to_dt.dateTime())
        start_ms = int(start.toMSecsSinceEpoch())
        end_ms = int(end.toMSecsSinceEpoch())
        if end_ms <= start_ms:
            self._show_placeholder(
                f"The end time ({end.toString(_RANGE_FORMAT)}) must be later than "
                f"the start time ({start.toString(_RANGE_FORMAT)})."
            )
            return

        # Always reflect the selected user range in the timeline immediately.
        self._timeline.set_range(start_ms, end_ms)
        self._timeline.set_camera_rows([])
        self._active_playhead_ms = start_ms

        # Stop any existing sessions first
        self._stop_all()
        self._show_placeholder("Loading recordings for selected cameras and time range…")

        self._load_btn.setEnabled(False)

        # Step 1: get_timeline for each camera, then open sessions
        calls = [
            ("playback.get_timeline", {"camera_id": cid, "start_ms": start_ms, "end_ms": end_ms})
            for cid in cam_ids
        ]

        def _on_timelines_done(
            result: object, c: list[str] = cam_ids, s: int = start_ms, e: int = end_ms
        ) -> None:
            self._on_timeline_loaded(result, c, s, e)

        self._start_worker(calls, _on_timelines_done, primary=True)

    def _on_timeline_loaded(
        self, result: object, cam_ids: list[str], start_ms: int, end_ms: int
    ) -> None:
        self._load_btn.setEnabled(True)
        if isinstance(result, Exception):
            logger.warning("Failed to load timeline: %s", result)
            self._show_placeholder(f"Could not load recordings: {result}")
            return

        timelines: list[list[dict]] = result  # type: ignore[assignment]

        camera_name_by_id = {cam.id: cam.name for cam in self._cameras}
        timeline_rows: list[dict] = []
        open_calls: list[tuple[str, dict]] = []
        cams_with_segs: list[str] = []
        open_start_by_cam: dict[str, int] = {}

        for cam_id, segs in zip(cam_ids, timelines, strict=True):
            row_segments: list[dict] = []
            for seg in segs:
                seg_start = int(seg.get("start_ms", start_ms))
                raw_end = seg.get("end_ms")
                seg_end = int(raw_end) if raw_end is not None else end_ms
                if seg_end <= seg_start:
                    seg_end = seg_start + 1
                seg_start = max(seg_start, start_ms)
                seg_end = min(seg_end, end_ms)
                if seg_end <= seg_start:
                    continue
                row_segments.append({"start_ms": seg_start, "end_ms": seg_end})
            timeline_rows.append(
                {
                    "camera_id": cam_id,
                    "label": camera_name_by_id.get(cam_id, cam_id[:8]),
                    "segments": row_segments,
                }
            )
            if not segs:
                logger.debug("No segments found for camera %s in range", cam_id)
                continue
            cams_with_segs.append(cam_id)
            # Open session at the start of the available range for this camera
            first_seg_start = int(segs[0].get("start_ms", start_ms))
            open_ts = max(start_ms, first_seg_start)
            open_start_by_cam[cam_id] = open_ts
            # Opened paused: each shows its first frame and waits for Play.
            open_calls.append(
                ("playback.open", {"camera_id": cam_id, "ts_ms": open_ts, "playing": False})
            )

        # Keep the timeline aligned to the selected range and show one row per camera.
        self._timeline.set_range(start_ms, end_ms)
        self._timeline.set_camera_rows(timeline_rows)
        self._ensure_timeline_visible()

        if not open_calls:
            logger.info("No recordings found in the selected range")
            self._show_placeholder("No recordings found in the selected range.")
            return

        # Step 2: open playback sessions (only for cameras that had segments)
        def _on_open_done(
            result: object,
            c: list[str] = cams_with_segs,
            o: dict[str, int] = open_start_by_cam,
        ) -> None:
            self._on_sessions_opened(result, c, o)

        self._start_worker(open_calls, _on_open_done, primary=True)

    def _on_sessions_opened(
        self, result: object, cam_ids: list[str], open_start_by_cam: dict[str, int]
    ) -> None:
        if isinstance(result, Exception):
            logger.warning("Failed to open playback sessions: %s", result)
            self._show_placeholder(f"Could not open recordings: {result}")
            return

        session_infos: list[dict] = result  # type: ignore[assignment]

        # Clear existing grid
        self._clear_grid()

        loaded_cam_ids = []
        loaded_playheads: list[int] = []
        target_fps = self._target_playback_fps(len(cam_ids))
        for cam_id, info in zip(cam_ids, session_infos, strict=True):
            if not isinstance(info, dict):
                continue
            session_id = info.get("session_id", "")
            shm_key = info.get("shm_key", "")
            if not session_id or not shm_key:
                continue
            loaded_cam_ids.append(cam_id)
            info_with_open = dict(info)
            info_with_open["open_ts"] = open_start_by_cam.get(
                cam_id, int(info.get("start_ts", 0) or 0)
            )
            self._sessions[cam_id] = info_with_open
            loaded_playheads.append(int(info_with_open["open_ts"]))

            # Create cell
            cell = CellWidget()
            cam = next((c for c in self._cameras if c.id == cam_id), None)
            if cam:
                cell.set_camera(cam)
            # Keep playback grid compact so timeline rows can grow vertically.
            cell.setMinimumSize(64, 36)
            cell.set_mode("playback")
            cell.audio_toggle_requested.connect(
                lambda _cam_id, sid=session_id: self._toggle_audio(sid)
            )
            if self._active_playhead_ms is not None:
                cell.set_overlay_timestamp(self._active_playhead_ms)
            self._cells[session_id] = cell

            # Start frame reader using shm_key (neurowatch_{shm_key} ring; see frame_reader)
            reader = FrameReaderThread(shm_key, max_emit_fps=target_fps)
            reader.frame_ready.connect(self._on_frame)
            reader.start()
            self._readers[session_id] = reader

        if loaded_cam_ids:
            self._rebuild_grid()
            if loaded_playheads:
                self._timeline.set_playhead(min(loaded_playheads))
            # The sessions were opened paused, so nothing plays until Play.
            self._stop_btn.setEnabled(True)
            self._audio_timer.start()
            QTimer.singleShot(AUDIO_FIRST_CHECK_MS, self._poll_audio)
            return

        self._show_placeholder("No playable recordings found for the selected cameras.")

    # ------------------------------------------------------------------
    # Grid management
    # ------------------------------------------------------------------

    def _rebuild_grid(self) -> None:
        """Lay out cells in an approximately square grid."""
        self._clear_grid()
        n = len(self._cells)
        if n == 0:
            self._show_placeholder(self._placeholder.text())
            return

        cols = max(1, math.ceil(math.sqrt(n)))
        for idx, cell in enumerate(self._cells.values()):
            r, c = divmod(idx, cols)
            self._grid_layout.addWidget(cell, r, c)

    def _clear_grid(self) -> None:
        while self._grid_layout.count():
            item = self._grid_layout.takeAt(0)
            w = item.widget() if item else None
            if w and w is not self._placeholder:
                w.hide()
                w.setParent(None)

    def _show_placeholder(self, text: str) -> None:
        self._placeholder.setText(text)
        if self._grid_layout.indexOf(self._placeholder) == -1:
            self._grid_layout.addWidget(self._placeholder, 0, 0)
        self._placeholder.show()

    # ------------------------------------------------------------------
    # Frame routing
    # ------------------------------------------------------------------

    def _on_frame(self, shm_id: str, data: bytes, width: int, height: int, fmt: str) -> None:
        # shm_id == shm_key == "pb_{session_id}"
        # Derive session_id from shm_key
        session_id = shm_id.removeprefix("pb_")
        cell = self._cells.get(session_id)
        if cell:
            fps = self._measure_fps(session_id)
            cell.set_stream_info(f"{width}×{height}", fps)
            cell.update_frame(data, width, height, fmt)

    def _measure_fps(self, session_id: str) -> float | None:
        """Frames shown per second, over windows of about a second."""
        now = time.monotonic()
        since, count = self._frame_counts.get(session_id, (now, 0))
        count += 1
        elapsed = now - since
        if elapsed >= 1.0:
            self._frame_counts[session_id] = (now, 0)
            return count / elapsed
        self._frame_counts[session_id] = (since, count)
        return None

    # ------------------------------------------------------------------
    # Seek / speed / play-pause
    # ------------------------------------------------------------------

    def _on_seek_all(self, ts_ms: int) -> None:
        if not self._sessions:
            return
        calls = [
            ("playback.seek", {"session_id": info["session_id"], "ts_ms": ts_ms})
            for info in self._sessions.values()
            if isinstance(info, dict) and "session_id" in info
        ]
        if calls:
            self._queue_controls(calls)

    def _on_playhead_changed(self, ts_ms: int) -> None:
        self._active_playhead_ms = ts_ms
        for cell in self._cells.values():
            cell.set_overlay_timestamp(ts_ms)

    def _on_speed_all(self, rate: float) -> None:
        if not self._sessions:
            return
        calls = [
            ("playback.set_speed", {"session_id": info["session_id"], "rate": rate})
            for info in self._sessions.values()
            if isinstance(info, dict) and "session_id" in info
        ]
        if calls:
            self._queue_controls(calls)

    def _on_play_pause(self, playing: bool) -> None:
        if not self._sessions:
            if playing:
                self._timeline.stop_playback()
            return
        self._set_playing_all(playing)

    def _set_playing_all(self, playing: bool) -> None:
        if not self._sessions:
            return
        calls = [
            ("playback.set_playing", {"session_id": info["session_id"], "playing": playing})
            for info in self._sessions.values()
            if isinstance(info, dict) and "session_id" in info
        ]
        if calls:
            self._queue_controls(calls)

    # ------------------------------------------------------------------
    # Stop all
    # ------------------------------------------------------------------

    # ------------------------------------------------------------------
    # Time range
    # ------------------------------------------------------------------

    def _toggle_play(self) -> None:
        if self._sessions:
            self._timeline.toggle_play()

    def _on_range_edited(self) -> None:
        if not self._setting_range:
            self._range_edited = True

    def _reset_range(self) -> None:
        """The last hour, ending now (whole minutes)."""
        start, end = _default_range()
        self._setting_range = True
        try:
            self._from_dt.setDateTime(start)
            self._to_dt.setDateTime(end)
        finally:
            self._setting_range = False

    def showEvent(self, event: QShowEvent) -> None:
        super().showEvent(event)
        # A range nobody changed follows the clock: opened hours after the app
        # started, it ends now rather than at the start time.
        if not self._range_edited and not self._sessions:
            self._reset_range()

    # ------------------------------------------------------------------
    # Audio: one camera heard at a time
    # ------------------------------------------------------------------

    def _poll_audio(self) -> None:
        """Ask which open sessions' recordings have audio (off the GUI thread)."""
        if self._audio_worker is not None or not self._cells:
            return
        session_ids = list(self._cells)
        calls = [("playback.status", {"session_id": sid}) for sid in session_ids]
        worker = _IpcWorker(calls, self._ipc_endpoint(), tolerant=True)
        self._audio_worker = worker
        self._bg_workers.add(worker)

        def _done(result: object, ids: list[str] = session_ids) -> None:
            self._on_audio_status(ids, result)

        def _finished(w: _IpcWorker = worker) -> None:
            self._bg_workers.discard(w)
            if self._audio_worker is w:
                self._audio_worker = None
            w.deleteLater()

        worker.done.connect(_done)
        worker.finished.connect(_finished)
        worker.start()

    def _on_audio_status(self, session_ids: list[str], result: object) -> None:
        if not isinstance(result, list):
            return
        for sid, status in zip(session_ids, result, strict=False):
            if sid not in self._cells or not isinstance(status, dict):
                continue
            if status.get("has_audio") is True:
                self._has_audio.add(sid)
            else:
                self._has_audio.discard(sid)
        if self._audio_session not in self._has_audio and self._audio_session is not None:
            self._audio_session = None
        if self._audio_session is None and not self._audio_chosen:
            # The first camera (in grid order) with audio is heard.
            first = next((sid for sid in self._cells if sid in self._has_audio), None)
            if first is not None:
                self._set_audio_session(first)
                return
        self._refresh_audio_cells()

    def _toggle_audio(self, session_id: str) -> None:
        """A tile's speaker: hear this camera, or mute it if it is the one heard."""
        self._audio_chosen = True
        self._set_audio_session(None if session_id == self._audio_session else session_id)

    def _set_audio_session(self, session_id: str | None) -> None:
        previous, self._audio_session = self._audio_session, session_id
        calls: list[tuple[str, dict]] = []
        if previous is not None and previous != session_id:
            calls.append(("playback.set_audio", {"session_id": previous, "enabled": False}))
        if session_id is not None:
            calls.append(("playback.set_audio", {"session_id": session_id, "enabled": True}))
        if calls:
            self._queue_controls(calls)
        self._refresh_audio_cells()

    def _refresh_audio_cells(self) -> None:
        for sid, cell in self._cells.items():
            if sid not in self._has_audio:
                cell.set_audio(AUDIO_NONE)
            else:
                cell.set_audio(AUDIO_ON if sid == self._audio_session else AUDIO_AVAILABLE)

    @property
    def audio_session(self) -> str | None:
        return self._audio_session

    def _stop_all(self) -> None:
        self._timeline.stop_playback()
        self._audio_timer.stop()
        self._audio_session = None
        self._has_audio.clear()
        self._audio_chosen = False
        # Controls not yet sent were for the sessions being closed.
        self._control_queue.clear()

        # Stop all frame readers
        for reader in self._readers.values():
            reader.stop()
        self._readers.clear()

        # Close all backend sessions
        if self._sessions:
            calls = [
                ("playback.close", {"session_id": info["session_id"]})
                for info in self._sessions.values()
                if isinstance(info, dict) and "session_id" in info
            ]
            if calls:
                try:
                    client = SyncIpcClient(endpoint=self._ipc_endpoint())
                    for method, params in calls:
                        try:
                            client.call(method, params)
                        except Exception:
                            pass
                except Exception:
                    pass
        self._sessions.clear()
        self._frame_counts.clear()
        self._active_playhead_ms = None

        # Clean up cells
        self._cells.clear()
        self._clear_grid()
        self._show_placeholder("Select cameras and a time range, then click  ▶ Load Recordings")

        self._stop_btn.setEnabled(False)

    def _ensure_timeline_visible(self) -> None:
        sizes = self._right_splitter.sizes()
        if len(sizes) != 2:
            return
        total = max(1, sizes[0] + sizes[1])
        min_timeline = max(self._timeline.minimumHeight(), 96)
        if sizes[1] >= min_timeline:
            return
        bottom = min(total - 80, min_timeline)
        top = max(80, total - bottom)
        self._right_splitter.setSizes([top, total - top])

    def _target_playback_fps(self, camera_count: int) -> float:
        """Adaptive playback preview FPS to keep CPU in check for multi-camera loads."""
        if camera_count <= 1:
            return 18.0
        if camera_count <= 4:
            return 12.0
        if camera_count <= 9:
            return 8.0
        return 6.0

    # ------------------------------------------------------------------
    # Cleanup
    # ------------------------------------------------------------------

    def closeEvent(self, event: QCloseEvent) -> None:
        self._stop_all()
        for worker in list(self._bg_workers):
            if worker.isRunning():
                worker.wait()
        self._bg_workers.clear()
        super().closeEvent(event)
