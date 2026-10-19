"""Add Camera's Search tab: tick cameras, one login for all, one click to add."""

from __future__ import annotations

import base64
import threading

import pytest

from neurowatch.dialogs import camera_dialog
from neurowatch.ipc.sync_client import IpcError
from neurowatch.models import bulk_add
from neurowatch.models.bulk_add import BatchSummary, RowProgress

ENDPOINT = "unix:/tmp/neurowatch-bulk-test.sock"


def _candidate(cid: str, address: str, state: str, model: str = "DS-2CD2143G2-I") -> dict:
    return {
        "candidate_id": cid,
        "address": address,
        "state": state,
        "manufacturer": "HIKVISION",
        "model": model,
    }


def _profiles(cid: str) -> dict:
    return {
        "candidate": _candidate(cid, "x", "ready"),
        "streams": [
            {"stream_id": f"{cid}-main", "name": "main", "width": 2560, "height": 1440},
            {"stream_id": f"{cid}-sub", "name": "sub", "width": 640, "height": 360},
        ],
        "main_stream_id": f"{cid}-main",
        "sub_stream_id": f"{cid}-sub",
    }


class FakeCameras:
    """The backend's discovery calls: ``open`` cameras accept any login,
    the others only admin / ``password``."""

    def __init__(self, password: str = "secret", open_: tuple[str, ...] = ("c1",)) -> None:
        self.password = password
        self.open = set(open_)
        self.calls: list[tuple[str, dict]] = []
        self.lock = threading.Lock()

    def client(self, endpoint: str | None = None) -> FakeCameras.Client:
        return FakeCameras.Client(self)

    class Client:
        def __init__(self, backend: FakeCameras) -> None:
            self.backend = backend

        def call(self, method: str, params: dict | None = None) -> object:
            params = dict(params or {})
            backend = self.backend
            with backend.lock:
                backend.calls.append((method, params))
            cid = params.get("candidate_id", "")
            if method == "camera.discovery.profiles":
                if cid not in backend.open:
                    given = base64.b64decode(params.get("password_b64", "")).decode()
                    if params.get("username") != "admin" or given != backend.password:
                        raise IpcError(-32602, "The camera refused that username and password.")
                return _profiles(cid)
            if method == "camera.discovery.add":
                return {"id": f"cam-{cid}", "name": params["name"], "url": f"rtsp://{cid}"}
            raise AssertionError(method)


RESULTS = {
    "scan_id": "s1",
    "status": "complete",
    "scope": "192.168.1.0/24",
    "candidates": [
        _candidate("c1", "192.168.1.64", "ready"),
        _candidate("c2", "192.168.1.65", "credentials_needed"),
        {
            **_candidate("c3", "192.168.1.66", "already_added"),
            "matched_camera": {"id": "g", "name": "Gate"},
        },
    ],
}


@pytest.fixture
def backend(monkeypatch: pytest.MonkeyPatch) -> FakeCameras:
    fake = FakeCameras()
    monkeypatch.setattr(camera_dialog, "SyncIpcClient", fake.client)
    return fake


@pytest.fixture
def dialog(qtbot, backend: FakeCameras) -> camera_dialog.AddEditCameraDialog:
    dlg = camera_dialog.AddEditCameraDialog(ipc_endpoint=ENDPOINT, existing_names=["Gate"])
    qtbot.addWidget(dlg)
    dlg._on_scan_done(RESULTS)
    return dlg


def _wait_batch(qtbot, dlg) -> None:
    qtbot.waitUntil(lambda: dlg._batch_worker is None, timeout=5000)


def _row(dlg, cid: str) -> int:
    return next(row for row, c in dlg._rows() if c.candidate_id == cid)


def test_found_cameras_are_ticked_and_one_button_adds_them(dialog) -> None:
    from PySide6.QtCore import Qt

    ticked = [c.candidate_id for c in dialog._ticked_candidates()]
    assert ticked == ["c1", "c2"], "addable cameras are ticked; an added one is not"
    gate = dialog._results_table.item(_row(dialog, "c3"), 0)
    assert not gate.flags() & Qt.ItemFlag.ItemIsUserCheckable
    assert dialog._ok_btn.text() == "Add 2 Cameras" and dialog._ok_btn.isEnabled()
    dialog._results_table.item(_row(dialog, "c2"), 0).setCheckState(Qt.CheckState.Unchecked)
    assert dialog._ok_btn.text() == "Add 1 Camera"


def test_one_login_adds_every_camera_and_closes(qtbot, dialog, backend) -> None:
    dialog._bulk_user.setText("admin")
    dialog._bulk_pass.setText("secret")
    dialog._on_accept()  # the dialog's Add button
    _wait_batch(qtbot, dialog)
    assert dialog.result() == dialog.DialogCode.Accepted, "all added: the dialog closes"
    added = {camera["id"] for camera in dialog.added_cameras()}
    assert added == {"cam-c1", "cam-c2"}
    adds = [params for method, params in backend.calls if method == "camera.discovery.add"]
    assert {(a["main_stream_id"], a["sub_stream_id"]) for a in adds} == {
        ("c1-main", "c1-sub"),
        ("c2-main", "c2-sub"),
    }, "the backend's stream picks are used: nothing to discover by hand"
    assert all(a["record"] is True for a in adds)
    # Same model twice: each name carries its address.
    assert {a["name"] for a in adds} == {
        "HIKVISION DS-2CD2143G2-I (192.168.1.64)",
        "HIKVISION DS-2CD2143G2-I (192.168.1.65)",
    }


