"""Render the app logo (``ui/neurowatch/resources/icons/neurowatch.svg``) into
the Windows icon next to it (``neurowatch.ico``: the .exe, the installer,
shortcuts and Add/Remove Programs) and the Linux packages' PNG icons
(``scripts/installer/linux/icons``: app menus and software centres, beside the
SVG). Run again after changing the SVG:

    QT_QPA_PLATFORM=offscreen uv run python scripts/make_app_icon.py
"""

from __future__ import annotations

import struct
import sys
from pathlib import Path

from PySide6.QtCore import QBuffer, QByteArray, QIODevice, Qt
from PySide6.QtGui import QGuiApplication, QImage, QPainter
from PySide6.QtSvg import QSvgRenderer

ICONS = Path(__file__).resolve().parents[1] / "ui" / "neurowatch" / "resources" / "icons"
LINUX_ICONS = Path(__file__).resolve().parent / "installer" / "linux" / "icons"
SIZES = (16, 20, 24, 32, 40, 48, 64, 128, 256)
#: hicolor sizes installed by the Linux packages (besides scalable/).
LINUX_SIZES = (48, 64, 128, 256)


def render_png(renderer: QSvgRenderer, size: int) -> bytes:
    image = QImage(size, size, QImage.Format.Format_ARGB32)
    image.fill(Qt.GlobalColor.transparent)
    painter = QPainter(image)
    painter.setRenderHint(QPainter.RenderHint.Antialiasing)
    renderer.render(painter)
    painter.end()
    data = QByteArray()
    buffer = QBuffer(data)
    buffer.open(QIODevice.OpenModeFlag.WriteOnly)
    image.save(buffer, "PNG")
    return bytes(data.data())


def ico(images: list[tuple[int, bytes]]) -> bytes:
    """An ICO file holding PNG images (Windows Vista and later)."""
    header = struct.pack("<HHH", 0, 1, len(images))
    offset = len(header) + 16 * len(images)
    entries, payload = b"", b""
    for size, png in images:
        side = 0 if size >= 256 else size  # 0 means 256
        entries += struct.pack("<BBBBHHII", side, side, 0, 0, 1, 32, len(png), offset)
        payload += png
        offset += len(png)
    return header + entries + payload


def main() -> int:
    _app = QGuiApplication(sys.argv)
    renderer = QSvgRenderer(str(ICONS / "neurowatch.svg"))
    if not renderer.isValid():
        print("neurowatch.svg could not be read", file=sys.stderr)
        return 1
    (ICONS / "neurowatch.ico").write_bytes(ico([(s, render_png(renderer, s)) for s in SIZES]))
    print(ICONS / "neurowatch.ico")
    LINUX_ICONS.mkdir(parents=True, exist_ok=True)
    for size in LINUX_SIZES:
        path = LINUX_ICONS / f"neurowatch-{size}.png"
        path.write_bytes(render_png(renderer, size))
        print(path)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
