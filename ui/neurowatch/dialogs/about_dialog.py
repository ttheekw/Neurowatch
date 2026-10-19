from __future__ import annotations

from PySide6.QtCore import QSize, Qt
from PySide6.QtWidgets import (
    QDialog,
    QDialogButtonBox,
    QFrame,
    QLabel,
    QWidget,
)

from neurowatch import build_info
from neurowatch.theme import app_icon, apply_dialog_theme, set_label_role, style_button_box
from neurowatch.widgets.window_chrome import install_dialog_chrome

#: The licence and its warranty disclaimer (NOTICE), in short.
NO_WARRANTY = (
    "Open source under the Apache License 2.0. Provided as is, WITHOUT ANY"
    " WARRANTY, at your own risk; the authors are not liable for any loss,"
    " including missed or lost recordings. See NOTICE and LICENSE."
)


class AboutDialog(QDialog):
    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setWindowTitle("About NeuroWatch")
        self.setMinimumWidth(420)
        apply_dialog_theme(self)

        # The window template: themed header (title, close), rounded card.
        layout = install_dialog_chrome(self, "About NeuroWatch")
        layout.setSpacing(6)

        logo = QLabel()
        logo.setPixmap(app_icon().pixmap(QSize(64, 64)))
        logo.setAlignment(Qt.AlignmentFlag.AlignCenter)
        logo.setAccessibleName("NeuroWatch logo")
        layout.addWidget(logo)
        layout.addSpacing(4)

        name = QLabel("NeuroWatch")
        name.setObjectName("aboutName")
        name.setAlignment(Qt.AlignmentFlag.AlignCenter)
        layout.addWidget(name)

        version = QLabel(build_info.build_identity().display)
        version.setObjectName("dialogSubtitle")
        version.setTextFormat(Qt.TextFormat.PlainText)
        version.setAlignment(Qt.AlignmentFlag.AlignCenter)
        version.setTextInteractionFlags(Qt.TextInteractionFlag.TextSelectableByMouse)
        layout.addWidget(version)

        layout.addSpacing(8)
        description = QLabel("Open-platform network video recorder for the desktop")
        description.setAlignment(Qt.AlignmentFlag.AlignCenter)
        description.setWordWrap(True)
        layout.addWidget(description)
        stack = QLabel("Backend: Rust + GStreamer  ·  UI: Python / PySide6")
        stack.setAlignment(Qt.AlignmentFlag.AlignCenter)
        set_label_role(stack, "hint")
        layout.addWidget(stack)

        layout.addSpacing(10)
        rule = QFrame()
        rule.setObjectName("aboutRule")
        rule.setFrameShape(QFrame.Shape.HLine)
        layout.addWidget(rule)
        layout.addSpacing(6)

        legal = QLabel(NO_WARRANTY)
        legal.setAlignment(Qt.AlignmentFlag.AlignCenter)
        legal.setWordWrap(True)
        legal.setAccessibleName("Licence and warranty")
        set_label_role(legal, "hint")
        layout.addWidget(legal)
        layout.addSpacing(12)

        btns = QDialogButtonBox(QDialogButtonBox.StandardButton.Close)
        btns.rejected.connect(self.reject)
        btns.accepted.connect(self.accept)
        style_button_box(btns)
        layout.addWidget(btns)
        # Enter closes; the window template starts the keyboard here.
        close = btns.button(QDialogButtonBox.StandardButton.Close)
        close.setDefault(True)
