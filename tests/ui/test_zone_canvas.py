from __future__ import annotations

from PySide6.QtCore import QPointF
from PySide6.QtGui import QPixmap


def test_zone_canvas_normalizes_coordinates_and_never_edits_input(qtbot):
    from neurowatch.widgets.zone_canvas import ZoneCanvas

    original = [
        {"id": "gate", "name": "Gate", "kind": "include", "points": [[0, 0], [1, 0], [1, 1]]}
    ]
    canvas = ZoneCanvas()
    qtbot.addWidget(canvas)
    canvas.resize(640, 360)
    picture = QPixmap(640, 360)
    picture.fill()
    canvas.set_snapshot(picture)
    canvas.set_zones(original)
    assert canvas.image_point(QPointF(320, 180)) == [0.5, 0.5]
    assert canvas.image_point(QPointF(-10, 500)) == [0.0, 1.0]
    canvas.append_point(QPointF(0, 180))
    assert original[0]["points"] == [[0, 0], [1, 0], [1, 1]]
    assert canvas.zones()[0]["points"][-1] == [0.0, 0.5]
