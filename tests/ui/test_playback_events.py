from __future__ import annotations

from neurowatch.models.events import Event


def _count(pane) -> int:
    return len(pane._list.events_model.events)


def _select(pane, row: int) -> None:
    """The row-th event from the top (newest first)."""
    pane._list.select_event(pane._list.events_model.events[row].id)


def test_hidden_live_history_is_not_rebuilt_until_shown(qtbot, monkeypatch):
    from neurowatch.widgets.playback_events import PlaybackEventsPane

    pane = PlaybackEventsPane("unix:/tmp/no-requests.sock")
    qtbot.addWidget(pane)
    pane.set_events([Event(1, "motion", "a", 1000, 1200)], {"a": "Gate"})
    renders = []
    render = pane._render
    monkeypatch.setattr(pane, "_render", lambda: (renders.append(True), render()))
    for id_ in range(2, 202):
        pane.update_event(Event(id_, "motion", "a", id_ * 1000, id_ * 1000 + 200))
    assert len(pane._events) == 201 and renders == []
    pane.show()
    qtbot.waitUntil(lambda: len(renders) == 1)
    assert _count(pane) == 201
    pane.stop()


def test_visible_live_notice_burst_has_one_coalesced_render(qtbot, monkeypatch):
    from neurowatch.widgets.playback_events import PlaybackEventsPane

    pane = PlaybackEventsPane("unix:/tmp/no-requests.sock")
    qtbot.addWidget(pane)
    pane.set_events([Event(1, "motion", "a", 1000, None)], {"a": "Gate"})
    pane.show()
    qtbot.waitExposed(pane)
    renders = []
    render = pane._render
    monkeypatch.setattr(pane, "_render", lambda: (renders.append(True), render()))
    for id_ in range(1, 101):
        pane.update_event(Event(id_, "motion", "a", id_ * 1000, id_ * 1000 + 200))
    assert renders == []
    qtbot.waitUntil(lambda: len(renders) == 1)
    assert _count(pane) == 100
    pane.stop()


def test_playback_event_panel_filters_loaded_range_and_keeps_navigation_ids(qtbot, monkeypatch):
    from neurowatch.widgets.playback_events import PlaybackEventsPane

    pane = PlaybackEventsPane("unix:/tmp/custom.sock")
    qtbot.addWidget(pane)
    monkeypatch.setattr(pane._preview, "set_event", lambda *args: None)
    pane.set_events(
        [
            Event(1, "motion", "a", 10000, 12000),
            Event(2, "bookmark", "b", 20000, 20000),
            Event(3, "tamper", "a", 30000, 35000, acknowledged=True),
        ],
        {"a": "Gate", "b": "Yard"},
    )
    assert _count(pane) == 3
    pane._search.setText("yard")
    assert _count(pane) == 1
    _select(pane, 0)
    seeks = []
    pane.seek_requested.connect(lambda *args: seeks.append(args))
    pane.review_selected()
    assert seeks == [("b", 17000)]
    pane._search.clear()
    pane._new_only.setChecked(True)
    assert _count(pane) == 2
    pane._kind.setCurrentIndex(pane._kind.findData("bookmark"))
    assert _count(pane) == 1


def test_playback_list_is_bounded_and_live_updates_preserve_selected_event(qtbot, monkeypatch):
    from neurowatch.widgets import playback_events
    from neurowatch.widgets.playback_events import PlaybackEventsPane

    pane = PlaybackEventsPane("unix:/tmp/custom.sock")
    qtbot.addWidget(pane)
    monkeypatch.setattr(pane._preview, "set_event", lambda *args: None)
    monkeypatch.setattr(playback_events, "PAGE_ROWS", 200)
    pane.set_events(
        [Event(i, "motion", "a", i * 1000, i * 1000 + 500) for i in range(1, 501)], {"a": "Gate"}
    )
    pane._filter_changed()
    assert _count(pane) == 200
    _select(pane, 3)
    selected = pane.selected_event().id
    pane.update_event(Event(selected, "motion", "a", selected * 1000, selected * 1000 + 900))
    pane._render()
    assert pane.selected_event().id == selected
    assert pane.selected_event().end_ms == selected * 1000 + 900
    pane.show_older()
    assert _count(pane) == 400
    pane.stop()
    assert _count(pane) == 0 and pane._preview.event_id is None


