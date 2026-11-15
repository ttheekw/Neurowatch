"""Small outline glyphs for live counters, rendered at the screen's native DPI."""

from __future__ import annotations

from functools import lru_cache

from PySide6.QtCore import QByteArray, QRectF, Qt
from PySide6.QtGui import QColor, QPainter, QPixmap
from PySide6.QtSvg import QSvgRenderer

# App-owned geometry only: no plugin-supplied paths, SVG or filenames.
_SHAPES = {
    "car": '<path d="m5 10 2-5h10l2 5"/><rect x="3" y="10" width="18" height="8" rx="2"/>'
    '<path d="M5 18v2m14-2v2M7 14h1m8 0h1"/>',
    "bus": '<rect x="5" y="3" width="14" height="17" rx="3"/>'
    '<path d="M5 12h14M12 3v9M7 20v1m10-1v1M8 16h1m6 0h1"/>',
    "lorry": '<path d="M3 6h11v11H3zM14 10h4l3 4v3h-7M17 10v4h4"/>'
    '<circle cx="7" cy="18" r="2"/><circle cx="18" cy="18" r="2"/>',
    "van": '<path d="M3 7h12l6 6v5H3zM14 7v7h7M8 7v7H3"/>'
    '<circle cx="7" cy="18" r="2"/><circle cx="18" cy="18" r="2"/>',
    "motorbike": '<circle cx="5" cy="17" r="3"/><circle cx="19" cy="17" r="3"/>'
    '<path d="m5 17 5-7h5l4 7M8 14h7l-4 3H5M8 10h3m3-4h3l2 8"/>',
    "bicycle": '<circle cx="5" cy="17" r="3.5"/><circle cx="19" cy="17" r="3.5"/>'
    '<path d="m5 17 5-8 5 8H5m5-8h7m-3-5h2l3 13M8 9h4"/>',
    "three_wheeler": '<path d="M5 17V9l3-5h7l5 9v4M6 9h11M12 4v13M3 17h18"/>'
    '<circle cx="6" cy="18" r="2"/><circle cx="18" cy="18" r="2"/>',
    "person": '<circle cx="12" cy="5" r="2.5"/>'
    '<path d="M7 12v-1a3 3 0 0 1 3-2h4a3 3 0 0 1 3 2v1M12 9v7m0-1-4 6m4-6 4 6"/>',
    "total": '<path d="M19 4H6l7 8-7 8h13"/>',
}
COUNT_ICON_NAMES = frozenset(_SHAPES)


@lru_cache(maxsize=128)
def count_icon(name: str, color: str, side: int, dpr: float, faded: bool = False) -> QPixmap:
    """Rasterize trusted vector geometry once per colour, size and screen scale."""
    shape = _SHAPES.get(name)
    if shape is None:
        return QPixmap()
    ink = QColor(color).name()
    svg = (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" '
        f'stroke="{ink}" stroke-width="1.75" stroke-linecap="round" '
        f'stroke-linejoin="round">{shape}</svg>'
    )
    renderer = QSvgRenderer(QByteArray(svg.encode()))
    pixmap = QPixmap(max(1, round(side * dpr)), max(1, round(side * dpr)))
    pixmap.fill(Qt.GlobalColor.transparent)
    pixmap.setDevicePixelRatio(dpr)
    painter = QPainter(pixmap)
    painter.setRenderHint(QPainter.RenderHint.Antialiasing)
    if faded:
        painter.setOpacity(0.45)
    renderer.render(painter, QRectF(0, 0, side, side))
    painter.end()
    return pixmap
