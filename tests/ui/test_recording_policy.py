import pytest
from PySide6.QtCore import QTime

from neurowatch.models.recording_policy import policy_problem


def test_grid_paint_preserves_minutes_overnight_edges_and_default_inheritance():
    from neurowatch.models.recording_policy import paint_windows, weekly_slots

    rules = [{"days": [6], "start": "22:15", "end": "02:45", "mode": "events"}]
    original = weekly_slots(rules)
    painted = paint_windows(rules, 2, 4, 8 * 60, 9 * 60, "off")
    slots = weekly_slots(painted)
    assert slots[: 2 * 1440] == original[: 2 * 1440]
    assert slots[6 * 1440 :] == original[6 * 1440 :]
    assert slots[2 * 1440 + 480 : 2 * 1440 + 540] == ["off"] * 60
    assert weekly_slots(paint_windows(painted, 2, 4, 480, 540, None)) == original
    assert rules[0]["start"] == "22:15"


def test_painted_schedule_and_exact_time_rows_stay_in_sync_and_can_undo(qtbot):
    from neurowatch.widgets.recording_policy import RecordingPolicyEditor

    original = {
        "mode": "events",
        "weekly_rules": [{"days": [0], "start": "08:15", "end": "18:45", "mode": "continuous"}],
    }
    editor = RecordingPolicyEditor(original)
    qtbot.addWidget(editor)
    editor.paint(2, 4, 480, 540, "off")
    assert policy_problem(editor.policy()) is None
    assert editor.grid.slots()[2 * 1440 + 480] == "off"
    editor.undo()
    assert editor.policy() == original
    editor._rows[0]["start"].setTime(QTime(8, 30))
    assert editor.grid.slots()[8 * 60 + 20] is None
    assert editor.grid.slots()[8 * 60 + 30] == "continuous"


def test_weekly_windows_wrap_at_sunday_and_refuse_overlap():
    policy = {
        "mode": "continuous",
        "weekly_rules": [
            {"days": [6], "start": "22:00", "end": "02:00", "mode": "events"},
        ],
    }
    assert policy_problem(policy) is None
    policy["weekly_rules"].append({"days": [0], "start": "01:00", "end": "03:00", "mode": "off"})
    assert "overlap" in policy_problem(policy).lower()


def test_recording_policy_refuses_bad_dates_and_out_of_range_padding():
    assert policy_problem({"mode": "unknown"})
    assert policy_problem({"mode": "events", "event_pre_sec": 3601})
    assert policy_problem(
        {"weekly_rules": [{"days": [0], "start": "24:00", "end": "08:00", "mode": "off"}]}
    )


def test_editor_round_trips_minute_precision_without_changing_saved_policy(qtbot):
    from neurowatch.widgets.recording_policy import RecordingPolicyEditor

    original = {
        "mode": "events",
        "event_pre_sec": 12,
        "event_post_sec": 20,
        "non_event_keep_hours": 2,
        "weekly_rules": [
            {"days": [0, 2, 4], "start": "08:15", "end": "18:45", "mode": "continuous"}
        ],
    }
    editor = RecordingPolicyEditor(original)
    qtbot.addWidget(editor)
    assert editor.policy() == original
    editor.mode.set_value("off")
    assert editor.policy()["mode"] == "off"
    assert original["mode"] == "events"
    assert editor.policy()["weekly_rules"] == original["weekly_rules"]


def test_camera_recording_tab_saves_policy_and_refuses_overlapping_windows(qtbot):
    from neurowatch.dialogs.camera_dialog import AddEditCameraDialog
    from neurowatch.models.camera import CameraConfig

    camera = CameraConfig.from_dict(
        {"id": "cam", "name": "Gate", "url": "rtsp://example.invalid/a"}
    )
    dialog = AddEditCameraDialog(camera=camera)
    qtbot.addWidget(dialog)
    editor = dialog._policy_editor
    editor.mode.set_value("events")
    rule = {"days": [0], "start": "08:00", "end": "18:00", "mode": "off"}
    editor.add_rule(rule)
    editor.add_rule(rule)
    dialog._on_accept()
    assert not dialog.result()
    assert "overlap" in dialog._rec_status_lbl.text().lower()
    assert dialog.get_config().to_dict()["recording"]["mode"] == "events"


