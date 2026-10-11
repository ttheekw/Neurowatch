"""Camera list sidebar — shows all configured cameras with status badges."""

from __future__ import annotations

from PySide6.QtCore import Qt, Signal
from PySide6.QtWidgets import (
    QHBoxLayout,
    QLabel,
    QListWidget,
    QListWidgetItem,
    QPushButton,
    QSizePolicy,
    QVBoxLayout,
    QWidget,
)

from neurowatch.models.camera import CameraConfig


class _CameraRow(QWidget):
    """A single row in the camera sidebar."""

    edit_clicked = Signal(str)  # cam_id
    remove_clicked = Signal(str)  # cam_id

    def __init__(self, cam: CameraConfig, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self._cam_id = cam.id
        layout = QHBoxLayout(self)
        layout.setContentsMargins(6, 2, 4, 2)
        layout.setSpacing(4)

        # Status dot placeholder (colour updated externally)
        self._dot = QLabel("●")
        self._dot.setStyleSheet("color: #606070; font-size: 10px;")
        self._dot.setFixedWidth(14)
        layout.addWidget(self._dot)

        # Camera name
        name_lbl = QLabel(cam.name or cam.url)
        name_lbl.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Preferred)
        name_lbl.setStyleSheet("color: #c8c8d8; font-size: 11px;")
        layout.addWidget(name_lbl)

        # Edit
        edit_btn = QPushButton("✏")
        edit_btn.setFixedSize(22, 22)
        edit_btn.setStyleSheet(
            "QPushButton { background: transparent; color: #8888aa; border: none; }"
            "QPushButton:hover { color: #c0c0ff; }"
        )
        edit_btn.clicked.connect(lambda: self.edit_clicked.emit(self._cam_id))
        layout.addWidget(edit_btn)

        # Remove
        del_btn = QPushButton("✕")
        del_btn.setFixedSize(22, 22)
        del_btn.setStyleSheet(
            "QPushButton { background: transparent; color: #886666; border: none; }"
            "QPushButton:hover { color: #ff6666; }"
        )
        del_btn.clicked.connect(lambda: self.remove_clicked.emit(self._cam_id))
        layout.addWidget(del_btn)

    def set_status_color(self, color: str) -> None:
        self._dot.setStyleSheet(f"color: {color}; font-size: 10px;")


class CameraSidebar(QWidget):
    """Collapsible left sidebar listing all cameras."""

    add_clicked = Signal()
    edit_clicked = Signal(str)  # cam_id
    remove_clicked = Signal(str)  # cam_id

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setFixedWidth(200)
        self.setStyleSheet("background-color: #0f0f1e; border-right: 1px solid #1e1e40;")

        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(0)

        # Header
        header = QWidget()
        header.setFixedHeight(36)
        header.setStyleSheet("background: #12122a;")
        h_layout = QHBoxLayout(header)
        h_layout.setContentsMargins(8, 0, 4, 0)

        title = QLabel("Cameras")
        title.setStyleSheet("color: #9090b0; font-size: 11px; font-weight: bold;")
        h_layout.addWidget(title)

        h_layout.addStretch()

        add_btn = QPushButton("＋")
        add_btn.setFixedSize(24, 24)
        add_btn.setStyleSheet(
            "QPushButton { background: #2a4a8a; color: white; border-radius: 3px; border: none; }"
            "QPushButton:hover { background: #3a6aaa; }"
        )
        add_btn.clicked.connect(self.add_clicked)
        h_layout.addWidget(add_btn)

        layout.addWidget(header)

        # Camera list
        self._list = QListWidget()
        self._list.setStyleSheet(
            "QListWidget { background: transparent; border: none; }"
            "QListWidget::item { border-bottom: 1px solid #1a1a30; padding: 0; }"
            "QListWidget::item:selected { background: #1e2848; }"
        )
        self._list.setSpacing(0)
        layout.addWidget(self._list)

        self._rows: dict[str, _CameraRow] = {}

    def set_cameras(self, cameras: list[CameraConfig]) -> None:
        """Rebuild the list from the given camera configs."""
        self._list.clear()
        self._rows.clear()

        for cam in cameras:
            row_widget = _CameraRow(cam)
            row_widget.edit_clicked.connect(self.edit_clicked)
            row_widget.remove_clicked.connect(self.remove_clicked)

            item = QListWidgetItem()
            item.setSizeHint(row_widget.sizeHint())
            item.setData(Qt.ItemDataRole.UserRole, cam.id)
            self._list.addItem(item)
            self._list.setItemWidget(item, row_widget)
            self._rows[cam.id] = row_widget

    def set_camera_status(self, cam_id: str, status: str) -> None:
        """Update the status dot colour for a camera."""
        color_map = {
            "streaming": "#32dc50",
            "connecting": "#e6c820",
            "error": "#dc3232",
            "offline": "#606070",
        }
        color = color_map.get(status, "#606070")
        if row := self._rows.get(cam_id):
            row.set_status_color(color)
