from __future__ import annotations

from neurowatch.widgets import playback_view


def test_playback_worker_reuses_selected_backend_endpoint(monkeypatch):
    seen: list[tuple[str, str, dict]] = []

    class FakeClient:
        def __init__(self, endpoint=None) -> None:
            self.endpoint = endpoint

        def call(self, method, params=None):
            seen.append((self.endpoint, method, params or {}))
            return {"ok": True}

    monkeypatch.setattr(playback_view, "SyncIpcClient", FakeClient)
    results: list[object] = []
    worker = playback_view._IpcWorker(
        [("playback.get_timeline", {"camera_id": "cam-1"})],
        "unix:/tmp/custom-neurowatch.sock",
    )
    worker.done.connect(results.append)

    worker.run()

    assert seen == [
        (
            "unix:/tmp/custom-neurowatch.sock",
            "playback.get_timeline",
            {"camera_id": "cam-1"},
        )
    ]
    assert results == [[{"ok": True}]]


def test_motion_history_loads_all_pages_with_range_and_camera_filters(monkeypatch):
    from neurowatch.ipc import event_feed

    monkeypatch.setattr(event_feed, "EVENT_PAGE_SIZE", 2)
    seen: list[tuple[str, str, dict]] = []
    pages = iter(
        [
            {"events": [{"id": 3, "start_ms": 1000}, {"id": 2, "start_ms": 1000}]},
            {"events": [{"id": 1, "start_ms": 500}]},
        ]
    )

    class Client:
        def __init__(self, endpoint):
            self.endpoint = endpoint

        def call(self, method, params):
            seen.append((self.endpoint, method, dict(params)))
            return next(pages)

    monkeypatch.setattr(playback_view, "SyncIpcClient", Client)
    worker = playback_view._MotionEventsWorker("unix:/tmp/selected.sock", ["cam-a"], 0, 5000)
    results: list = []
    worker.done.connect(results.append)
    worker.run()
    assert [item["id"] for item in results[0]] == [3, 2, 1]
    for endpoint, method, params in seen:
        assert endpoint == "unix:/tmp/selected.sock" and method == "events.list"
        assert params["camera_ids"] == ["cam-a"]
        assert params["from_ms"] == 0 and params["to_ms"] == 5000
        from neurowatch.models.events import PLAYBACK_KINDS

        assert params["kinds"] == sorted(PLAYBACK_KINDS)
    assert seen[1][2]["before_ms"] == 1000 and seen[1][2]["before_id"] == 2


def test_playback_event_history_has_a_memory_bound_without_truncating_feed_defaults(monkeypatch):
    from neurowatch.ipc import event_feed

    monkeypatch.setattr(event_feed, "EVENT_PAGE_SIZE", 2)

    class Client:
        def __init__(self):
            self.reads = 0

        def call(self, method, params):
            self.reads += 1
            return (
                {
                    "events": [
                        {"id": 10 - self.reads * 2, "start_ms": 100},
                        {"id": 9 - self.reads * 2, "start_ms": 100},
                    ]
                }
                if self.reads <= 3
                else {"events": []}
            )

    limited = Client()
    assert len(event_feed.read_events(limited, {}, lambda: False, max_events=3)) == 3
    assert limited.reads == 2
    complete = Client()
    assert len(event_feed.read_events(complete, {}, lambda: False)) == 6
    assert complete.reads == 4


def test_load_recordings_explains_missing_camera_selection(qtbot):
    class FakeClient:
        endpoint = "unix:/tmp/custom-neurowatch.sock"

    view = playback_view.PlaybackView(FakeClient(), [])
    qtbot.addWidget(view)

    view._on_load()

    assert "Select at least one camera" in view._placeholder.text()


class _Ipc:
    endpoint = "unix:/tmp/custom-neurowatch.sock"


