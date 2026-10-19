"""Events in the desktop UI: the Events tab, MOTION badges on tiles and in the
camera list, the new-events count, and opening an event in Playback."""

from __future__ import annotations

from typing import Any

from neurowatch.models.camera import CameraConfig
from neurowatch.models.events import ActivityTracker, Event, duration_text, kinds_text


def _event(id_: int, kind: str = "motion", cam: str | None = "cam-a", **extra: Any) -> dict:
    return {
        "id": id_,
        "kind": kind,
        "camera_id": cam,
        "start_ms": id_ * 1000,
        "end_ms": None,
        **extra,
    }


def _notice(phase: str, event: dict) -> dict:
    return {"seq": event["id"], "phase": phase, "event": event}


CAMERAS = [
    CameraConfig(id="cam-a", name="Gate", url="rtsp://x/a"),
    CameraConfig(id="cam-b", name="Yard", url="rtsp://x/b"),
]


# ── Model ──────────────────────────────────────────────────────────────────


def test_activity_follows_open_events_and_flashes_instant_ones():
    tracker = ActivityTracker()
    motion = Event.from_dict(_event(1))
    assert motion is not None
    tracker.apply("start", motion, now=0)
    assert tracker.active(now=0) == {"cam-a"}
    tracker.apply("end", Event.from_dict({**_event(1), "end_ms": 5000}), now=1)  # type: ignore[arg-type]
    assert tracker.active(now=1) == set()

    crossed = Event.from_dict({**_event(2, "line_crossing", "cam-b"), "end_ms": 2000})
    assert crossed is not None
    tracker.apply("start", crossed, now=10)
    tracker.apply("end", crossed, now=10)
    assert tracker.active(now=12) == {"cam-b"}
    assert tracker.active(now=16) == set()

    offline = Event.from_dict(_event(3, "camera_offline"))
    assert offline is not None
    tracker.apply("start", offline, now=20)
    assert tracker.active(now=20) == set(), "only movement in the picture"

    tracker.reset([motion])
    assert tracker.active(now=30) == {"cam-a"}


def test_durations_and_kind_lists_read_naturally():
    def make(start: int, end: int | None) -> Event:
        event = Event.from_dict({"id": 1, "kind": "motion", "start_ms": start, "end_ms": end})
        assert event is not None
        return event

    assert duration_text(make(0, None)) == "now"
    assert duration_text(make(0, 0)) == ""
    assert duration_text(make(0, 400)) == "1 s"
    assert duration_text(make(0, 3 * 60_000)) == "3 min"
    assert duration_text(make(0, 125 * 60_000)) == "2 h 5 min"
    assert kinds_text(["motion"]) == "motion"
    assert kinds_text(["motion", "line_crossing", "digital_input"]) == (
        "motion, line crossing and alarm inputs"
    )
    assert Event.from_dict({"kind": "motion"}) is None


def test_motion_alert_lasts_until_acknowledged_and_new_motion_raises_it_again():
    tracker = ActivityTracker()
    motion = Event.from_dict(_event(1))
    ended = Event.from_dict(_event(1, end_ms=4000))
    assert motion is not None and ended is not None
    tracker.apply("start", motion, now=0)
    tracker.apply("end", ended, now=1)
    assert tracker.active(now=100) == set()
    assert tracker.pending() == {"cam-a"}, "ending motion must not clear the alert"

    tracker.reset([])  # backend reconnect
    assert tracker.pending() == {"cam-a"}
    tracker.acknowledge("cam-a", tracker.pending_ids("cam-a"))
    tracker.apply("end", ended, now=101)  # a delayed or replayed notice
    tracker.restore_pending([ended])  # a snapshot taken before the acknowledgement
    assert tracker.pending() == set()

    newer = Event.from_dict(_event(2))
    assert newer is not None
    tracker.apply("start", newer, now=102)
    assert tracker.pending() == {"cam-a"}
    assert tracker.active(now=102) == {"cam-a"}
    tracker.acknowledge("cam-a", [2])
    assert tracker.pending() == set()
    assert tracker.active(now=102) == {"cam-a"}, "acknowledgement never hides current activity"