def test_a_camera_with_another_password_stays_for_a_second_try(qtbot, dialog, backend) -> None:
    from PySide6.QtCore import Qt

    dialog._bulk_user.setText("admin")
    dialog._bulk_pass.setText("wrong")
    dialog._on_accept()
    _wait_batch(qtbot, dialog)
    assert dialog.result() != dialog.DialogCode.Accepted, "not everything was added"
    assert dialog._results_table.item(_row(dialog, "c1"), 2).text() == "Added ✓"
    assert dialog._results_table.item(_row(dialog, "c2"), 2).text() == "Wrong username or password"
    c1 = dialog._results_table.item(_row(dialog, "c1"), 0)
    assert not c1.flags() & Qt.ItemFlag.ItemIsUserCheckable, "added: cannot be added twice"
    assert dialog._ok_btn.text() == "Add 1 Camera"
    assert "refused" in dialog._search_status.text()
    assert dialog._search_status.property("tone") == "warning"

    dialog._bulk_pass.setText("secret")
    dialog._on_accept()
    _wait_batch(qtbot, dialog)
    assert dialog.result() == dialog.DialogCode.Accepted
    assert [c["id"] for c in dialog.added_cameras()] == ["cam-c1", "cam-c2"]


def test_check_login_signs_in_without_adding(qtbot, dialog, backend) -> None:
    dialog._bulk_user.setText("admin")
    dialog._bulk_pass.setText("secret")
    dialog._check_btn.click()
    _wait_batch(qtbot, dialog)
    assert not [m for m, _ in backend.calls if m == "camera.discovery.add"]
    assert dialog._results_table.item(_row(dialog, "c2"), 2).text() == "Login works · 2560×1440"
    assert dialog._search_status.property("tone") == "success"
    assert dialog.added_cameras() == []


def test_the_main_window_takes_the_cameras_the_dialog_added(monkeypatch) -> None:
    from types import SimpleNamespace

    from PySide6.QtCore import QObject, Signal

    import neurowatch.main as main_mod

    class Dialog(QObject):
        DialogCode = camera_dialog.AddEditCameraDialog.DialogCode
        finished = Signal(int)  # the window deletes its dialogs after use

        def __init__(self, **kwargs) -> None:
            super().__init__()
            assert kwargs["existing_names"] == ["Gate"]

        def exec(self):
            return self.DialogCode.Rejected  # closed after adding some

        def added_cameras(self):
            return [{"id": "cam-c1", "name": "Door", "url": "rtsp://c1"}]

        def get_config(self):
            raise AssertionError("nothing more to add")

    monkeypatch.setattr(main_mod, "AddEditCameraDialog", Dialog)
    results: list[str] = []
    fake = SimpleNamespace(
        _cameras=[SimpleNamespace(name="Gate")],
        _ipc_endpoint=lambda: ENDPOINT,
        _on_add_camera_result=lambda result, submitted: results.append(submitted.id),
    )
    main_mod.MainWindow._on_add_camera(fake)  # type: ignore[arg-type]
    assert results == ["cam-c1"]


# ── Plain logic ──────────────────────────────────────────────────────────


def test_names_carry_the_address_only_when_needed() -> None:
    rows = [
        ("a", "Front Door", "10.0.0.1"),
        ("b", "Lobby", "10.0.0.2"),
        ("c", "Lobby", "10.0.0.3"),
        ("d", "Gate", "10.0.0.4"),
        ("e", "ONVIF device", "10.0.0.5"),
    ]
    names = bulk_add.camera_names(rows, taken={"gate"})
    assert names == {
        "a": "Front Door",
        "b": "Lobby (10.0.0.2)",
        "c": "Lobby (10.0.0.3)",
        "d": "Gate (10.0.0.4)",
        "e": "ONVIF device (10.0.0.5)",
    }


def test_errors_and_summaries_read_plainly() -> None:
    assert bulk_add.phase_for_error("The camera refused that username and password.") == (
        bulk_add.LOGIN_FAILED
    )
    assert bulk_add.phase_for_error("The camera asks for a username and password.") == (
        bulk_add.LOGIN_FAILED
    )
    assert bulk_add.phase_for_error("connection timed out") == bulk_add.FAILED
    progress = {
        "a": RowProgress(bulk_add.ADDED),
        "b": RowProgress(bulk_add.LOGIN_FAILED),
        "c": RowProgress(bulk_add.NO_STREAMS),
    }
    text, tone = BatchSummary.of(progress, ["a", "b", "c"]).message(3, adding=True)
    assert text.startswith("Added 1 of 3.") and "refused" in text and "could not" in text
    assert tone == "warning"
    assert BatchSummary.of({"a": RowProgress(bulk_add.ADDED)}, ["a"]).message(1, True) == (
        "Added 1 camera.",
        "success",
    )
    assert bulk_add.stream_detail(_profiles("x")) == "2560×1440"
