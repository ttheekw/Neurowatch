from __future__ import annotations

from PySide6.QtCore import Qt
from PySide6.QtWidgets import QDialog, QDialogButtonBox, QLabel, QVBoxLayout, QWidget

from neurowatch import build_info
from neurowatch.theme import apply_dialog_theme, set_label_role, style_button_box


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
        layout.addSpacing(14)

        btns = QDialogButtonBox(QDialogButtonBox.StandardButton.Close)
        btns.rejected.connect(self.reject)
        btns.accepted.connect(self.accept)
        style_button_box(btns)
        btns.setCenterButtons(True)
        layout.addWidget(btns)
