"""Paint analytics plugins' drawings over a camera's picture.

Coordinates are normalised to the picture, so a drawing fits whichever
stream and size the tile shows. Colours come from a fixed palette chosen to
read on video (plugins name a style, never a colour).
"""

from __future__ import annotations

import math
import time
from collections.abc import Iterable
from dataclasses import dataclass

from PySide6.QtCore import QPointF, QRect, QRectF, Qt
from PySide6.QtGui import (
    QColor,
    QFont,
    QFontMetrics,
    QLinearGradient,
    QPainter,
    QPainterPath,
    QPen,
    QPixmap,
    QPolygonF,
)

from neurowatch.models.analytics import Drawing, matches_picture
from neurowatch.theme import ui_font

_STYLE = {
    "accent": QColor(77, 163, 255),
    "success": QColor(60, 207, 110),
    "warning": QColor(255, 176, 32),
    "danger": QColor(255, 90, 90),
    "muted": QColor(205, 205, 215),
    "series1": QColor(0x3B, 0x82, 0xF6),
    "series2": QColor(0xF5, 0x9E, 0x0B),
    "series3": QColor(0x10, 0xB9, 0x81),
    "series4": QColor(0x63, 0x66, 0xF1),
    "series5": QColor(0xEC, 0x48, 0x99),
    "series6": QColor(0x8B, 0x5C, 0xF6),
    "series7": QColor(0xF9, 0x73, 0x16),
    "series8": QColor(0x06, 0xB6, 0xD4),
}
_PANEL_BG = QColor(8, 10, 16, 225)
_LABEL_TEXT = QColor(12, 12, 18)
#: Text panels at the top stay below the tile's name strip.
TOP_RESERVED = 32
#: Text panels at the bottom stay above the tile's time and rate strip.
BOTTOM_RESERVED = 26


def style_hex(style: str) -> str | None:
    """A style's colour as ``#rrggbb``, or None for an unknown one."""
    color = _STYLE.get(style)
    return color.name() if color is not None else None


def _color(style: object) -> QColor:
    return _STYLE.get(str(style), _STYLE["accent"])


@dataclass(frozen=True)
class DrawingMetrics:
    font_px: int
    stroke: float
    padding: int
    margin: int
    gap: int


def drawing_metrics(height: int) -> DrawingMetrics:
    """Display-space sizes: 18 px text at 720p, with a readable small-tile floor."""
    scale = height / 720.0
    return DrawingMetrics(
        max(12, round(18 * scale)),
        max(1.25, 2 * scale),
        max(4, round(6 * scale)),
        max(4, round(8 * scale)),
        max(2, round(4 * scale)),
    )


def paint_drawings(
    painter: QPainter,
    picture: QRect,
    drawings: Iterable[Drawing],
    now: float | None = None,
    frame_identity: tuple[int, int] | tuple[int, int, int] = (0, 0),
) -> None:
    """Draw normalized geometry as vectors at the displayed picture's resolution."""
    if picture.isEmpty():
        return
    now = time.monotonic() if now is None else now
    live = [d for d in drawings if d.until > now and matches_picture(d, *frame_identity)]
    if not live:
        return
    painter.save()
    painter.setClipRect(picture)
    painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
    painter.setRenderHint(QPainter.RenderHint.TextAntialiasing, True)
    sizes = drawing_metrics(picture.height())
    font = ui_font(10, QFont.Weight.DemiBold)
    font.setPixelSize(sizes.font_px)
    painter.setFont(font)
    metrics = QFontMetrics(font, painter.device())
    corners: dict[str, int] = {}

    def at(x: float, y: float) -> QPointF:
        return QPointF(picture.x() + x * picture.width(), picture.y() + y * picture.height())

    for drawing in live:
        for item in drawing.items:
            kind = item.get("type")
            color = _color(item.get("style"))
            if kind == "box":
                x, y, w, h = (float(v) for v in item["rect"])
                rect = QRectF(at(x, y), at(x + w, y + h))
                _shape(painter, color, bool(item.get("fill")), sizes.stroke)
                painter.drawRect(rect)
            elif kind in ("polygon", "polyline"):
                polygon = QPolygonF([at(float(px), float(py)) for px, py in item["points"]])
                _shape(painter, color, bool(item.get("fill")) and kind == "polygon", sizes.stroke)
                if kind == "polygon":
                    painter.drawPolygon(polygon)
                else:
                    painter.drawPolyline(polygon)
    # Paint every label last: later debug grids/fills must not tint or cover
    # glyphs from a preceding detection layer. An area's name is not painted:
    # the key under the picture lists it (FiguresLegend.set_markings).
    for drawing in live:
        for item in drawing.items:
            kind = item.get("type")
            color = _color(item.get("style"))
            if kind == "box":
                x, y, _, _ = (float(v) for v in item["rect"])
                _label(painter, metrics, color, item.get("label"), at(x, y), picture, sizes)
            elif kind == "text":
                anchor = str(item.get("anchor") or "top_left")
                offset = corners.get(anchor, 0)
                used = _text_panel(
                    painter, metrics, picture, color, item["lines"], anchor, offset, sizes
                )
                corners[anchor] = offset + used + sizes.gap
    painter.restore()