def test_event_navigation_preserves_epoch_milliseconds_without_qt_int32_overflow(qtbot):
    from neurowatch.widgets.events_view import EventsView
    from neurowatch.widgets.playback_events import PlaybackEventsPane

    at = 1_800_000_000_000
    event = Event(1, "motion", "a", at, at + 1000)
    pane = PlaybackEventsPane("unix:/tmp/custom.sock")
    qtbot.addWidget(pane)
    pane.set_events([event], {"a": "Gate"})
    _select(pane, 0)
    seeks = []
    pane.seek_requested.connect(lambda *args: seeks.append(args))
    pane.review_selected()
    assert seeks == [("a", at - 3000)]
    events = EventsView("unix:/tmp/custom.sock", [])
    qtbot.addWidget(events)
    events._events = [event]
    events._render()
    events._list.select_event(1)
    plays = []
    events.play_requested.connect(lambda *args: plays.append(args))
    events._play_selected()
    assert plays == [("a", at - 5000)]


def test_event_review_surfaces_follow_both_page_themes():
    from neurowatch.theme import events_stylesheet, playback_stylesheet

    for theme in ("dark", "light"):
        assert "QWidget#eventPreview" in events_stylesheet(theme)
        sheet = playback_stylesheet(theme)
        assert "QWidget#eventPreview" in sheet
        assert "QWidget#playbackEvents" in sheet
        assert "QListView#eventList" in sheet


def test_selected_event_export_preserves_id_and_does_not_seek(qtbot):
    from neurowatch.models.events import Event
    from neurowatch.widgets.playback_events import PlaybackEventsPane

    pane = PlaybackEventsPane("unix:/tmp/no-requests.sock")
    qtbot.addWidget(pane)
    event = Event(92, "motion", "gate", 1_800_000_000_000, 1_800_000_010_000)
    pane.set_events([event], {"gate": "Gate"})
    requested, seeks = [], []
    pane.export_requested.connect(requested.append)
    pane.seek_requested.connect(lambda *args: seeks.append(args))
    _select(pane, 0)
    pane._preview._export.click()
    assert requested == [event] and seeks == []
    pane.stop()


def test_review_export_and_paging_controls_fit_the_sidebar(qtbot):
    from neurowatch.widgets.playback_events import PlaybackEventsPane

    pane = PlaybackEventsPane("unix:/tmp/no-requests.sock")
    qtbot.addWidget(pane)
    pane.set_events(
        [Event(i, "motion", "gate", i * 1000, i * 1000 + 500) for i in range(1, 402)],
        {"gate": "Gate"},
    )
    assert pane.minimumSizeHint().width() <= 320


def test_events_export_uses_event_id_and_system_events_stay_disabled(qtbot):
    from neurowatch.models.camera import CameraConfig
    from neurowatch.widgets.events_view import EventsView

    view = EventsView(None, [CameraConfig(id="gate", name="Gate")])
    qtbot.addWidget(view)
    saved = Event(91, "motion", "gate", 1_800_000_000_000, 1_800_000_001_000)
    system = Event(92, "backend_started", None, saved.start_ms + 2000, saved.start_ms + 2000)
    view._events = [system, saved]
    view._render()
    requested = []
    view.export_requested.connect(requested.append)
    view._list.select_event(91)
    view._preview._export.click()
    assert requested == [saved]
    view._list.select_event(92)
    assert not view._preview._export.isEnabled()
    view.stop()


def test_window_event_export_uses_existing_dialog_lifecycle(monkeypatch):
    from types import SimpleNamespace

    from neurowatch import main

    calls = []

    class Dialog:
        def __init__(self, cameras, endpoint, parent, *, camera_id):
            calls.append(("construct", cameras, endpoint, camera_id))

        def load_event(self, event):
            calls.append(("preset", event))

        def exec(self):
            calls.append("review")

        def shutdown(self):
            calls.append("shutdown")

        def deleteLater(self):  # noqa: N802 — Qt's lifecycle method
            calls.append("delete")

    monkeypatch.setattr(main, "ExportVideoDialog", Dialog)
    window = SimpleNamespace(_cameras=[], _ipc=SimpleNamespace(endpoint="unix:/tmp/fake.sock"))
    event = Event(91, "motion", "gate", 1_800_000_000_000, 1_800_000_001_000)
    main.MainWindow._export_event(window, event)
    assert calls == [
        ("construct", [], "unix:/tmp/fake.sock", "gate"),
        ("preset", event),
        "review",
        "shutdown",
        "delete",
    ]
