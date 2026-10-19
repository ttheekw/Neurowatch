"""Dialogs centred where windows cannot be placed (Wayland), and themed messages."""

from __future__ import annotations

import re
from pathlib import Path

import pytest
from PySide6.QtCore import QEvent, QPoint, QPointF, QSize, Qt, QTimer
from PySide6.QtGui import QMouseEvent
from PySide6.QtWidgets import QApplication, QDialog, QLabel, QMainWindow, QWidget

from neurowatch.dialogs import message
from neurowatch.dialogs.message import MessageDialog, ask, show_message
from neurowatch.widgets import window_chrome
from neurowatch.widgets.window_chrome import (
    CardOverlay,
    ChromeTitleBar,
    card_overlay,
    install_dialog_chrome,
    resize_window,
)

ROOT = Path(__file__).resolve().parents[2]


@pytest.fixture
def wayland(monkeypatch: pytest.MonkeyPatch) -> None:
    """As on Wayland: the application cannot place its windows."""
    monkeypatch.setattr(window_chrome, "positions_windows", lambda: False)


@pytest.fixture
def parent(qtbot) -> QMainWindow:
    window = QMainWindow()
    window.resize(800, 600)
    qtbot.addWidget(window)
    window.show()
    return window


def _shell(dialog: QDialog) -> QWidget:
    overlay = card_overlay(dialog)
    assert overlay is not None
    return overlay._shell


def _chrome_dialog(parent: QWidget, *, resizable: bool = False) -> QDialog:
    dialog = QDialog(parent)
    content = install_dialog_chrome(dialog, "Test", resizable=resizable)
    content.addWidget(QLabel("Some content for the card"))
    return dialog


def _mouse(widget: QWidget, kind: QEvent.Type, pos: QPoint, buttons: Qt.MouseButton) -> None:
    local = QPointF(pos)
    event = QMouseEvent(
        kind,
        local,
        QPointF(widget.mapToGlobal(pos)),
        Qt.MouseButton.LeftButton,
        buttons,
        Qt.KeyboardModifier.NoModifier,
    )
    QApplication.sendEvent(widget, event)


def test_on_wayland_a_dialog_covers_the_screen_with_its_card_centred(qtbot, wayland, parent):
    dialog = _chrome_dialog(parent)
    qtbot.addWidget(dialog)
    dialog.show()
    qtbot.waitUntil(lambda: dialog.isVisible())
    screen = parent.screen().availableGeometry()
    assert dialog.testAttribute(Qt.WidgetAttribute.WA_TranslucentBackground)
    assert dialog.windowState() & Qt.WindowState.WindowMaximized
    assert dialog.size() == screen.size()
    shell = _shell(dialog)
    card = shell.geometry()
    assert card.size() == shell.sizeHint(), "the card keeps its content's size"
    assert abs(card.center().x() - dialog.rect().center().x()) <= 1
    assert abs(card.center().y() - dialog.rect().center().y()) <= 1
    # Only the card takes clicks; the rest reaches the windows below.
    assert dialog.mask().boundingRect() == card
    # Its size is the screen's: never "restored" by the size guard.
    assert dialog.property("nwSizeGuarded") is True
    assert not dialog.findChildren(window_chrome.WindowSizeGuard)


def test_every_themed_dialog_gets_the_overlay_on_wayland(qtbot, wayland, parent):
    from neurowatch.dialogs.camera_dialog import AddEditCameraDialog
    from neurowatch.dialogs.settings_dialog import SettingsDialog

    for dialog in (
        SettingsDialog({}, parent),
        AddEditCameraDialog(parent=parent),
        MessageDialog(parent, "Remove Camera", "Remove 'Door'?"),
    ):
        qtbot.addWidget(dialog)
        assert card_overlay(dialog) is not None, type(dialog).__name__


def test_the_header_drags_the_card_and_it_stays_on_the_screen(qtbot, wayland, parent):
    dialog = _chrome_dialog(parent)
    qtbot.addWidget(dialog)
    dialog.show()
    shell = _shell(dialog)
    start = shell.geometry()
    header = dialog.findChild(ChromeTitleBar)
    assert header is not None
    _mouse(header, QEvent.Type.MouseButtonPress, QPoint(30, 10), Qt.MouseButton.LeftButton)
    _mouse(header, QEvent.Type.MouseMove, QPoint(130, 60), Qt.MouseButton.LeftButton)
    _mouse(header, QEvent.Type.MouseButtonRelease, QPoint(130, 60), Qt.MouseButton.NoButton)
    assert shell.geometry() == start.translated(100, 50)
    assert dialog.windowState() & Qt.WindowState.WindowMaximized, "the window did not move"
    card_overlay(dialog).move_card(QPoint(100_000, 100_000))  # type: ignore[union-attr]
    assert dialog.rect().contains(shell.geometry()), "the header stays reachable"
    assert dialog.mask().boundingRect() == shell.geometry()