def test_pending_motion_restores_from_history_and_acknowledges_only_seen_events():
    old = Event.from_dict(_event(1, end_ms=4000))
    new = Event.from_dict(_event(2))
    other = Event.from_dict(_event(3, cam="cam-b", end_ms=3000))
    acknowledged = Event.from_dict(_event(4, acknowledged=True))
    assert old is not None and new is not None and other is not None and acknowledged is not None
    tracker = ActivityTracker()
    tracker.restore_pending([old, other, acknowledged])
    assert tracker.pending() == {"cam-a", "cam-b"}
    seen = tracker.pending_ids("cam-a")
    tracker.apply("start", new)
    tracker.acknowledge("cam-a", seen)
    assert tracker.pending_ids("cam-a") == [2], "motion during the RPC stays pending"
    # A new UI restores the same unacknowledged state from stored records.
    reopened = ActivityTracker()
    reopened.restore_pending([new, other, acknowledged])
    assert reopened.pending() == {"cam-a", "cam-b"}


def test_event_feed_takes_cursor_before_snapshots_and_restores_pending_history(monkeypatch):
    from neurowatch.ipc.event_feed import EventFeedWorker

    calls: list[str] = []
    stopped = False

    class Client:
        def call(self, method, params=None, **_kwargs):
            nonlocal stopped
            calls.append(method)
            if method == "events.wait" and not params.get("after"):
                return {"boot": "boot", "seq": 10}
            if method == "events.open":
                return {"events": []}
            if method == "events.list":
                assert params["unacknowledged"]
                return {"events": [_event(1, end_ms=4000)]}
            assert params["after"] == 10
            stopped = True
            return {"boot": "boot", "seq": 11, "notices": [_notice("start", _event(2))]}

    worker = EventFeedWorker("unix:/tmp/private.sock", client_factory=Client)
    monkeypatch.setattr(worker, "isInterruptionRequested", lambda: stopped)
    pending: list = []
    live: list = []
    worker.pending_over.connect(pending.append)
    worker.notices.connect(live.append)
    worker.run()
    assert calls == ["events.wait", "events.open", "events.list", "events.wait"]
    assert pending[0][0]["end_ms"] == 4000, "past motion survives opening the app"
    assert live[0][0]["event"]["id"] == 2, "events during the snapshots are replayed"


def test_clearing_a_tile_alert_uses_selected_endpoint_and_updates_all_walls(qtbot, monkeypatch):
    from types import SimpleNamespace

    from PySide6.QtCore import QObject, Signal

    from neurowatch import main
    from neurowatch.widgets.video_wall import VideoWallWidget

    workers: list = []

    class Worker(QObject):
        result = Signal(object)
        finished = Signal()

        def __init__(self, method, params, endpoint):
            super().__init__()
            self.method, self.params, self.endpoint = method, params, endpoint
            workers.append(self)

        def start(self):
            pass

    monkeypatch.setattr(main, "_IpcCallWorker", Worker)
    walls = [VideoWallWidget(), VideoWallWidget()]
    for wall in walls:
        qtbot.addWidget(wall)
        wall.set_cameras(CAMERAS)
    tracker = ActivityTracker()
    event = Event.from_dict(_event(1, end_ms=4000))
    assert event is not None
    tracker.restore_pending([event])
    owner = SimpleNamespace(
        _activity=tracker,
        _motion_cameras=set(),
        _pending_motion_cameras=set(),
        _motion_ack_workers={},
        _walls=lambda: walls,
        _ipc_endpoint=lambda: "unix:/tmp/selected.sock",
    )
    owner._apply_activity = lambda: main.MainWindow._apply_activity(owner)
    owner._apply_activity()
    assert all(wall._cell_for_cam_id("cam-a").motion_pending for wall in walls)
    main.MainWindow._acknowledge_motion(owner, "cam-a")
    assert workers[0].method == "events.acknowledge"
    assert workers[0].params == {"ids": [1]}
    assert workers[0].endpoint == "unix:/tmp/selected.sock"
    main.MainWindow._acknowledge_motion(owner, "cam-a")
    assert len(workers) == 1, "repeated clicks share the in-flight request"
    workers[0].result.emit({"changed": 1})
    assert all(not wall._cell_for_cam_id("cam-a").motion_pending for wall in walls)
    workers[0].finished.emit()
    assert not owner._motion_ack_workers