def test_events_only_button_uses_camera_padding_and_resets_on_stop(qtbot, monkeypatch):
    view, _ = _loaded_view(qtbot, monkeypatch)
    assert not view._events_only.isEnabled() and not view._events_only.isChecked()
    view._cameras[0].recording.event_pre_sec = 0
    view._cameras[0].recording.event_post_sec = 0
    view._timeline.set_range(0, 100_000)
    view._timeline.set_camera_rows(
        [{"camera_id": "cam-a", "segments": [{"start_ms": 0, "end_ms": 100_000}]}]
    )
    view._on_motion_loaded(
        [{"id": 1, "kind": "motion", "camera_id": "cam-a", "start_ms": 10_000, "end_ms": 20_000}],
        view._motion_load_id,
    )
    assert view._events_only.isEnabled()
    view._events_only.setChecked(True)
    assert view._timeline._event_review
    assert view._timeline._review_plan().intervals == ((10_000, 20_000),)
    view._stop_all()
    assert not view._events_only.isChecked() and not view._events_only.isEnabled()
    assert not view._timeline._event_review


def test_scrubbing_keeps_latest_pending_seek_per_camera_without_crossing_controls(
    qtbot, monkeypatch
):
    view, _ = _loaded_view(qtbot, monkeypatch)
    monkeypatch.setattr(view, "_drain_controls", lambda: None)
    for at in range(1000):
        view._queue_controls(
            [
                ("playback.seek", {"session_id": "s1", "ts_ms": at}),
                ("playback.seek", {"session_id": "s2", "ts_ms": at}),
            ]
        )
    assert view._control_queue == [
        ("playback.seek", {"session_id": "s1", "ts_ms": 999}),
        ("playback.seek", {"session_id": "s2", "ts_ms": 999}),
    ]
    view._queue_controls([("playback.set_playing", {"session_id": "s1", "playing": False})])
    view._queue_controls([("playback.seek", {"session_id": "s1", "ts_ms": 1000})])
    assert len(view._control_queue) == 4
    assert view._control_queue[2][0] == "playback.set_playing"


def test_playback_review_list_uses_loaded_camera_range_and_keeps_seek_controls(qtbot, monkeypatch):

    view, _ = _loaded_view(qtbot, monkeypatch)
    view._timeline.set_range(1000, 120000)
    view._timeline.set_camera_rows(
        [
            {
                "camera_id": "cam-a",
                "label": "Gate",
                "segments": [{"start_ms": 1000, "end_ms": 120000}],
            }
        ]
    )
    view._on_motion_loaded(
        [{"id": 1, "kind": "bookmark", "camera_id": "cam-a", "start_ms": 10000, "end_ms": 10000}],
        view._motion_load_id,
    )
    assert len(view._events_pane._list.events_model.events) == 1
    seen = []
    view._sessions = {"cam-a": {"session_id": "s1"}}
    monkeypatch.setattr(view, "_queue_controls", lambda calls: seen.extend(calls))
    view._review_event("cam-a", 7000)
    assert view._active_playhead_ms == 7000
    assert seen == [("playback.seek", {"session_id": "s1", "ts_ms": 7000})]
    view.add_notices(
        [
            {
                "phase": "start",
                "event": {
                    "id": 2,
                    "kind": "tamper",
                    "camera_id": "cam-a",
                    "start_ms": 20000,
                    "end_ms": 21000,
                },
            }
        ]
    )
    assert len(view._events_pane._events) == 2
    assert len(view._events_pane._list.events_model.events) == 1, (
        "hidden history waits until it is shown"
    )
    view.show()
    view._events_toggle.setChecked(True)
    qtbot.waitUntil(lambda: len(view._events_pane._list.events_model.events) == 2)
    view._sessions.clear()
    view._stop_all()
    assert len(view._events_pane._list.events_model.events) == 0


def test_playback_event_sidebar_does_not_expand_the_window_minimum(qtbot, monkeypatch):
    view, _ = _loaded_view(qtbot, monkeypatch)
    view.resize(1100, 720)
    view._events_toggle.setChecked(True)
    assert not view._events_pane.isHidden()
    assert view.minimumSizeHint().height() < 720
    assert view._timeline_card.minimumSizeHint().height() <= view._timeline_card.height() or (
        not view.isVisible()
    )


