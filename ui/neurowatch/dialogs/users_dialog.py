"""Users & Permissions: the recorder's local accounts (``accounts.*``) and
whether it requires sign-in; and changing your own password.

Changes apply at once on the recorder (each on a worker thread): there is
nothing to save at the end.
"""

from __future__ import annotations

from typing import Any

from PySide6.QtCore import Signal
from PySide6.QtWidgets import (
    QButtonGroup,
    QCheckBox,
    QComboBox,
    QDialog,
    QDialogButtonBox,
    QFormLayout,
    QGridLayout,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QListWidget,
    QPushButton,
    QRadioButton,
    QVBoxLayout,
    QWidget,
)

from neurowatch.ipc import auth
from neurowatch.ipc.sync_client import SyncIpcClient
from neurowatch.theme import (
    apply_dialog_theme,
    set_button_variant,
    set_label_role,
    set_status_tone,
    style_button_box,
)
from neurowatch.utils.threads import Worker
from neurowatch.widgets.window_chrome import install_dialog_chrome

ROLES: tuple[tuple[str, str, str], ...] = (
    ("viewer", "Viewer", "Watches live video and recordings and reviews events."),
    (
        "operator",
        "Operator",
        "Also steers PTZ cameras, adds bookmarks, exports clips and starts or stops recording.",
    ),
    ("admin", "Admin", "Also changes settings, cameras, alerts and accounts, on every camera."),
)
ROLE_NAMES = {key: name for key, name, _ in ROLES}
MIN_PASSWORD = 8


class _Call(Worker):
    done = Signal(object)

    def __init__(self, endpoint: str | None, method: str, params: dict[str, Any]) -> None:
        super().__init__()
        self._endpoint, self._method, self._params = endpoint, method, params

    def run(self) -> None:
        try:
            self.done.emit(SyncIpcClient(endpoint=self._endpoint).call(self._method, self._params))
        except Exception as exc:
            self.done.emit(exc)


def user_summary(user: dict[str, Any], cameras: dict[str, str]) -> str:
    role = ROLE_NAMES.get(str(user.get("role")), str(user.get("role")))
    allowed = user.get("cameras") or []
    where = ", ".join(cameras.get(c, c) for c in allowed) if allowed else "every camera"
    return f"{user.get('username', '')} · {role} · {where}"


def password_problem(password: str, confirm: str, required: bool) -> str | None:
    if not password and not required:
        return None
    if len(password) < MIN_PASSWORD:
        return f"Use a password of at least {MIN_PASSWORD} characters."
    if password != confirm:
        return "The two passwords differ."
    return None


class _Worker:
    """Runs calls one at a time for a dialog and keeps them alive."""

    def __init__(self, endpoint: str | None) -> None:
        self.endpoint = endpoint
        self.running: set[_Call] = set()

    def run(self, method: str, params: dict[str, Any], done: Any) -> None:
        call = _Call(self.endpoint, method, params)
        self.running.add(call)
        call.done.connect(done)
        call.finished.connect(lambda: self.running.discard(call))
        call.finished.connect(call.deleteLater)
        call.start()

    def wait(self) -> None:
        for call in list(self.running):
            call.wait(10_000)