def test_a_card_that_grows_stays_centred(qtbot, wayland, parent):
    dialog = _chrome_dialog(parent)
    qtbot.addWidget(dialog)
    dialog.show()
    shell = _shell(dialog)
    before = shell.size()
    content = next(
        label
        for label in dialog.findChildren(QLabel)
        if label.text() == "Some content for the card"
    )
    body = content.parentWidget()
    more = QLabel("Another line\nand another\nand a third")
    body.layout().addWidget(more)  # type: ignore[union-attr]
    more.show()
    qtbot.waitUntil(lambda: shell.height() > before.height())
    assert abs(shell.geometry().center().y() - dialog.rect().center().y()) <= 1
    assert dialog.mask().boundingRect() == shell.geometry()


def test_a_resizable_card_takes_sizes_from_code_and_from_its_edges(qtbot, wayland, parent):
    dialog = _chrome_dialog(parent, resizable=True)
    qtbot.addWidget(dialog)
    resize_window(dialog, QSize(400, 300))
    dialog.show()
    shell = _shell(dialog)
    assert shell.size() == QSize(400, 300)
    left = shell.geometry().left()
    edge = QPoint(shell.width() - 2, shell.height() // 2)
    _mouse(shell, QEvent.Type.MouseButtonPress, edge, Qt.MouseButton.LeftButton)
    _mouse(shell, QEvent.Type.MouseMove, edge + QPoint(60, 0), Qt.MouseButton.LeftButton)
    _mouse(shell, QEvent.Type.MouseButtonRelease, edge + QPoint(60, 0), Qt.MouseButton.NoButton)
    assert shell.size() == QSize(460, 300)
    assert shell.geometry().left() == left, "the opposite edge stays put"
    resize_window(dialog, QSize(10, 10))
    assert shell.width() >= shell.minimumSizeHint().width(), "never below its content"


def test_where_windows_can_be_placed_dialogs_stay_normal_windows(qtbot, monkeypatch, parent):
    monkeypatch.setattr(window_chrome, "positions_windows", lambda: True)
    dialog = _chrome_dialog(parent)
    qtbot.addWidget(dialog)
    dialog.show()
    assert card_overlay(dialog) is None
    assert not dialog.windowState() & Qt.WindowState.WindowMaximized
    assert dialog.size() != parent.screen().availableGeometry().size()


# ── Themed messages ──────────────────────────────────────────────────────


@pytest.fixture
def real_exec(monkeypatch: pytest.MonkeyPatch) -> None:
    """These tests answer the dialog themselves (conftest refuses others)."""
    monkeypatch.setattr(MessageDialog, "exec", QDialog.exec)


def _answer(action) -> None:
    """Run ``action`` on the message once it is open."""

    def act() -> None:
        dialog = QApplication.activeModalWidget()
        assert isinstance(dialog, MessageDialog)
        action(dialog)

    QTimer.singleShot(50, act)


def _button(dialog: MessageDialog, text: str):
    from PySide6.QtWidgets import QPushButton

    return next(b for b in dialog.findChildren(QPushButton) if b.text() == text)


def test_messages_use_the_themed_frame(qtbot, parent, real_exec):
    seen: list[MessageDialog] = []

    def ok(dialog: MessageDialog) -> None:
        seen.append(dialog)
        assert dialog.windowFlags() & Qt.WindowType.FramelessWindowHint
        assert dialog.findChild(ChromeTitleBar) is not None
        assert dialog.text_label.text() == "Lost connection"
        assert dialog.kind == "error"
        _button(dialog, "OK").click()

    _answer(ok)
    show_message(parent, "Backend Offline", "Lost connection", kind="error")
    assert seen


def test_ask_is_yes_only_when_yes_is_pressed(qtbot, parent, real_exec):
    _answer(lambda d: _button(d, "Reload").click())
    assert ask(parent, "Changed", "Reload?", yes_text="Reload", no_text="Not now") is True
    _answer(lambda d: _button(d, "Not now").click())
    assert ask(parent, "Changed", "Reload?", yes_text="Reload", no_text="Not now") is False
    _answer(lambda d: d.reject())  # Escape or the close button
    assert ask(parent, "Changed", "Reload?") is False


def test_destructive_questions_default_to_no(qtbot, parent, real_exec):
    def check(dialog: MessageDialog) -> None:
        assert dialog.default_button() is _button(dialog, "Cancel")
        assert _button(dialog, "Replace").property("variant") == "danger"
        dialog.default_button().click()  # Enter

    _answer(check)
    assert (
        ask(parent, "Export", "Replace?", yes_text="Replace", no_text="Cancel", destructive=True)
        is False
    )


def test_long_messages_wrap_to_their_full_height(qtbot):
    dialog = MessageDialog(None, "Recording Drive", "word " * 200)
    qtbot.addWidget(dialog)
    label = dialog.text_label
    assert label.width() == message.TEXT_WIDTH
    assert label.height() >= label.heightForWidth(message.TEXT_WIDTH)


def test_an_unexpected_message_fails_the_test_instead_of_hanging(qtbot):
    with pytest.raises(AssertionError, match="unexpected modal message 'Surprise'"):
        show_message(None, "Surprise", "nobody stubbed this")


def test_the_app_no_longer_uses_plain_message_boxes():
    uses = [
        f"{path.relative_to(ROOT)}:{number}"
        for path in (ROOT / "ui" / "neurowatch").rglob("*.py")
        if path.name not in {"theme.py", "message.py"}
        for number, line in enumerate(path.read_text().splitlines(), 1)
        if re.search(r"\bQMessageBox\b", line)
    ]
    assert uses == [], "use neurowatch.dialogs.message (show_message, ask)"


def test_card_overlay_is_a_direct_child_of_its_dialog(qtbot, wayland, parent):
    dialog = _chrome_dialog(parent)
    qtbot.addWidget(dialog)
    overlays = dialog.findChildren(CardOverlay)
    assert len(overlays) == 1 and overlays[0].parent() is dialog


def test_chrome_filters_ignore_events_once_their_python_side_is_gone(qtbot, wayland, parent):
    """At exit Python clears objects while Qt still delivers events (hide,
    resize) to dialogs it has not destroyed yet; the filters must not raise
    ("'CardOverlay' object has no attribute '_dialog'")."""
    dialog = _chrome_dialog(parent, resizable=True)
    qtbot.addWidget(dialog)
    dialog.show()
    overlay = card_overlay(dialog)
    assert overlay is not None
    resizers = dialog.findChildren(window_chrome.EdgeResizer)
    # As during interpreter shutdown: the attributes are already gone.
    del overlay._dialog
    for resizer in resizers:
        del resizer._surface
    QApplication.sendEvent(dialog, QEvent(QEvent.Type.Resize))
    QApplication.sendEvent(_shell(dialog), QEvent(QEvent.Type.Resize))
    dialog.hide()


def test_dialogs_opened_from_the_window_are_deleted_after_use(qtbot, monkeypatch):
    """A dialog shown with exec() and left as the window's child stays alive,
    hidden, until exit (and gets events there)."""
    from PySide6.QtCore import QCoreApplication

    from neurowatch import main
    from neurowatch.dialogs.about_dialog import AboutDialog
    from neurowatch.dialogs.custom_grid_dialog import CustomGridDialog

    monkeypatch.setattr(AboutDialog, "exec", lambda self: self.done(0) or 0)
    monkeypatch.setattr(CustomGridDialog, "exec", lambda self: self.done(0) or 0)
    window = QWidget()
    qtbot.addWidget(window)
    window._config = {"wall": {}}  # type: ignore[attr-defined]
    main.MainWindow._on_about(window)  # type: ignore[arg-type]
    main.MainWindow._on_custom_grid(window)  # type: ignore[arg-type]
    QCoreApplication.sendPostedEvents(None, QEvent.Type.DeferredDelete)
    assert window.findChildren(QDialog) == []


@pytest.mark.parametrize("on_wayland", [False, True])
def test_a_dialog_opens_with_focus_in_its_content_not_on_the_header(
    qtbot, monkeypatch, parent, on_wayland
):
    """The header's close button turns red when focused: a dialog opening
    there looked like it was about to close."""
    from PySide6.QtWidgets import QLineEdit, QPushButton

    monkeypatch.setattr(window_chrome, "positions_windows", lambda: not on_wayland)
    for content in (QLabel("No field, one button"), QLineEdit()):
        dialog = QDialog(parent)
        layout = install_dialog_chrome(dialog, "Focus")
        layout.addWidget(content)
        done = QPushButton("Done")
        layout.addWidget(done)
        qtbot.addWidget(dialog)
        dialog.show()
        qtbot.waitUntil(lambda d=dialog: d.focusWidget() is not None)
        QApplication.processEvents()
        expected = content if isinstance(content, QLineEdit) else done
        assert dialog.focusWidget() is expected
        dialog.hide()


@pytest.mark.parametrize("on_wayland", [True, False])
def test_the_resize_cursor_clears_once_the_pointer_is_over_the_content(
    qtbot, monkeypatch, parent, on_wayland
) -> None:
    """Children inherit the edge band's cursor and the surface gets no Leave
    when the pointer moves on into them: the two-sided arrow stayed over the
    whole dialog (Alert notifications, Users & permissions)."""
    monkeypatch.setattr(window_chrome, "positions_windows", lambda: not on_wayland)
    dialog = _chrome_dialog(parent, resizable=True)
    qtbot.addWidget(dialog)
    dialog.show()
    qtbot.waitUntil(lambda: dialog.isVisible())
    if on_wayland:
        surface = _shell(dialog)
    else:
        resizer = dialog.findChild(window_chrome.EdgeResizer)
        assert resizer is not None
        surface = resizer._surface
    content = surface.findChild(QLabel)
    assert content is not None and surface.isAncestorOf(content)

    _mouse(
        surface, QEvent.Type.MouseMove, QPoint(1, surface.height() // 2), Qt.MouseButton.NoButton
    )
    assert surface.testAttribute(Qt.WidgetAttribute.WA_SetCursor), "the edge shows the arrow"
    assert surface.cursor().shape() == Qt.CursorShape.SizeHorCursor

    QApplication.sendEvent(content, QEvent(QEvent.Type.Enter))
    assert not surface.testAttribute(Qt.WidgetAttribute.WA_SetCursor), "the content's own cursor"
