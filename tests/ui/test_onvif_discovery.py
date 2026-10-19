"""Add Camera: network search and ONVIF lookup go through the backend.

The dialog's workers call ``camera.discovery.*`` on the main window's IPC
endpoint; a fake client stands in for the backend here.
"""

from __future__ import annotations

import base64

import pytest

from neurowatch.dialogs import camera_dialog
from neurowatch.ipc.sync_client import IpcError

ENDPOINT = "unix:/tmp/neurowatch-test.sock"
CANDIDATE = {
    "candidate_id": "c1",
    "address": "192.168.1.64",
    "state": "credentials_needed",
    "manufacturer": "HIKVISION",
    "model": "DS-2CD2143G2-I",
    "endpoint_reference": "urn:uuid:cam",
}
PROFILES = {
    "candidate": {**CANDIDATE, "state": "ready"},
    "streams": [
        {
            "stream_id": "s2",
            "name": "subStream",
            "width": 640,
            "height": 360,
            "source": "Profile_102",
            "url": "rtsp://192.168.1.64:554/Streaming/Channels/102",
        },
        {
            "stream_id": "s1",
            "name": "mainStream",
            "width": 2560,
            "height": 1440,
            "source": "Profile_101",
            "url": "rtsp://192.168.1.64:554/Streaming/Channels/101",
        },
    ],
    "main_stream_id": "s1",
    "sub_stream_id": "s2",
}


class FakeBackend:
    def __init__(self, answers: dict[str, list[object]]) -> None:
        self.answers = answers
        self.calls: list[tuple[str, dict, str | None]] = []

    def client(self, endpoint: str | None = None) -> FakeBackend.Client:
        return FakeBackend.Client(self, endpoint)

    class Client:
        def __init__(self, backend: FakeBackend, endpoint: str | None) -> None:
            self.backend = backend
            self.endpoint = endpoint

        def call(self, method: str, params: dict | None = None) -> object:
            self.backend.calls.append((method, dict(params or {}), self.endpoint))
            queue = self.backend.answers.get(method, [])
            answer = queue.pop(0) if len(queue) > 1 else queue[0]
            if isinstance(answer, Exception):
                raise answer
            return answer


@pytest.fixture
def backend(monkeypatch: pytest.MonkeyPatch) -> FakeBackend:
    fake = FakeBackend({})
    monkeypatch.setattr(camera_dialog, "SyncIpcClient", fake.client)
    return fake


def _run(worker: object, signal_name: str = "done") -> list[object]:
    received: list[object] = []
    getattr(worker, signal_name).connect(received.append)
    worker.run()  # type: ignore[attr-defined]
    return received


def test_a_typed_address_is_looked_up_then_read_with_its_own_password(backend: FakeBackend) -> None:
    backend.answers = {
        "camera.discovery.lookup": [{"candidate": CANDIDATE}],
        "camera.discovery.profiles": [PROFILES],
    }
    worker = camera_dialog._StreamsWorker(ENDPOINT, None, "192.168.1.64", 80, "admin", "pä55")
    assert _run(worker) == [PROFILES]
    assert backend.calls == [
        ("camera.discovery.lookup", {"address": "192.168.1.64", "port": 80}, ENDPOINT),
        (
            "camera.discovery.profiles",
            {
                "candidate_id": "c1",
                "username": "admin",
                "password_b64": base64.b64encode("pä55".encode()).decode(),
            },
            ENDPOINT,
        ),
    ]


def test_a_searched_device_is_read_directly_and_backend_refusals_come_back(
    backend: FakeBackend,
) -> None:
    refused = IpcError(-32602, "The camera refused that username and password.")
    backend.answers = {"camera.discovery.profiles": [refused]}
    worker = camera_dialog._StreamsWorker(ENDPOINT, "c1", "192.168.1.64", 80, "", "")
    assert _run(worker) == [refused]
    assert backend.calls == [("camera.discovery.profiles", {"candidate_id": "c1"}, ENDPOINT)]


