"""Export Video: cut a clip from a camera's recordings and save it as an MP4.

Pick a camera and a time range and load it: the recordings show on a
timeline and in a preview that plays them, as the Playback tab does. Drag the
timeline's yellow handles (or type the clip's start and end, or set them at
the playhead), then **Export…** asks where to save the file. The backend
writes the MP4 (``export.start``, ``crates/nw-backend/src/export``); the
dialog follows its progress (``export.status``) and can cancel it
(``export.cancel``). Every IPC call runs on a worker thread.
"""

from __future__ import annotations

import logging
import os
from collections.abc import Callable
from typing import Any, ClassVar

from PySide6.QtCore import QDateTime, QSize, QStandardPaths, Qt, QThread, QTimer, QUrl, Signal
from PySide6.QtGui import QDesktopServices, QGuiApplication
from PySide6.QtWidgets import (
    QComboBox,
    QDateTimeEdit,
    QDialog,
    QFileDialog,
    QHBoxLayout,
    QLabel,
    QProgressBar,
    QPushButton,
    QSizePolicy,
    QStackedWidget,
    QWidget,
)

from neurowatch.dialogs.message import ask
from neurowatch.ipc.frame_reader import FrameReaderThread
from neurowatch.ipc.sync_client import SyncIpcClient
from neurowatch.models import clip_export
from neurowatch.models.camera import CameraConfig
from neurowatch.theme import (
    apply_dialog_theme,
    set_button_variant,
    set_label_role,
    set_status_tone,
    style_calendar,
)
from neurowatch.widgets.cell_widget import CellWidget
from neurowatch.widgets.trim_timeline import MIN_TRIM_MS, TrimTimeline
from neurowatch.widgets.wheel_guard import guard_wheel
from neurowatch.widgets.window_chrome import install_dialog_chrome, resize_window

logger = logging.getLogger(__name__)

_RANGE_FORMAT = "yyyy-MM-dd HH:mm"
_CLIP_FORMAT = "yyyy-MM-dd HH:mm:ss"
#: Preview frame rate (one camera, a small tile).
_PREVIEW_FPS = 15.0
#: Status polls that may fail in a row before the export is given up on.
_MAX_POLL_FAILURES = 5
_PLACEHOLDER = "Choose a camera and a time range, then Load."
#: The window's size when it opens, at most ``MAX_SCREEN_SHARE`` of the screen
#: each way.
DEFAULT_WIDTH = 1180
DEFAULT_HEIGHT = 860
MAX_SCREEN_SHARE = 0.75
#: The preview never gets narrower than this (16:9).
PREVIEW_MIN_WIDTH = 480

# Workers outlive a closed dialog (closing the preview session, cancelling an
# export); keep them referenced until they finish.
_LIVE_WORKERS: set[QThread] = set()


class _IpcCalls(QThread):
    """IPC calls run in order off the GUI thread; emits the results, or the
    first exception."""

    done = Signal(object)  # list[result] or Exception

    def __init__(self, endpoint: str, calls: list[tuple[str, dict[str, Any]]]) -> None:
        super().__init__()
        self._endpoint = endpoint
        self._calls = calls

    def run(self) -> None:
        try:
            client = SyncIpcClient(endpoint=self._endpoint or None)
            self.done.emit([client.call(method, params) for method, params in self._calls])
        except Exception as exc:  # reported in the dialog, never raised in Qt
            self.done.emit(exc)


def _to_minute(value: QDateTime) -> QDateTime:
    time = value.time()
    return QDateTime(value.date(), time.addSecs(-time.second()).addMSecs(-time.msec()))


def _ms(value: QDateTime) -> int:
    return int(value.toMSecsSinceEpoch())


def _shown(ms: int) -> str:
    return QDateTime.fromMSecsSinceEpoch(ms).toString(_RANGE_FORMAT)


