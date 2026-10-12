"""Subprocess probe: Add Camera closes while its backend call still runs.

Before ``utils.threads.retire`` the dialog dropped the running worker and the
whole app aborted ("QThread: Destroyed while thread is still running").
"""
import gc
import sys

from PySide6.QtCore import QThread, QTimer
from PySide6.QtWidgets import QApplication, QMainWindow

app = QApplication(sys.argv)
from neurowatch.dialogs import camera_dialog  # noqa: E402
from neurowatch.utils import threads  # noqa: E402


def slow_run(self):
    QThread.msleep(3000)  # longer than the dialog's 2 s wait


camera_dialog._RpcWorker.run = slow_run
main = QMainWindow()
main.show()


def step():
    dialog = camera_dialog.AddEditCameraDialog(parent=main, ipc_endpoint="unix:/nonexistent.sock")
    dialog.show()
    app.processEvents()
    dialog.reject()
    del dialog
    gc.collect()
    print(f"RETIRED {threads.retired_count()}", flush=True)
    QTimer.singleShot(2000, lambda: (print(f"LEFT {threads.retired_count()}", flush=True), app.quit()))


QTimer.singleShot(0, step)
app.exec()
print("EXIT OK", flush=True)
