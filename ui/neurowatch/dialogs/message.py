"""Messages and questions in the app's dialog style.

Replaces ``QMessageBox``, which shows the desktop's own window frame: these
use the themed frame and header (``install_dialog_chrome``), so on Wayland
they are centred like every other dialog (``CardOverlay``).
"""

from __future__ import annotations

from PySide6.QtCore import Qt
from PySide6.QtGui import QKeyEvent
from PySide6.QtWidgets import QDialog, QHBoxLayout, QLabel, QPushButton, QWidget

from neurowatch.theme import apply_dialog_theme, set_button_variant, set_label_role
from neurowatch.widgets.window_chrome import install_dialog_chrome

#: Width of a message's text; it wraps, and the card grows in height only.
TEXT_WIDTH = 420

#: ``kind`` of a message: shown in the accessible description.
KINDS = ("info", "warning", "error", "question")


def _wrapped(text: str, parent: QWidget) -> QLabel:
    """A wrapping label as tall as its text: a fixed-size card is sized from
    size hints, and a wrapping label's hint alone would clip its last line."""
    label = QLabel(text, parent)
    label.setWordWrap(True)
    label.setTextFormat(Qt.TextFormat.PlainText)
    label.setFixedWidth(TEXT_WIDTH)
    label.ensurePolished()  # the theme's font decides the wrapped height
    label.setFixedHeight(max(label.heightForWidth(TEXT_WIDTH), label.sizeHint().height()))
    return label


class MessageDialog(QDialog):
    """A heading, a message, an optional detail and buttons (``add_button``).

    ``exec()`` returns Accepted for an accepting button; ``clicked_button()``
    is the button pressed (None when closed or dismissed with Escape).
    """

    def __init__(
        self,
        parent: QWidget | None,
        title: str,
        text: str,
        *,
        detail: str = "",
        kind: str = "info",
    ) -> None:
        super().__init__(parent)
        self.setWindowTitle(title)
        apply_dialog_theme(self)
        self.kind = kind if kind in KINDS else "info"
        self._clicked: QPushButton | None = None
        self._escape: QPushButton | None = None
        layout = install_dialog_chrome(self, title)
        self.setAccessibleDescription(f"{self.kind}: {text}")

        self.text_label = _wrapped(text, self)
        self.text_label.setObjectName("messageText")
        self.text_label.setTextInteractionFlags(Qt.TextInteractionFlag.TextSelectableByMouse)
        layout.addWidget(self.text_label)
        self.detail_label = _wrapped(detail, self)
        set_label_role(self.detail_label, "hint")
        self.detail_label.setTextInteractionFlags(Qt.TextInteractionFlag.TextSelectableByMouse)
        self.detail_label.setVisible(bool(detail))
        layout.addWidget(self.detail_label)
        layout.addSpacing(6)
        self._row = QHBoxLayout()
        self._row.setSpacing(8)
        self._row.addStretch(1)
        layout.addLayout(self._row)

    def add_button(
        self, text: str, *, variant: str = "secondary", accepts: bool = True
    ) -> QPushButton:
        """A button; ``accepts`` buttons accept the dialog, others reject it."""
        button = QPushButton(text, self)
        set_button_variant(button, variant)
        button.setAutoDefault(False)
        button.clicked.connect(lambda: self._finish(button, accepts))
        self._row.addWidget(button)
        return button

    def set_default_button(self, button: QPushButton) -> None:
        """The button Enter presses (focused when shown)."""
        button.setDefault(True)
        button.setFocus()

    def default_button(self) -> QPushButton | None:
        for button in self.findChildren(QPushButton):
            if button.isDefault():
                return button
        return None

    def set_escape_button(self, button: QPushButton) -> None:
        """The button Escape stands for (else Escape only closes)."""
        self._escape = button

    def clicked_button(self) -> QPushButton | None:
        return self._clicked

    def _finish(self, button: QPushButton, accepts: bool) -> None:
        self._clicked = button
        if accepts:
            self.accept()
        else:
            self.reject()

    def keyPressEvent(self, event: QKeyEvent) -> None:
        if event.key() == Qt.Key.Key_Escape and self._escape is not None:
            self._escape.click()
            return
        super().keyPressEvent(event)


def show_message(
    parent: QWidget | None,
    title: str,
    text: str,
    *,
    kind: str = "info",
    detail: str = "",
) -> None:
    """Tell the user something; one OK button."""
    dialog = MessageDialog(parent, title, text, detail=detail, kind=kind)
    ok = dialog.add_button("OK", variant="primary")
    dialog.set_default_button(ok)
    dialog.exec()
    dialog.deleteLater()


def ask(
    parent: QWidget | None,
    title: str,
    text: str,
    *,
    yes_text: str = "Yes",
    no_text: str = "No",
    default_yes: bool = True,
    destructive: bool = False,
    detail: str = "",
) -> bool:
    """A yes/no question; True only for yes. Escape and closing mean no."""
    dialog = MessageDialog(parent, title, text, detail=detail, kind="question")
    no = dialog.add_button(no_text, variant="secondary", accepts=False)
    yes = dialog.add_button(yes_text, variant="danger" if destructive else "primary")
    dialog.set_escape_button(no)
    dialog.set_default_button(yes if default_yes and not destructive else no)
    answered = dialog.exec() == QDialog.DialogCode.Accepted and dialog.clicked_button() is yes
    dialog.deleteLater()
    return answered
