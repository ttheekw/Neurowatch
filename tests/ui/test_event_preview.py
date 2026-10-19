from __future__ import annotations

import base64


def _jpeg(width: int, height: int) -> bytes:
    from PySide6.QtCore import QBuffer, QByteArray, QIODevice
    from PySide6.QtGui import QImage

    picture = QImage(width, height, QImage.Format.Format_RGB32)
    picture.fill(0xFF224466)
    data = QByteArray()
    buffer = QBuffer(data)
    buffer.open(QIODevice.OpenModeFlag.WriteOnly)
    assert picture.save(buffer, "JPEG")
    return bytes(data)


class _FakeWorkers:
    """Image workers that never run: the test answers for the recorder."""

    def __init__(self, monkeypatch):
        from PySide6.QtCore import QObject, Signal

        from neurowatch.widgets import event_list

        made = self.made = []

        class Worker(QObject):
            done = Signal(object, int, object)
            finished = Signal()

            def __init__(self, endpoint, event_id, generation):
                super().__init__()
                self.endpoint, self.event_id, self.generation = endpoint, event_id, generation
                made.append(self)

            def start(self):
                pass

            def isRunning(self):  # noqa: N802 - fake Qt interface
                return False

        monkeypatch.setattr(event_list, "_ImageWorker", Worker)

    def answer(self, result):
        worker = self.made[-1]
        worker.done.emit(worker.event_id, worker.generation, result)
        worker.finished.emit()


def test_event_metadata_is_bounded_and_invalid_values_do_not_hide_history():
    from neurowatch.models.events import Event, metadata_lines

    raw = {
        "id": 7,
        "kind": "motion",
        "start_ms": 1000,
        "end_ms": 2000,
        "score": 0.125,
        "zones": ["gate", "removed"],
        "bbox": [0.1, 0.2, 0.3, 0.4],
        "thumbnail": "7.jpg",
    }
    event = Event.from_dict(raw)
    assert event is not None and event.has_image
    assert metadata_lines(event, {"gate": "Front gate"}) == [
        "Peak motion score: 12.5%",
        "Zones: Front gate, removed",
        "Changed area: x 10%, y 20%, width 30%, height 40% (analysis image)",
    ]
    for bad in [True, float("nan"), -1, 2, "unknown"]:
        invalid = Event.from_dict({**raw, "score": bad, "bbox": [0.8, 0, 0.5, 1]})
        assert invalid is not None and invalid.score is None and invalid.bbox is None
    bounded = Event.from_dict({**raw, "zones": ["gate"] * 100 + [42]})
    assert bounded is not None and bounded.zones == ("gate",)
    unknown = Event.from_dict({"id": 9, "kind": "intrusion"})
    assert unknown is not None and metadata_lines(unknown, {}) == [] and not unknown.has_image


def test_selected_preview_shows_metadata_as_plain_text(qtbot, monkeypatch):
    from neurowatch.models.events import Event
    from neurowatch.widgets.event_list import EventImageLoader
    from neurowatch.widgets.event_preview import EventPreview

    workers = _FakeWorkers(monkeypatch)
    preview = EventPreview(EventImageLoader("unix:/tmp/selected.sock"))
    qtbot.addWidget(preview)
    preview.show()
    preview.set_zone_names({"gate": "<Front gate>"})
    event = Event.from_dict(
        {
            "id": 1,
            "kind": "motion",
            "camera_id": "c",
            "score": 0.25,
            "zones": ["gate"],
            "thumbnail": "1.jpg",
            "start_ms": 1000,
            "end_ms": 4000,
            "source": "builtin",
        }
    )
    preview.set_event(event, "Gate")
    text = preview.details_text()
    assert "Motion · Gate" in text
    assert "Motion score: 25%" in text
    assert "Zones: <Front gate>" in text
    assert "Source: NeuroWatch motion detection" in text
    assert [w.event_id for w in workers.made] == [1]
    assert workers.made[0].endpoint == "unix:/tmp/selected.sock"


def test_events_without_saved_pictures_ask_nothing(qtbot, monkeypatch):
    from neurowatch.models.events import Event
    from neurowatch.widgets.event_list import EventImageLoader
    from neurowatch.widgets.event_preview import EventPreview

    workers = _FakeWorkers(monkeypatch)
    preview = EventPreview(EventImageLoader(None))
    qtbot.addWidget(preview)
    preview.show()
    preview.set_event(Event(1, "camera_offline", "cam", 1000, None), "Gate")
    assert workers.made == []
    assert "No picture" in preview.picture_message
    assert "Ongoing for" in preview.details_text()


def test_saved_image_validation_rejects_wrong_type_size_and_invalid_base64():
    from neurowatch.models.event_images import decode_saved_image

    assert decode_saved_image({"available": False}) is None
    good = {
        "available": True,
        "mime": "image/jpeg",
        "jpeg_b64": base64.b64encode(b"\xff\xd8\xff\xd9").decode(),
    }
    assert decode_saved_image(good) == b"\xff\xd8\xff\xd9"
    for bad in [
        {**good, "mime": "text/html"},
        {**good, "jpeg_b64": "!invalid!"},
        {**good, "jpeg_b64": "A" * 180000},
    ]:
        try:
            decode_saved_image(bad)
        except ValueError:
            pass
        else:
            raise AssertionError("Malformed image must be refused")


