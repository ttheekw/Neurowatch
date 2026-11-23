"""The screen a widget is on, without ``QWidget.screen()``.

PySide records the ``QScreen`` an instance method returns as a child of the
object it was called on: the one wrapper of each screen moves to whichever
widget asked last. When Python frees that widget the screen's wrapper is
invalidated ("Internal C++ object (QScreen) already deleted"), and when the
garbage collector frees it the real screen is deleted and the app crashes
later in ``QScreen::virtualSiblings``. The static ``QGuiApplication`` lookups
used here give the screen no owner.
"""

from __future__ import annotations

from PySide6.QtGui import QGuiApplication, QScreen
from PySide6.QtWidgets import QWidget


def screen_of(widget: QWidget | None) -> QScreen | None:
    """The screen ``widget``'s window is on; for a window not shown yet, its
    nearest shown parent window's; else the primary screen.

    Found by the window's position. On Wayland, where clients cannot place
    windows, Qt puts each top-level window at its screen's origin
    (``QWaylandWindow::fixedToplevelPositions``), so the position still names
    the screen."""
    primary = QGuiApplication.primaryScreen()
    if widget is None or len(QGuiApplication.screens()) <= 1:
        return primary
    window = widget.window()
    while not window.isVisible() and (above := window.parentWidget()) is not None:
        window = above.window()
    if not window.isVisible():
        return primary
    frame = window.frameGeometry()
    return (
        QGuiApplication.screenAt(frame.center())
        or QGuiApplication.screenAt(frame.topLeft())
        or primary
    )
