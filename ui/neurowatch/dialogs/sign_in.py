"""Sign in to a recorder that requires it (local accounts, ``auth.login``).

Shown before the main window when ``auth.status`` says sign-in is on. The
password check runs on the recorder (deliberately slow), so it is asked on a
worker thread. A successful sign-in sets this app's session
(``ipc.auth.set_session``): every request carries it from then on.
"""

from __future__ import annotations

from collections.abc import Callable
from typing import Any

from PySide6.QtCore import Signal
from PySide6.QtWidgets import QFormLayout, QLabel, QLineEdit, QWidget

from neurowatch.ipc import auth
from neurowatch.service import probe
from neurowatch.theme import set_status_tone
from neurowatch.utils.threads import Worker
from neurowatch.widgets.centered_prompt import CenteredPrompt

Ask = Callable[[str, dict[str, Any]], Any]


def _ask(endpoint: str | None) -> Ask:
    def ask(method: str, params: dict[str, Any]) -> Any:
        # The probe answers None for errors too; ask the client for the reason.
        from neurowatch.ipc.sync_client import SyncIpcClient

        try:
            return SyncIpcClient(endpoint=endpoint).call(method, params)
        except Exception as exc:
            return exc

    return ask


def sign_in_required(endpoint: str | None = None) -> bool:
    """Whether the recorder requires sign-in and this app has no session."""
    status = probe.request("auth.status", {"token": auth.current_session()}, endpoint=endpoint)
    return isinstance(status, dict) and bool(status.get("enabled")) and status.get("user") is None


class _SignIn(Worker):
    done = Signal(object)

    def __init__(self, ask: Ask, username: str, password: str) -> None:
        super().__init__()
        self._ask, self._username, self._password = ask, username, password

    def run(self) -> None:
        self.done.emit(
            self._ask("auth.login", {"username": self._username, "password": self._password})
        )


class SignInPrompt(CenteredPrompt):
    """User name and password; ``choose()`` returns "sign_in" once signed in."""

    def __init__(self, endpoint: str | None = None, ask: Ask | None = None, note: str = "") -> None:
        super().__init__(
            "Sign in",
            "Sign in to NeuroWatch",
            note or "This recorder asks who is using it. Your account decides what you can do.",
        )
        self._ask = ask or _ask(endpoint)
        self._worker: _SignIn | None = None
        fields = QWidget(self)
        form = QFormLayout(fields)
        form.setContentsMargins(0, 6, 0, 0)
        self.username = QLineEdit()
        self.username.setAccessibleName("User name")
        self.username.setMaxLength(32)
        self.password = QLineEdit()
        self.password.setAccessibleName("Password")
        self.password.setEchoMode(QLineEdit.EchoMode.Password)
        self.password.returnPressed.connect(lambda: self._choose("sign_in"))
        self.username.returnPressed.connect(self.password.setFocus)
        form.addRow("User name", self.username)
        form.addRow("Password", self.password)
        self.error = QLabel()
        self.error.setWordWrap(True)
        set_status_tone(self.error, "error")
        self.error.hide()
        form.addRow(self.error)
        self.add_content(fields)
        self.add_button("quit", "Quit")
        self.sign_in = self.add_button("sign_in", "Sign In", "primary")
        self.sign_in.setDefault(True)
        self.username.setFocus()

    def _choose(self, key: str) -> None:
        if key != "sign_in":
            super()._choose(key)
            return
        if self._worker is not None:
            return
        if not self.username.text().strip() or not self.password.text():
            self._show_error("Enter your user name and password.")
            return
        self.sign_in.setEnabled(False)
        self.sign_in.setText("Signing in…")
        worker = _SignIn(self._ask, self.username.text().strip(), self.password.text())
        self._worker = worker
        worker.done.connect(self._signed_in)
        worker.finished.connect(worker.deleteLater)
        worker.start()

    def _show_error(self, text: str) -> None:
        self.error.setText(text)
        self.error.show()
        self.adjustSize()

    def _signed_in(self, result: object) -> None:
        self._worker = None
        self.sign_in.setEnabled(True)
        self.sign_in.setText("Sign In")
        if isinstance(result, dict) and isinstance(result.get("token"), str):
            auth.set_session(
                result["token"],
                result.get("user") if isinstance(result.get("user"), dict) else None,
            )
            super()._choose("sign_in")
            return
        message = str(result) if isinstance(result, Exception) else "The recorder did not answer."
        self.password.clear()
        self.password.setFocus()
        self._show_error(message[:1].upper() + message[1:])

    def done(self, result: int) -> None:
        if self._worker is not None and self._worker.isRunning():
            self._worker.wait(10_000)
        super().done(result)


def sign_out(endpoint: str | None = None) -> None:
    token = auth.current_session()
    if token:
        probe.request("auth.logout", {"token": token}, endpoint=endpoint)
    auth.set_session(None)