def test_clicking_the_motion_alert_does_not_expand_or_move_the_tile(qtbot):
    from PySide6.QtCore import Qt

    from neurowatch.widgets.cell_widget import AUDIO_AVAILABLE, CellWidget

    cell = CellWidget()
    qtbot.addWidget(cell)
    cell.resize(360, 200)
    cell.set_camera(CAMERAS[0])
    cell.set_motion_pending(True)
    cell.set_audio(AUDIO_AVAILABLE)
    cell.set_layout_state(editable=True, span=(1, 1), grid=(3, 3))
    cleared: list[str] = []
    clicks: list[str] = []
    expanded: list[str] = []
    cell.single_click_requested.connect(clicks.append)
    cell.expand_requested.connect(expanded.append)

    def clear(camera_id: str) -> None:
        cleared.append(camera_id)
        cell.set_motion_pending(False)

    cell.motion_acknowledge_requested.connect(clear)
    cell.show()
    point = cell.motion_alert_rect().center()
    qtbot.mouseClick(cell, Qt.MouseButton.LeftButton, pos=point)
    qtbot.mouseDClick(cell, Qt.MouseButton.LeftButton, pos=point)
    assert cleared == ["cam-a"]
    assert not clicks and not expanded and cell._gesture is None
    assert not cell.motion_pending


def test_pending_alerts_follow_camera_ids_across_pages_and_grid_changes(qtbot):
    from neurowatch.widgets.video_wall import VideoWallWidget

    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall.set_cameras(CAMERAS)
    wall.set_grid(1, 1)
    wall.set_pending_motion_cameras({"cam-b"})
    wall.set_page(1)
    assert wall._cells[0]._camera.id == "cam-b"
    assert wall._cells[0].motion_pending
    wall.set_grid(2, 2)
    assert wall._cell_for_cam_id("cam-b").motion_pending
    assert not wall._cell_for_cam_id("cam-a").motion_pending
    wall.set_pending_motion_cameras(set())
    assert not wall._cell_for_cam_id("cam-b").motion_pending


# ── Events tab ─────────────────────────────────────────────────────────────


def _view(qtbot):
    from neurowatch.widgets.events_view import EventsView

    view = EventsView(None, CAMERAS)
    qtbot.addWidget(view)
    view.show()
    return view


def _ids(view) -> list[int]:
    """The listed events, top to bottom."""
    return [event.id for event in view._list.events_model.events]


def _listed(view, event_id: int):
    model = view._list.events_model
    row = model.row_of(event_id)
    return None if row is None else model.event_at(row)


def test_events_tab_lists_names_and_updates_live(qtbot):
    view = _view(qtbot)
    view._on_loaded(
        {
            "events": [
                {**_event(2, "camera_offline", "cam-b", detail="Yard")},
                {**_event(1), "end_ms": 4000},
            ]
        }
    )
    model = view._list.events_model
    assert _ids(view) == [2, 1]
    assert model.describe(_listed(view, 2)).startswith("Camera offline · Yard")
    assert model.describe(_listed(view, 1)).startswith("Motion · Gate")
    assert "2 events · 1 ongoing" == view._count.text()

    view.add_notices(
        [
            _notice("start", _event(5, "intrusion")),
            _notice("end", {**_event(2, "camera_offline", "cam-b"), "end_ms": 9000}),
        ]
    )
    assert _ids(view) == [5, 2, 1]
    assert _listed(view, 5).label == "Intrusion"
    assert duration_text(_listed(view, 2)) == "7 s"


def test_events_tab_filters_new_events_by_camera_and_kind(qtbot):
    view = _view(qtbot)
    view._worker = None
    view._camera_filter.blockSignals(True)
    view._camera_filter.setCurrentIndex(view._camera_filter.findData("cam-b"))
    view._camera_filter.blockSignals(False)
    view.add_notices(
        [_notice("start", _event(1, cam="cam-a")), _notice("start", _event(2, cam="cam-b"))]
    )
    assert _ids(view) == [2]
    assert view._filter_params()["camera_ids"] == ["cam-b"]
    view._kind_filter.blockSignals(True)
    view._kind_filter.setCurrentIndex(3)  # bookmarks
    view._kind_filter.blockSignals(False)
    assert view._filter_params()["kinds"] == ["bookmark"]


