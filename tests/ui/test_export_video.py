"""Export Video: the clip model, the trim timeline and the dialog's flow
(IPC stubbed; the backend's own tests cover the MP4 writing)."""

from __future__ import annotations

import datetime
from typing import Any, ClassVar

import pytest
from PySide6.QtCore import QPointF, QSize, Qt
from PySide6.QtWidgets import QFileDialog

from neurowatch.dialogs import export_video_dialog
from neurowatch.dialogs.export_video_dialog import ExportVideoDialog
from neurowatch.models import clip_export
from neurowatch.models.camera import CameraConfig
from neurowatch.widgets.trim_timeline import END, MIN_TRIM_MS, START, TrimTimeline
from neurowatch.widgets.window_chrome import resize_window

T0 = 1_700_000_000_000  # an epoch-ms moment


# ---------------------------------------------------------------------------
# Model
# ---------------------------------------------------------------------------


def test_job_status_is_read_tolerantly():
    job = clip_export.ExportJob.from_json(
        {
            "job_id": "j",
            "state": "done",
            "progress": 1.4,
            "path": "/v/clip.mp4",
            "start_ms": 10_000,
            "end_ms": 70_000,
            "first_ms": 8_000,
            "duration_ms": 62_000,
            "size_bytes": 12_345_678,
            "audio": True,
            "gaps": 2,
            "truncated": True,
        }
    )
    assert job.finished and job.progress == 1.0
    assert job.first_ms == 8_000 and job.audio is True
    running = clip_export.ExportJob.from_json({"job_id": "k", "state": "running"})
    assert not running.finished and running.first_ms is None and running.audio is None


def test_result_text_explains_what_the_clip_holds():
    job = clip_export.ExportJob(
        job_id="j",
        state="done",
        progress=1.0,
        path="/videos/Gate_clip.mp4",
        start_ms=10_000,
        end_ms=70_000,
        first_ms=8_000,
        duration_ms=62_000,
        size_bytes=12_345_678,
        gaps=1,
        truncated=True,
        skipped_files=2,
    )
    text = clip_export.result_text(job)
    assert text.startswith("Saved Gate_clip.mp4 (1:02, 12 MB)")
    assert "starts 2.0 s early, at the previous keyframe" in text
    assert "1 gap in the recording was left out" in text
    assert "ends early" in text
    assert "2 unreadable recording files were skipped" in text
    exact = clip_export.ExportJob(
        job_id="j", state="done", progress=1, path="/a.mp4", start_ms=0, end_ms=1, first_ms=0
    )
    assert clip_export.result_text(exact) == "Saved a.mp4."
    failed = clip_export.ExportJob(
        job_id="j", state="failed", progress=0, path="", start_ms=0, end_ms=1, error="disk full"
    )
    assert clip_export.result_text(failed) == "Export failed: disk full"
    cancelled = clip_export.ExportJob(
        job_id="j", state="cancelled", progress=0, path="", start_ms=0, end_ms=1
    )
    assert "cancelled" in clip_export.result_text(cancelled)


def test_names_paths_and_ranges():
    start = int(datetime.datetime(2026, 3, 4, 5, 6, 7).timestamp() * 1000)
    assert clip_export.default_clip_name("Front door/porch", start) == (
        "Front_door_porch_2026-03-04_05-06-07.mp4"
    )
    assert clip_export.default_clip_name("///", start).startswith("camera_")
    assert clip_export.with_mp4_suffix("/v/clip") == "/v/clip.mp4"
    assert clip_export.with_mp4_suffix("/v/clip.MP4") == "/v/clip.MP4"
    assert clip_export.with_mp4_suffix("/v/clip.mkv") == "/v/clip.mkv.mp4"
    assert clip_export.check_clip(0, 1_000) is None
    assert clip_export.check_clip(1_000, 1_000)
    assert clip_export.check_clip(0, clip_export.MAX_CLIP_MS + 1)
    assert clip_export.format_length(61_000) == "1:01"
    assert clip_export.format_length(3_725_000) == "1:02:05"
    assert clip_export.format_size(999) == "999 B"
    assert clip_export.format_size(1_500_000) == "1.5 MB"
    assert clip_export.start_params("cam", 1, 2, "/a.mp4", overwrite=True) == {
        "camera_id": "cam",
        "start_ms": 1,
        "end_ms": 2,
        "path": "/a.mp4",
        "overwrite": True,
    }