def test_a_search_is_polled_until_it_completes_and_can_be_stopped(
    backend: FakeBackend, monkeypatch: pytest.MonkeyPatch
) -> None:
    running = {"scan_id": "s1", "status": "running", "scope": "192.168.1.0/24", "candidates": []}
    complete = {**running, "status": "complete", "candidates": [CANDIDATE]}
    backend.answers = {
        "camera.discovery.start": [running],
        "camera.discovery.results": [running, complete],
    }
    monkeypatch.setattr(camera_dialog._ScanWorker, "msleep", lambda self, ms: None)
    worker = camera_dialog._ScanWorker(ENDPOINT, {"scope": "current"})
    progress = _run(worker, "progress")
    assert progress[-1] == complete
    assert [c[0] for c in backend.calls] == [
        "camera.discovery.start",
        "camera.discovery.results",
        "camera.discovery.results",
    ]

    stopped = FakeBackend(
        {
            "camera.discovery.start": [running],
            "camera.discovery.cancel": [{"cancelled": True}],
            "camera.discovery.results": [{**running, "status": "cancelled"}],
        }
    )
    monkeypatch.setattr(camera_dialog, "SyncIpcClient", stopped.client)
    worker = camera_dialog._ScanWorker(ENDPOINT, {"scope": "current"})
    worker.stop()
    done = _run(worker)
    assert done[-1]["status"] == "cancelled"  # type: ignore[index]
    assert ("camera.discovery.cancel", {"scan_id": "s1"}, ENDPOINT) in stopped.calls


def test_search_results_feed_the_onvif_tab_and_the_saved_camera_remembers_its_identity(
    qtbot: object,
) -> None:
    dlg = camera_dialog.AddEditCameraDialog(ipc_endpoint=ENDPOINT)
    qtbot.addWidget(dlg)  # type: ignore[attr-defined]
    assert dlg._tabs.currentWidget() is dlg._search_tab
    added = {
        **CANDIDATE,
        "candidate_id": "c2",
        "address": "192.168.1.65",
        "state": "already_added",
        "matched_camera": {"id": "a", "name": "Gate"},
    }
    dlg._on_scan_done(
        {
            "scan_id": "s1",
            "status": "complete",
            "scope": "192.168.1.0/24",
            "candidates": [added, CANDIDATE],
        }
    )
    assert dlg._results_table.rowCount() == 2
    assert dlg._results_table.item(0, 2).text() == "Needs password"
    assert dlg._results_table.item(1, 2).text() == "Added as Gate"
    assert dlg._search_status.property("tone") == "success"

    dlg._results_table.selectRow(1)
    assert not dlg._use_btn.isEnabled()  # already added
    dlg._results_table.selectRow(0)
    assert dlg._use_btn.isEnabled()
    dlg._use_selected()
    assert dlg._tabs.currentWidget() is dlg._camera_tab
    assert dlg._mode() == "onvif"
    assert dlg._s_host.text() == "192.168.1.64"
    assert dlg._name.text() == "HIKVISION DS-2CD2143G2-I"

    dlg._user.setText("admin")
    dlg._pass.setText("secret")
    dlg._on_discover_done(PROFILES)
    assert dlg._s_main_combo.currentText() == "mainStream · 2560×1440"
    assert dlg._s_sub_combo.currentText() == "subStream · 640×360"
    cam = dlg.get_config()
    assert cam.url == "rtsp://192.168.1.64:554/Streaming/Channels/101"
    assert cam.sub_url == "rtsp://192.168.1.64:554/Streaming/Channels/102"
    assert "secret" not in cam.url
    assert cam.username == "admin"
    assert base64.b64decode(cam.password_b64).decode() == "secret"
    assert cam.streams[0].url == cam.url
    assert cam.discovery is not None
    assert (cam.discovery.endpoint_reference, cam.discovery.source) == (
        "urn:uuid:cam",
        "Profile_101",
    )