def test_double_clicking_an_event_asks_to_play_it(qtbot):
    from neurowatch.widgets.events_view import PLAY_LEAD_MS

    view = _view(qtbot)
    view._on_loaded({"events": [_event(70, "disk_low", None), _event(60)]})
    asked: list[tuple[str, int]] = []
    view.play_requested.connect(lambda cam, ts: asked.append((cam, ts)))
    view._list.select_event(60)
    assert view._preview._play.isEnabled()
    model = view._list.events_model
    view._list.doubleClicked.emit(model.index(model.row_of(60)))
    assert asked == [("cam-a", 60_000 - PLAY_LEAD_MS)]
    view._list.select_event(70)  # a drive event has no recording to play
    assert not view._preview._play.isEnabled()


def test_a_hidden_tab_redraws_once_when_shown_and_keeps_a_bounded_list(qtbot, monkeypatch):
    from neurowatch.widgets import events_view

    monkeypatch.setattr(events_view, "MAX_ROWS", 5)
    view = events_view.EventsView(None, CAMERAS)
    qtbot.addWidget(view)
    view._on_loaded({"events": [_event(i) for i in range(4, 0, -1)]})
    assert _ids(view) == [4, 3, 2, 1]
    view.add_notices([_notice("start", _event(i)) for i in range(5, 9)])
    assert _ids(view) == [4, 3, 2, 1], "hidden: nothing redrawn yet"
    assert view._dirty
    view.show()
    assert _ids(view) == [8, 7, 6, 5, 4], "redrawn once, newest kept"
    assert not view._older_btn.isHidden(), "the oldest dropped off; Load Older reads them"


def test_a_change_updates_only_its_row(qtbot):
    view = _view(qtbot)
    view._on_loaded({"events": [_event(2), _event(1)]})
    model = view._list.events_model
    resets, changed = [], []
    model.modelReset.connect(lambda: resets.append(True))
    model.dataChanged.connect(lambda first, last: changed.append((first.row(), last.row())))
    view.add_notices([_notice("end", {**_event(1), "end_ms": 4000})])
    assert not resets
    assert changed == [(model.row_of(1), model.row_of(1))]
    assert duration_text(_listed(view, 1)) == "3 s"
    view.add_notices([_notice("start", _event(3))])
    assert not resets
    assert _ids(view) == [3, 2, 1]


def test_a_full_page_offers_older_events(qtbot):
    from neurowatch.widgets.events_view import PAGE_SIZE

    view = _view(qtbot)
    view._on_loaded({"events": [_event(i) for i in range(PAGE_SIZE, 0, -1)]})
    assert not view._older_btn.isHidden()
    started: list[dict] = []
    view._load = lambda params, append: started.append({**params, "append": append})  # type: ignore[method-assign]
    view._load_older()
    assert started[0]["before_id"] == 1 and started[0]["append"] is True


def test_events_are_newest_first_and_a_change_keeps_the_selected_event(qtbot):
    view = _view(qtbot)
    view._on_loaded(
        {
            "events": [
                _event(3, cam="cam-b", start_ms=86_400_000, end_ms=86_410_000),
                _event(2, start_ms=30_000, end_ms=32_000),
                _event(1, start_ms=20_000, end_ms=80_000),
            ]
        }
    )
    assert _ids(view) == [3, 2, 1]
    view._list.select_event(3)
    asked = []
    view.play_requested.connect(lambda cam, ts: asked.append((cam, ts)))
    view._play_selected()
    assert asked == [("cam-b", 86_395_000)]
    view.add_notices(
        [_notice("end", _event(3, cam="cam-b", start_ms=86_400_000, end_ms=86_500_000))]
    )
    assert view._selected().id == 3
    assert duration_text(view._selected()) == "1 min"
    assert "Duration: 1 min" in view._preview.details_text()