# ---------------------------------------------------------------------------
# Trim timeline
# ---------------------------------------------------------------------------


def _timeline(qtbot) -> TrimTimeline:
    timeline = TrimTimeline()
    qtbot.addWidget(timeline)
    timeline.resize(900, 200)
    timeline.set_range(T0, T0 + 600_000)
    timeline.set_camera_rows(
        [{"label": "Gate", "segments": [{"start_ms": T0, "end_ms": T0 + 600_000}]}]
    )
    timeline.show()
    qtbot.waitExposed(timeline)
    return timeline


def test_trim_is_kept_inside_the_range_and_apart(qtbot):
    timeline = _timeline(qtbot)
    timeline.set_trim(T0 - 5_000, T0 + 900_000)
    assert timeline.trim() == (T0, T0 + 600_000)
    timeline.set_trim(T0 + 100_000, T0 + 100_000)
    start, end = timeline.trim() or (0, 0)
    assert end - start == MIN_TRIM_MS
    timeline.clear_trim()
    assert timeline.trim() is None and timeline.handle_x(START) is None


def test_dragging_a_handle_moves_the_clip_and_shows_the_cut(qtbot):
    timeline = _timeline(qtbot)
    timeline.set_trim(T0 + 60_000, T0 + 300_000)
    trims: list[tuple[int, int]] = []
    seeks: list[int] = []
    timeline.trim_changed.connect(lambda s, e: trims.append((s, e)))
    timeline.seek_requested.connect(seeks.append)

    x = timeline.handle_x(START)
    assert x is not None
    y = 60
    qtbot.mousePress(timeline, Qt.MouseButton.LeftButton, pos=QPointF(x, y).toPoint())
    target = x + 100
    from PySide6.QtGui import QMouseEvent

    move = QMouseEvent(
        QMouseEvent.Type.MouseMove,
        QPointF(target, y),
        QPointF(target, y),
        Qt.MouseButton.NoButton,
        Qt.MouseButton.LeftButton,
        Qt.KeyboardModifier.NoModifier,
    )
    timeline.mouseMoveEvent(move)
    qtbot.mouseRelease(timeline, Qt.MouseButton.LeftButton, pos=QPointF(target, y).toPoint())

    start, end = timeline.trim() or (0, 0)
    assert start > T0 + 60_000 and end == T0 + 300_000
    assert trims and trims[-1] == (start, end)
    assert seeks == [start], "the preview seeks to the moved cut"
    assert timeline.playhead() == start


def test_a_press_away_from_the_handles_seeks_as_before(qtbot):
    timeline = _timeline(qtbot)
    timeline.set_trim(T0 + 60_000, T0 + 300_000)
    seeks: list[int] = []
    timeline.seek_requested.connect(seeks.append)
    end_x = timeline.handle_x(END)
    assert end_x is not None
    x = end_x + 120
    qtbot.mouseClick(timeline, Qt.MouseButton.LeftButton, pos=QPointF(x, 60).toPoint())
    assert len(seeks) == 1 and seeks[0] > T0 + 300_000
    assert timeline.trim() == (T0 + 60_000, T0 + 300_000)


