"""Opening the desktop app: the background service, the tray, one window.

The backend is a background service (``neurowatch.service``) that outlives
the UI. Opening the app makes sure it runs (starting it, and by default
registering it to start at login), starts the tray icon if it is not
running, and brings an already open window to the front instead of opening
a second one.

``NW_SERVICE=external`` leaves the backend alone: something else runs it
(tests, or a backend started by hand), so nothing is started or registered.

In development (a source checkout) the service lives with the app instead:
nothing is registered to start at login, no tray is started, and closing the
app stops the service (dev supervisor and backend). ``NW_DEV_KEEP_SERVICE=1``
keeps the installed behaviour, to try the tray or background recording.
"""

from __future__ import annotations

import logging
import os
import subprocess
from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

from PySide6.QtCore import QEventLoop, QObject, QThread, Signal
from PySide6.QtNetwork import QLocalServer, QLocalSocket
from PySide6.QtWidgets import QApplication

from neurowatch.service import probe
from neurowatch.service.lock import InstanceLock
from neurowatch.service.manager import DESKTOP, ServiceController, ServiceError, running_mode
from neurowatch.service.paths import IS_WINDOWS, dev_mode, lock_path, log_path, python_command
from neurowatch.utils.threads import Worker

logger = logging.getLogger(__name__)

_ACTIVATE = b"activate\n"


#: Commands of ``neurowatch.headless`` the launcher passes on as they are.
CORE_COMMANDS = ("events", "cloud")


@dataclass(frozen=True)
class LaunchOptions:
    """How ``neurowatch-ui`` was asked to start."""

    mode: str = "ui"  # "ui" | "tray" | "service" | "headless" | "command"
    autostart: bool = True  # register the service (and tray) to start at login
    rest: tuple[str, ...] = ()  # service or command arguments / Qt arguments
    manage_service: bool = True  # False with NW_SERVICE=external


def parse_launch_args(argv: list[str]) -> LaunchOptions:
    """``neurowatch-ui [--no-autostart] | --tray | --headless ... | service ... | COMMAND ...``.

    ``COMMAND`` is ``events`` or ``cloud``; its arguments go to
    ``neurowatch.headless`` as they are.
    """
    args = argv[1:]
    if args and args[0] == "service":
        return LaunchOptions(mode="service", rest=tuple(args[1:]))
    if args and args[0] in CORE_COMMANDS:
        return LaunchOptions(mode="command", rest=tuple(args))
    if "--headless" in args:
        return LaunchOptions(mode="headless", rest=tuple(a for a in args if a != "--headless"))
    if "--tray" in args:
        return LaunchOptions(mode="tray", rest=tuple(a for a in args if a != "--tray"))
    return LaunchOptions(
        autostart="--no-autostart" not in args,
        rest=tuple(a for a in args if a != "--no-autostart"),
        manage_service=os.environ.get("NW_SERVICE") != "external",
    )


# ---------------------------------------------------------------------------
# The background service
# ---------------------------------------------------------------------------


class _ServiceStart(Worker):
    """``ServiceController.ensure_running`` (desktop mode) off the GUI thread."""

    def __init__(self, controller: ServiceController, autostart: bool) -> None:
        super().__init__()
        self._controller = controller
        self._autostart = autostart
        self.result: tuple[bool, str] = (False, "the service start was interrupted")

    def run(self) -> None:
        try:
            running = self._controller.ensure_running(autostart=self._autostart, mode=DESKTOP)
            self.result = (running, "")
        except Exception as error:
            logger.exception("Starting the NeuroWatch service failed")
            self.result = (False, str(error))


def _qt_sleep(seconds: float) -> None:
    QThread.msleep(max(1, int(seconds * 1000)))


def service_follows_ui() -> bool:
    """Development: the service runs only while the app is open."""
    return dev_mode() and os.environ.get("NW_DEV_KEEP_SERVICE") != "1"


def stop_service(controller: ServiceController | None = None) -> bool:
    """Stop the service as the app closes (development); True once stopped.

    Called after the main loop has ended: the window is gone, so waiting for
    the backend to finish its recordings blocks nothing on screen.
    """
    service = controller or ServiceController(sleep=_qt_sleep)
    logger.info("Stopping the development service with the app")
    try:
        stopped = service.stop()
    except (OSError, ServiceError):
        logger.exception("Stopping the development service failed")
        return False
    if not stopped:
        logger.warning("The development service did not stop; ./neurowatch.sh service stop")
    return stopped


