"""Frameless chrome: rounded, shadowed dialogs only where windows composite."""

from __future__ import annotations

import pytest
from PySide6.QtCore import QEvent, QObject, QSize, Qt
from PySide6.QtWidgets import QDialog, QFrame

from neurowatch.theme import apply_dialog_theme
from neurowatch.widgets import window_chrome
from neurowatch.widgets.window_chrome import (
    SHADOW_MARGIN,
    ChromeTitleBar,
    install_dialog_chrome,
    window_effects_supported,
)


def _dialog(qtbot) -> QDialog:
    dlg = QDialog()
    qtbot.addWidget(dlg)
    apply_dialog_theme(dlg)
    install_dialog_chrome(dlg, "Heading")
    return dlg


def _frame(dlg: QDialog) -> QFrame:
    frame = dlg.findChild(QFrame, "chromeFrame")
    assert frame is not None
    return frame


def test_composited_desktops_get_rounded_shadowed_dialogs(qtbot, monkeypatch) -> None:
    monkeypatch.setattr(window_chrome, "window_effects_supported", lambda: True)
    dlg = _dialog(qtbot)
    assert dlg.testAttribute(Qt.WidgetAttribute.WA_TranslucentBackground)
    assert dlg.property("chromeEffects") is True
    assert _frame(dlg).property("rounded") is True
    shell = _frame(dlg).parentWidget()
    assert shell is not None and shell.layout().contentsMargins().left() == SHADOW_MARGIN
    header = dlg.findChild(ChromeTitleBar)
    assert header is not None and header.property("variant") == "dialog"
    assert header._title.text() == "Heading"


def test_without_compositing_dialogs_stay_opaque_and_square(qtbot, monkeypatch) -> None:
    monkeypatch.setattr(window_chrome, "window_effects_supported", lambda: False)
    dlg = _dialog(qtbot)
    assert dlg.windowFlags() & Qt.WindowType.FramelessWindowHint
    assert not dlg.testAttribute(Qt.WidgetAttribute.WA_TranslucentBackground)
    assert _frame(dlg).property("rounded") is False
    shell = _frame(dlg).parentWidget()
    assert shell is not None and shell.layout().contentsMargins().left() == 0


def test_dialogs_keep_their_content_size(qtbot, monkeypatch) -> None:
    """The compositor cannot stretch a dialog (GNOME did on focus loss)."""
    monkeypatch.setattr(window_chrome, "window_effects_supported", lambda: False)
    dlg = QDialog()
    qtbot.addWidget(dlg)
    dlg.setMinimumWidth(460)
    install_dialog_chrome(dlg, "Heading")
    dlg.show()
    qtbot.waitExposed(dlg)
    size = dlg.size()
    assert size.width() == 460
    dlg.resize(size.width(), size.height() + 500)
    assert dlg.size() == size
    assert dlg.maximumSize() == size


def test_dialogs_return_to_their_size_when_the_window_system_changes_it(qtbot, monkeypatch) -> None:
    """GNOME (Wayland) doubled Settings when it opened a child dialog; Qt
    applies a compositor's size past the window's maximum."""
    monkeypatch.setattr(window_chrome, "window_effects_supported", lambda: False)
    dlg = QDialog()
    qtbot.addWidget(dlg)
    dlg.setMinimumWidth(460)
    install_dialog_chrome(dlg, "Heading")
    dlg.show()
    qtbot.waitExposed(dlg)
    size = dlg.size()
    handle = dlg.windowHandle()
    assert handle is not None

    grown: list[tuple[int, int]] = []
    dlg.installEventFilter(_ResizeLog(dlg, grown))
    handle.resize(size.width() * 2, size.height() * 2)  # as a compositor configure does
    qtbot.waitUntil(lambda: bool(grown) and dlg.size() == size == handle.size(), timeout=1000)
    assert grown[0] == (size.width() * 2, size.height() * 2)


class _ResizeLog(QObject):
    def __init__(self, parent: QObject, sizes: list[tuple[int, int]]) -> None:
        super().__init__(parent)
        self._sizes = sizes

    def eventFilter(self, watched: QObject, event: QEvent) -> bool:  # noqa: N802
        if event.type() == QEvent.Type.Resize:
            self._sizes.append(watched.size().toTuple())  # type: ignore[attr-defined]
        return False