def test_a_typed_range_is_checked_and_failures_are_explained(qtbot: object) -> None:
    dlg = camera_dialog.AddEditCameraDialog(ipc_endpoint=ENDPOINT)
    qtbot.addWidget(dlg)  # type: ignore[attr-defined]
    dlg._net_combo.setCurrentIndex(dlg._net_combo.count() - 1)  # Another range…
    assert dlg._range_edit.isVisibleTo(dlg)
    dlg._range_edit.setText("10.0.0.0/16")
    dlg._on_search()
    assert dlg._scan_worker is None
    assert "1024" in dlg._search_status.text()
    dlg._on_scan_done(IpcError(-32602, "A network search is already running on this recorder."))
    assert dlg._search_status.property("tone") == "error"
    dlg._on_discover_done(IpcError(-32602, "The camera asks for a username and password."))
    assert "username and password" in dlg._discover_status.text()
    assert dlg._discover_status.property("tone") == "error"


def test_editing_a_camera_has_no_search_tab(qtbot: object) -> None:
    from neurowatch.models.camera import CameraConfig

    dlg = camera_dialog.AddEditCameraDialog(
        camera=CameraConfig(name="Gate", url="rtsp://10.0.0.5/live"), ipc_endpoint=ENDPOINT
    )
    qtbot.addWidget(dlg)  # type: ignore[attr-defined]
    assert dlg._search_tab is None
    # Entered by hand: it opens on its URLs.
    assert dlg._tabs.currentWidget() is dlg._camera_tab
    assert dlg._mode() == "urls"


def test_search_list_explains_itself_when_it_has_no_rows(qtbot: object) -> None:
    dlg = camera_dialog.AddEditCameraDialog(ipc_endpoint=ENDPOINT)
    qtbot.addWidget(dlg)  # type: ignore[attr-defined]
    dlg.show()
    panel = dlg._results_empty
    assert dlg._results_stack.currentWidget() is panel
    assert panel.title.text() == "Search for cameras"
    assert not panel._actions.isVisible()

    dlg._set_results_state("searching")
    assert panel.title.text() == "Searching…"

    dlg._on_scan_done(
        {"scan_id": "s1", "status": "complete", "scope": "192.168.1.0/24", "candidates": []}
    )
    assert dlg._results_stack.currentWidget() is panel
    assert panel.title.text() == "No cameras found"
    assert "192.168.1.0/24" in panel.detail.text()
    assert panel._actions.isVisible()
    # Said once, in the panel: no status line and no empty selection row.
    assert dlg._search_status.property("tone") == "warning"
    assert not dlg._search_status.isVisible()
    assert not dlg._use_row.isVisible()

    panel.lookup.click()
    assert (dlg._tabs.currentWidget(), dlg._mode()) == (dlg._camera_tab, "onvif")
    panel.manual.click()
    assert (dlg._tabs.currentWidget(), dlg._mode()) == (dlg._camera_tab, "urls")
    dlg._tabs.setCurrentWidget(dlg._search_tab)

    dlg._on_scan_done(IpcError(-32000, "Discovery is not available"))
    assert panel.title.text() == "Search failed"
    assert panel.detail.text() == "Discovery is not available"
    assert dlg._search_status.property("tone") == "error"

    dlg._on_scan_done(
        {"scan_id": "s2", "status": "complete", "scope": "x", "candidates": [CANDIDATE]}
    )
    assert dlg._results_stack.currentWidget() is dlg._results_table
    assert dlg._search_status.isVisible() and dlg._use_row.isVisible()


def test_add_camera_dialog_has_no_desktop_border(qtbot: object) -> None:
    from PySide6.QtCore import Qt

    from neurowatch.widgets.window_chrome import ChromeTitleBar

    dlg = camera_dialog.AddEditCameraDialog(ipc_endpoint=ENDPOINT)
    qtbot.addWidget(dlg)  # type: ignore[attr-defined]
    assert dlg.windowFlags() & Qt.WindowType.FramelessWindowHint
    bar = dlg.findChild(ChromeTitleBar)
    assert bar is not None and bar._title.text() == "Add camera"
    assert dlg.windowTitle() == "Add Camera"  # still named for the taskbar