class _FakeReader:
    def __init__(self, shm_key, max_emit_fps=None) -> None:
        self.shm_key = shm_key
        self.max_emit_fps = max_emit_fps

        class _Signal:
            def connect(self, _slot) -> None:
                pass

        self.frame_ready = _Signal()

    def start(self) -> None:
        pass

    def stop(self) -> None:
        pass

    def set_max_emit_fps(self, value) -> None:
        self.max_emit_fps = value


def _loaded_view(qtbot, monkeypatch):
    from neurowatch.models.camera import CameraConfig

    cameras = [
        CameraConfig(id="cam-a", name="Gate", url="rtsp://x/a"),
        CameraConfig(id="cam-b", name="Yard", url="rtsp://x/b"),
    ]
    view = playback_view.PlaybackView(_Ipc(), cameras)
    qtbot.addWidget(view)
    calls: list[tuple[str, dict]] = []
    monkeypatch.setattr(view, "_start_worker", lambda batch, *_a, **_k: calls.extend(batch))
    monkeypatch.setattr(view, "_load_motion_events", lambda *_a: None)
    monkeypatch.setattr(playback_view, "FrameReaderThread", _FakeReader)
    return view, calls


def test_loaded_recordings_open_paused_and_wait_for_play(qtbot, monkeypatch):
    view, calls = _loaded_view(qtbot, monkeypatch)
    seg = [{"start_ms": 1_000, "end_ms": 61_000}]
    view._on_timeline_loaded([seg, seg], ["cam-a", "cam-b"], 0, 120_000)

    opens = [params for method, params in calls if method == "playback.open"]
    assert len(opens) == 2
    assert all(params["playing"] is False for params in opens)

    calls.clear()
    view._on_sessions_opened(
        [
            {"session_id": "s1", "shm_key": "pb_s1"},
            {"session_id": "s2", "shm_key": "pb_s2"},
        ],
        ["cam-a", "cam-b"],
        {"cam-a": 1_000, "cam-b": 1_000},
    )
    # No play/pause race after opening: the sessions are already paused.
    assert not [c for c in calls if c[0] == "playback.set_playing"]
    assert len(view._cells) == 2


def test_redesigned_page_keeps_transport_accessible_when_the_timeline_is_collapsed(
    qtbot, monkeypatch
):
    from PySide6.QtCore import QPoint

    from neurowatch import theme

    view, calls = _loaded_view(qtbot, monkeypatch)
    monkeypatch.setattr(view, "_queue_controls", calls.extend)
    view.resize(1024, 640)
    view.show()
    qtbot.waitExposed(view)
    view._timeline.set_range(0, 60_000)
    view._timeline.set_camera_rows(
        [{"camera_id": "cam-a", "label": "Gate", "segments": [{"start_ms": 0, "end_ms": 60_000}]}]
    )
    view._on_sessions_opened([{"session_id": "s1", "shm_key": "pb_s1"}], ["cam-a"], {"cam-a": 0})
    view._audio_timer.stop()
    view._right_splitter.setSizes([500, 1])
    qtbot.wait(1)
    timeline = view._timeline
    for control in [timeline._play_btn, *timeline._speed_buttons.values(), timeline._fit_btn]:
        position = control.mapTo(timeline, QPoint())
        assert timeline.rect().contains(position)
        assert timeline.rect().contains(position + control.rect().bottomRight())
    assert view._from_dt.styleSheet() == theme.datetime_input_stylesheet()
    assert view._load_btn.isEnabled() and view._stop_btn.isEnabled()
    timeline._play_btn.click()
    assert ("playback.set_playing", {"session_id": "s1", "playing": True}) in calls