def test_trim_handles_stay_below_the_date_header_on_multi_day_ranges(qtbot):
    from PySide6.QtGui import QMouseEvent

    start = int(datetime.datetime(2025, 12, 31, 23).timestamp() * 1000)
    timeline = TrimTimeline()
    qtbot.addWidget(timeline)
    timeline.resize(1000, 200)
    timeline.set_range(start, start + 8 * 3_600_000)
    timeline.set_camera_rows(
        [{"label": "Gate", "segments": [{"start_ms": start, "end_ms": start + 8 * 3_600_000}]}]
    )
    original_start, original_end = start + 30 * 60_000, start + 150 * 60_000
    timeline.set_trim(original_start, original_end)
    timeline.show()
    qtbot.waitExposed(timeline)
    x = timeline.handle_x(START)
    assert x is not None
    assert timeline._handle_at(x, timeline.ruler_height() - 1) is None
    y = timeline.ruler_height() + 10
    assert timeline._handle_at(x, y) == START
    seeks: list[int] = []
    timeline.seek_requested.connect(seeks.append)
    qtbot.mousePress(timeline, Qt.MouseButton.LeftButton, pos=QPointF(x, y).toPoint())
    move = QMouseEvent(
        QMouseEvent.Type.MouseMove,
        QPointF(x + 40, y),
        QPointF(x + 40, y),
        Qt.MouseButton.NoButton,
        Qt.MouseButton.LeftButton,
        Qt.KeyboardModifier.NoModifier,
    )
    timeline.mouseMoveEvent(move)
    qtbot.mouseRelease(timeline, Qt.MouseButton.LeftButton, pos=QPointF(x + 40, y).toPoint())
    moved_start, moved_end = timeline.trim()
    assert moved_start > original_start and moved_end == original_end
    assert seeks == [moved_start]


# ---------------------------------------------------------------------------
# Dialog
# ---------------------------------------------------------------------------


class _Signal:
    def __init__(self) -> None:
        self.slots: list[Any] = []

    def connect(self, slot: Any) -> None:
        self.slots.append(slot)


class _Worker:
    def __init__(self) -> None:
        self.finished = _Signal()


class _Reader:
    instances: ClassVar[list[_Reader]] = []

    def __init__(self, shm_key: str, max_emit_fps: float | None = None) -> None:
        self.shm_key = shm_key
        self.frame_ready = _Signal()
        self.stopped = False
        _Reader.instances.append(self)

    def start(self) -> None:
        pass

    def stop(self) -> None:
        self.stopped = True


class _Calls:
    """Stands in for the dialog's workers: records each batch and its handler."""

    def __init__(self) -> None:
        self.batches: list[tuple[list[tuple[str, dict]], Any]] = []

    def __call__(self, calls, handler=None):
        self.batches.append((list(calls), handler))
        return _Worker()

    def methods(self) -> list[str]:
        return [method for calls, _ in self.batches for method, _ in calls]

    def last(self, method: str) -> tuple[dict, Any]:
        for calls, handler in reversed(self.batches):
            for name, params in calls:
                if name == method:
                    return params, handler
        raise AssertionError(f"{method} was not called: {self.methods()}")


@pytest.fixture
def dialog(qtbot, monkeypatch):
    _Reader.instances.clear()
    monkeypatch.setattr(export_video_dialog, "FrameReaderThread", _Reader)
    cameras = [
        CameraConfig(id="cam-a", name="Gate", url="rtsp://x/a"),
        CameraConfig(id="cam-b", name="Yard", url="rtsp://x/b"),
    ]
    dlg = ExportVideoDialog(cameras, "unix:/tmp/nw-test.sock", camera_id="cam-b")
    qtbot.addWidget(dlg)
    calls = _Calls()
    monkeypatch.setattr(dlg, "_run", calls)
    dlg.calls = calls  # type: ignore[attr-defined]
    yield dlg
    dlg.shutdown()


def _load(dlg: ExportVideoDialog, segments: list[dict]) -> None:
    from PySide6.QtCore import QDateTime

    dlg.from_edit.setDateTime(QDateTime.fromMSecsSinceEpoch(T0))
    dlg.to_edit.setDateTime(QDateTime.fromMSecsSinceEpoch(T0 + 3_600_000))
    dlg.load_button.click()
    params, handler = dlg.calls.last("playback.get_timeline")  # type: ignore[attr-defined]
    assert params["camera_id"] == "cam-b"
    handler([segments])


def _minute(ms: int) -> int:
    return ms - ms % 60_000


def test_preview_scrubbing_does_not_queue_every_obsolete_seek(dialog, monkeypatch):
    dialog._session_id = "preview"
    monkeypatch.setattr(dialog, "_drain_controls", lambda: None)
    for at in range(1000):
        dialog._control("playback.seek", ts_ms=at)
    assert dialog._controls == [("playback.seek", {"session_id": "preview", "ts_ms": 999})]
    dialog._control("playback.set_playing", playing=False)
    dialog._control("playback.seek", ts_ms=1000)
    assert len(dialog._controls) == 3