@pytest.mark.parametrize(
    ("platform", "expected"),
    [("wayland", True), ("wayland-egl", True), ("offscreen", False), ("minimal", False)],
)
def test_platforms_that_composite(monkeypatch, platform: str, expected: bool) -> None:
    window_chrome._platform_composites.cache_clear()
    monkeypatch.setattr(window_chrome.QGuiApplication, "platformName", lambda: platform)
    monkeypatch.delenv("NW_WINDOW_EFFECTS", raising=False)
    assert window_effects_supported() is expected
    monkeypatch.setenv("NW_WINDOW_EFFECTS", "0")
    assert window_effects_supported() is False
    window_chrome._platform_composites.cache_clear()


def test_x11_uses_the_compositing_manager_check(monkeypatch) -> None:
    window_chrome._platform_composites.cache_clear()
    monkeypatch.setattr(window_chrome.QGuiApplication, "platformName", lambda: "xcb")
    monkeypatch.delenv("NW_WINDOW_EFFECTS", raising=False)
    monkeypatch.setattr(window_chrome, "_x11_compositor_running", lambda: False)
    assert window_effects_supported() is False
    window_chrome._platform_composites.cache_clear()
    monkeypatch.setattr(window_chrome, "_x11_compositor_running", lambda: True)
    assert window_effects_supported() is True
    window_chrome._platform_composites.cache_clear()


def _shown(qtbot, widget):
    qtbot.addWidget(widget)
    widget.show()
    qtbot.waitExposed(widget)
    return widget


def test_a_resizable_window_undoes_a_doubled_size_only(qtbot) -> None:
    """GNOME doubled frameless windows when a dialog opened over them; sizes
    the user drags to are kept."""
    from PySide6.QtWidgets import QWidget

    win = _shown(qtbot, QWidget())
    window_chrome.guard_window_size(win)
    win.resize(700, 400)
    qtbot.waitUntil(lambda: win.size().toTuple() == (700, 400), timeout=1000)
    handle = win.windowHandle()
    assert handle is not None

    handle.resize(1400, 800)  # as GNOME's configure does
    qtbot.waitUntil(
        lambda: win.size().toTuple() == (700, 400) == handle.size().toTuple(), timeout=1000
    )

    win.resize(900, 500)  # the user resizing: kept
    qtbot.wait(50)
    assert win.size().toTuple() == (900, 500)


def test_windows_are_guarded_once_when_they_take_focus(qtbot) -> None:
    from PySide6.QtWidgets import QMessageBox

    box = _shown(qtbot, QMessageBox(QMessageBox.Icon.Question, "Title", "Text"))
    window_chrome.guard_window_of(box.windowHandle())
    assert box.property("nwSizeGuarded") is True
    guards = box.findChildren(window_chrome.WindowSizeGuard)
    window_chrome.guard_window_of(box.windowHandle())
    assert box.findChildren(window_chrome.WindowSizeGuard) == guards and len(guards) == 1
    window_chrome.guard_window_of(None)  # no window has focus


@pytest.mark.parametrize("effects", [True, False])
def test_resizable_dialogs_resize_from_their_edges(qtbot, monkeypatch, effects) -> None:
    from PySide6.QtCore import QPoint
    from PySide6.QtWidgets import QLabel

    monkeypatch.setattr(window_chrome, "window_effects_supported", lambda: effects)
    dlg = QDialog()
    qtbot.addWidget(dlg)
    content = install_dialog_chrome(dlg, "Heading", resizable=True)
    content.addWidget(QLabel("body"), stretch=1)
    dlg.show()
    qtbot.waitExposed(dlg)

    assert dlg.minimumSize() != dlg.maximumSize(), "not fixed"
    before = dlg.size()
    window_chrome.resize_window(dlg, QSize(before.width() + 300, before.height() + 200))
    qtbot.waitUntil(lambda: dlg.width() == before.width() + 300)
    assert dlg.height() == before.height() + 200, "the size guard leaves it alone"

    resizer = dlg.findChild(window_chrome.EdgeResizer)
    assert resizer is not None
    band = SHADOW_MARGIN if effects else window_chrome.GRIP_WIDTH
    shell = _frame(dlg).parentWidget()
    assert shell is not None and shell.layout().contentsMargins().left() == band
    right = dlg.width() - 1
    assert resizer.edges_at(QPoint(right, 200)) == Qt.Edge.RightEdge
    assert resizer.edges_at(QPoint(right, dlg.height() - 1)) == (
        Qt.Edge.RightEdge | Qt.Edge.BottomEdge
    )
    assert resizer.edges_at(QPoint(band - 1, 200)) == Qt.Edge.LeftEdge
    assert resizer.edges_at(QPoint(band + 10, 200)) == Qt.Edge(0), "inside the frame"


