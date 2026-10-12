"""A themed question in the app's dialog style, centred on the screen.

For windows with no parent to centre on, such as the tray's confirmations: the
frame, header and buttons of the app's dialogs (``install_dialog_chrome``),
centred on the screen under the mouse.

Where the app may place its windows (X11, Windows) the prompt is moved to
the centre. Wayland ignores that (GNOME puts new windows top-left), so there
the prompt is a transparent maximized window with the card centred and a
mask that lets clicks outside the card reach the desktop, as the welcome
screen does.
"""

from __future__ import annotations

from PySide6.QtCore import QEvent, QObject, Qt
from PySide6.QtGui import QCursor, QGuiApplication, QKeyEvent, QRegion, QResizeEvent
from PySide6.QtWidgets import (
    QDialog,
    QFrame,
    QHBoxLayout,
    QLabel,
    QPushButton,
    QVBoxLayout,
    QWidget,
)

from neurowatch.theme import apply_dialog_theme, set_button_variant, set_label_role
from neurowatch.widgets.window_chrome import (
    SHADOW_MARGIN,
    ChromeTitleBar,
    _ShadowShell,
    positions_windows,
    window_effects_supported,
)

#: Width of the card's text (the card grows only in height).
_TEXT_WIDTH = 380


def _wrapped_label(text: str, parent: QWidget) -> QLabel:
    """A label of the prompt's text width, as tall as its wrapped text: the
    card is centred by alignment, which sizes it from size hints, and a
    wrapping label's hint alone would clip its last line."""
    label = QLabel(text, parent)
    label.setWordWrap(True)
    label.setFixedWidth(_TEXT_WIDTH)
    label.ensurePolished()  # the theme's font decides the wrapped height
    label.setFixedHeight(max(label.heightForWidth(_TEXT_WIDTH), label.sizeHint().height()))
    return label


class _NoDrag(QObject):
    """The prompt's header does not move it: on Wayland its window is the
    whole screen, and dragging that would unmaximize it."""

    def eventFilter(self, _watched: QObject, event: QEvent) -> bool:  # noqa: N802
        return event.type() in (
            QEvent.Type.MouseButtonPress,
            QEvent.Type.MouseButtonDblClick,
            QEvent.Type.MouseMove,
        )


class CenteredPrompt(QDialog):
    """``add_button`` the choices, then ``choose()`` returns the one clicked
    (its key), or None when closed or cancelled with Escape."""

    def __init__(self, title: str, text: str, detail: str = "") -> None:
        super().__init__(None)
        self._fill_screen = not positions_windows()
        effects = window_effects_supported()
        self.setWindowTitle(title)
        self.setWindowFlag(Qt.WindowType.FramelessWindowHint, True)
        self.setWindowFlag(Qt.WindowType.WindowStaysOnTopHint, True)
        if effects or self._fill_screen:
            self.setAttribute(Qt.WidgetAttribute.WA_TranslucentBackground, True)
        self.setProperty("chromeEffects", effects or self._fill_screen)
        self.setAccessibleName(title)
        apply_dialog_theme(self)
        self._chosen: str | None = None
        self._buttons: dict[str, QPushButton] = {}

        outer = QVBoxLayout(self)
        outer.setContentsMargins(0, 0, 0, 0)
        margin = SHADOW_MARGIN if effects else 0
        self._shell = _ShadowShell(self, margin)
        outer.addWidget(self._shell, alignment=Qt.AlignmentFlag.AlignCenter)
        shell_layout = QVBoxLayout(self._shell)
        shell_layout.setContentsMargins(margin, margin, margin, margin)
        frame = QFrame(self._shell)
        frame.setObjectName("chromeFrame")
        frame.setProperty("rounded", effects)
        shell_layout.addWidget(frame)
        inner = QVBoxLayout(frame)
        inner.setContentsMargins(1, 1, 1, 1)
        inner.setSpacing(0)
        self._header = ChromeTitleBar(title, frame, variant="dialog")
        self._no_drag = _NoDrag(self)
        self._header.installEventFilter(self._no_drag)
        inner.addWidget(self._header)

        body = QWidget(frame)
        inner.addWidget(body)
        content = QVBoxLayout(body)
        content.setContentsMargins(20, 2, 20, 16)
        content.setSpacing(8)
        self.text_label = _wrapped_label(text, body)
        content.addWidget(self.text_label)
        self.detail_label = _wrapped_label(detail, body)
        self.detail_label.setTextInteractionFlags(Qt.TextInteractionFlag.TextSelectableByMouse)
        set_label_role(self.detail_label, "hint")
        self.detail_label.setVisible(bool(detail))
        content.addWidget(self.detail_label)
        content.addSpacing(6)
        self._row = QHBoxLayout()
        self._row.setSpacing(8)
        self._row.addStretch(1)
        content.addLayout(self._row)

    def add_button(self, key: str, label: str, variant: str = "secondary") -> QPushButton:
        """A choice; variants are the app's (primary, secondary, danger)."""
        button = QPushButton(label, self)
        set_button_variant(button, variant)
        button.setAutoDefault(False)
        button.clicked.connect(lambda: self._choose(key))
        self._row.addWidget(button)
        self._buttons[key] = button
        return button

    def set_default(self, key: str) -> None:
        button = self._buttons[key]
        button.setDefault(True)
        button.setFocus()

    def button(self, key: str) -> QPushButton:
        return self._buttons[key]

    def choose(self) -> str | None:
        """Show centred and wait for a choice."""
        self._chosen = None
        self._present()
        self.exec()
        return self._chosen

    def _choose(self, key: str) -> None:
        self._chosen = key
        self.accept()

    def _present(self) -> None:
        screen = QGuiApplication.screenAt(QCursor.pos()) or QGuiApplication.primaryScreen()
        if screen is not None:
            self.setScreen(screen)
        if self._fill_screen:
            if screen is not None:
                self.setGeometry(screen.availableGeometry())
            self.setWindowState(Qt.WindowState.WindowMaximized)
            return
        self.adjustSize()
        if screen is not None:
            frame = self.frameGeometry()
            frame.moveCenter(screen.availableGeometry().center())
            self.move(frame.topLeft())

    def keyPressEvent(self, event: QKeyEvent) -> None:
        if event.key() == Qt.Key.Key_Escape:
            self.reject()
            return
        super().keyPressEvent(event)

    def resizeEvent(self, event: QResizeEvent) -> None:
        super().resizeEvent(event)
        if self._fill_screen:
            # Clicks outside the card reach whatever is underneath.
            self.setMask(QRegion(self._shell.geometry()))
