from __future__ import annotations

from PySide6.QtCore import QPointF
from PySide6.QtGui import QPixmap


def test_zone_canvas_normalizes_coordinates_and_never_edits_input(qtbot):
    from neurowatch.widgets.motion_zones import ZoneCanvas

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


def test_analysis_preview_bounds_decode_and_block_overlays(qtbot):
    import base64

    from neurowatch.widgets.motion_zones import AnalysisPreview

    preview = AnalysisPreview()
    qtbot.addWidget(preview)
    data = base64.b64encode(bytes(320 * 180)).decode()
    valid = {"width": 320, "height": 180, "gray_b64": data}
    preview.set_analysis(valid, [0, 143, 144, -1, True, 2.5, "3", 0])
    assert preview.has_frame()
    assert preview.active_blocks() == [0, 143]
    preview.set_analysis(valid, [0] * 145)
    assert preview.active_blocks() == []
    for invalid in (
        None,
        {**valid, "width": 1920},
        {**valid, "gray_b64": data * 2},
        {**valid, "gray_b64": "!" * len(data)},
    ):
        preview.set_analysis(invalid, [0])
        assert not preview.has_frame()
        assert preview.active_blocks() == []
