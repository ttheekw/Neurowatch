"""Video wall grid state — pages, cell assignment, overflow."""

from __future__ import annotations

import math
from dataclasses import dataclass

from PySide6.QtCore import QObject, Signal

from .camera import CameraConfig


@dataclass
class GridLayout:
    rows: int = 3
    cols: int = 3
    auto_fit: bool = True

    @property
    def capacity(self) -> int:
        return self.rows * self.cols


class WallStateModel(QObject):
    """Manages camera ↔ grid cell assignment and page overflow."""

    changed = Signal()

    def __init__(self, parent=None) -> None:
        super().__init__(parent)
        self._cameras: list[CameraConfig] = []
        self._layout = GridLayout()

    @property
    def layout(self) -> GridLayout:
        return self._layout

    def set_grid(self, rows: int, cols: int) -> None:
        self._layout.rows = rows
        self._layout.cols = cols
        self.changed.emit()

    @property
    def page_count(self) -> int:
        if not self._cameras:
            return 1
        return math.ceil(len(self._cameras) / self._layout.capacity)

    def cameras_on_page(self, page: int) -> list[CameraConfig]:
        cap = self._layout.capacity
        start = page * cap
        return self._cameras[start : start + cap]

    def add_camera(self, camera: CameraConfig) -> None:
        self._cameras.append(camera)
        self.changed.emit()

    def remove_camera(self, cam_id: str) -> None:
        self._cameras = [c for c in self._cameras if c.id != cam_id]
        self.changed.emit()