class ExportVideoDialog(QDialog):
    """Choose a clip from one camera's recordings and export it as an MP4."""

    #: The folder the last clip was saved to (for the next save dialog).
    last_folder: ClassVar[str] = ""

    def __init__(
        self,
        cameras: list[CameraConfig],
        endpoint: str,
        parent: QWidget | None = None,
        *,
        camera_id: str | None = None,
    ) -> None:
        super().__init__(parent)
        self.setWindowTitle("Export Video")
        apply_dialog_theme(self)
        self._cameras = list(cameras)
        self._endpoint = endpoint
        self._closed = False
        # The loaded camera and range, once recordings were found.
        self._loaded: tuple[str, int, int] | None = None
        self._load_generation = 0
        self._session_id: str | None = None
        self._reader: FrameReaderThread | None = None
        self._syncing = False
        # Seek / play / speed reach the backend in order, one batch at a time.
        self._controls: list[tuple[str, dict[str, Any]]] = []
        self._control_worker: _IpcCalls | None = None
        self._job: clip_export.ExportJob | None = None
        self._starting = False
        self._poll_worker: _IpcCalls | None = None
        self._poll_failures = 0
        self._poll_timer = QTimer(self)
        self._poll_timer.setInterval(clip_export.POLL_INTERVAL_MS)
        self._poll_timer.timeout.connect(self._poll)

        layout = install_dialog_chrome(self, "Export video", resizable=True)
        intro = QLabel(
            "Load a camera's recordings, then drag the yellow handles on the timeline "
            "to choose the clip. Export saves it as an MP4 file."
        )
        intro.setObjectName("dialogSubtitle")
        intro.setWordWrap(True)
        layout.addWidget(intro)

        layout.addLayout(self._build_source_row())

        # Preview: a placeholder until a session shows the recording.
        # The preview takes the space the window has to spare (the picture is
        # letterboxed inside it); everything else keeps its height.
        self._preview = QStackedWidget()
        self._preview.setMinimumSize(PREVIEW_MIN_WIDTH, PREVIEW_MIN_WIDTH * 9 // 16)
        self._preview.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Expanding)
        self._placeholder = QLabel(_PLACEHOLDER)
        self._placeholder.setAlignment(Qt.AlignmentFlag.AlignCenter)
        self._placeholder.setWordWrap(True)
        set_label_role(self._placeholder, "hint")
        self._cell = CellWidget()
        self._cell.set_mode("playback")
        self._preview.addWidget(self._placeholder)
        self._preview.addWidget(self._cell)
        layout.addWidget(self._preview, stretch=1)

        self.timeline = TrimTimeline()
        self.timeline.setMinimumHeight(140)
        self.timeline.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Fixed)
        self.timeline.seek_requested.connect(self._on_seek)
        self.timeline.playhead_changed.connect(self._cell.set_overlay_timestamp)
        self.timeline.play_pause_toggled.connect(self._on_play_pause)
        self.timeline.speed_changed.connect(self._on_speed)
        self.timeline.trim_changed.connect(self._show_trim)
        layout.addWidget(self.timeline)

        layout.addLayout(self._build_clip_row())

        self.status_label = QLabel("")
        self.status_label.setWordWrap(True)
        self.status_label.setTextInteractionFlags(Qt.TextInteractionFlag.TextSelectableByMouse)
        layout.addWidget(self.status_label)
        self.progress = QProgressBar()
        self.progress.setRange(0, 1000)
        self.progress.setTextVisible(False)
        self.progress.setAccessibleName("Export progress")
        self.progress.hide()
        layout.addWidget(self.progress)

        layout.addLayout(self._build_button_row())

        # A scroll passing over a field must not change it (one notch moved
        # the range's year back unnoticed): only a field clicked into scrolls.
        self._wheel_guard = guard_wheel(
            self,
            self.camera_combo,
            self.from_edit,
            self.to_edit,
            self.start_edit,
            self.end_edit,
        )
        resize_window(self, self._initial_size())

        if camera_id is not None:
            index = self.camera_combo.findData(camera_id)
            if index >= 0:
                self.camera_combo.setCurrentIndex(index)
        if not self._cameras:
            self.load_button.setEnabled(False)
            self._set_status("Add a camera first: there is nothing to export.", "muted")
        self._set_clip_enabled(False)

    # ------------------------------------------------------------------
    # Layout
    # ------------------------------------------------------------------

    def _build_source_row(self) -> QHBoxLayout:
        row = QHBoxLayout()
        row.setSpacing(8)
        self.camera_combo = QComboBox()
        self.camera_combo.setAccessibleName("Camera")
        self.camera_combo.setMinimumWidth(180)
        for cam in self._cameras:
            self.camera_combo.addItem(cam.name or cam.id[:8], cam.id)
        end = _to_minute(QDateTime.currentDateTime())
        self.from_edit = self._datetime_edit(end.addSecs(-3600), _RANGE_FORMAT, "Range start")
        self.to_edit = self._datetime_edit(end, _RANGE_FORMAT, "Range end")
        self.load_button = QPushButton("Load")
        self.load_button.setAccessibleName("Load recordings")
        set_button_variant(self.load_button, "secondary")
        self.load_button.clicked.connect(self._on_load)
        for label, widget in (
            ("Camera", self.camera_combo),
            ("From", self.from_edit),
            ("To", self.to_edit),
        ):
            row.addWidget(QLabel(label))
            row.addWidget(widget)
        row.addStretch()
        row.addWidget(self.load_button)
        return row

    def _build_clip_row(self) -> QHBoxLayout:
        row = QHBoxLayout()
        row.setSpacing(8)
        now = QDateTime.currentDateTime()
        self.start_edit = self._datetime_edit(now, _CLIP_FORMAT, "Clip start")
        self.end_edit = self._datetime_edit(now, _CLIP_FORMAT, "Clip end")
        self.start_here = QPushButton("Start at playhead")
        self.end_here = QPushButton("End at playhead")
        for button in (self.start_here, self.end_here):
            set_button_variant(button, "secondary")
        self.start_here.clicked.connect(lambda: self._set_edge_at_playhead(start=True))
        self.end_here.clicked.connect(lambda: self._set_edge_at_playhead(start=False))
        for edit in (self.start_edit, self.end_edit):
            edit.dateTimeChanged.connect(self._on_clip_edited)
        self.length_label = QLabel("")
        set_label_role(self.length_label, "emphasis")
        row.addWidget(QLabel("Clip"))
        row.addWidget(self.start_edit)
        row.addWidget(self.start_here)
        row.addWidget(QLabel("to"))
        row.addWidget(self.end_edit)
        row.addWidget(self.end_here)
        row.addStretch()
        row.addWidget(self.length_label)
        return row

    def _build_button_row(self) -> QHBoxLayout:
        row = QHBoxLayout()
        self.show_button = QPushButton("Show in Folder")
        set_button_variant(self.show_button, "secondary")
        self.show_button.clicked.connect(self._show_in_folder)
        self.show_button.hide()
        self.cancel_export_button = QPushButton("Cancel Export")
        set_button_variant(self.cancel_export_button, "danger")
        self.cancel_export_button.clicked.connect(self._cancel_export)
        self.cancel_export_button.hide()
        self.close_button = QPushButton("Close")
        set_button_variant(self.close_button, "secondary")
        self.close_button.clicked.connect(self.reject)
        self.export_button = QPushButton("Export…")
        self.export_button.setAccessibleName("Export clip")
        set_button_variant(self.export_button, "primary")
        self.export_button.clicked.connect(self._on_export)
        row.addWidget(self.show_button)
        row.addWidget(self.cancel_export_button)
        row.addStretch()
        row.addWidget(self.close_button)
        row.addWidget(self.export_button)
        return row

    def _initial_size(self) -> QSize:
        """Large enough for a useful preview, well within the screen: GNOME
        maximizes a window that opens nearly as large as the screen."""
        wanted = QSize(DEFAULT_WIDTH, DEFAULT_HEIGHT)
        screen = self.screen() or QGuiApplication.primaryScreen()
        if screen is not None:
            room = screen.availableGeometry().size() * MAX_SCREEN_SHARE
            wanted = wanted.boundedTo(room)
        return wanted.expandedTo(self.minimumSizeHint())

    def _datetime_edit(self, value: QDateTime, fmt: str, name: str) -> QDateTimeEdit:
        edit = QDateTimeEdit(value)
        edit.setDisplayFormat(fmt)
        edit.setCalendarPopup(True)
        edit.setAccessibleName(name)
        style_calendar(edit.calendarWidget())
        return edit

    # ------------------------------------------------------------------
    # Workers
    # ------------------------------------------------------------------

    def _run(
        self,
        calls: list[tuple[str, dict[str, Any]]],
        handler: Callable[[object], None] | None = None,
    ) -> _IpcCalls:
        worker = _IpcCalls(self._endpoint, calls)
        if handler is not None:

            def _deliver(result: object) -> None:
                if not self._closed:
                    handler(result)

            worker.done.connect(_deliver)
        _LIVE_WORKERS.add(worker)

        def _finished(w: _IpcCalls = worker) -> None:
            _LIVE_WORKERS.discard(w)
            w.deleteLater()

        worker.finished.connect(_finished)
        worker.start()
        return worker

    def _control(self, method: str, **params: Any) -> None:
        """A preview control, sent after every earlier one."""
        if self._session_id is None:
            return
        self._controls.append((method, {"session_id": self._session_id, **params}))
        self._drain_controls()

    def _drain_controls(self) -> None:
        if self._control_worker is not None or not self._controls or self._closed:
            return
        calls, self._controls = self._controls, []
        worker = self._run(calls, self._on_control_done)
        self._control_worker = worker
        worker.finished.connect(self._on_control_finished)

    def _on_control_done(self, result: object) -> None:
        if isinstance(result, Exception):
            logger.warning("Export preview control failed: %s", result)

    def _on_control_finished(self) -> None:
        self._control_worker = None
        self._drain_controls()

    # ------------------------------------------------------------------
    # Loading recordings and the preview
    # ------------------------------------------------------------------

    def _camera(self, camera_id: str) -> CameraConfig | None:
        return next((c for c in self._cameras if c.id == camera_id), None)

    def _on_load(self) -> None:
        camera_id = self.camera_combo.currentData()
        if not camera_id:
            return
        start = _ms(_to_minute(self.from_edit.dateTime()))
        end = _ms(_to_minute(self.to_edit.dateTime()))
        if end <= start:
            self._set_status(
                f"The range's end ({_shown(end)}) must be later than its start ({_shown(start)}).",
                "error",
            )
            return
        self._close_preview()
        self._load_generation += 1
        generation = self._load_generation
        self._loaded = None
        self._set_clip_enabled(False)
        self.timeline.set_range(start, end)
        self.timeline.set_camera_rows([])
        self.timeline.clear_trim()
        self.load_button.setEnabled(False)
        self._set_status("Loading recordings…", "muted")
        params = {"camera_id": camera_id, "start_ms": start, "end_ms": end}

        def _loaded(result: object) -> None:
            self._on_timeline(result, generation, str(camera_id), start, end)

        self._run([("playback.get_timeline", params)], _loaded)

    def _on_timeline(
        self, result: object, generation: int, camera_id: str, start: int, end: int
    ) -> None:
        if generation != self._load_generation:
            return
        self.load_button.setEnabled(True)
        if isinstance(result, Exception) or not isinstance(result, list) or not result:
            self._set_status(f"Could not load the recordings: {result}", "error")
            return
        segments = []
        for seg in result[0] or []:
            seg_start = max(start, int(seg.get("start_ms", start)))
            raw_end = seg.get("end_ms")
            seg_end = min(end, int(raw_end) if raw_end is not None else end)
            if seg_end > seg_start:
                segments.append({"start_ms": seg_start, "end_ms": seg_end})
        camera = self._camera(camera_id)
        name = camera.name if camera and camera.name else camera_id[:8]
        self.timeline.set_camera_rows(
            [{"camera_id": camera_id, "label": name, "segments": segments}]
        )
        if not segments:
            self._set_status(f"No recordings of {name} in this range.", "warning")
            self._placeholder.setText("No recordings in this range.")
            return
        first = min(s["start_ms"] for s in segments)
        last = max(s["end_ms"] for s in segments)
        last = min(last, first + clip_export.MAX_CLIP_MS)
        self._loaded = (camera_id, start, end)
        for edit in (self.start_edit, self.end_edit):
            edit.setDateTimeRange(
                QDateTime.fromMSecsSinceEpoch(start), QDateTime.fromMSecsSinceEpoch(end)
            )
        self.timeline.set_trim(first, last)
        trim = self.timeline.trim() or (first, last)
        self._show_trim(*trim)
        self.timeline.set_playhead(first)
        self._set_clip_enabled(True)
        self._set_status("", "muted")
        self._placeholder.setText("Opening the preview…")
        if camera is not None:
            self._cell.set_camera(camera)

        def _opened(result: object) -> None:
            self._on_preview_opened(result, generation)

        self._run(
            [("playback.open", {"camera_id": camera_id, "ts_ms": first, "playing": False})],
            _opened,
        )

    def _on_preview_opened(self, result: object, generation: int) -> None:
        info = result[0] if isinstance(result, list) and result else None
        session_id = str(info.get("session_id", "")) if isinstance(info, dict) else ""
        shm_key = str(info.get("shm_key", "")) if isinstance(info, dict) else ""
        if generation != self._load_generation:
            if session_id:
                self._run([("playback.close", {"session_id": session_id})])
            return
        if not session_id or not shm_key:
            logger.warning("Export preview could not be opened: %s", result)
            self._placeholder.setText("The preview could not be opened; export still works.")
            return
        self._session_id = session_id
        reader = FrameReaderThread(shm_key, max_emit_fps=_PREVIEW_FPS)
        reader.frame_ready.connect(self._on_frame)
        reader.start()
        self._reader = reader
        self._preview.setCurrentWidget(self._cell)

    def _on_frame(self, _shm_id: str, data: bytes, width: int, height: int, fmt: str) -> None:
        self._cell.update_frame(data, width, height, fmt)

    def _close_preview(self) -> None:
        self.timeline.stop_playback()
        if self._reader is not None:
            self._reader.stop()
            self._reader = None
        self._controls.clear()
        if self._session_id is not None:
            self._run([("playback.close", {"session_id": self._session_id})])
            self._session_id = None
        self._placeholder.setText(_PLACEHOLDER)
        self._preview.setCurrentWidget(self._placeholder)

    def _on_seek(self, ts_ms: int) -> None:
        self._control("playback.seek", ts_ms=int(ts_ms))

    def _on_play_pause(self, playing: bool) -> None:
        if self._session_id is None:
            if playing:
                self.timeline.stop_playback()
            return
        self._control("playback.set_playing", playing=playing)

    def _on_speed(self, rate: float) -> None:
        self._control("playback.set_speed", rate=rate)

    # ------------------------------------------------------------------
    # The clip
    # ------------------------------------------------------------------

    def clip(self) -> tuple[int, int] | None:
        """The chosen clip (epoch ms), once recordings are loaded."""
        return self.timeline.trim() if self._loaded else None

    def _show_trim(self, start: int, end: int) -> None:
        """Show the clip in the fields and its length."""
        self._syncing = True
        try:
            self.start_edit.setDateTime(QDateTime.fromMSecsSinceEpoch(int(start)))
            self.end_edit.setDateTime(QDateTime.fromMSecsSinceEpoch(int(end)))
        finally:
            self._syncing = False
        problem = clip_export.check_clip(int(start), int(end))
        self.length_label.setText(problem or f"Length {clip_export.format_length(end - start)}")
        self._update_export_button()

    def _on_clip_edited(self) -> None:
        if self._syncing or not self._loaded:
            return
        start = _ms(self.start_edit.dateTime())
        end = _ms(self.end_edit.dateTime())
        if self.sender() is self.start_edit:
            end = max(end, start + MIN_TRIM_MS)
        else:
            start = min(start, end - MIN_TRIM_MS)
        self.timeline.set_trim(start, end)
        trim = self.timeline.trim()
        if trim is not None:
            self._show_trim(*trim)

    def _set_edge_at_playhead(self, *, start: bool) -> None:
        trim = self.timeline.trim()
        if trim is None:
            return
        at = self.timeline.playhead()
        if start:
            self.timeline.set_trim(at, max(trim[1], at + MIN_TRIM_MS))
        else:
            self.timeline.set_trim(min(trim[0], at - MIN_TRIM_MS), at)
        trim = self.timeline.trim()
        if trim is not None:
            self._show_trim(*trim)

    def _set_clip_enabled(self, enabled: bool) -> None:
        for widget in (self.start_edit, self.end_edit, self.start_here, self.end_here):
            widget.setEnabled(enabled)
        if not enabled:
            self.length_label.setText("")
        self._update_export_button()

    def _update_export_button(self) -> None:
        clip = self.clip()
        valid = clip is not None and clip_export.check_clip(*clip) is None
        self.export_button.setEnabled(valid and not self.export_running())

    # ------------------------------------------------------------------
    # Export
    # ------------------------------------------------------------------

    def export_running(self) -> bool:
        return self._starting or (self._job is not None and not self._job.finished)

    def _save_folder(self) -> str:
        if ExportVideoDialog.last_folder and os.path.isdir(ExportVideoDialog.last_folder):
            return ExportVideoDialog.last_folder
        movies = QStandardPaths.writableLocation(QStandardPaths.StandardLocation.MoviesLocation)
        return movies if movies and os.path.isdir(movies) else os.path.expanduser("~")

    def _on_export(self) -> None:
        clip = self.clip()
        if clip is None or self._loaded is None or self.export_running():
            return
        problem = clip_export.check_clip(*clip)
        if problem:
            self._set_status(problem, "error")
            return
        camera_id = self._loaded[0]
        camera = self._camera(camera_id)
        suggested = os.path.join(
            self._save_folder(),
            clip_export.default_clip_name(camera.name if camera else camera_id, clip[0]),
        )
        path, _ = QFileDialog.getSaveFileName(self, "Export Video", suggested, "MP4 video (*.mp4)")
        if not path:
            return
        final = clip_export.with_mp4_suffix(path)
        # The save dialog asked about replacing `path`, not the name with
        # `.mp4` added.
        if final != path and os.path.exists(final):
            if not ask(
                self,
                "Export Video",
                f"{os.path.basename(final)} already exists. Replace it?",
                yes_text="Replace",
                no_text="Cancel",
                destructive=True,
            ):
                return
        ExportVideoDialog.last_folder = os.path.dirname(final)
        # The preview pauses: the export reads the same disk.
        if self._session_id is not None:
            self.timeline.stop_playback()
            self._control("playback.set_playing", playing=False)
        self._starting = True
        self._job = None
        self._poll_failures = 0
        self.show_button.hide()
        self.cancel_export_button.show()
        self.cancel_export_button.setEnabled(True)
        self.progress.setValue(0)
        self.progress.show()
        self._set_status("Starting the export…", "muted")
        self._update_export_button()
        params = clip_export.start_params(camera_id, clip[0], clip[1], final, overwrite=True)
        self._run([("export.start", params)], self._on_export_started)

    def _on_export_started(self, result: object) -> None:
        self._starting = False
        status = result[0] if isinstance(result, list) and result else None
        if not isinstance(status, dict):
            self._end_export_ui()
            self._set_status(f"Export failed: {result}", "error")
            return
        self._show_job(clip_export.ExportJob.from_json(status))
        if self._job is not None and not self._job.finished:
            self._poll_timer.start()

    def _poll(self) -> None:
        if self._job is None or self._job.finished or self._poll_worker is not None:
            return
        worker = self._run([("export.status", {"job_id": self._job.job_id})], self._on_job_status)
        self._poll_worker = worker
        worker.finished.connect(self._clear_poll_worker)

    def _clear_poll_worker(self) -> None:
        self._poll_worker = None

    def _on_job_status(self, result: object) -> None:
        status = result[0] if isinstance(result, list) and result else None
        if not isinstance(status, dict):
            self._poll_failures += 1
            logger.warning("Export status unavailable: %s", result)
            if self._poll_failures >= _MAX_POLL_FAILURES:
                self._poll_timer.stop()
                self._job = None
                self._end_export_ui()
                self._set_status(
                    "Lost track of the export (is the NeuroWatch service running?).", "error"
                )
            return
        self._poll_failures = 0
        self._show_job(clip_export.ExportJob.from_json(status))

    def _show_job(self, job: clip_export.ExportJob) -> None:
        self._job = job
        if not job.finished:
            self.progress.setValue(round(job.progress * 1000))
            self._set_status(clip_export.progress_text(job), "muted")
            return
        self._poll_timer.stop()
        self._end_export_ui()
        tone = {clip_export.DONE: "success", clip_export.CANCELLED: "warning"}.get(
            job.state, "error"
        )
        self._set_status(clip_export.result_text(job), tone)
        self.show_button.setVisible(job.state == clip_export.DONE)

    def _end_export_ui(self) -> None:
        self.progress.hide()
        self.cancel_export_button.hide()
        self._update_export_button()

    def _cancel_export(self) -> None:
        if self._job is None or self._job.finished:
            return
        self.cancel_export_button.setEnabled(False)
        self._set_status("Cancelling the export…", "muted")
        self._run([("export.cancel", {"job_id": self._job.job_id})])

    def _show_in_folder(self) -> None:
        if self._job is not None and self._job.path:
            folder = os.path.dirname(self._job.path)
            QDesktopServices.openUrl(QUrl.fromLocalFile(folder))

    def _set_status(self, text: str, tone: str) -> None:
        self.status_label.setText(text)
        set_status_tone(self.status_label, tone)
        self.status_label.setVisible(bool(text))

    # ------------------------------------------------------------------
    # Closing
    # ------------------------------------------------------------------

    def reject(self) -> None:
        if self.export_running():
            if not ask(
                self,
                "Export Video",
                "An export is still running. Stop it and close?",
                yes_text="Stop export",
                no_text="Keep exporting",
                destructive=True,
            ):
                return
            if self._job is not None:
                self._run([("export.cancel", {"job_id": self._job.job_id})])
        self.shutdown()
        super().reject()

    def shutdown(self) -> None:
        """Stop polling and close the preview session (idempotent)."""
        if self._closed:
            return
        self._poll_timer.stop()
        self._close_preview()
        self._closed = True
