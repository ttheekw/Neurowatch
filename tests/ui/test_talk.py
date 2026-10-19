from __future__ import annotations

import threading


class FakeClient:
    def __init__(self, refuse: str | None = None) -> None:
        self.calls: list[tuple[str, dict]] = []
        self.refuse = refuse
        self.lock = threading.Lock()

    def call(self, method, params, timeout=None):
        with self.lock:
            self.calls.append((method, params))
        if self.refuse and method == "talk.start":
            raise RuntimeError(self.refuse)
        return {"codec": "G.711 µ-law"} if method == "talk.start" else {}

    def methods(self):
        with self.lock:
            return [method for method, _ in self.calls]


def test_holding_starts_renews_and_releasing_stops(qtbot, monkeypatch):
    from neurowatch.ipc import talk_worker

    monkeypatch.setattr(talk_worker, "RENEW_S", 0.05)
    client = FakeClient()
    worker = talk_worker.TalkWorker(None, "porch", "desk-1", client=client)
    states = []
    worker.state.connect(lambda state, detail: states.append((state, detail)))
    worker.start()
    worker.press("tone")
    qtbot.waitUntil(lambda: client.methods().count("talk.renew") >= 2, timeout=3000)
    worker.release()
    qtbot.waitUntil(lambda: ("stopped", "") in states, timeout=3000)
    worker.shutdown()
    worker.wait(3000)
    assert client.calls[0] == ("talk.start", {"client_id": "desk-1", "camera_id": "porch", "source": "tone"})
    assert client.methods()[-1] == "talk.stop"
    assert states[:2] == [("connecting", ""), ("talking", "G.711 µ-law")]


def test_a_refused_start_is_shown_and_not_retried(qtbot):
    from neurowatch.ipc.talk_worker import TalkWorker

    client = FakeClient(refuse="this camera does not accept sound")
    worker = TalkWorker(None, "porch", "desk-1", client=client)
    states = []
    worker.state.connect(lambda state, detail: states.append((state, detail)))
    worker.start()
    worker.press()
    qtbot.waitUntil(lambda: any(s == "error" for s, _ in states), timeout=3000)
    qtbot.wait(100)
    worker.shutdown()
    worker.wait(3000)
    assert client.methods() == ["talk.start"], "nothing more until pressed again"
    assert "does not accept sound" in states[-1][1]


def test_the_dialog_talks_while_the_button_or_space_is_held(qtbot):
    from PySide6.QtCore import QEvent, Qt
    from PySide6.QtGui import QKeyEvent

    from neurowatch.dialogs.talk_dialog import TalkDialog

    class Worker:
        def __init__(self):
            from PySide6.QtCore import QObject, Signal

            class Holder(QObject):
                state = Signal(str, str)

            self._holder = Holder()
            self.state = self._holder.state
            self.events = []

        def start(self):
            pass

        def press(self, source="microphone"):
            self.events.append(("press", source))

        def release(self):
            self.events.append(("release",))

        def shutdown(self):
            self.events.append(("shutdown",))

        def wait(self, _ms):
            return True

    worker = Worker()
    dialog = TalkDialog(None, "porch", "Porch", worker=worker)
    qtbot.addWidget(dialog)
    dialog.keyPressEvent(QKeyEvent(QEvent.Type.KeyPress, Qt.Key.Key_Space, Qt.KeyboardModifier.NoModifier))
    dialog.keyReleaseEvent(QKeyEvent(QEvent.Type.KeyRelease, Qt.Key.Key_Space, Qt.KeyboardModifier.NoModifier))
    dialog.tone.setChecked(True)
    dialog.button.pressed.emit()
    assert worker.events[:3] == [("press", "microphone"), ("release",), ("press", "tone")]
    worker.state.emit("talking", "G.711 µ-law")
    assert "Talking (G.711 µ-law)" in dialog.status.text()
    worker.state.emit("error", "the camera did not answer in time")
    assert "Could not talk" in dialog.status.text()
    dialog.close()
    assert ("shutdown",) in worker.events