def test_event_preset_loads_and_trims_without_starting_export(dialog):
    from neurowatch.models.events import Event

    event = Event(31, "motion", "cam-b", T0 + 120_000, T0 + 125_000)
    dialog.load_event(event, now_ms=T0 + 600_000)
    params, loaded = dialog.calls.last("playback.get_timeline")
    assert params["camera_id"] == "cam-b"
    assert params["start_ms"] <= event.start_ms - 10_000
    assert params["end_ms"] >= event.end_ms + 15_000
    loaded([[{"start_ms": T0 + 115_000, "end_ms": T0 + 132_000}]])
    assert dialog.clip() == (T0 + 115_000, T0 + 132_000)
    assert "adjusted" in dialog.status_label.text().lower()
    assert "export.start" not in dialog.calls.methods()
    assert dialog.export_button.isEnabled()


def test_event_preset_does_not_select_non_overlapping_recordings(dialog):
    from neurowatch.models.events import Event

    dialog.load_event(Event(32, "motion", "cam-b", T0 + 120_000, T0 + 125_000), now_ms=T0 + 600_000)
    _, loaded = dialog.calls.last("playback.get_timeline")
    loaded([[{"start_ms": T0 + 145_000, "end_ms": T0 + 155_000}]])
    assert not dialog.export_button.isEnabled()
    assert "event" in dialog.status_label.text().lower()


def test_event_preset_preserves_camera_zero_padding_and_manual_source_selection(dialog):
    from neurowatch.models.events import Event

    camera = dialog._cameras[1]
    camera.recording.event_pre_sec = 0
    camera.recording.event_post_sec = 0
    event = Event(33, "motion", "cam-b", T0 + 120_000, T0 + 125_000)
    dialog.load_event(event, now_ms=T0 + 600_000)
    _, loaded = dialog.calls.last("playback.get_timeline")
    loaded([[{"start_ms": T0, "end_ms": T0 + 600_000}]])
    assert dialog.clip() == (event.start_ms, event.end_ms)
    dialog.camera_combo.setCurrentIndex(0)
    dialog._on_load()
    _, loaded = dialog.calls.last("playback.get_timeline")
    loaded([[{"start_ms": T0 + 110_000, "end_ms": T0 + 160_000}]])
    assert dialog.clip() == (T0 + 110_000, T0 + 160_000)
    assert "export.start" not in dialog.calls.methods()


def test_manual_reload_after_event_preset_can_choose_other_footage(dialog):
    from neurowatch.models.events import Event

    dialog.load_event(Event(34, "motion", "cam-b", T0 + 120_000, T0 + 125_000), now_ms=T0 + 600_000)
    _, loaded = dialog.calls.last("playback.get_timeline")
    loaded([[{"start_ms": T0 + 110_000, "end_ms": T0 + 145_000}]])
    dialog._on_load()
    _, loaded = dialog.calls.last("playback.get_timeline")
    loaded([[{"start_ms": T0 + 145_000, "end_ms": T0 + 155_000}]])
    assert dialog.clip() == (T0 + 145_000, T0 + 155_000)


def test_loading_shows_the_recordings_and_selects_all_of_them(dialog):
    assert not dialog.export_button.isEnabled(), "nothing to export before loading"
    start = _minute(T0)
    _load(
        dialog,
        [
            {"start_ms": start + 120_000, "end_ms": start + 300_000},
            {"start_ms": start + 400_000, "end_ms": start + 500_000},
        ],
    )
    assert dialog.clip() == (start + 120_000, start + 500_000)
    assert dialog.export_button.isEnabled()
    assert dialog.length_label.text() == "Length 6:20"

    params, handler = dialog.calls.last("playback.open")
    assert params == {"camera_id": "cam-b", "ts_ms": start + 120_000, "playing": False}
    handler([{"session_id": "s1", "shm_key": "pb_s1"}])
    assert _Reader.instances[-1].shm_key == "pb_s1"

    dialog.timeline.seek_requested.emit(start + 200_000)
    seek, _ = dialog.calls.last("playback.seek")
    assert seek == {"session_id": "s1", "ts_ms": start + 200_000}