def test_manual_recording_buttons_use_background_worker_and_policy_status_is_explained(
    qtbot, monkeypatch
):
    from PySide6.QtCore import QObject, Signal

    import neurowatch.dialogs.camera_dialog as module
    from neurowatch.models.camera import CameraConfig

    calls = []

    class Worker(QObject):
        done = Signal(object)
        finished = Signal()

        def __init__(self, endpoint, method, params=None):
            super().__init__()
            self.method = method
            self.params = params

        def start(self):
            calls.append((self.method, self.params))

        def isRunning(self):
            return False

    class NoBlockingClient:
        def __init__(self, *args, **kwargs):
            pass

        def call(self, *args, **kwargs):
            raise AssertionError("RPC must run in a worker")

    monkeypatch.setattr(module, "_RpcWorker", Worker)
    monkeypatch.setattr(module, "SyncIpcClient", NoBlockingClient)
    dialog = module.AddEditCameraDialog(
        camera=CameraConfig.from_dict(
            {"id": "cam", "name": "Gate", "url": "rtsp://example.invalid/1"}
        )
    )
    qtbot.addWidget(dialog)
    calls.clear()
    dialog._on_rec_start()
    assert calls == [("recording.start", {"id": "cam"})]
    dialog._on_recording_action({"ok": True})
    dialog._local_enabled.setChecked(True)
    dialog._on_policy_status(
        {
            "cam": {
                "active_mode": "events",
                "detail": "Footage is kept while detection reconnects.",
                "cleanup_active": False,
                "manual_override": False,
                "next_change_wall_minutes": 60,
            }
        }
    )
    assert "kept" in dialog._policy_status_lbl.text()
    assert "1 h 0 min" in dialog._policy_status_lbl.text()
    dialog._local_enabled.setChecked(False)
    dialog._on_policy_status({"cam": {"active_mode": "continuous", "detail": ""}})
    assert dialog._policy_status_lbl.text().startswith("Not recording")


def test_copy_policy_requires_selection_freezes_settings_and_uses_one_background_batch(
    qtbot, monkeypatch
):
    from PySide6.QtCore import QObject, Qt, Signal

    import neurowatch.dialogs.copy_recording_policy_dialog as module

    workers = []

    class Worker(QObject):
        done = Signal(object)
        finished = Signal()

        def __init__(self, endpoint, method, params=None):
            super().__init__()
            self.method, self.params = method, params
            workers.append(self)

        def start(self):
            pass

        def isRunning(self):
            return False

    monkeypatch.setattr(module, "_PolicyWorker", Worker)
    policy = {"mode": "events", "event_pre_sec": 0, "weekly_rules": []}
    dialog = module.CopyRecordingPolicyDialog("unix:/offline-test", "source", policy)
    qtbot.addWidget(dialog)
    dialog.show()
    policy["mode"] = "off"
    assert workers[-1].method == "recording.policy_targets"
    assert not dialog.copy.isEnabled()
    workers[-1].done.emit(
        [
            {"id": "source", "name": "Source", "recording_enabled": True, "manual_override": False},
            {"id": "target", "name": "Gate", "recording_enabled": False, "manual_override": True},
        ]
    )
    assert dialog.targets.count() == 1
    assert "disabled" in dialog.targets.item(0).text()
    dialog.targets.item(0).setCheckState(Qt.CheckState.Checked)
    assert dialog.copy.isEnabled()
    dialog._copy()
    assert workers[-1].method == "recording.copy_policy"
    assert workers[-1].params["target_ids"] == ["target"]
    assert workers[-1].params["policy"]["mode"] == "events"
    assert workers[-1].params["policy"]["event_pre_sec"] == 0
    dialog.reject()
    assert dialog.isVisible()  # cannot lose the result of a pending save
    dialog.close()
    assert dialog.isVisible()
    count = len(workers)
    dialog._copy()
    assert len(workers) == count
    workers[-1].done.emit(RuntimeError("write failed"))
    assert "write failed" in dialog.status.text()
    assert dialog.copy.isEnabled()
    dialog._copy()
    workers[-1].done.emit({"ok": True, "copied": ["target"]})
    assert "saved to 1" in dialog.status.text()
    assert not dialog.copy.isEnabled()
    dialog.reject()
    assert not dialog.isVisible()


def test_schedule_drag_and_keyboard_paint_the_selected_rectangle(qtbot):
    from PySide6.QtCore import QPoint, Qt

    from neurowatch.widgets.recording_policy import RecordingPolicyEditor

    editor = RecordingPolicyEditor()
    qtbot.addWidget(editor)
    editor.resize(700, 800)
    editor.show()
    grid = editor.grid
    a = grid._rect(1, 6 * 60, 7 * 60).center().toPoint()
    b = grid._rect(3, 8 * 60, 9 * 60).center().toPoint()
    qtbot.mousePress(grid, Qt.MouseButton.LeftButton, pos=a)
    qtbot.mouseMove(grid, b)
    qtbot.mouseRelease(grid, Qt.MouseButton.LeftButton, pos=b)
    slots = grid.slots()
    for day in (1, 2, 3):
        assert slots[day * 1440 + 360 : day * 1440 + 540] == ["events"] * 180
    assert slots[360] is None
    qtbot.keyClick(grid, Qt.Key.Key_Delete)
    assert grid.slots()[3 * 1440 + 480] is None
    editor.resolution.setCurrentIndex(1)
    qtbot.mouseClick(grid, Qt.MouseButton.LeftButton, pos=QPoint(grid.width() - 4, grid.TOP + 5))
    assert grid.slots()[1425:] == ["events"] * 15 + grid.slots()[1440:]
    editor.resolution.setCurrentIndex(0)
    qtbot.keyClick(grid, Qt.Key.Key_Space)
    assert grid.slots()[1380:1440] == ["events"] * 60