def _shape(painter: QPainter, color: QColor, fill: bool, width: float) -> None:
    # Logical-pixel widths follow the image scale and Qt's device pixel ratio.
    # A cosmetic pen would stay a fixed number of physical pixels on HiDPI.
    painter.setPen(QPen(color, width))
    if fill:
        wash = QColor(color)
        wash.setAlpha(70)
        painter.setBrush(wash)
    else:
        painter.setBrush(Qt.BrushStyle.NoBrush)


def _label(
    painter: QPainter,
    metrics: QFontMetrics,
    color: QColor,
    text: object,
    corner: QPointF,
    picture: QRect,
    sizes: DrawingMetrics,
) -> None:
    if not isinstance(text, str) or not text:
        return
    available = picture.width() - 2 * sizes.margin - 2 * sizes.padding
    if available <= 0:
        return
    shown = metrics.elidedText(text, Qt.TextElideMode.ElideRight, available)
    width = metrics.horizontalAdvance(shown) + 2 * sizes.padding
    height = metrics.height() + 2 * sizes.gap
    left = max(
        picture.left() + sizes.margin,
        min(corner.x(), picture.x() + picture.width() - sizes.margin - width),
    )
    top = max(corner.y() - height, float(picture.top() + TOP_RESERVED))
    top = min(top, picture.y() + picture.height() - sizes.margin - height)
    tag = QRectF(round(left), round(top), width, height)
    painter.setPen(Qt.PenStyle.NoPen)
    painter.setBrush(color)
    painter.drawRect(tag)
    painter.setPen(_LABEL_TEXT)
    painter.drawText(tag, Qt.AlignmentFlag.AlignCenter, shown)


