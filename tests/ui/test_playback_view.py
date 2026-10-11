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


def test_load_recordings_explains_missing_camera_selection(qtbot):
    class FakeClient:
        endpoint = "unix:/tmp/custom-neurowatch.sock"

    view = playback_view.PlaybackView(FakeClient(), [])
    qtbot.addWidget(view)

    view._on_load()

    assert "Select at least one camera" in view._placeholder.text()


class _Ipc:
    endpoint = "unix:/tmp/custom-neurowatch.sock"


class _FakeReader:
    def __init__(self, shm_key, max_emit_fps=None) -> None:
        self.shm_key = shm_key

        class _Signal:
            def connect(self, _slot) -> None:
                pass

        self.frame_ready = _Signal()

    def start(self) -> None:
        pass

    def stop(self) -> None:
        pass


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