def test_motion_results_never_replace_a_newer_selection_or_interrupt_playback(qtbot, monkeypatch):
    view, _calls = _loaded_view(qtbot, monkeypatch)
    view._timeline.set_range(0, 5000)
    view._timeline.set_camera_rows(
        [{"camera_id": "cam-a", "label": "Gate", "segments": [{"start_ms": 0, "end_ms": 5000}]}]
    )
    view._motion_load_id = 2
    event = {"id": 1, "camera_id": "cam-a", "kind": "motion", "start_ms": 1000, "end_ms": 2000}
    view._on_motion_loaded([event], 1)
    assert not view._timeline._motion_events
    view._on_motion_loaded([event], 2)
    assert len(view._timeline._motion_events["cam-a"]) == 1
    view._timeline.set_playhead(3000)
    view._on_motion_loaded(RuntimeError("events unavailable"), 2)
    assert "unavailable" in view._timeline._motion_legend.text()
    assert view._timeline._playhead_ms == 3000
    assert view._timeline._play_btn.isEnabled()


def test_motion_ending_during_the_history_read_is_not_restored_as_ongoing(qtbot, monkeypatch):
    view, _calls = _loaded_view(qtbot, monkeypatch)
    view._timeline.set_range(0, 5000)
    view._timeline.set_camera_rows(
        [{"camera_id": "cam-a", "label": "Gate", "segments": [{"start_ms": 0, "end_ms": 5000}]}]
    )
    event = {"id": 1, "camera_id": "cam-a", "kind": "motion", "start_ms": 1000, "end_ms": None}
    view.add_notices([{"phase": "end", "event": {**event, "end_ms": 2000}}])
    view._on_motion_loaded([event], view._motion_load_id)
    shown = view._timeline._motion_events["cam-a"]
    assert len(shown) == 1 and shown[0].end_ms == 2000
    view.add_notices([{"phase": "start", "event": {**event, "id": 2, "start_ms": 6000}}])
    assert len(shown) == 1, "activity outside the selected range is ignored"


def test_playback_cells_show_picture_size_and_frame_rate(qtbot, monkeypatch):
    view, _calls = _loaded_view(qtbot, monkeypatch)
    view._on_sessions_opened(
        [{"session_id": "s1", "shm_key": "pb_s1"}], ["cam-a"], {"cam-a": 1_000}
    )
    clock = [100.0]
    monkeypatch.setattr(playback_view.time, "monotonic", lambda: clock[0])
    frame = bytes(64 * 36 * 3)
    for _ in range(31):
        view._on_frame("pb_s1", frame, 64, 36, "RGB24")
        clock[0] += 1 / 15
    cell = view._cells["s1"]
    assert cell._encoding == "64×36"
    assert cell._fps is not None and 14 <= cell._fps <= 16


def test_a_failed_control_does_not_stop_the_other_cameras(monkeypatch):
    seen: list[tuple[str, dict]] = []

    class FakeClient:
        def __init__(self, endpoint=None) -> None:
            pass

        def call(self, method, params=None):
            seen.append((method, params))
            if params["session_id"] == "gone":
                raise RuntimeError("Session not found")
            return {"ok": True}

    monkeypatch.setattr(playback_view, "SyncIpcClient", FakeClient)
    results: list[object] = []
    worker = playback_view._IpcWorker(
        [
            ("playback.set_playing", {"session_id": "gone", "playing": False}),
            ("playback.set_playing", {"session_id": "s2", "playing": False}),
        ],
        "unix:/tmp/x.sock",
        tolerant=True,
    )
    worker.done.connect(results.append)
    worker.run()

    assert [params["session_id"] for _method, params in seen] == ["gone", "s2"]
    assert isinstance(results[0][0], RuntimeError) and results[0][1] == {"ok": True}