def _text_panel(
    painter: QPainter,
    metrics: QFontMetrics,
    picture: QRect,
    color: QColor,
    lines: list[str],
    anchor: str,
    offset: int,
    sizes: DrawingMetrics,
) -> int:
    top_edge = picture.top() + max(TOP_RESERVED, sizes.margin)
    bottom_edge = picture.y() + picture.height() - max(BOTTOM_RESERVED, sizes.margin)
    count = min(
        len(lines), max(0, (bottom_edge - top_edge - offset - 2 * sizes.gap) // metrics.height())
    )
    available = picture.width() - 2 * sizes.margin - 2 * sizes.padding
    if count <= 0 or available <= 0:
        return 0
    shown = [
        metrics.elidedText(line, Qt.TextElideMode.ElideRight, available) for line in lines[:count]
    ]
    if count < len(lines):
        shown[-1] = metrics.elidedText(shown[-1] + "…", Qt.TextElideMode.ElideRight, available)
    width = max(metrics.horizontalAdvance(line) for line in shown) + 2 * sizes.padding
    height = metrics.height() * count + 2 * sizes.gap
    left = (
        picture.left() + sizes.margin
        if anchor.endswith("left")
        else picture.x() + picture.width() - sizes.margin - width
    )
    top = top_edge + offset if anchor.startswith("top") else bottom_edge - height - offset
    panel = QRect(round(left), round(top), width, height)
    painter.setPen(Qt.PenStyle.NoPen)
    painter.setBrush(_PANEL_BG)
    painter.drawRoundedRect(panel, sizes.gap, sizes.gap)
    painter.setPen(color)
    for i, line in enumerate(shown):
        painter.drawText(
            panel.adjusted(sizes.padding, sizes.gap + i * metrics.height(), -sizes.padding, 0),
            Qt.AlignmentFlag.AlignLeft | Qt.AlignmentFlag.AlignTop,
            line,
        )
    return height


#: The marker's colours: violet while a plugin runs on the camera, grey not.
_MARKER_TOP = {True: QColor(139, 92, 246), False: QColor(113, 113, 122)}  # violet-500 / zinc-500
_MARKER_BOTTOM = {True: QColor(109, 40, 217), False: QColor(82, 82, 91)}  # violet-700 / zinc-600
#: Rendered this many times larger than shown, then scaled down smoothly:
#: crisp edges at any screen scale, also at 100%.
_SUPERSAMPLE = 4


def marker_pixmap(size: int, dpr: float, running: bool) -> QPixmap:
    """The analytics marker (a plugin's hexagon with its eye), ``size``
    logical pixels square, drawn at high resolution. Draw it into a
    ``size``×``size`` box with smooth scaling."""
    scale = max(1.0, dpr) * _SUPERSAMPLE
    side = max(1, round(size * scale))
    picture = QPixmap(side, side)
    picture.fill(Qt.GlobalColor.transparent)
    painter = QPainter(picture)
    painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
    unit = side / 24.0  # the design is on a 24-unit grid

    # Rounded tile with a soft top-to-bottom gradient and a light top edge.
    tile = QRectF(0, 0, side, side)
    gradient = QLinearGradient(0, 0, 0, side)
    gradient.setColorAt(0.0, _MARKER_TOP[running])
    gradient.setColorAt(1.0, _MARKER_BOTTOM[running])
    painter.setPen(Qt.PenStyle.NoPen)
    painter.setBrush(gradient)
    painter.drawRoundedRect(tile, 5 * unit, 5 * unit)
    highlight = QPen(QColor(255, 255, 255, 60), 1.0 * unit)
    painter.setPen(highlight)
    painter.setBrush(Qt.BrushStyle.NoBrush)
    painter.drawRoundedRect(
        tile.adjusted(0.5 * unit, 0.5 * unit, -0.5 * unit, -0.5 * unit), 4.5 * unit, 4.5 * unit
    )

    # The hexagon (a plugin), lightly filled, with a crisp white outline.
    centre = QPointF(side / 2, side / 2)
    radius = 7.6 * unit
    hexagon = QPolygonF(
        [
            centre
            + QPointF(
                radius * math.cos(math.radians(60 * i - 90)),
                radius * math.sin(math.radians(60 * i - 90)),
            )
            for i in range(6)
        ]
    )
    outline = QPen(QColor(255, 255, 255), 1.8 * unit)
    outline.setJoinStyle(Qt.PenJoinStyle.RoundJoin)
    painter.setPen(outline)
    painter.setBrush(QColor(255, 255, 255, 38))
    painter.drawPolygon(hexagon)

    # The eye: what the plugin watches.
    eye = QPen(QColor(255, 255, 255), 1.5 * unit)
    eye.setCapStyle(Qt.PenCapStyle.RoundCap)
    painter.setPen(eye)
    painter.setBrush(Qt.BrushStyle.NoBrush)
    painter.drawEllipse(centre, 3.2 * unit, 3.2 * unit)
    painter.setPen(Qt.PenStyle.NoPen)
    painter.setBrush(QColor(255, 255, 255))
    painter.drawEllipse(centre, 1.4 * unit, 1.4 * unit)
    painter.end()
    picture.setDevicePixelRatio(scale)
    return picture


#: The backup symbol's colours by state (the status bar's green, red, grey).
_BACKUP_TOP = {
    "ok": QColor(34, 197, 94),
    "failing": QColor(239, 68, 68),
    "starting": QColor(113, 113, 122),
}
_BACKUP_BOTTOM = {
    "ok": QColor(21, 128, 61),
    "failing": QColor(185, 28, 28),
    "starting": QColor(82, 82, 91),
}


def backup_marker_pixmap(size: int, dpr: float, state: str) -> QPixmap:
    """A tile's Cloud Backup symbol, like the analytics marker: a rounded
    tile (green backed up, red failing, grey starting) with a white cloud,
    an upward arrow in it, or "!" while failing. ``size`` logical pixels."""
    scale = max(1.0, dpr) * _SUPERSAMPLE
    side = max(1, round(size * scale))
    picture = QPixmap(side, side)
    picture.fill(Qt.GlobalColor.transparent)
    painter = QPainter(picture)
    painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
    unit = side / 24.0
    tile = QRectF(0, 0, side, side)
    gradient = QLinearGradient(0, 0, 0, side)
    gradient.setColorAt(0.0, _BACKUP_TOP.get(state, _BACKUP_TOP["starting"]))
    gradient.setColorAt(1.0, _BACKUP_BOTTOM.get(state, _BACKUP_BOTTOM["starting"]))
    painter.setPen(Qt.PenStyle.NoPen)
    painter.setBrush(gradient)
    painter.drawRoundedRect(tile, 5 * unit, 5 * unit)
    painter.setPen(QPen(QColor(255, 255, 255, 60), 1.0 * unit))
    painter.setBrush(Qt.BrushStyle.NoBrush)
    painter.drawRoundedRect(
        tile.adjusted(0.5 * unit, 0.5 * unit, -0.5 * unit, -0.5 * unit), 4.5 * unit, 4.5 * unit
    )
    # The cloud: a white outline, lightly filled.
    cloud = QPainterPath(QPointF(7.2 * unit, 17.5 * unit))
    cloud.cubicTo(
        QPointF(3.4 * unit, 17.5 * unit),
        QPointF(3.2 * unit, 11.6 * unit),
        QPointF(7.3 * unit, 11.3 * unit),
    )
    cloud.cubicTo(
        QPointF(7.8 * unit, 6.4 * unit),
        QPointF(14.6 * unit, 5.2 * unit),
        QPointF(16.2 * unit, 9.8 * unit),
    )
    cloud.cubicTo(
        QPointF(21.0 * unit, 9.8 * unit),
        QPointF(21.3 * unit, 17.5 * unit),
        QPointF(16.8 * unit, 17.5 * unit),
    )
    cloud.closeSubpath()
    outline = QPen(QColor(255, 255, 255), 1.7 * unit)
    outline.setJoinStyle(Qt.PenJoinStyle.RoundJoin)
    outline.setCapStyle(Qt.PenCapStyle.RoundCap)
    painter.setPen(outline)
    painter.setBrush(QColor(255, 255, 255, 38))
    painter.drawPath(cloud)
    mark = QPen(QColor(255, 255, 255), 1.6 * unit)
    mark.setCapStyle(Qt.PenCapStyle.RoundCap)
    painter.setPen(mark)
    if state == "failing":
        painter.drawLine(QPointF(12.0 * unit, 10.2 * unit), QPointF(12.0 * unit, 13.4 * unit))
        painter.drawPoint(QPointF(12.0 * unit, 15.6 * unit))
    else:
        painter.drawLine(QPointF(12.0 * unit, 15.4 * unit), QPointF(12.0 * unit, 10.4 * unit))
        painter.drawLine(QPointF(10.0 * unit, 12.4 * unit), QPointF(12.0 * unit, 10.4 * unit))
        painter.drawLine(QPointF(14.0 * unit, 12.4 * unit), QPointF(12.0 * unit, 10.4 * unit))
    painter.end()
    picture.setDevicePixelRatio(scale)
    return picture
