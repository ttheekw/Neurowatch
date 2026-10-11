"""Subprocess probe: actual OS signals and Qt threads, no backend or user config writes."""
import os
import signal
import sys

from PySide6.QtCore import QTimer
from PySide6.QtWidgets import QApplication

import neurowatch.main as ui


class Client:
    _endpoint = "unix:/nonexistent/shutdown-test.sock"

    def is_online(self):
        return True

    def call(self, method, params=None):
        if method == "config.get":
            return {"wall": {"rows": 4, "cols": 4}}
        if method == "camera.list":
            return [{"id": f"shutdown-{i}", "name": str(i), "url": "/missing.avi"}
                    for i in range(16)]
        return {}


ui.SyncIpcClient = Client
app = QApplication([])
with ui._shutdown_signals(app):
    window = ui.MainWindow()
    readers = list(window._wall._readers.values())
    assert len(readers) == 16
    app.aboutToQuit.connect(window.shutdown)
    window.show()

    def finish():
        if sys.argv[1] == "quit":
            app.quit()
        else:
            os.kill(os.getpid(), getattr(signal, sys.argv[1]))

    QTimer.singleShot(100, finish)
    app.exec()
    assert all(not reader.isRunning() for reader in readers)
    assert not window._poll_worker.isRunning()
    assert not window._wall._readers
    print("JOINED 16 READERS")