def test_playback_controls_reach_the_backend_in_order(qtbot, monkeypatch):
    started: list[_ManualWorker] = []

    class _Finished:
        def __init__(self) -> None:
            self.slots: list = []

        def connect(self, slot) -> None:
            self.slots.append(slot)

    class _ManualWorker:
        def __init__(self, calls, endpoint, *, tolerant=False) -> None:
            self.calls = calls
            self.tolerant = tolerant
            self.finished = _Finished()

        def start(self) -> None:
            started.append(self)

        def finish(self) -> None:
            for slot in self.finished.slots:
                slot()

        def deleteLater(self) -> None:  # noqa: N802 - Qt's name
            pass

    view, _calls = _loaded_view(qtbot, monkeypatch)
    monkeypatch.setattr(playback_view, "_IpcWorker", _ManualWorker)
    view._sessions = {"cam-a": {"session_id": "s1"}}

    view._on_play_pause(True)
    view._on_seek_all(5_000)
    view._on_play_pause(False)

    # Play goes first; the seek and pause wait for it instead of racing it.
    assert len(started) == 1
    assert [m for m, _p in started[0].calls] == ["playback.set_playing"]
    assert started[0].tolerant
    started[0].finish()
    assert len(started) == 2
    assert started[1].calls == [
        ("playback.seek", {"session_id": "s1", "ts_ms": 5_000}),
        ("playback.set_playing", {"session_id": "s1", "playing": False}),
    ]
    started[1].finish()
    assert len(started) == 2


# ---------------------------------------------------------------------------
# The time range: "end must be later than start" appeared without reason
# ---------------------------------------------------------------------------


def _tick_first_camera(view) -> None:
    from PySide6.QtCore import Qt

    view._cam_list.item(0).setCheckState(Qt.CheckState.Checked)


def test_the_scroll_wheel_does_not_change_a_field_it_passes_over(qtbot, monkeypatch):
    from PySide6.QtCore import QPoint, QPointF, Qt
    from PySide6.QtGui import QWheelEvent
    from PySide6.QtWidgets import QApplication

    view, _calls = _loaded_view(qtbot, monkeypatch)
    view.show()
    qtbot.waitExposed(view)
    field = view._to_dt
    before = field.dateTime()

    def wheel() -> None:
        event = QWheelEvent(
            QPointF(5, 5),
            QPointF(field.mapToGlobal(QPoint(5, 5))),
            QPoint(0, 0),
            QPoint(0, -120),
            Qt.MouseButton.NoButton,
            Qt.KeyboardModifier.NoModifier,
            Qt.ScrollPhase.NoScrollPhase,
            False,
        )
        QApplication.sendEvent(field, event)

    # Offscreen windows never get the focus for real: say whether it has it.
    monkeypatch.setattr(field, "hasFocus", lambda: False)
    wheel()
    assert field.dateTime() == before, "scrolling past it leaves it alone"
    monkeypatch.setattr(field, "hasFocus", lambda: True)
    wheel()
    assert field.dateTime() != before, "clicked into, the wheel still edits it"


def test_space_plays_and_pauses_and_never_reloads(qtbot, monkeypatch):
    from PySide6.QtCore import Qt

    view, _calls = _loaded_view(qtbot, monkeypatch)
    loads: list[bool] = []
    monkeypatch.setattr(view, "_on_load", lambda: loads.append(True))
    view.show()
    qtbot.waitExposed(view)
    # A clicked Load button does not keep the focus (Space would click it).
    assert view._load_btn.focusPolicy() == Qt.FocusPolicy.TabFocus
    assert view._stop_btn.focusPolicy() == Qt.FocusPolicy.TabFocus

    view._on_sessions_opened(
        [{"session_id": "s1", "shm_key": "pb_s1"}], ["cam-a"], {"cam-a": 1_000}
    )
    view._timeline._set_controls_enabled(True)  # as once its rows are loaded
    toggles: list[bool] = []
    view._timeline.play_pause_toggled.connect(toggles.append)
    view._space.activated.emit()
    view._space.activated.emit()
    assert toggles == [True, False], "Space: play, then pause"
    assert loads == []


def test_the_range_compares_whole_minutes_and_says_which_times(qtbot, monkeypatch):
    from PySide6.QtCore import QDate, QDateTime, QTime

    view, _calls = _loaded_view(qtbot, monkeypatch)
    _tick_first_camera(view)
    day = QDate(2026, 10, 7)
    # Both read 18:05; the hidden seconds used to decide.
    view._from_dt.setDateTime(QDateTime(day, QTime(18, 5, 10)))
    view._to_dt.setDateTime(QDateTime(day, QTime(18, 5, 50)))
    view._on_load()
    text = view._placeholder.text()
    assert "(2026-10-07 18:05) must be later than the start time (2026-10-07 18:05)" in text

    view._to_dt.setDateTime(QDateTime(day, QTime(18, 6, 0)))
    view._on_load()
    assert "must be later" not in view._placeholder.text()
    assert view._timeline._range_start_ms % 60_000 == 0