def test_policy_worker_calls_private_client_without_blocking_the_dialog(monkeypatch):
    import neurowatch.dialogs.copy_recording_policy_dialog as module

    class Client:
        def __init__(self, endpoint):
            assert endpoint == "unix:/isolated-test"

        def call(self, method, params):
            assert method == "recording.copy_policy"
            assert params == {"test": True}
            return {"ok": True}

    monkeypatch.setattr(module, "SyncIpcClient", Client)
    results = []
    worker = module._PolicyWorker("unix:/isolated-test", "recording.copy_policy", {"test": True})
    worker.done.connect(results.append)
    worker.run()
    assert results == [{"ok": True}]


def test_camera_save_uses_background_update_without_creating_manual_recording_override(monkeypatch):
    from types import SimpleNamespace

    from PySide6.QtCore import QObject, Signal

    import neurowatch.main as module
    from neurowatch.models.camera import CameraConfig

    camera = CameraConfig.from_dict(
        {
            "id": "cam",
            "name": "Gate",
            "url": "rtsp://offline/test",
            "recording": {"mode": "off", "local": {"enabled": True}},
        }
    )
    calls = []

    class Dialog(QObject):
        recording_policy_copied = Signal(object, object)
        finished = Signal(int)  # the window deletes its dialogs after use
        DialogCode = SimpleNamespace(Accepted=1)

        def __init__(self, **kwargs):
            super().__init__()

        def exec(self):
            return 1

        def get_config(self):
            return camera

    class Worker(QObject):
        result = Signal(object)
        finished = Signal()

        def __init__(self, method, params, endpoint):
            super().__init__()
            calls.append((method, params, endpoint))

        def start(self):
            pass

    monkeypatch.setattr(module, "AddEditCameraDialog", Dialog)
    monkeypatch.setattr(module, "_IpcCallWorker", Worker)
    fake = SimpleNamespace(
        _cameras=[camera],
        _rec_states={},
        _edit_workers={},
        _ipc_endpoint=lambda: "unix:/isolated-test",
        _on_camera_updated=lambda result: None,
        _on_recording_policy_copied=lambda ids, policy: None,
    )
    module.MainWindow._on_edit_camera(fake, "cam")
    assert len(calls) == 1
    assert calls[0][0] == "camera.update"
    assert calls[0][1]["recording"]["mode"] == "off"
    # A canceled source editor cannot make later target edits overwrite the copy.
    original_local = camera.recording.local
    policy = {"mode": "events", "weekly_rules": [], "event_pre_sec": 0}
    module.MainWindow._on_recording_policy_copied(fake, ["cam"], policy)
    policy["mode"] = "off"
    assert camera.recording.mode == "events"
    assert camera.recording.local is original_local
    assert camera.recording.event_pre_sec == 0


def test_paint_refuses_window_limit_without_discarding_existing_rules(qtbot):
    from neurowatch.widgets.recording_policy import RecordingPolicyEditor

    rules = []
    for index in range(64):
        start, end = index * 2, index * 2 + 1
        rules.append(
            {
                "days": [0],
                "start": f"{start // 60:02}:{start % 60:02}",
                "end": f"{end // 60:02}:{end % 60:02}",
                "mode": "off",
            }
        )
    original = {"mode": "continuous", "weekly_rules": rules}
    editor = RecordingPolicyEditor(original)
    qtbot.addWidget(editor)
    before = editor.grid.slots()
    editor.paint(1, 1, 480, 540, "events")
    assert editor.policy() == original
    assert editor.grid.slots() == before
    assert "64" in editor.problem.text()


@pytest.mark.parametrize("screen_height", [600, 800])
def test_recording_tab_opens_with_readable_background_and_visible_grid(
    qtbot, monkeypatch, screen_height
):
    from PySide6.QtGui import QColor

    from neurowatch.dialogs.camera_dialog import AddEditCameraDialog
    from neurowatch.models.camera import CameraConfig
    from neurowatch.theme import active_colors

    dialog = AddEditCameraDialog(
        camera=CameraConfig.from_dict(
            {"id": "offline-example", "name": "Example", "url": "rtsp://example.invalid/demo"}
        )
    )
    qtbot.addWidget(dialog)
    monkeypatch.setattr(dialog, "_available_height", lambda: screen_height)
    index = next(i for i in range(dialog._tabs.count()) if dialog._tabs.tabText(i) == "Recording")
    dialog._tabs.setCurrentIndex(index)
    dialog.show()
    qtbot.wait(30)
    assert dialog.height() <= screen_height
    scroll = dialog._tabs.currentWidget()
    viewport = scroll.viewport()
    assert viewport.grab().toImage().pixelColor(1, 1) == QColor(active_colors().surface)
    for widget in (dialog._policy_editor.grid, dialog._policy_editor.copy_button):
        rect = widget.rect()
        rect.moveTopLeft(widget.mapTo(viewport, rect.topLeft()))
        if widget is dialog._policy_editor.grid and screen_height < 800:
            # A short screen shows where the schedule starts; it scrolls into view.
            rect.setHeight(40)
        assert viewport.rect().contains(rect), f"{type(widget).__name__} hidden below initial view"
