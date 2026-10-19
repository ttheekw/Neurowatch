from __future__ import annotations


def test_bookmark_parameters_cover_a_moment_or_a_span_and_trim_the_note():
    from neurowatch.dialogs.bookmark_dialog import bookmark_params

    assert bookmark_params("cam", 1_000, 0, "  ", False) == {
        "camera_id": "cam",
        "time_ms": 1_000,
        "kept": False,
    }
    assert bookmark_params(None, 1_000, 5, " delivery van ", True) == {
        "camera_id": None,
        "time_ms": 1_000,
        "kept": True,
        "end_ms": 301_000,
        "note": "delivery van",
    }
    # Never past the recorder's one-day limit.
    assert bookmark_params("cam", 0, 10_000, "", False)["end_ms"] == 86_400_000


def test_a_new_bookmark_asks_for_the_chosen_camera_span_note_and_kept_footage(qtbot):
    from neurowatch.dialogs.bookmark_dialog import BookmarkDialog

    dialog = BookmarkDialog(
        None, [("a", "Gate"), ("b", "Yard")], 5_000, allow_every_camera=True
    )
    qtbot.addWidget(dialog)
    assert dialog._camera.count() == 3, "two cameras and every camera"
    dialog._camera.setCurrentIndex(1)
    dialog._span.set_value(1)
    dialog._minutes.setValue(2)
    dialog._note.setText("ladder left out")
    dialog._keep.setChecked(True)
    assert dialog.request() == (
        "events.bookmark",
        {
            "camera_id": "b",
            "time_ms": 5_000,
            "kept": True,
            "end_ms": 125_000,
            "note": "ladder left out",
        },
    )
    dialog._camera.setCurrentIndex(2)
    assert dialog.request()[1]["camera_id"] is None


def test_editing_changes_only_the_note_and_kept_footage(qtbot):
    from neurowatch.dialogs.bookmark_dialog import BookmarkDialog
    from neurowatch.models.events import Event

    event = Event(7, "bookmark", "a", 1_000, 1_000, detail="old", kept=True)
    dialog = BookmarkDialog(None, [("a", "Gate")], 0, event=event)
    qtbot.addWidget(dialog)
    assert dialog._note.text() == "old" and dialog._keep.isChecked()
    dialog._note.setText("   ")
    dialog._keep.setChecked(False)
    assert dialog.request() == (
        "events.update_bookmark",
        {"id": 7, "note": None, "kept": False},
    )


def test_saving_runs_off_the_ui_thread_and_reports_the_stored_bookmark(qtbot, monkeypatch):
    from neurowatch.dialogs import bookmark_dialog

    calls = []

    class Client:
        def __init__(self, endpoint):
            self.endpoint = endpoint

        def call(self, method, params):
            calls.append((self.endpoint, method, params))
            if params.get("note") == "refuse":
                raise RuntimeError("Camera not found")
            return {"id": 3, "kind": "bookmark", "camera_id": "a", "start_ms": 9,
                    "end_ms": 9, "kept": True, "detail": params.get("note")}

    monkeypatch.setattr(bookmark_dialog, "SyncIpcClient", Client)
    dialog = bookmark_dialog.BookmarkDialog("unix:/tmp/bm.sock", [("a", "Gate")], 9)
    qtbot.addWidget(dialog)
    dialog._note.setText("refuse")
    dialog._submit()
    qtbot.waitUntil(lambda: dialog._error.isVisibleTo(dialog))
    assert "Camera not found" in dialog._error.text()
    dialog._note.setText("fine")
    dialog._keep.setChecked(True)
    with qtbot.waitSignal(dialog.saved) as saved:
        dialog._submit()
    assert saved.args[0].kept and saved.args[0].detail == "fine"
    assert calls[-1][0] == "unix:/tmp/bm.sock" and calls[-1][1] == "events.bookmark"


def test_a_bookmark_in_the_detail_pane_offers_edit_and_delete_and_says_if_kept(
    qtbot, monkeypatch
):
    from neurowatch.models.events import Event
    from neurowatch.widgets import event_list
    from neurowatch.widgets.event_list import EventImageLoader
    from neurowatch.widgets.event_preview import EventPreview

    monkeypatch.setattr(event_list, "_ImageWorker", None)
    preview = EventPreview(EventImageLoader(None))
    qtbot.addWidget(preview)
    preview.show()
    kept = Event(4, "bookmark", "cam", 1_000, 1_000, detail="note", kept=True)
    preview.set_event(kept, "Gate")
    assert preview._bookmark_actions.isVisibleTo(preview)
    assert "Footage: Kept" in preview.details_text()
    with qtbot.waitSignal(preview.edit_requested) as edit:
        preview._edit.click()
    assert edit.args[0] is kept
    with qtbot.waitSignal(preview.delete_requested):
        preview._delete.click()
    preview.set_event(Event(5, "motion", "cam", 1_000, 2_000), "Gate")
    assert not preview._bookmark_actions.isVisibleTo(preview)
    assert Event.from_dict({"id": 1, "kind": "bookmark", "kept": True}).kept


def test_live_tiles_offer_a_bookmark_of_this_moment(qtbot):
    from neurowatch.models.camera import CameraConfig
    from neurowatch.widgets.cell_widget import CellWidget

    cell = CellWidget()
    qtbot.addWidget(cell)
    cell.set_camera(CameraConfig(id="cam", name="Gate", url="rtsp://offline/fixture"))
    menu = cell._build_context_menu()
    [action] = [a for a in menu.actions() if a.text() == "Add Bookmark…"]
    with qtbot.waitSignal(cell.bookmark_requested) as requested:
        action.trigger()
    assert requested.args == ["cam"]
    menu.deleteLater()
