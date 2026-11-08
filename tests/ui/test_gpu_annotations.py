"""Exercise Qt's actual GL glyph uploads, which raster/offscreen tests cannot cover."""

import pytest
from PySide6.QtCore import QRect, Qt
from PySide6.QtGui import QColor, QFont, QImage, QPainter
from PySide6.QtWidgets import QApplication, QWidget

from neurowatch.widgets.gpu_video_surface import GpuVideoSurface


@pytest.mark.parametrize("width", [319, 320])
def test_nv12_upload_does_not_corrupt_overlay_glyph_rows(qtbot, width):
    if QApplication.platformName() in ("offscreen", "minimal"):
        pytest.skip("requires an OpenGL display (Xvfb with Mesa is sufficient)")
    parent = QWidget()
    qtbot.addWidget(parent)
    parent.resize(640, 180)
    font = QFont("DejaVu Sans")
    font.setPixelSize(24)
    font.setWeight(QFont.Weight.DemiBold)

    def paint(painter):
        painter.setFont(font)
        painter.setPen(QColor("#ffb020"))
        painter.drawText(20, 55, "Score 8% - motion")
        painter.drawText(20, 90, "3 blocks changed - 2.8 fps")

    surface = GpuVideoSurface(parent, lambda _w, _h: QRect(0, 0, 640, 180), paint)
    surface.resize(640, 180)
    data = bytes([16]) * width * 180 + bytes([128]) * ((width + 1) // 2) * 90 * 2
    surface.set_frame(data, width, 180, 2)
    parent.show()
    qtbot.waitUntil(surface.isValid)
    try:
        qtbot.waitUntil(lambda: surface.upload_count > 0)
        assert not surface._failed
        actual = surface.grabFramebuffer()
        reference = QImage(640, 180, QImage.Format.Format_RGB32)
        reference.fill(Qt.GlobalColor.black)
        painter = QPainter(reference)
        paint(painter)
        painter.end()
        # Raster and GL use the same glyph masks; only edge rounding may differ.
        expected_ink = actual_ink = common = 0
        dpr = actual.devicePixelRatio()
        for y in range(180):
            for x in range(640):
                expected = reference.pixelColor(x, y).red() > 120
                shown = actual.pixelColor(round(x * dpr), round(y * dpr)).red() > 120
                expected_ink += expected
                actual_ink += shown
                common += expected and shown
        assert common / max(expected_ink, actual_ink) > 0.95
        # Repeat an upload: Qt's paint engine and our video shader share context state.
        surface.set_frame(data, width, 180, 2)
        surface.grabFramebuffer()
        assert not surface._failed
    finally:
        surface.cleanup()
        parent.close()