def test_a_range_without_recordings_says_so(dialog):
    _load(dialog, [])
    assert "No recordings of Yard" in dialog.status_label.text()
    assert dialog.clip() is None
    assert not dialog.export_button.isEnabled()
    assert "playback.open" not in dialog.calls.methods()


def test_clip_fields_and_playhead_buttons_move_the_handles(dialog):
    from PySide6.QtCore import QDateTime

    start = _minute(T0)
    _load(dialog, [{"start_ms": start, "end_ms": start + 600_000}])
    dialog.start_edit.setDateTime(QDateTime.fromMSecsSinceEpoch(start + 60_000))
    assert dialog.timeline.trim() == (start + 60_000, start + 600_000)
    dialog.timeline.set_playhead(start + 90_000)
    dialog.end_here.click()
    assert dialog.timeline.trim() == (start + 60_000, start + 90_000)
    assert dialog.length_label.text() == "Length 0:30"
    assert dialog.end_edit.dateTime().toMSecsSinceEpoch() == start + 90_000


def test_export_asks_for_a_file_then_follows_the_job(dialog, monkeypatch, tmp_path):
    start = _minute(T0)
    _load(dialog, [{"start_ms": start, "end_ms": start + 600_000}])
    asked: list[str] = []

    def _save(_parent, _title, suggested, _filter):
        asked.append(suggested)
        return str(tmp_path / "gate clip"), "MP4 video (*.mp4)"

    monkeypatch.setattr(QFileDialog, "getSaveFileName", _save)
    dialog.export_button.click()

    assert asked and asked[0].endswith(".mp4") and "Yard_" in asked[0]
    params, handler = dialog.calls.last("export.start")
    assert params == {
        "camera_id": "cam-b",
        "start_ms": start,
        "end_ms": start + 600_000,
        "path": str(tmp_path / "gate clip.mp4"),
        "overwrite": True,
    }
    assert not dialog.export_button.isEnabled(), "one export at a time"
    assert dialog.export_running()

    handler([{"job_id": "j1", "state": "running", "progress": 0.0, "path": params["path"]}])
    dialog._poll()
    status, on_status = dialog.calls.last("export.status")
    assert status == {"job_id": "j1"}
    on_status([{"job_id": "j1", "state": "running", "progress": 0.42}])
    assert dialog.status_label.text() == "Exporting… 42%"
    assert dialog.progress.value() == 420

    on_status(
        [
            {
                "job_id": "j1",
                "state": "done",
                "progress": 1.0,
                "path": params["path"],
                "start_ms": start,
                "end_ms": start + 600_000,
                "first_ms": start,
                "duration_ms": 600_000,
                "size_bytes": 5_000_000,
            }
        ]
    )
    assert dialog.status_label.text() == "Saved gate clip.mp4 (10:00, 5.0 MB)."
    assert not dialog.export_running()
    assert dialog.export_button.isEnabled()
    assert not dialog.show_button.isHidden()
    assert dialog.progress.isHidden()
    assert ExportVideoDialog.last_folder == str(tmp_path)


def test_a_failed_start_is_reported(dialog, monkeypatch, tmp_path):
    start = _minute(T0)
    _load(dialog, [{"start_ms": start, "end_ms": start + 600_000}])
    monkeypatch.setattr(QFileDialog, "getSaveFileName", lambda *a: (str(tmp_path / "a.mp4"), ""))
    dialog.export_button.click()
    _, handler = dialog.calls.last("export.start")
    handler(RuntimeError("Not enough free space"))
    assert "Not enough free space" in dialog.status_label.text()
    assert not dialog.export_running()
    assert dialog.export_button.isEnabled()


def test_cancelling_the_save_dialog_starts_nothing(dialog, monkeypatch):
    _load(dialog, [{"start_ms": _minute(T0), "end_ms": _minute(T0) + 60_000}])
    monkeypatch.setattr(QFileDialog, "getSaveFileName", lambda *a: ("", ""))
    dialog.export_button.click()
    assert "export.start" not in dialog.calls.methods()