class UserDialog(QDialog):
    """A new user, or an existing user's role, cameras and password."""

    def __init__(
        self,
        endpoint: str | None,
        cameras: list[tuple[str, str]],
        user: dict[str, Any] | None = None,
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        self._calls = _Worker(endpoint)
        self._new = user is None
        self.saved: dict[str, Any] | None = None
        self.setWindowTitle("User")
        self.setMinimumWidth(480)
        apply_dialog_theme(self)
        title = "Add user" if user is None else f"Edit {user['username']}"
        layout = install_dialog_chrome(self, title)
        layout.setSpacing(12)
        form = QFormLayout()
        form.setVerticalSpacing(10)
        self.username = QLineEdit("" if user is None else str(user["username"]))
        self.username.setMaxLength(32)
        self.username.setPlaceholderText("letters, digits, . _ -")
        self.username.setAccessibleName("User name")
        self.username.setReadOnly(not self._new)
        form.addRow("User name", self.username)
        self.role = QComboBox()
        self.role.setAccessibleName("Role")
        for key, name, _ in ROLES:
            self.role.addItem(name, key)
        self.role.setCurrentIndex(max(0, self.role.findData((user or {}).get("role", "viewer"))))
        form.addRow("Role", self.role)
        self.role_help = QLabel()
        self.role_help.setWordWrap(True)
        set_label_role(self.role_help, "hint")
        form.addRow("", self.role_help)
        layout.addLayout(form)

        self.cameras_box = QWidget()
        cameras_layout = QVBoxLayout(self.cameras_box)
        cameras_layout.setContentsMargins(0, 0, 0, 0)
        heading = QLabel("Cameras")
        set_label_role(heading, "sectionLabel")
        cameras_layout.addWidget(heading)
        self.all_cameras = QRadioButton("Every camera")
        self.some_cameras = QRadioButton("Only these cameras")
        group = QButtonGroup(self)
        group.addButton(self.all_cameras)
        group.addButton(self.some_cameras)
        cameras_layout.addWidget(self.all_cameras)
        cameras_layout.addWidget(self.some_cameras)
        grid = QGridLayout()
        grid.setContentsMargins(24, 0, 0, 0)
        allowed = (user or {}).get("cameras") or []
        self.camera_boxes: dict[str, QCheckBox] = {}
        for index, (camera_id, name) in enumerate(cameras):
            box = QCheckBox(name)
            box.setChecked(camera_id in allowed)
            self.camera_boxes[camera_id] = box
            grid.addWidget(box, index // 3, index % 3)
        cameras_layout.addLayout(grid)
        (self.some_cameras if allowed else self.all_cameras).setChecked(True)
        self.some_cameras.toggled.connect(self._sync)
        layout.addWidget(self.cameras_box)

        passwords = QFormLayout()
        self.password = QLineEdit()
        self.password.setEchoMode(QLineEdit.EchoMode.Password)
        self.password.setAccessibleName("Password")
        self.password.setPlaceholderText(
            f"at least {MIN_PASSWORD} characters"
            if self._new
            else "leave empty to keep the password"
        )
        self.confirm = QLineEdit()
        self.confirm.setEchoMode(QLineEdit.EchoMode.Password)
        self.confirm.setAccessibleName("Repeat password")
        passwords.addRow("Password", self.password)
        passwords.addRow("Repeat it", self.confirm)
        layout.addLayout(passwords)

        self.error = QLabel()
        self.error.setWordWrap(True)
        set_status_tone(self.error, "error")
        self.error.hide()
        layout.addWidget(self.error)
        buttons = QDialogButtonBox(
            QDialogButtonBox.StandardButton.Save | QDialogButtonBox.StandardButton.Cancel
        )
        self._save = buttons.button(QDialogButtonBox.StandardButton.Save)
        buttons.accepted.connect(self._submit)
        buttons.rejected.connect(self.reject)
        style_button_box(buttons)
        layout.addWidget(buttons)
        self.role.currentIndexChanged.connect(self._sync)
        self._sync()

    def _sync(self) -> None:
        role = self.role.currentData()
        self.role_help.setText(next(text for key, _, text in ROLES if key == role))
        # Admins manage every camera.
        self.cameras_box.setVisible(role != "admin")
        for box in self.camera_boxes.values():
            box.setEnabled(self.some_cameras.isChecked())

    def request(self) -> dict[str, Any] | str:
        """``accounts.set_user`` parameters, or what is wrong."""
        name = self.username.text().strip()
        if not name:
            return "Enter a user name."
        problem = password_problem(self.password.text(), self.confirm.text(), self._new)
        if problem:
            return problem
        role = str(self.role.currentData())
        cameras = (
            [cid for cid, box in self.camera_boxes.items() if box.isChecked()]
            if role != "admin" and self.some_cameras.isChecked()
            else []
        )
        if role != "admin" and self.some_cameras.isChecked() and not cameras:
            return "Choose at least one camera, or Every camera."
        params: dict[str, Any] = {"username": name, "role": role, "cameras": cameras}
        if self.password.text():
            params["password"] = self.password.text()
        return params

    def _submit(self) -> None:
        params = self.request()
        if isinstance(params, str):
            self._show(params)
            return
        self._save.setEnabled(False)
        self._calls.run("accounts.set_user", params, self._done)

    def _show(self, text: str) -> None:
        self.error.setText(text)
        self.error.show()

    def _done(self, result: object) -> None:
        self._save.setEnabled(True)
        if isinstance(result, dict):
            self.saved = result
            self.accept()
        else:
            self._show(f"Not saved: {result}")

    def done(self, result: int) -> None:
        self._calls.wait()
        super().done(result)


class UsersDialog(QDialog):
    def __init__(
        self, endpoint: str | None, cameras: list[tuple[str, str]], parent: QWidget | None = None
    ) -> None:
        super().__init__(parent)
        self._endpoint = endpoint
        self._cameras = cameras
        self._calls = _Worker(endpoint)
        self._users: list[dict[str, Any]] = []
        self._enabled = False
        #: Sign-in was turned on here while this app had no session.
        self.requires_sign_in_now = False
        self.setWindowTitle("Users & Permissions")
        self.setMinimumSize(560, 460)
        apply_dialog_theme(self)
        layout = install_dialog_chrome(self, "Users & permissions", resizable=True)
        layout.setSpacing(10)
        intro = QLabel(
            "Accounts decide who may use NeuroWatch on this recorder and what they may do. "
            "They protect the app and its connection; whoever controls this computer's "
            "user account can still change NeuroWatch's files."
        )
        intro.setWordWrap(True)
        set_label_role(intro, "hint")
        layout.addWidget(intro)
        self.require = QCheckBox("Require sign-in on this recorder")
        self.require.setAccessibleName("Require sign-in")
        self.require.clicked.connect(self._toggle_required)
        layout.addWidget(self.require)
        self.users = QListWidget()
        self.users.setAccessibleName("Users")
        self.users.itemDoubleClicked.connect(lambda _item: self._edit())
        self.users.currentRowChanged.connect(lambda _row: self._sync())
        layout.addWidget(self.users, 1)
        row = QHBoxLayout()
        self.add = QPushButton("Add User…")
        set_button_variant(self.add, "primary")
        self.add.clicked.connect(self._add)
        self.edit = QPushButton("Edit…")
        self.edit.clicked.connect(self._edit)
        self.remove = QPushButton("Remove")
        set_button_variant(self.remove, "ghost")
        self.remove.clicked.connect(self._remove)
        for button in (self.add, self.edit, self.remove):
            row.addWidget(button)
        row.addStretch(1)
        layout.addLayout(row)
        self.status = QLabel()
        self.status.setWordWrap(True)
        layout.addWidget(self.status)
        close = QDialogButtonBox(QDialogButtonBox.StandardButton.Close)
        close.rejected.connect(self.reject)
        style_button_box(close)
        layout.addWidget(close)
        self._calls.run("accounts.get", {}, self._loaded)
        self._sync()

    def _note(self, text: str, tone: str = "muted") -> None:
        self.status.setText(text)
        set_status_tone(self.status, tone)

    def _loaded(self, result: object) -> None:
        if not isinstance(result, dict):
            self._note(f"Could not read the accounts: {result}", "error")
            return
        self._users = [u for u in result.get("users", []) if isinstance(u, dict)]
        self._enabled = bool(result.get("enabled"))
        self.require.setChecked(self._enabled)
        names = dict(self._cameras)
        self.users.clear()
        for user in self._users:
            self.users.addItem(user_summary(user, names))
        if not self._users:
            self._note("Add an admin first; then turn on sign-in.")
        self._sync()

    def _sync(self) -> None:
        has = 0 <= self.users.currentRow() < len(self._users)
        self.edit.setEnabled(has)
        self.remove.setEnabled(has)
        has_admin = any(u.get("role") == "admin" for u in self._users)
        self.require.setEnabled(self._enabled or has_admin)
        self.require.setToolTip("" if has_admin else "Add an admin account first")

    def _toggle_required(self, on: bool) -> None:
        if on:
            from neurowatch.dialogs.confirm import confirm_action

            if not confirm_action(
                self,
                "Require sign-in",
                "Require sign-in on this recorder?",
                confirm_text="Require Sign-In",
                informative_text="Everyone using NeuroWatch here must sign in, and accounts "
                "limit what they can do. Make sure you know an admin password.",
            ):
                self.require.setChecked(False)
                return
        self.require.setEnabled(False)

        def done(result: object) -> None:
            if isinstance(result, dict):
                self._enabled = bool(result.get("enabled"))
                if self._enabled and auth.current_session() is None:
                    self.requires_sign_in_now = True
                self._note(
                    "Sign-in is required now." if self._enabled else "Sign-in is off.", "success"
                )
            else:
                self._note(f"Not changed: {result}", "error")
            self.require.setChecked(self._enabled)
            self._sync()

        self._calls.run("accounts.set_enabled", {"enabled": on}, done)

    def _add(self) -> None:
        dialog = UserDialog(self._endpoint, self._cameras, None, self)
        if dialog.exec() and dialog.saved is not None:
            self._loaded({**dialog.saved, "enabled": self._enabled})
            self._note("User added.", "success")
        dialog.deleteLater()

    def _edit(self) -> None:
        row = self.users.currentRow()
        if not 0 <= row < len(self._users):
            return
        dialog = UserDialog(self._endpoint, self._cameras, self._users[row], self)
        if dialog.exec() and dialog.saved is not None:
            self._loaded({**dialog.saved, "enabled": self._enabled})
            self._note("User saved.", "success")
        dialog.deleteLater()

    def _remove(self) -> None:
        row = self.users.currentRow()
        if not 0 <= row < len(self._users):
            return
        from neurowatch.dialogs.confirm import confirm_action

        name = str(self._users[row].get("username"))
        if not confirm_action(
            self,
            "Remove user",
            f"Remove {name}?",
            confirm_text="Remove User",
            destructive=True,
            informative_text="They are signed out at once.",
        ):
            return

        def done(result: object) -> None:
            if isinstance(result, dict):
                self._loaded({**result, "enabled": self._enabled})
                self._note(f"{name} removed.", "success")
            else:
                self._note(f"Not removed: {result}", "error")

        self._calls.run("accounts.remove_user", {"username": name}, done)

    def done(self, result: int) -> None:
        self._calls.wait()
        super().done(result)


class ChangePasswordDialog(QDialog):
    def __init__(self, endpoint: str | None, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self._calls = _Worker(endpoint)
        self.setWindowTitle("Change Password")
        self.setMinimumWidth(420)
        apply_dialog_theme(self)
        layout = install_dialog_chrome(self, "Change password")
        form = QFormLayout()
        self.current = QLineEdit()
        self.new = QLineEdit()
        self.confirm = QLineEdit()
        for edit, label in (
            (self.current, "Current"),
            (self.new, "New"),
            (self.confirm, "Repeat new"),
        ):
            edit.setEchoMode(QLineEdit.EchoMode.Password)
            edit.setAccessibleName(f"{label} password")
            form.addRow(label, edit)
        layout.addLayout(form)
        self.error = QLabel()
        self.error.setWordWrap(True)
        set_status_tone(self.error, "error")
        self.error.hide()
        layout.addWidget(self.error)
        buttons = QDialogButtonBox(
            QDialogButtonBox.StandardButton.Save | QDialogButtonBox.StandardButton.Cancel
        )
        self._save = buttons.button(QDialogButtonBox.StandardButton.Save)
        self._save.setText("Change Password")
        buttons.accepted.connect(self._submit)
        buttons.rejected.connect(self.reject)
        style_button_box(buttons)
        layout.addWidget(buttons)

    def _submit(self) -> None:
        problem = password_problem(self.new.text(), self.confirm.text(), True)
        if problem:
            self.error.setText(problem)
            self.error.show()
            return
        self._save.setEnabled(False)
        self._calls.run(
            "auth.change_password",
            {
                "token": auth.current_session() or "",
                "current": self.current.text(),
                "new": self.new.text(),
            },
            self._done,
        )

    def _done(self, result: object) -> None:
        self._save.setEnabled(True)
        if isinstance(result, dict):
            self.accept()
        else:
            self.error.setText(str(result))
            self.error.show()

    def done(self, result: int) -> None:
        self._calls.wait()
        super().done(result)
