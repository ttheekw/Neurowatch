"""Render the app SVG as a Windows ICO with legacy-compatible small images.

Run: QT_QPA_PLATFORM=offscreen uv run python scripts/make_windows_icon.py
"""

from __future__ import annotations

import logging
import os
import struct
from pathlib import Path

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")

from PySide6.QtCore import QBuffer, QByteArray, QIODevice, Qt
from PySide6.QtGui import QGuiApplication, QImage, QPainter
from PySide6.QtSvg import QSvgRenderer

logger = logging.getLogger(__name__)

SIZES = (16, 20, 24, 32, 40, 48, 64, 128, 256)
ICONS = Path(__file__).resolve().parents[1] / "ui/neurowatch/resources/icons"


def render(renderer: QSvgRenderer, size: int) -> QImage:
    """Render straight-alpha pixels; ICO DIBs must not use premultiplied alpha."""
    image = QImage(size, size, QImage.Format.Format_RGBA8888)
    image.fill(Qt.GlobalColor.transparent)
    painter = QPainter(image)
    renderer.render(painter)
    painter.end()
    return image


def dib(image: QImage) -> bytes:
    """BITMAPINFOHEADER, bottom-up BGRA pixels, then a DWORD-aligned AND mask."""
    size = image.width()
    pixels = bytes(image.constBits())
    bgra = bytearray()
    mask_stride = ((size + 31) // 32) * 4
    mask = bytearray(mask_stride * size)
    for row, y in enumerate(range(size - 1, -1, -1)):
        for x in range(size):
            offset = y * image.bytesPerLine() + x * 4
            red, green, blue, alpha = pixels[offset : offset + 4]
            bgra.extend((blue, green, red, alpha))
            if alpha == 0:
                mask[row * mask_stride + x // 8] |= 0x80 >> (x % 8)
    header = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, len(bgra), 0, 0, 0, 0)
    return header + bgra + mask


def png(image: QImage) -> bytes:
    """PNG is used only for the Vista-era 256 px entry."""
    data = QByteArray()
    buffer = QBuffer(data)
    if not buffer.open(QIODevice.OpenModeFlag.WriteOnly) or not image.save(buffer, "PNG"):
        raise RuntimeError("Could not encode the 256 px icon")
    return bytes(data)


def main() -> None:
    """Regenerate the checked-in Windows icon from the canonical SVG."""
    app = QGuiApplication([])
    renderer = QSvgRenderer(str(ICONS / "neurowatch.svg"))
    if not renderer.isValid():
        raise ValueError("Could not load neurowatch.svg")
    entries = []
    images = []
    offset = 6 + 16 * len(SIZES)
    for size in SIZES:
        image = render(renderer, size)
        payload = png(image) if size == 256 else dib(image)
        dimension = size % 256  # ICO encodes 256 as zero.
        entries.append(
            struct.pack("<BBBBHHII", dimension, dimension, 0, 0, 1, 32, len(payload), offset)
        )
        images.append(payload)
        offset += len(payload)
        logger.info("%s px: %s (%s bytes)", size, "PNG" if size == 256 else "DIB", len(payload))
    (ICONS / "neurowatch.ico").write_bytes(
        struct.pack("<HHH", 0, 1, len(SIZES)) + b"".join(entries) + b"".join(images)
    )
    app.quit()


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO, format="%(message)s")
    main()
