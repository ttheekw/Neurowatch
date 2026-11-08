"""Render annotations onto real Qt paint devices, independent of camera pixels."""

from __future__ import annotations

import numpy as np
from PySide6.QtCore import QRect
from PySide6.QtGui import QColor, QImage, QPainter

from neurowatch.models.analytics import Drawing
from neurowatch.widgets.analytics_paint import paint_drawings


def render(width: int, height: int, dpr: int = 1, picture: QRect | None = None) -> QImage:
    image = QImage(width * dpr, height * dpr, QImage.Format.Format_RGB32)
    image.setDevicePixelRatio(dpr)
    image.fill(QColor(20, 24, 30))
    painter = QPainter(image)
    drawing = Drawing(
        "camera",
        "plugin",
        "boxes",
        (
            {
                "type": "box",
                "rect": [0.2, 0.4, 0.4, 0.4],
                "label": "Scaled label",
                "style": "warning",
            },
        ),
        1000,
    )
    paint_drawings(painter, picture or QRect(0, 0, width, height), [drawing], now=0)
    painter.end()
    return image


def pixels(image: QImage) -> np.ndarray:
    return np.frombuffer(image.constBits(), dtype=np.uint8).reshape(
        image.height(), image.width(), 4
    )


def label_bounds(image: QImage, logical_height: int, dpr: int) -> tuple[float, float]:
    # The solid yellow label sits above the normalized box; only vector text
    # and its tag are measured, never the source camera's raster dimensions.
    region = pixels(image)[: round(0.4 * logical_height * dpr) - 3 * dpr]
    rows, cols = np.where(
        (region[:, :, 0] == 32) & (region[:, :, 1] == 176) & (region[:, :, 2] == 255)
    )
    assert len(rows)
    return (int(cols.max()) - int(cols.min()) + 1) / dpr, (
        int(rows.max()) - int(rows.min()) + 1
    ) / dpr


def test_annotations_scale_with_picture_and_keep_native_hidpi_detail(qapp) -> None:
    small = render(640, 360)
    large = render(1280, 720)
    retina = render(1280, 720, 2)
    sw, sh = label_bounds(small, 360, 1)
    lw, lh = label_bounds(large, 720, 1)
    rw, rh = label_bounds(retina, 720, 2)
    assert lw > sw and lh > sh, "font/tag size must grow with displayed video"
    assert abs(rw - lw) <= 2 and abs(rh - lh) <= 2, (
        "HiDPI changes physical detail, not logical size"
    )
    for image, dpr in [(large, 1), (retina, 2)]:
        # Both views map the same normalized edge to the same logical point.
        edge = image.pixelColor(round(0.2 * 1280 * dpr), round(0.6 * 720 * dpr))
        assert edge.red() > 100 and edge.green() > 80


def test_annotations_stay_inside_the_actual_letterboxed_picture(qapp) -> None:
    image = render(800, 600, picture=QRect(0, 75, 800, 450))
    data = pixels(image)
    assert np.all(data[:75, :, :3] == [30, 24, 20])
    assert np.all(data[525:, :, :3] == [30, 24, 20])
    edge = image.pixelColor(160, 75 + 270)
    assert edge.red() > 100, "normalized geometry must use picture height, not widget height"


def test_later_debug_shapes_cannot_wash_out_an_earlier_label(qapp) -> None:
    image = QImage(640, 360, QImage.Format.Format_RGB32)
    image.fill(QColor(20, 24, 30))
    painter = QPainter(image)
    shapes = [
        Drawing(
            "camera",
            "plugin",
            "detections",
            (
                {
                    "type": "box",
                    "rect": [0.2, 0.4, 0.4, 0.4],
                    "label": "Clear label",
                    "style": "warning",
                },
            ),
            1000,
        ),
        Drawing(
            "camera",
            "plugin",
            "debug",
            ({"type": "box", "rect": [0, 0, 1, 1], "fill": True, "style": "accent"},),
            1000,
        ),
    ]
    paint_drawings(painter, QRect(0, 0, 640, 360), shapes, now=0)
    painter.end()
    assert label_bounds(image, 360, 1)[0] > 0, "labels must remain above every debug fill"


def test_an_areas_name_is_not_written_on_the_picture(qapp) -> None:
    """The key under the picture names it; only its outline is on the video."""

    def painted(label: str | None) -> np.ndarray:
        image = QImage(640, 360, QImage.Format.Format_RGBA8888)
        image.fill(QColor(20, 24, 30))
        painter = QPainter(image)
        item = {
            "type": "polygon",
            "points": [[0.2, 0.3], [0.8, 0.3], [0.8, 0.9], [0.2, 0.9]],
            "style": "success",
        }
        if label is not None:
            item["label"] = label
        drawing = Drawing("camera", "plugin", "lines", (item,), 1000)
        paint_drawings(painter, QRect(0, 0, 640, 360), [drawing], now=0)
        painter.end()
        return pixels(image).copy()

    assert np.array_equal(painted("Lane 1"), painted(None))


def test_an_objects_own_label_stays_on_its_box(qapp) -> None:
    assert label_bounds(render(640, 360), 360, 1)[0] > 0