def test_fixed_dialogs_have_no_resize_grips(qtbot, monkeypatch) -> None:
    monkeypatch.setattr(window_chrome, "window_effects_supported", lambda: False)
    dlg = _dialog(qtbot)
    assert dlg.findChild(window_chrome.EdgeResizer) is None


def _resizable(qtbot, monkeypatch) -> QDialog:
    from PySide6.QtWidgets import QLabel

    monkeypatch.setattr(window_chrome, "window_effects_supported", lambda: False)
    dlg = QDialog()
    qtbot.addWidget(dlg)
    content = install_dialog_chrome(dlg, "Heading", resizable=True)
    content.addWidget(QLabel("body"), stretch=1)
    window_chrome.resize_window(dlg, QSize(700, 500))
    dlg.show()
    qtbot.waitExposed(dlg)
    qtbot.waitUntil(lambda: dlg.size() == QSize(700, 500))
    # The resize asked for above is over.
    dlg.setProperty("nwResizeAllowedUntil", 0.0)
    return dlg


@pytest.mark.parametrize("forced", [QSize(1400, 1000), QSize(1400, 940), QSize(900, 520)])
def test_resizable_dialogs_undo_sizes_nobody_asked_for(qtbot, monkeypatch, forced) -> None:
    """GNOME on mixed-scale Wayland doubles a frameless window when a popup
    opens over it; one taller than the screen comes back clamped, not double."""
    dlg = _resizable(qtbot, monkeypatch)
    dlg.resize(forced)  # as the window system would
    qtbot.waitUntil(lambda: dlg.size() == QSize(700, 500))


def test_resizable_dialogs_take_the_sizes_the_user_drags_to(qtbot, monkeypatch) -> None:
    dlg = _resizable(qtbot, monkeypatch)
    window_chrome.allow_resize(dlg)  # the edge press starts a resize
    dlg.resize(QSize(820, 560))
    qtbot.wait(50)
    assert dlg.size() == QSize(820, 560)
    dlg.resize(QSize(900, 600))  # the drag goes on
    qtbot.wait(50)
    assert dlg.size() == QSize(900, 600)


def test_a_size_while_a_popup_opens_is_undone_even_during_a_resize(qtbot, monkeypatch) -> None:
    from PySide6.QtWidgets import QApplication, QWidget

    dlg = _resizable(qtbot, monkeypatch)
    window_chrome.allow_resize(dlg)
    popup = QWidget()
    monkeypatch.setattr(QApplication, "activePopupWidget", staticmethod(lambda: popup))
    dlg.resize(QSize(1400, 1000))
    qtbot.waitUntil(lambda: dlg.size() == QSize(700, 500))


def test_resizable_dialogs_are_not_maximized_behind_their_back(qtbot, monkeypatch) -> None:
    dlg = _resizable(qtbot, monkeypatch)
    dlg.showMaximized()  # GNOME auto-maximizes windows near the screen's size
    qtbot.waitUntil(lambda: not dlg.isMaximized())
    qtbot.waitUntil(lambda: dlg.size() == QSize(700, 500))


def test_an_edge_press_allows_the_drag_that_follows(qtbot, monkeypatch) -> None:
    from PySide6.QtCore import QPoint, QPointF
    from PySide6.QtGui import QMouseEvent

    dlg = _resizable(qtbot, monkeypatch)
    shell = _frame(dlg).parentWidget()
    assert shell is not None
    handle = dlg.windowHandle()
    assert handle is not None
    started: list[object] = []
    monkeypatch.setattr(handle, "startSystemResize", lambda edges: started.append(edges) or True)
    pos = QPointF(dlg.width() - 1, 200)
    press = QMouseEvent(
        QEvent.Type.MouseButtonPress,
        pos,
        shell.mapToGlobal(QPoint(int(pos.x()), int(pos.y()))).toPointF(),
        Qt.MouseButton.LeftButton,
        Qt.MouseButton.LeftButton,
        Qt.KeyboardModifier.NoModifier,
    )
    _send(shell, press)
    assert started == [Qt.Edge.RightEdge]
    dlg.resize(QSize(800, 500))
    qtbot.wait(50)
    assert dlg.size() == QSize(800, 500)


def _send(widget: object, event: object) -> None:
    from PySide6.QtWidgets import QApplication

    QApplication.sendEvent(widget, event)
