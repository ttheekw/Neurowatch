from __future__ import annotations

import pytest


@pytest.fixture(autouse=True)
def _signed_out():
    from neurowatch.ipc import auth

    auth.set_session(None)
    yield
    auth.set_session(None)


def test_requests_carry_the_session_and_roles_grow():
    from neurowatch.ipc import auth

    request = {"jsonrpc": "2.0", "method": "camera.list"}
    assert "session" not in auth.with_auth(request, "unix:/tmp/x.sock")
    assert auth.may("admin"), "without sign-in the recorder decides"
    auth.set_session("tok", {"username": "ana", "role": "operator", "cameras": []})
    assert auth.with_auth(request, "unix:/tmp/x.sock")["session"] == "tok"
    assert auth.may("viewer") and auth.may("operator") and not auth.may("admin")
    auth.set_session(None, {"username": "ignored", "role": "admin"})
    assert auth.current_user() is None


def test_sign_in_asks_off_the_ui_thread_and_keeps_the_session(qtbot):
    from neurowatch.dialogs.sign_in import SignInPrompt
    from neurowatch.ipc import auth
    from neurowatch.ipc.sync_client import IpcError

    def ask(method, params):
        assert method == "auth.login"
        if params["password"] != "password-1":
            return IpcError(-32602, "wrong user name or password")
        return {"token": "t0k", "user": {"username": params["username"], "role": "viewer", "cameras": []}}

    prompt = SignInPrompt(ask=ask)
    qtbot.addWidget(prompt)
    prompt._choose("sign_in")
    assert prompt.error.text() == "Enter your user name and password."
    prompt.username.setText("guard")
    prompt.password.setText("nope")
    prompt._choose("sign_in")
    qtbot.waitUntil(lambda: prompt.error.text().startswith("Wrong"))
    assert prompt.password.text() == "" and auth.current_session() is None
    prompt.password.setText("password-1")
    with qtbot.waitSignal(prompt.accepted):
        prompt._choose("sign_in")
    assert prompt._chosen == "sign_in"
    assert auth.current_session() == "t0k" and auth.current_user()["role"] == "viewer"


def test_sign_in_is_asked_only_when_the_recorder_requires_it(monkeypatch):
    from neurowatch.dialogs import sign_in

    answers = {"value": {"enabled": True, "user": None}}
    monkeypatch.setattr(sign_in.probe, "request", lambda method, params, endpoint=None: answers["value"])
    assert sign_in.sign_in_required()
    answers["value"] = {"enabled": True, "user": {"username": "ana"}}
    assert not sign_in.sign_in_required()
    answers["value"] = {"enabled": False, "user": None}
    assert not sign_in.sign_in_required()
    answers["value"] = None  # an older recorder without accounts
    assert not sign_in.sign_in_required()


def test_menus_offer_only_what_the_account_may_do(qtbot):
    from neurowatch.ipc import auth
    from neurowatch.models.camera import CameraConfig
    from neurowatch.widgets.cell_widget import CellWidget
    from neurowatch.widgets.title_bar import TitleBar
    from neurowatch.widgets.toolbar import ToolBar

    auth.set_session("t", {"username": "guard", "role": "viewer", "cameras": []})
    bar = TitleBar()
    qtbot.addWidget(bar)
    labels = [a.text() for a in bar._build_file_menu().actions() if not a.isSeparator()]
    assert labels == ["Diagnostics", "Change Password…", "Sign Out (guard)", "About Neurowatch"]
    bar.apply_role()
    assert bar._tab_btns["settings"].isHidden()
    cell = CellWidget()
    qtbot.addWidget(cell)
    cell.set_camera(CameraConfig(id="cam", name="Gate", url="rtsp://offline/fixture"))
    texts = [a.text() for a in cell._build_context_menu().actions()]
    assert not {"Edit Camera", "Remove Camera", "PTZ Controls…", "Add Bookmark…", "Talk…"} & set(texts)
    toolbar = ToolBar()
    qtbot.addWidget(toolbar)
    toolbar.apply_role()
    assert toolbar._add_btn.isHidden() and toolbar._ptz_btn.isHidden()
    auth.set_session("t", {"username": "op", "role": "operator", "cameras": []})
    texts = [a.text() for a in cell._build_context_menu().actions()]
    assert "PTZ Controls…" in texts and "Edit Camera" not in texts
    labels = [a.text() for a in bar._build_file_menu().actions() if not a.isSeparator()]
    assert "Export Video…" in labels and "Settings" not in labels


def test_user_editor_checks_names_passwords_and_cameras(qtbot):
    from neurowatch.dialogs.users_dialog import UserDialog

    dialog = UserDialog(None, [("cam-1", "Gate"), ("cam-2", "Yard")])
    qtbot.addWidget(dialog)
    assert dialog.request() == "Enter a user name."
    dialog.username.setText("guard")
    dialog.password.setText("short")
    assert "at least 8" in dialog.request()
    dialog.password.setText("password-1")
    dialog.confirm.setText("password-2")
    assert dialog.request() == "The two passwords differ."
    dialog.confirm.setText("password-1")
    dialog.some_cameras.setChecked(True)
    assert "camera" in dialog.request()
    dialog.camera_boxes["cam-2"].setChecked(True)
    assert dialog.request() == {"username": "guard", "role": "viewer", "cameras": ["cam-2"],
                                "password": "password-1"}
    dialog.role.setCurrentIndex(dialog.role.findData("admin"))
    assert dialog.cameras_box.isHidden() and dialog.request()["cameras"] == []
    edit = UserDialog(None, [], {"username": "ana", "role": "operator", "cameras": []})
    qtbot.addWidget(edit)
    assert edit.username.isReadOnly()
    assert edit.request() == {"username": "ana", "role": "operator", "cameras": []}, "password kept"


def test_users_dialog_lists_accounts_and_needs_an_admin_before_sign_in(qtbot, monkeypatch):
    from neurowatch.dialogs import users_dialog

    reply = {"enabled": False, "users": [{"username": "guard", "role": "viewer", "cameras": ["cam-1"]}]}

    class Client:
        def __init__(self, endpoint):
            pass

        def call(self, method, params):
            return reply

    monkeypatch.setattr(users_dialog, "SyncIpcClient", Client)
    dialog = users_dialog.UsersDialog(None, [("cam-1", "Gate")])
    qtbot.addWidget(dialog)
    qtbot.waitUntil(lambda: dialog.users.count() == 1)
    assert dialog.users.item(0).text() == "guard · Viewer · Gate"
    assert not dialog.require.isEnabled(), "no admin yet"
    reply["users"].append({"username": "root", "role": "admin", "cameras": []})
    dialog._loaded(reply)
    assert dialog.require.isEnabled()
