"""Vector glyphs for event kinds, painted at any size and device pixel ratio.

Each glyph is drawn in a 24×24 design box scaled to the target rectangle, with
round strokes in one colour, so it reads the same on every theme.
"""

from __future__ import annotations

from collections.abc import Callable

from PySide6.QtCore import QPointF, QRectF, Qt
from PySide6.QtGui import QColor, QPainter, QPainterPath, QPen

_Glyph = Callable[[QPainter], None]


def _motion(p: QPainter) -> None:
    # A figure in motion: head, body and speed lines.
    p.drawEllipse(QPointF(14.5, 5.0), 2.0, 2.0)
    path = QPainterPath(QPointF(9.0, 11.0))
    path.lineTo(12.5, 9.0)
    path.lineTo(15.5, 11.5)
    path.lineTo(18.0, 14.0)
    p.drawPath(path)
    legs = QPainterPath(QPointF(8.5, 20.0))
    legs.lineTo(11.5, 16.0)
    legs.lineTo(13.0, 12.0)
    legs.lineTo(15.0, 16.0)
    legs.lineTo(15.0, 20.5)
    p.drawPath(legs)
    p.drawLine(QPointF(3.0, 9.0), QPointF(6.5, 9.0))
    p.drawLine(QPointF(2.0, 13.0), QPointF(6.0, 13.0))
    p.drawLine(QPointF(3.0, 17.0), QPointF(6.5, 17.0))


def _intrusion(p: QPainter) -> None:
    shield = QPainterPath(QPointF(12.0, 3.0))
    shield.lineTo(19.5, 6.0)
    shield.cubicTo(19.5, 13.0, 16.5, 18.5, 12.0, 21.0)
    shield.cubicTo(7.5, 18.5, 4.5, 13.0, 4.5, 6.0)
    shield.closeSubpath()
    p.drawPath(shield)
    p.drawLine(QPointF(12.0, 8.5), QPointF(12.0, 13.0))
    p.drawPoint(QPointF(12.0, 16.0))


def _line_crossing(p: QPainter) -> None:
    pen = p.pen()
    dashed = QPen(pen)
    dashed.setDashPattern([1.5, 2.0])
    p.setPen(dashed)
    p.drawLine(QPointF(12.0, 3.0), QPointF(12.0, 21.0))
    p.setPen(pen)
    p.drawLine(QPointF(4.0, 12.0), QPointF(19.0, 12.0))
    arrow = QPainterPath(QPointF(15.5, 8.5))
    arrow.lineTo(19.5, 12.0)
    arrow.lineTo(15.5, 15.5)
    p.drawPath(arrow)


def _tamper(p: QPainter) -> None:
    eye = QPainterPath(QPointF(2.5, 12.0))
    eye.cubicTo(6.0, 6.0, 18.0, 6.0, 21.5, 12.0)
    eye.cubicTo(18.0, 18.0, 6.0, 18.0, 2.5, 12.0)
    p.drawPath(eye)
    p.drawEllipse(QPointF(12.0, 12.0), 2.6, 2.6)
    p.drawLine(QPointF(4.0, 20.0), QPointF(20.0, 4.0))


def _digital_input(p: QPainter) -> None:
    bolt = QPainterPath(QPointF(13.5, 2.5))
    bolt.lineTo(5.5, 13.5)
    bolt.lineTo(11.5, 13.5)
    bolt.lineTo(10.5, 21.5)
    bolt.lineTo(18.5, 10.5)
    bolt.lineTo(12.5, 10.5)
    bolt.closeSubpath()
    p.drawPath(bolt)


def _camera_body(p: QPainter) -> None:
    body = QPainterPath()
    body.addRoundedRect(QRectF(2.5, 7.0, 13.5, 10.0), 2.0, 2.0)
    p.drawPath(body)
    lens = QPainterPath(QPointF(16.0, 10.5))
    lens.lineTo(21.5, 7.5)
    lens.lineTo(21.5, 16.5)
    lens.lineTo(16.0, 13.5)
    p.drawPath(lens)


def _camera_event(p: QPainter) -> None:
    _camera_body(p)
    p.drawPoint(QPointF(9.25, 12.0))


def _camera_offline(p: QPainter) -> None:
    _camera_body(p)
    p.drawLine(QPointF(3.0, 3.0), QPointF(21.0, 21.0))


def _recording_failed(p: QPainter) -> None:
    p.drawEllipse(QPointF(12.0, 12.0), 8.5, 8.5)
    p.drawLine(QPointF(9.0, 9.0), QPointF(15.0, 15.0))
    p.drawLine(QPointF(15.0, 9.0), QPointF(9.0, 15.0))


def _disk_low(p: QPainter) -> None:
    drive = QPainterPath()
    drive.addRoundedRect(QRectF(3.0, 6.0, 18.0, 12.0), 2.5, 2.5)
    p.drawPath(drive)
    p.drawLine(QPointF(3.0, 13.5), QPointF(21.0, 13.5))
    p.drawPoint(QPointF(17.5, 16.0))
    p.drawLine(QPointF(6.5, 9.5), QPointF(16.0, 9.5))


def _storage_missing(p: QPainter) -> None:
    _disk_low(p)
    p.drawLine(QPointF(3.0, 3.0), QPointF(21.0, 21.0))


def _backend_started(p: QPainter) -> None:
    arc = QRectF(4.0, 4.5, 16.0, 16.0)
    p.drawArc(arc, 125 * 16, 290 * 16)
    p.drawLine(QPointF(12.0, 3.0), QPointF(12.0, 11.5))


def _bookmark(p: QPainter) -> None:
    mark = QPainterPath(QPointF(6.5, 3.5))
    mark.lineTo(17.5, 3.5)
    mark.lineTo(17.5, 20.5)
    mark.lineTo(12.0, 16.0)
    mark.lineTo(6.5, 20.5)
    mark.closeSubpath()
    p.drawPath(mark)


def _generic(p: QPainter) -> None:
    p.drawEllipse(QPointF(12.0, 12.0), 8.5, 8.5)
    p.drawLine(QPointF(12.0, 7.5), QPointF(12.0, 12.5))
    p.drawPoint(QPointF(12.0, 16.0))


_GLYPHS: dict[str, _Glyph] = {
    "motion": _motion,
    "intrusion": _intrusion,
    "line_crossing": _line_crossing,
    "tamper": _tamper,
    "digital_input": _digital_input,
    "camera_event": _camera_event,
    "camera_offline": _camera_offline,
    "recording_failed": _recording_failed,
    "disk_low": _disk_low,
    "storage_missing": _storage_missing,
    "backend_started": _backend_started,
    "bookmark": _bookmark,
}


def paint_event_icon(painter: QPainter, rect: QRectF, kind: str, color: QColor) -> None:
    """Draw ``kind``'s glyph centred in ``rect`` (square or not)."""
    side = min(rect.width(), rect.height())
    if side <= 0:
        return
    painter.save()
    painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
    painter.translate(rect.center().x() - side / 2, rect.center().y() - side / 2)
    painter.scale(side / 24.0, side / 24.0)
    pen = QPen(color, 1.8)
    pen.setCapStyle(Qt.PenCapStyle.RoundCap)
    pen.setJoinStyle(Qt.PenJoinStyle.RoundJoin)
    painter.setPen(pen)
    painter.setBrush(Qt.BrushStyle.NoBrush)
    _GLYPHS.get(kind, _generic)(painter)
    painter.restore()