def ensure_service(
    progress: Callable[[str], None],
    *,
    autostart: bool,
    controller: ServiceController | None = None,
) -> tuple[bool, str]:
    """Start the service unless it runs; the GUI stays responsive meanwhile.

    Returns (running, error message). ``progress`` gets a line to show.
    """
    service = controller or ServiceController(sleep=_qt_sleep)
    status = probe.backend_status(service.endpoint, timeout=1.0)
    if status is None:
        progress(
            "Building and starting the backend — the first build takes a few minutes"
            if dev_mode()
            else "Starting the NeuroWatch background service"
        )
    elif running_mode(status) != DESKTOP:
        progress("Switching the NeuroWatch service from headless to desktop mode")
    worker = _ServiceStart(service, autostart)
    worker.start()
    while not worker.wait(30):
        QApplication.processEvents(QEventLoop.ProcessEventsFlag.AllEvents, 30)
    ok, error = worker.result
    if not ok and not error:
        error = f"It did not answer in time. Log: {log_path('service')}"
    return ok, error


def start_tray() -> bool:
    """Start the tray icon unless it already runs; True if started."""
    probe_lock = InstanceLock(lock_path("tray"))
    if not probe_lock.acquire():
        return False  # the tray holds it
    probe_lock.release()
    kwargs: dict[str, Any] = {
        "stdin": subprocess.DEVNULL,
        "stdout": subprocess.DEVNULL,
        "stderr": open(log_path("tray"), "ab"),  # the child keeps its own copy
        "close_fds": True,
    }
    if IS_WINDOWS:
        kwargs["creationflags"] = subprocess.DETACHED_PROCESS  # type: ignore[attr-defined]
    else:
        kwargs["start_new_session"] = True
    try:
        subprocess.Popen(python_command("neurowatch.tray", windowless=True), **kwargs)
    except OSError:
        logger.exception("Could not start the NeuroWatch tray")
        return False
    finally:
        kwargs["stderr"].close()
    return True


# ---------------------------------------------------------------------------
# One window per user
# ---------------------------------------------------------------------------


def instance_server_name() -> str:
    """Per-user name of the open app's activation channel."""
    override = os.environ.get("NW_UI_INSTANCE")
    if override:
        return override
    user = os.environ.get("USERNAME") or os.environ.get("USER") or str(os.getuid())
    if IS_WINDOWS:
        return f"neurowatch-ui-{user}"
    runtime = os.environ.get("XDG_RUNTIME_DIR")
    if runtime and os.path.isdir(runtime):
        return os.path.join(runtime, "neurowatch-ui.sock")
    return f"neurowatch-ui-{user}"


def activate_running_instance(name: str | None = None, timeout_ms: int = 500) -> bool:
    """Ask an open NeuroWatch window to come to the front; True if one did."""
    socket = QLocalSocket()
    socket.connectToServer(name or instance_server_name())
    if not socket.waitForConnected(timeout_ms):
        return False
    socket.write(_ACTIVATE)
    socket.flush()
    socket.waitForBytesWritten(timeout_ms)
    socket.disconnectFromServer()
    return True


class SingleInstance(QObject):
    """Listens for later launches and brings the window to the front."""

    activate_requested = Signal()

    def __init__(self, name: str | None = None, parent: QObject | None = None) -> None:
        super().__init__(parent)
        self._name = name or instance_server_name()
        self._server = QLocalServer(self)
        self._server.newConnection.connect(self._on_connection)

    def listen(self) -> bool:
        QLocalServer.removeServer(self._name)  # a crashed instance's leftover
        if not self._server.listen(self._name):
            logger.warning("Single-instance channel unavailable: %s", self._server.errorString())
            return False
        return True

    def close(self) -> None:
        self._server.close()

    def _on_connection(self) -> None:
        # The launcher writes its one line right after connecting; read it
        # here rather than keeping the socket (and a slot) alive.
        while self._server.hasPendingConnections():
            socket = self._server.nextPendingConnection()
            if socket.bytesAvailable() or socket.waitForReadyRead(250):
                message = bytes(socket.readAll().data())
            else:
                message = b""
            socket.abort()
            socket.deleteLater()
            if message.startswith(_ACTIVATE.strip()):
                self.activate_requested.emit()