def test_closing_during_an_export_asks_and_cancels_it(dialog, monkeypatch, tmp_path):
    start = _minute(T0)
    _load(dialog, [{"start_ms": start, "end_ms": start + 600_000}])
    monkeypatch.setattr(QFileDialog, "getSaveFileName", lambda *a: (str(tmp_path / "a.mp4"), ""))
    dialog.export_button.click()
    _, handler = dialog.calls.last("export.start")
    handler([{"job_id": "j1", "state": "running", "progress": 0.1}])

    answers = [False, True]
    monkeypatch.setattr(export_video_dialog, "ask", lambda *a, **k: answers.pop(0))
    dialog.reject()
    assert "export.cancel" not in dialog.calls.methods(), "kept running on No"
    dialog.reject()
    cancel, _ = dialog.calls.last("export.cancel")
    assert cancel == {"job_id": "j1"}


def test_closing_closes_the_preview_session(dialog):
    _load(dialog, [{"start_ms": _minute(T0), "end_ms": _minute(T0) + 60_000}])
    _, handler = dialog.calls.last("playback.open")
    handler([{"session_id": "s1", "shm_key": "pb_s1"}])
    dialog.reject()
    close, _ = dialog.calls.last("playback.close")
    assert close == {"session_id": "s1"}
    assert _Reader.instances[-1].stopped


def test_a_stale_preview_is_closed_when_the_range_was_reloaded(dialog):
    _load(dialog, [{"start_ms": _minute(T0), "end_ms": _minute(T0) + 60_000}])
    _, first_open = dialog.calls.last("playback.open")
    _load(dialog, [{"start_ms": _minute(T0), "end_ms": _minute(T0) + 60_000}])
    first_open([{"session_id": "old", "shm_key": "pb_old"}])
    close, _ = dialog.calls.last("playback.close")
    assert close == {"session_id": "old"}
    assert not _Reader.instances, "no reader for the stale session"


def test_the_window_resizes_and_the_preview_takes_the_space(dialog, qtbot):
    _load(dialog, [{"start_ms": _minute(T0), "end_ms": _minute(T0) + 600_000}])
    dialog.show()
    qtbot.waitExposed(dialog)
    assert dialog.minimumSize() != dialog.maximumSize(), "resizable"
    layout = dialog.layout()
    assert layout is not None
    layout.activate()  # the loaded clip's labels settle the minimum
    minimum = dialog.minimumSize()
    assert minimum.width() <= 900 and minimum.height() <= 720

    resize_window(dialog, minimum)
    qtbot.waitUntil(lambda: dialog.size() == minimum)
    small_preview = dialog._preview.size()
    small_timeline = dialog.timeline.size()
    assert small_preview.width() >= export_video_dialog.PREVIEW_MIN_WIDTH

    resize_window(dialog, QSize(minimum.width() + 500, minimum.height() + 300))
    qtbot.waitUntil(lambda: dialog.width() == minimum.width() + 500)
    assert dialog._preview.width() == small_preview.width() + 500
    # Nearly all the spare height (rows squeezed at the minimum get theirs back).
    assert dialog._preview.height() >= small_preview.height() + 290
    assert dialog.timeline.width() == small_timeline.width() + 500
    assert dialog.timeline.height() == small_timeline.height(), "the timeline keeps its height"
    # The handles follow the wider timeline.
    start_x = dialog.timeline.handle_x("start")
    assert start_x is not None and start_x > 0


def test_the_window_opens_large_but_within_the_screen(dialog):
    size = dialog.size()
    screen = dialog.screen().availableGeometry()
    assert size.width() <= max(screen.width(), dialog.minimumSizeHint().width())
    assert size.width() >= dialog.minimumSizeHint().width()
    share = export_video_dialog.MAX_SCREEN_SHARE
    assert size.width() >= min(export_video_dialog.DEFAULT_WIDTH, int(screen.width() * share))
    # Well under the screen, or GNOME maximizes it as it opens.
    assert size.height() <= max(int(screen.height() * share), dialog.minimumSizeHint().height())