def test_image_worker_keeps_endpoint_and_does_not_request_video(
    monkeypatch, _no_event_image_network
):
    from neurowatch.widgets import event_list

    seen = []

    class Client:
        def __init__(self, endpoint):
            self.endpoint = endpoint

        def call(self, method, params):
            seen.append((self.endpoint, method, params))
            return {"available": False}

    monkeypatch.setattr(event_list, "SyncIpcClient", Client)
    answers = []
    worker = _no_event_image_network("unix:/tmp/selected.sock", 7, 2)
    worker.done.connect(lambda *args: answers.append(args))
    worker.run()
    assert seen == [("unix:/tmp/selected.sock", "events.thumbnail", {"id": 7})]
    assert answers == [(7, 2, None)]


def test_loader_runs_one_request_newest_wish_first_and_forgets_after_stop(qtbot, monkeypatch):
    from neurowatch.widgets.event_list import EventImageLoader

    workers = _FakeWorkers(monkeypatch)
    loader = EventImageLoader(None)
    ready = []
    loader.ready.connect(ready.append)
    for event_id in (1, 2, 3):
        loader.thumbnail(event_id)
    # One request at a time: the first; then the newest wish (3), not 2.
    assert [w.event_id for w in workers.made] == [1]
    workers.answer(None)
    assert loader.state(1) == EventImageLoader.MISSING
    assert [w.event_id for w in workers.made] == [1, 3]
    # A picture asked for urgently (the detail pane) jumps the queue.
    loader.image(9)
    workers.answer(_jpeg(320, 180))
    assert loader.thumbnail(3, request=False) is not None
    assert workers.made[-1].event_id == 9
    # Missing pictures are not asked for again until retried.
    loader.thumbnail(1)
    assert workers.made[-1].event_id == 9
    generation = workers.made[-1].generation
    loader.stop()
    workers.made[-1].done.emit(9, generation, _jpeg(320, 180))
    assert loader.image(9, request=False) is None
    assert ready == [1, 3]


def test_loader_bounds_caches_and_refuses_oversized_pictures(qtbot, monkeypatch):
    from neurowatch.widgets import event_list

    workers = _FakeWorkers(monkeypatch)
    loader = event_list.EventImageLoader(None)
    data = _jpeg(320, 180)
    for event_id in range(1, 34):
        loader.image(event_id)
        workers.answer(data)
    assert len(loader._full) == event_list.FULL_CACHE and 1 not in loader._full
    assert loader.thumbnail(1, request=False) is not None
    loader.image(50)
    workers.answer(_jpeg(2048, 2048))
    assert loader.image(50, request=False) is None
    assert loader.state(50) == event_list.EventImageLoader.FAILED


def test_detail_pane_offers_retry_only_when_the_picture_failed(qtbot, monkeypatch):
    from neurowatch.models.events import Event
    from neurowatch.widgets.event_list import EventImageLoader
    from neurowatch.widgets.event_preview import EventPreview

    workers = _FakeWorkers(monkeypatch)
    preview = EventPreview(EventImageLoader(None))
    qtbot.addWidget(preview)
    preview.show()
    event = Event(3, "motion", "cam", 5000, 6000, has_image=True)
    preview.set_event(event, "Gate")
    assert "Loading" in preview.picture_message
    workers.answer(RuntimeError("offline"))
    assert "click to try again" in preview.picture_message
    preview.retry()
    assert workers.made[-1].event_id == 3 and len(workers.made) == 2
    workers.answer(_jpeg(320, 180))
    assert preview.picture_message == ""
    preview.stop()
    assert preview.event_id is None


def test_model_groups_days_and_updates_rows_in_place(qtbot):
    import time

    from neurowatch.models.events import Event
    from neurowatch.widgets.event_list import EVENT_ROLE, HEADER_ROLE, EventListModel

    now = int(time.time() * 1000)
    day = 86_400_000
    model = EventListModel()
    model.set_events(
        [
            Event(1, "motion", "c", now - 2 * day, now - 2 * day + 1000),
            Event(2, "motion", "c", now - 1000, None),
        ]
    )
    rows = [
        model.index(r).data(HEADER_ROLE) or model.index(r).data(EVENT_ROLE).id
        for r in range(model.rowCount())
    ]
    assert rows[0] == "Today" and rows[1] == 2 and rows[3] == 1
    changed = []
    reset = []
    model.dataChanged.connect(lambda a, b: changed.append((a.row(), b.row())))
    model.modelReset.connect(lambda: reset.append(True))
    model.upsert(Event(2, "motion", "c", now - 1000, now))
    assert changed == [(1, 1)] and not reset
    inserted = []
    model.rowsInserted.connect(lambda _p, first, last: inserted.append((first, last)))
    model.upsert(Event(3, "motion", "c", now, None))
    assert inserted == [(1, 1)] and model.row_of(3) == 1 and model.row_of(2) == 2
    model.remove(1)
    assert model.rowCount() == 3  # yesterday's heading went with its only event