def test_an_unedited_range_follows_the_clock(qtbot, monkeypatch):
    from PySide6.QtCore import QDateTime

    view, _calls = _loaded_view(qtbot, monkeypatch)
    old = QDateTime.currentDateTime().addDays(-1)
    view._setting_range = True  # as if built yesterday, not edited since
    view._to_dt.setDateTime(old)
    view._from_dt.setDateTime(old.addSecs(-3600))
    view._setting_range = False
    view.show()  # the Playback tab opened
    qtbot.waitExposed(view)
    assert abs(view._to_dt.dateTime().secsTo(QDateTime.currentDateTime())) < 120

    edited = QDateTime.currentDateTime().addDays(-2)
    view._from_dt.setDateTime(edited)  # by hand
    view.hide()
    view.show()
    assert view._from_dt.dateTime() == edited, "a range chosen by hand stays"


def test_playback_shows_frames_at_the_live_preview_limits(qtbot, monkeypatch):
    """Playback was capped at 18 fps (one camera) whatever the recording held;
    it follows Settings → Video Wall → Live Preview Frame Rate now."""
    from neurowatch.models.preview_fps import PreviewFps

    view, _calls = _loaded_view(qtbot, monkeypatch)
    view.set_preview_fps(PreviewFps(single=30, wall=20, dense=4, hidden=2))
    view._on_sessions_opened([{"session_id": "s1", "shm_key": "pb_s1"}], ["cam-a"], {"cam-a": 0})
    assert view._readers["s1"].max_emit_fps == 30.0, "one camera: all 30 frames"

    view._on_sessions_opened(
        [{"session_id": "s1", "shm_key": "pb_s1"}, {"session_id": "s2", "shm_key": "pb_s2"}],
        ["cam-a", "cam-b"],
        {"cam-a": 0, "cam-b": 0},
    )
    assert {r.max_emit_fps for r in view._readers.values()} == {20.0}

    view.set_preview_fps(PreviewFps(single=30, wall=30, dense=4, hidden=2))
    assert {r.max_emit_fps for r in view._readers.values()} == {30.0}, "retuned in place"


def test_leaving_playback_pauses_the_video(qtbot, monkeypatch):
    from unittest.mock import MagicMock

    from neurowatch.main import MainWindow

    view, calls = _loaded_view(qtbot, monkeypatch)
    monkeypatch.setattr(view, "_queue_controls", calls.extend)
    view._timeline.set_range(0, 60_000)
    view._timeline.set_camera_rows(
        [{"camera_id": "cam-a", "label": "Gate", "segments": [{"start_ms": 0, "end_ms": 60_000}]}]
    )
    view._on_sessions_opened([{"session_id": "s1", "shm_key": "pb_s1"}], ["cam-a"], {"cam-a": 0})
    view._audio_timer.stop()
    calls.clear()
    view._timeline.toggle_play()
    assert calls[-1] == ("playback.set_playing", {"session_id": "s1", "playing": True})

    view.pause()
    assert calls[-1] == ("playback.set_playing", {"session_id": "s1", "playing": False})
    assert not view._timeline._is_playing and "Play" in view._timeline._play_btn.text()
    calls.clear()
    view.pause()
    assert calls == [], "nothing to pause"

    window = MagicMock()
    for tab in ("live", "events", "diagnostics"):
        MainWindow._on_tab_changed(window, tab)
    assert window._playback_view.pause.call_count == 3
    window.reset_mock()
    MainWindow._on_tab_changed(window, "playback")
    MainWindow._on_tab_changed(window, "settings")  # a dialog over Playback
    window._playback_view.pause.assert_not_called()
