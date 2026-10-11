"""Subprocess probe: an OS signal arrives while the pre-exec startup error popup is open.

Runs the real ``main()`` with a backend that is reachable but fails the camera
load, so the deferred startup-load-error QMessageBox runs its nested event loop
before ``app.exec()``. The signal is sent from inside that popup.
"""
import os
import signal
import sys

from PySide6.QtCore import QTimer
from PySide6.QtWidgets import QApplication

import neurowatch.main as ui
from neurowatch.ipc.sync_client import IpcError

SIGNAL = getattr(signal, sys.argv[1])


class Client:
    _endpoint = "unix:/nonexistent/startup-signal-test.sock"

    def is_online(self):
        return True

    def call(self, method, params=None):
        raise IpcError(-32000, "config unavailable")


real_warning = ui.QMessageBox.warning


def warning(*args, **kwargs):
    print("POPUP OPEN", flush=True)
    assert QApplication.instance().thread().loopLevel() == 0, "popup must precede app.exec()"
    QTimer.singleShot(200, lambda: os.kill(os.getpid(), SIGNAL))
    result = real_warning(*args, **kwargs)
    print("POPUP RETURNED", flush=True)
    return result


real_exec = QApplication.exec


def app_exec():
    print("APP EXEC", flush=True)
    return real_exec()


real_shutdown = ui.MainWindow.shutdown


def shutdown(self):
    if not self._shutting_down:
        print("WINDOW SHUTDOWN", flush=True)
    real_shutdown(self)


ui.SyncIpcClient = Client
ui.QMessageBox.warning = staticmethod(warning)
ui.MainWindow.shutdown = shutdown
QApplication.exec = staticmethod(app_exec)
try:
    ui.main()
except SystemExit as exc:
    print(f"EXIT {exc.code}", flush=True)
    raise
