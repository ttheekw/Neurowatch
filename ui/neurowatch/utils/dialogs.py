"""Showing a modal dialog so that Qt deletes it, never Python's garbage collector.

PySide's ``QDialog.exec()`` hands the dialog to Python (it removes the parent
relation, so the Python object owns the C++ dialog from then on). Our dialogs
refer to themselves (their chrome's event filters keep the dialog, slots are
bound methods), so a closed dialog is not freed when the caller lets go of it
but later, by the cycle collector, at whatever moment it next runs. Tearing a
Qt widget tree down from inside garbage collection reaches objects it already
freed: the desktop app crashed that way (2026-11-22, segfault "Garbage-
collecting" in ``QBoxLayout::~QBoxLayout``) just after the Plugins page's Bind
Camera dialog closed.

``run_dialog`` keeps the dialog reachable until Qt has deleted it from the
event loop (``deleteLater``), after the caller has read its values.
"""

from __future__ import annotations

from PySide6.QtWidgets import QDialog
from shiboken6 import Shiboken

#: Closed dialogs waiting for Qt to delete them, by id. Reachable, so the cycle
#: collector never frees them; an entry is dropped once its C++ dialog is gone.
_closed: dict[int, QDialog] = {}


def run_dialog(dialog: QDialog) -> int:
    """``dialog.exec()``; Qt deletes the dialog once control is back in the
    event loop. The dialog's values stay readable until then; do not keep it
    for later use."""
    for key, old in list(_closed.items()):
        if not Shiboken.isValid(old):
            del _closed[key]
    try:
        return dialog.exec()
    finally:
        # Only a Qt object has a C++ side to protect (tests pass stand-ins).
        if isinstance(dialog, Shiboken.Object):
            _closed[id(dialog)] = dialog
            dialog.deleteLater()


def closed_count() -> int:
    """Closed dialogs whose C++ object Qt has not deleted yet (for tests)."""
    return sum(1 for dialog in list(_closed.values()) if Shiboken.isValid(dialog))