def test_events_search_matches_names_labels_and_details_and_keeps_live_rows(qtbot):
    view = _view(qtbot)
    view._search.blockSignals(True)
    view._search.setText("  ALARM INPUT ")
    view._search.blockSignals(False)
    view._on_loaded({"events": [_event(2, "digital_input"), _event(1)]})
    assert view._filter_params()["search"] == "ALARM INPUT"
    assert _ids(view) == [2]
    view.add_notices(
        [_notice("start", _event(3, "digital_input", cam="cam-b")), _notice("start", _event(4))]
    )
    assert _ids(view) == [3, 2]
    view._search.setText("yard")
    view.stop()  # no real IPC in a UI test
    view._on_loaded({"events": [_event(5, cam="cam-b"), _event(6)]})
    assert _ids(view) == [5]
    view._search.setText("covered lens")
    view.stop()
    view._on_loaded({"events": [_event(7, "tamper", detail="Covered lens detected")]})
    assert _ids(view) == [7]


def test_ongoing_filter_removes_an_ended_event_and_finished_filter_adds_it(qtbot):
    view = _view(qtbot)
    view._state_filter.setCurrentIndex(1)
    view.stop()
    view._on_loaded({"events": [_event(2), _event(1, end_ms=4000)]})
    assert view._filter_params()["ongoing"] is True
    assert _ids(view) == [2]
    view.add_notices([_notice("end", _event(2, end_ms=5000))])
    assert _ids(view) == []
    assert view._empty_title.text() == "No matching events"
    view._state_filter.setCurrentIndex(2)
    view.stop()
    assert view._filter_params()["ongoing"] is False
    view.add_notices([_notice("start", _event(3)), _notice("end", _event(3, end_ms=7000))])
    assert _ids(view) == [3]
    assert not _listed(view, 3).going_on


def test_custom_dates_are_inclusive_and_match_overlapping_events(qtbot):
    from PySide6.QtCore import QDate

    view = _view(qtbot)
    view._range_filter.setCurrentIndex(view._range_filter.findData("custom"))
    view._from_date.setDate(QDate(2026, 10, 12))
    view._to_date.setDate(QDate(2026, 10, 14))
    view.stop()
    params = view._filter_params()
    assert params["from_ms"] == QDate(2026, 10, 12).startOfDay().toMSecsSinceEpoch()
    assert params["to_ms"] == QDate(2026, 10, 15).startOfDay().toMSecsSinceEpoch() - 1
    start, end = params["from_ms"], params["to_ms"]
    view._on_loaded(
        {
            "events": [
                _event(5, start_ms=end + 1, end_ms=end + 2),
                _event(4, start_ms=end, end_ms=end),
                _event(3, start_ms=start - 20, end_ms=start),
                _event(2, start_ms=start - 30, end_ms=start - 1),
            ]
        }
    )
    assert _ids(view) == [4, 3]
    view._from_date.setDate(QDate(2026, 10, 20))
    view.stop()
    assert view._to_date.date() == view._from_date.date()
    view._to_date.setDate(QDate(2026, 10, 10))
    view.stop()
    assert view._from_date.date() == view._to_date.date()


def test_event_search_debounces_and_clearing_filters_resets_all_fields(qtbot, monkeypatch):
    view = _view(qtbot)
    calls = []
    monkeypatch.setattr(view, "_load", lambda params, append: calls.append(params))
    view._search.setText("ga")
    view._search.setText("gate")
    assert calls == []
    qtbot.waitUntil(lambda: bool(calls))
    assert len(calls) == 1 and calls[0]["search"] == "gate"
    view._camera_filter.setCurrentIndex(1)
    view._kind_filter.setCurrentIndex(1)
    view._state_filter.setCurrentIndex(1)
    view._range_filter.setCurrentIndex(1)
    view._clear_filters()
    assert view._filter_params() == {"limit": 200}
    assert not view._clear_btn.isEnabled()
    view.stop()


def test_stale_list_reply_is_ignored_and_live_changes_win_over_snapshot(qtbot):
    view = _view(qtbot)
    old_generation = view._generation
    view.stop()
    view._on_result(old_generation, {"events": [_event(99)]})
    assert _ids(view) == []
    view._loading = True
    view.add_notices([_notice("end", _event(2, end_ms=5000)), _notice("start", _event(3))])
    view._on_loaded({"events": [_event(2), _event(1)]})
    assert _ids(view) == [3, 2, 1]
    assert not _listed(view, 2).going_on


