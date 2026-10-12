from __future__ import annotations

from PySide6.QtCore import Qt
from PySide6.QtWidgets import QDialog, QDialogButtonBox, QLabel, QVBoxLayout, QWidget

from neurowatch import build_info
from neurowatch.theme import apply_dialog_theme, set_label_role, style_button_box

#: The licence's "Appropriate Legal Notices" (AGPL section 5(d)), in short.
NO_WARRANTY = (
    "Free software under the GNU AGPL v3 or later. Provided as is, WITHOUT ANY"
    " WARRANTY, at your own risk; the authors are not liable for any loss,"
    " including missed or lost recordings. See NOTICE and LICENSE."
)


class AboutDialog(QDialog):
    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setWindowTitle("About Neurowatch")
        self.setMinimumWidth(380)
        apply_dialog_theme(self)

        layout = QVBoxLayout(self)
        layout.setContentsMargins(24, 24, 24, 18)
        layout.setSpacing(6)

        title = QLabel("⬡ Neurowatch")
        title.setObjectName("dialogTitle")
        title.setAlignment(Qt.AlignmentFlag.AlignCenter)
        layout.addWidget(title)

        version = QLabel(build_info.build_identity().display)
        version.setObjectName("dialogSubtitle")
        version.setTextFormat(Qt.TextFormat.PlainText)
        version.setAlignment(Qt.AlignmentFlag.AlignCenter)
        layout.addWidget(version)

        layout.addSpacing(10)
        layout.addWidget(
            QLabel(
                "Open-platform NVR Desktop Application",
                alignment=Qt.AlignmentFlag.AlignCenter,
            )
        )
        stack = QLabel(
            "Backend: Rust + GStreamer  ·  UI: Python / PySide6",
            alignment=Qt.AlignmentFlag.AlignCenter,
        )
        set_label_role(stack, "hint")
        layout.addWidget(stack)
        layout.addSpacing(10)
        legal = QLabel(NO_WARRANTY, alignment=Qt.AlignmentFlag.AlignCenter)
        legal.setWordWrap(True)
        legal.setAccessibleName("Licence and warranty")
        set_label_role(legal, "hint")
        layout.addWidget(legal)
        layout.addSpacing(14)

        btns = QDialogButtonBox(QDialogButtonBox.StandardButton.Close)
        btns.rejected.connect(self.reject)
        btns.accepted.connect(self.accept)
        style_button_box(btns)
        btns.setCenterButtons(True)
        layout.addWidget(btns)