def test_the_file_menu_offers_export_video(qtbot):
    from neurowatch.widgets.title_bar import TitleBar

    bar = TitleBar()
    qtbot.addWidget(bar)
    menu = bar._build_file_menu()
    action = next(a for a in menu.actions() if a.text() == "Export Video…")
    with qtbot.waitSignal(bar.export_video_requested, timeout=500):
        action.trigger()


def _scroll(widget, notches: int = -1) -> None:
    from PySide6.QtCore import QPoint
    from PySide6.QtGui import QWheelEvent
    from PySide6.QtWidgets import QApplication

    pos = QPointF(widget.width() * 0.3, widget.height() / 2)
    event = QWheelEvent(
        pos,
        widget.mapToGlobal(pos),
        QPoint(0, 0),
        QPoint(0, 120 * notches),
        Qt.MouseButton.NoButton,
        Qt.KeyboardModifier.NoModifier,
        Qt.ScrollPhase.NoScrollPhase,
        False,
    )
    QApplication.sendEvent(widget, event)


def test_a_scroll_passing_over_a_field_does_not_change_it(dialog, qtbot):
    """One notch over the To field moved its year back unnoticed, and Load
    said the range ended before it started."""
    dialog.show()
    qtbot.waitExposed(dialog)
    dialog.load_button.setFocus()
    fields = [dialog.from_edit, dialog.to_edit, dialog.start_edit, dialog.end_edit]
    before = [f.dateTime() for f in fields]
    camera = dialog.camera_combo.currentIndex()
    for field in [*fields, dialog.camera_combo]:
        _scroll(field)
    assert [f.dateTime() for f in fields] == before
    assert dialog.camera_combo.currentIndex() == camera

    dialog.to_edit.setFocus()  # clicked into: it takes the wheel
    qtbot.waitUntil(dialog.to_edit.hasFocus)
    _scroll(dialog.to_edit)
    assert dialog.to_edit.dateTime() != before[1]


def test_a_backwards_range_says_which_times_it_compared(dialog):
    from PySide6.QtCore import QDateTime

    dialog.from_edit.setDateTime(QDateTime.fromMSecsSinceEpoch(T0))
    dialog.to_edit.setDateTime(QDateTime.fromMSecsSinceEpoch(T0).addYears(-1))
    dialog.load_button.click()
    text = dialog.status_label.text()
    assert "end (" in text and "start (" in text
    assert dialog.to_edit.dateTime().toString("yyyy") in text
    assert "playback.get_timeline" not in dialog.calls.methods()


def test_timelapse_uses_shared_export_jobs_and_freezes_sampling_controls(
    dialog, monkeypatch, tmp_path
):
    start = _minute(T0)
    _load(dialog, [{"start_ms": start, "end_ms": start + 600_000}])
    dialog.export_mode.setCurrentIndex(dialog.export_mode.findData("timelapse"))
    dialog.sample_interval.setValue(5)
    dialog.output_fps.setValue(10)
    assert "silent MP4" in dialog.timelapse_hint.text()
    monkeypatch.setattr(
        QFileDialog, "getSaveFileName", lambda *a: (str(tmp_path / "timelapse.mp4"), "")
    )
    dialog.export_button.click()
    params, done = dialog.calls.last("export.start")
    assert params["timelapse"] == {"interval_sec": 5, "fps": 10}
    assert not dialog.output_fps.isEnabled()
    done(
        [
            {
                "job_id": "j",
                "state": "done",
                "progress": 1,
                "path": params["path"],
                "start_ms": start,
                "end_ms": start + 600_000,
                "duration_ms": 12_000,
                "audio": False,
            }
        ]
    )
    assert dialog.output_fps.isEnabled()
    assert dialog.show_button.isVisibleTo(dialog)


def test_timelapse_range_limit_is_independent_of_normal_video_and_caps_picture_count():
    assert clip_export.check_timelapse(0, 48 * 3_600_000, 10, 25) is None
    assert clip_export.check_clip(0, 48 * 3_600_000)
    assert clip_export.check_timelapse(0, 1000, 0, 25)
    assert clip_export.check_timelapse(0, 1000, 10, 61)
    assert clip_export.check_timelapse(0, 31 * clip_export.MAX_CLIP_MS, 1, 25)