def test_history_paging_keeps_the_chronological_cursor(qtbot):
    from neurowatch.widgets.events_view import PAGE_SIZE

    view = _view(qtbot)
    view._on_loaded({"events": [_event(i, end_ms=i * 1000 + 500) for i in range(PAGE_SIZE, 0, -1)]})
    calls = []
    view._load = lambda params, append: calls.append((params, append))
    view._load_older()
    assert calls[0][0]["before_id"] == 1
    assert calls[0][1] is True


def test_modern_events_page_wraps_filters_and_distinguishes_error_and_empty_states(qtbot):
    view = _view(qtbot)
    assert view.objectName() == "eventsPage"
    view.resize(800, 700)
    qtbot.waitUntil(lambda: view._filter_columns == 5)
    view._on_loaded(RuntimeError("recorder offline"))
    assert view._empty_title.text() == "Could not load events"
    assert "recorder offline" in view._status.text()
    assert view._status.isVisible()
    view._on_loaded({"events": []})
    assert view._empty_title.text() == "No events yet"
    view.resize(1280, 700)
    qtbot.waitUntil(lambda: view._filter_columns == 6)


def test_the_detail_pane_always_shows_something(qtbot):
    view = _view(qtbot)
    view._on_loaded({"events": [_event(2), _event(1, end_ms=4000)]})
    assert view._preview.isVisible() and view._preview.event_id is None
    view._list.select_event(1)
    assert view._preview.event_id == 1
    assert "Motion · Gate" in view._preview.details_text()


def test_typing_a_search_does_not_repaint_the_history_on_every_keystroke(qtbot, monkeypatch):
    view = _view(qtbot)
    view._on_loaded({"events": [_event(2), _event(1)]})
    fills = []
    monkeypatch.setattr(view._list, "set_events", lambda events, **_: fills.append(events))
    for text in ("g", "ga", "gat", "gate"):
        view._search.setText(text)
    assert not fills
    view.stop()


def test_events_theme_switch_keeps_selection_and_uses_the_theme_colours(qtbot):
    from neurowatch.theme import LIGHT_COLORS

    view = _view(qtbot)
    view._on_loaded({"events": [_event(2), _event(1, end_ms=4000)]})
    view._list.select_event(2)
    view.apply_theme("light")
    assert LIGHT_COLORS.surface in view.styleSheet()
    assert view._selected().id == 2
    assert view._list.delegate.colors == LIGHT_COLORS


def test_marking_an_event_seen_uses_the_selected_endpoint(qtbot, monkeypatch):
    from neurowatch.widgets import events_view

    started = []
    monkeypatch.setattr(events_view._CallWorker, "start", lambda worker: started.append(worker))
    view = events_view.EventsView("unix:/tmp/chosen-events.sock", CAMERAS)
    qtbot.addWidget(view)
    view.show()
    view._on_loaded({"events": [_event(4, end_ms=6000)]})
    view._list.select_event(4)
    assert view._preview._ack.isVisible()
    view._preview._ack.click()
    assert [(w._endpoint, w._method, w._params) for w in started] == [
        ("unix:/tmp/chosen-events.sock", "events.acknowledge", {"ids": [4]})
    ]


def test_history_worker_uses_the_selected_endpoint_and_tags_its_reply(monkeypatch):
    from neurowatch.widgets import events_view

    calls = []

    class Client:
        def __init__(self, *, endpoint):
            calls.append(endpoint)

        def call(self, method, params):
            calls.append((method, params))
            return {"events": [_event(1)]}

    monkeypatch.setattr(events_view, "SyncIpcClient", Client)
    params = {"limit": 200, "search": "gate", "ongoing": False}
    worker = events_view._ListWorker("unix:/tmp/chosen-events.sock", params, 17)
    replies = []
    worker.done.connect(lambda generation, result: replies.append((generation, result)))
    worker.run()
    assert calls == ["unix:/tmp/chosen-events.sock", ("events.list", params)]
    assert replies == [(17, {"events": [_event(1)]})]


