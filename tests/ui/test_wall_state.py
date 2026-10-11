"""Tests for WallStateModel — grid layout and page overflow logic."""
from __future__ import annotations

import pytest

from neurowatch.models.camera import CameraConfig
from neurowatch.models.wall_state import WallStateModel


def test_default_grid_capacity():
    model = WallStateModel()
    assert model.layout.capacity == 9  # 3×3 default


def test_page_count_no_cameras():
    model = WallStateModel()
    assert model.page_count == 1


def test_page_overflow():
    model = WallStateModel()
    model.set_grid(rows=2, cols=2)  # capacity 4
    for i in range(6):
        model.add_camera(CameraConfig(id=str(i), name=f"Cam {i}", url="rtsp://x"))
    assert model.page_count == 2
    assert len(model.cameras_on_page(0)) == 4
    assert len(model.cameras_on_page(1)) == 2


def test_remove_camera():
    model = WallStateModel()
    cam = CameraConfig(id="abc", name="Test", url="rtsp://x")
    model.add_camera(cam)
    assert len(model.cameras_on_page(0)) == 1
    model.remove_camera("abc")
    assert len(model.cameras_on_page(0)) == 0
