"""Closed dialogs are deleted by Qt, never by Python's garbage collector, and
finding a widget's screen leaves the screen without an owner.

PySide's ``QDialog.exec()`` hands the dialog to Python; a chrome dialog refers
to itself, so the cycle collector used to free it at any later moment, and
tearing the widget tree down from inside garbage collection crashed the app
(2026-11-22, after the Plugins page's Bind Camera dialog closed)."""

from __future__ import annotations

import gc
import re
from pathlib import Path

import pytest
from PySide6.QtCore import QPoint, QRect, QTimer
from PySide6.QtGui import QGuiApplication
from PySide6.QtWidgets import QDialog, QLabel, QLineEdit, QMainWindow, QVBoxLayout, QWidget
from shiboken6 import Shiboken

from neurowatch.utils import dialogs as dialog_lifetime
from neurowatch.utils import screens
from neurowatch.utils.dialogs import run_dialog
from neurowatch.utils.screens import screen_of
from neurowatch.widgets import window_chrome
from neurowatch.widgets.window_chrome import install_dialog_chrome

ROOT = Path(__file__).resolve().parents[2]


@pytest.fixture(params=["wayland", "positioned"])
def chrome_mode(request, monkeypatch: pytest.MonkeyPatch) -> str:
    """Both chrome kinds: the Wayland card overlay and a normal window."""
    if request.param == "wayland":
        monkeypatch.setattr(window_chrome, "positions_windows", lambda: False)
    return str(request.param)


@pytest.fixture
def parent(qtbot) -> QMainWindow:
    window = QMainWindow()
    window.resize(800, 600)
    qtbot.addWidget(window)
    window.show()
    return window


def _dialog(parent: QWidget) -> QDialog:
    dialog = QDialog(parent)
    content = install_dialog_chrome(dialog, "Test", resizable=True)
    form = QVBoxLayout()
    form.addWidget(QLabel("Name"))
    form.addWidget(QLineEdit())
    content.addLayout(form)
    return dialog


def _dialogs_in_garbage() -> list[str]:
    gc.set_debug(gc.DEBUG_SAVEALL)
    try:
        gc.collect()
        return [type(o).__name__ for o in gc.garbage if isinstance(o, QDialog)]
    finally:
        gc.garbage.clear()
        gc.set_debug(0)


def test_a_closed_dialog_waits_for_qt_and_never_reaches_the_garbage_collector(
    qtbot, chrome_mode, parent
) -> None:
    dialog = _dialog(parent)
    QTimer.singleShot(0, dialog.reject)
    assert run_dialog(dialog) == QDialog.DialogCode.Rejected
    assert dialog.findChild(QLineEdit) is not None, "the caller still reads its values"
    del dialog
    assert _dialogs_in_garbage() == [], "kept until Qt deletes it"
    qtbot.waitUntil(lambda: dialog_lifetime.closed_count() == 0, timeout=2000)
    assert parent.findChildren(QDialog) == [], "Qt deleted it"
    assert _dialogs_in_garbage() == []


def test_a_dialog_is_deleted_also_when_exec_raises(qtbot, parent, monkeypatch) -> None:
    dialog = _dialog(parent)

    def failing_exec() -> int:
        raise RuntimeError("boom")

    monkeypatch.setattr(dialog, "exec", failing_exec)
    with pytest.raises(RuntimeError):
        run_dialog(dialog)
    qtbot.waitUntil(lambda: not Shiboken.isValid(dialog), timeout=2000)


def test_finding_a_widgets_screen_leaves_the_screen_without_an_owner(qtbot) -> None:
    widget = QWidget()
    widget.show()
    found = screen_of(widget)
    assert found is QGuiApplication.primaryScreen()
    widget.deleteLater()
    del widget
    gc.collect()
    qtbot.wait(10)
    assert Shiboken.isValid(found), "PySide would have tied the screen to the widget"
    assert found.availableGeometry().isValid()


class _Screen:
    def __init__(self, name: str, rect: QRect) -> None:
        self.name, self.rect = name, rect


class _Screens:
    """Two screens side by side, looked up like ``QGuiApplication``'s."""

    left = _Screen("left", QRect(0, 0, 1920, 1080))
    right = _Screen("right", QRect(1920, 0, 2560, 1440))

    @classmethod
    def screens(cls) -> list[_Screen]:
        return [cls.left, cls.right]

    @classmethod
    def primaryScreen(cls) -> _Screen:  # noqa: N802
        return cls.left

    @classmethod
    def screenAt(cls, point: QPoint) -> _Screen | None:  # noqa: N802
        return next((s for s in cls.screens() if s.rect.contains(point)), None)


def test_with_two_screens_the_window_position_names_the_screen(qtbot, monkeypatch) -> None:
    monkeypatch.setattr(screens, "QGuiApplication", _Screens)
    window = QWidget()
    qtbot.addWidget(window)
    window.show()
    monkeypatch.setattr(window, "frameGeometry", lambda: QRect(2000, 100, 800, 600))
    child = QDialog(window)  # not shown yet: its parent window's screen
    assert screen_of(child) is _Screens.right
    monkeypatch.setattr(window, "frameGeometry", lambda: QRect(100, 100, 800, 600))
    assert screen_of(window) is _Screens.left
    monkeypatch.setattr(window, "frameGeometry", lambda: QRect(-5000, -5000, 10, 10))
    assert screen_of(window) is _Screens.left, "off every screen: the primary"
    assert screen_of(None) is _Screens.left


def _uses(pattern: str, allowed: str) -> list[str]:
    """Lines of the app's code matching ``pattern``, outside ``allowed`` and
    outside comments and docstrings that only mention it."""
    return [
        f"{path.relative_to(ROOT)}:{number}"
        for path in (ROOT / "ui" / "neurowatch").rglob("*.py")
        if path.relative_to(ROOT).as_posix() != allowed
        for number, line in enumerate(path.read_text().splitlines(), 1)
        if re.search(pattern, line) and not line.lstrip().startswith(("#", '"', "`"))
    ]


def test_dialogs_are_shown_with_run_dialog() -> None:
    # app.exec() runs the application, not a dialog.
    uses = _uses(r"(?<!app)\.exec\(\)", "ui/neurowatch/utils/dialogs.py")
    assert uses == [], "show dialogs with neurowatch.utils.dialogs.run_dialog"


def test_screens_are_found_with_screen_of() -> None:
    uses = _uses(r"\.screen\(\)", "ui/neurowatch/utils/screens.py")
    assert uses == [], "find a widget's screen with neurowatch.utils.screens.screen_of"