def test_live_burst_during_a_history_read_keeps_the_limit_and_recoverable_cursor(
    qtbot, monkeypatch
):
    from neurowatch.widgets import events_view

    monkeypatch.setattr(events_view, "MAX_ROWS", 5)
    view = _view(qtbot)
    view._loading = True
    view.add_notices([_notice("start", _event(i)) for i in range(5, 12)])
    view._on_loaded({"events": [_event(i) for i in range(4, 0, -1)]})
    assert len(view._events) == 5
    assert _ids(view) == [11, 10, 9, 8, 7]
    assert view._cursor == (7000, 7)
    assert view._has_more


def test_tiles_and_the_camera_list_show_motion(qtbot):
    from neurowatch.widgets.camera_panel import CameraPanel
    from neurowatch.widgets.cell_widget import CellWidget
    from neurowatch.widgets.video_wall import VideoWallWidget

    cell = CellWidget()
    qtbot.addWidget(cell)
    cell.set_camera(CAMERAS[0])
    cell.set_motion(True)
    assert cell.motion_active
    plain = cell._render_label_strip("Gate", "idle", "LIVE", cell._badge()[1], 1.0)
    with_motion = cell._render_label_strip("Gate", "idle", "LIVE", cell._badge()[1], 1.0, True)
    assert with_motion.width() > plain.width()
    cell.set_camera(CAMERAS[1])
    assert not cell.motion_active, "another camera starts without the badge"

    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall.set_cameras(CAMERAS)
    wall.set_motion_cameras({"cam-b"})
    assert wall._cell_for_cam_id("cam-b").motion_active
    assert not wall._cell_for_cam_id("cam-a").motion_active

    panel = CameraPanel()
    qtbot.addWidget(panel)
    panel.set_cameras(CAMERAS)
    panel.set_motion_cameras({"cam-a"})
    assert not panel._item_widgets["cam-a"]._motion.isHidden()
    assert panel._item_widgets["cam-b"]._motion.isHidden()


def test_the_events_tab_counts_new_events(qtbot):
    from neurowatch.widgets.title_bar import TitleBar

    bar = TitleBar()
    qtbot.addWidget(bar)
    bar.set_tab_badge("events", 3)
    assert bar._tab_btns["events"].text() == "Events (3)"
    bar.set_tab_badge("events", 0)
    assert bar._tab_btns["events"].text() == "Events"


# ── Playback ───────────────────────────────────────────────────────────────


def test_an_event_opens_its_moment_in_playback(qtbot, monkeypatch):
    from neurowatch.widgets import playback_view

    class _Ipc:
        endpoint = "unix:/tmp/test.sock"

    view = playback_view.PlaybackView(_Ipc(), CAMERAS)  # type: ignore[arg-type]
    qtbot.addWidget(view)
    calls: list[tuple[str, dict]] = []
    monkeypatch.setattr(view, "_start_worker", lambda batch, *_a, **_k: calls.extend(batch))
    monkeypatch.setattr(view, "_load_motion_events", lambda *_a: None)
    moment = 1_791_900_000_000
    view.show_moment("cam-b", moment)
    assert view._checked_camera_ids() == ["cam-b"]
    timeline = [c for c in calls if c[0] == "playback.get_timeline"]
    assert timeline[0][1]["start_ms"] <= moment <= timeline[0][1]["end_ms"]

    start, end = timeline[0][1]["start_ms"], timeline[0][1]["end_ms"]
    view._on_timeline_loaded([[{"start_ms": start, "end_ms": end}]], ["cam-b"], start, end)
    opened = [c for c in calls if c[0] == "playback.open"]
    assert opened[0][1]["ts_ms"] == moment
    # Only that load: a later one opens at the range start again.
    assert view._open_at_ms is None


def test_a_moment_between_recordings_opens_at_the_next_one():
    from neurowatch.widgets.playback_view import _recorded_moment

    segments = [{"start_ms": 0, "end_ms": 100}, {"start_ms": 200, "end_ms": 300}]
    assert _recorded_moment(segments, 50) == 50
    assert _recorded_moment(segments, 150) == 200
    assert _recorded_moment(segments, 350) is None
