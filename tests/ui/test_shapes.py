"""Everything a plugin draws on the picture, on the desktop: the same rules and the
same editor as the portal's form (``shapes.ts``, ``PluginDrawingEditor.vue``)."""

from __future__ import annotations

from typing import Any

from PySide6.QtCore import QPoint, Qt
from PySide6.QtTest import QTest

from neurowatch.models.plugins import FormField, Manifest, PluginBinding, form_fields
from neurowatch.models.shapes import ShapeSet, issue, lane_summary

LINE = [[0.1, 0.6], [0.5, 0.6]]
SCHEMA: dict[str, Any] = {
    "type": "object",
    "properties": {
        "classes": {
            "type": "array",
            "title": "What to count",
            "default": ["car"],
            "items": {"type": "string", "enum": ["car", "bus", "truck"]},
            "x-nw-labels": {"car": "Cars", "bus": "Buses", "truck": "Trucks"},
        },
        "zones": {"type": "array", "x-nw-widget": "nw:zones", "title": "Detection areas"},
        "areas": {
            "type": "array",
            "x-nw-widget": "nw:areas",
            "title": "Crossing areas",
            "maxItems": 2,
            "items": {"type": "object", "properties": {"points": {"maxItems": 4}}},
        },
        "lines": {"type": "array", "x-nw-widget": "nw:lines", "title": "Counting lines"},
        "lanes": {
            "type": "array",
            "x-nw-widget": "nw:lanes",
            "x-nw-lines": "lines",
            "title": "Lanes",
        },
        "fps": {"type": "integer", "minimum": 1, "maximum": 25, "default": 10},
    },
}


def _fields() -> list[FormField]:
    return form_fields(SCHEMA)


def _click(widget, x: float, y: float, button=Qt.MouseButton.LeftButton) -> None:
    rect = widget.image_rect()
    at = QPoint(round(rect.left() + x * rect.width()), round(rect.top() + y * rect.height()))
    QTest.mouseClick(widget, button, Qt.KeyboardModifier.NoModifier, at)


def _key_of(editor):
    from neurowatch.widgets.shape_editor import _key_entries

    return _key_entries(editor.shapes.fields)


def _manifest() -> Manifest:
    manifest = Manifest.from_dict(
        {
            "id": "nw.vcount",
            "name": "Vehicle counting",
            "vendor": "NeuroWatch",
            "version": "0.1.0",
            "config_schema": SCHEMA,
        }
    )
    assert manifest is not None
    return manifest


def test_the_form_knows_every_kind_of_setting_the_portal_shows() -> None:
    fields = {f.key: f for f in _fields()}
    assert fields["lanes"].kind == "lanes" and fields["lanes"].lines_key == "lines"
    assert fields["lines"].kind == "lines"
    assert fields["areas"].kind == "areas"
    assert fields["areas"].max_items == 2 and fields["areas"].max_points == 4
    assert fields["zones"].kind == "zones"
    many = fields["classes"]
    assert many.kind == "many" and many.choices == ["car", "bus", "truck"]
    assert many.choice_labels["bus"] == "Buses"


def test_shapes_keep_what_they_hold_and_say_what_is_missing() -> None:
    config = {
        "lanes": [{"id": "l1", "name": "To Galle", "points": []}],
        "lines": [
            {"id": "x", "lane": "l1", "points": LINE},
            {"id": "gate", "name": "Gate", "points": LINE, "in_label": "Enter", "note": "kept"},
        ],
        "areas": [{"id": "a", "name": "Bay", "points": [[0, 0], [1, 0], [1, 1]], "note": "kept"}],
        "zones": [
            {"id": "z", "name": "Road", "kind": "exclude", "points": [[0, 0], [1, 0], [0, 1]]}
        ],
    }
    shapes = ShapeSet(_fields(), config)
    assert [f.kind for f in shapes.fields] == ["lanes", "lines", "areas", "zones"]
    assert shapes.problem() is None
    saved = shapes.values()
    assert saved["lines"][0]["id"] == "gate" and saved["lines"][0]["note"] == "kept"
    assert saved["lines"][1]["lane"] == "l1" and saved["lines"][1]["name"] == "To Galle"
    assert saved["areas"][0]["note"] == "kept" and saved["zones"][0]["kind"] == "exclude"

    # The portal's words, for the same reasons, in its order: lanes first.
    fields = {f.kind: f for f in shapes.fields}
    shapes.items(fields["zones"])[0]["points"] = [[0, 0]]
    assert shapes.problem() == "Complete Road with 3 corners or more, or remove it."
    assert issue(fields["zones"], shapes.items(fields["zones"])[0]) == "needs 3 corners"
    shapes.items(fields["areas"])[0]["points"] = []
    assert shapes.problem() == "Complete Bay with 3 corners or more, or remove it."
    shapes.items(fields["lines"])[0]["points"] = [LINE[0], LINE[0]]
    assert shapes.problem() == "Draw Gate: two different points."
    assert issue(fields["lines"], shapes.items(fields["lines"])[0]) == "needs 2 points"
    shapes.lanes.lanes[0].line = []
    assert shapes.problem() == "Draw the crossing line of To Galle: two different points."
    assert issue(fields["lanes"], shapes.lanes.lanes[0]) == "needs a crossing line"

    # The list tells a lane's two parts apart.
    shapes.lanes.lanes[0].line = LINE
    assert lane_summary(shapes.lanes.lanes[0]) == "Crossing line, no area"
    shapes.lanes.lanes[0].area = [[0, 0], [1, 0], [1, 1]]
    assert lane_summary(shapes.lanes.lanes[0]) == "Crossing line and area"
    shapes.lanes.lanes[0].line = []
    assert lane_summary(shapes.lanes.lanes[0]) == "", "its issue says what is missing"

    # The schema's limit on how many.
    assert shapes.can_add(fields["areas"])
    assert shapes.add(fields["areas"]) is not None
    assert not shapes.can_add(fields["areas"]) and shapes.add(fields["areas"]) is None


def test_a_settings_value_that_is_not_a_list_reads_as_nothing() -> None:
    shapes = ShapeSet(_fields(), {"lanes": "junk", "lines": 3, "areas": {"a": 1}, "zones": None})
    assert all(shapes.items(f) == [] for f in shapes.fields)
    assert shapes.problem() is None


def test_draw_every_kind_of_shape_on_one_picture(qtbot) -> None:
    from neurowatch.widgets.shape_editor import AREA, ShapeEditor

    editor = ShapeEditor(_fields(), {})
    qtbot.addWidget(editor)
    editor.resize(1000, 560)
    editor.show()
    canvas = editor.canvas
    assert editor.tabs.value() == "lanes", "lanes come first"
    assert not editor.clear_button.isVisible() and not editor.name.isVisible()
    _click(canvas, 0.5, 0.5)  # nothing chosen: a click draws nothing
    assert editor.values()["lanes"] == []

    editor.add()
    assert editor.problem() == "Draw the crossing line of Lane 1: two different points."
    editor.name.setText("Towards Galle")
    editor.name.textEdited.emit("Towards Galle")
    _click(canvas, 0.2, 0.5)
    _click(canvas, 0.6, 0.5)
    _click(canvas, 0.9, 0.9)  # a line has two points: a third click adds nothing
    lane = editor.shapes.lanes.lanes[0]
    assert len(lane.line) == 2 and abs(lane.line[1][0] - 0.6) < 0.01
    assert editor.problem() is None
    editor.mode.set_value(AREA)
    for x, y in ((0.1, 0.3), (0.7, 0.3), (0.7, 0.95), (0.1, 0.95)):
        _click(canvas, x, y)
    _click(canvas, 0.1, 0.95, Qt.MouseButton.RightButton)
    assert len(lane.area) == 3

    editor.tabs.set_value("lines")
    assert editor.canvas.item is None and "Add a line" in editor.hint.text()
    editor.add()
    assert editor.problem() == "Draw Line 1: two different points."
    assert "needs 2 points" in editor.picker.item(0).text()
    _click(canvas, 0.1, 0.8)
    _click(canvas, 0.9, 0.8)
    editor.in_label.setText("Enter")
    editor.in_label.textEdited.emit("Enter")
    assert "needs" not in editor.picker.item(0).text()

    editor.tabs.set_value("areas")
    editor.add()
    assert editor.problem() == "Complete Area 1 with 3 corners or more, or remove it."
    for x, y in ((0.1, 0.1), (0.3, 0.1), (0.3, 0.3), (0.1, 0.3), (0.05, 0.2)):
        _click(canvas, x, y)
    area = editor.shapes.items(editor.field)[0]
    assert len(area["points"]) == 4, "the schema allows 4 corners"
    editor.add()
    assert not editor.add_button.isEnabled(), "the schema allows 2 areas"
    editor.remove()

    editor.tabs.set_value("zones")
    assert editor.add_ignored.isVisible()
    editor.add("exclude")
    assert editor.zone_kind.value() == "exclude"
    for x, y in ((0.5, 0.1), (0.7, 0.1), (0.7, 0.3)):
        _click(canvas, x, y)
    editor.zone_kind.set_value("include")
    assert editor.shapes.items(editor.field)[0]["kind"] == "include"
    editor.grab()  # paints every kind without failing

    assert editor.problem() is None
    saved = editor.values()
    assert saved["lanes"][0]["name"] == "Towards Galle" and len(saved["lanes"][0]["points"]) == 3
    assert [line.get("lane") for line in saved["lines"]] == [None, lane.id]
    assert saved["lines"][0]["in_label"] == "Enter"
    assert len(saved["areas"]) == 1 and len(saved["zones"]) == 1

    # What was drawn survives leaving and coming back.
    editor.tabs.set_value("lanes")
    assert editor.canvas.item is lane and editor.canvas.mode == "line"


def test_points_move_and_go_with_the_keyboard_as_well(qtbot) -> None:
    from neurowatch.widgets.shape_editor import ShapeEditor

    editor = ShapeEditor(_fields(), {})
    qtbot.addWidget(editor)
    editor.resize(1000, 560)
    editor.show()
    canvas = editor.canvas
    editor.add()
    _click(canvas, 0.2, 0.5)
    _click(canvas, 0.6, 0.5)
    line = editor.shapes.lanes.lanes[0].line
    start = list(line[1])
    canvas.setFocus()
    QTest.keyClick(canvas, Qt.Key.Key_Right)
    assert abs(line[1][0] - (start[0] + 0.01)) < 1e-6 and line[1][1] == start[1]
    QTest.keyClick(canvas, Qt.Key.Key_Up, Qt.KeyboardModifier.ShiftModifier)
    assert abs(line[1][1] - (start[1] - 0.05)) < 1e-6
    QTest.keyClick(canvas, Qt.Key.Key_Delete)
    assert len(line) == 1
    assert editor.problem() is not None
    # A point dragged is the one the keys then move.
    _click(canvas, 0.6, 0.5)
    _click(canvas, 0.2, 0.5)
    assert canvas.active_point == len(line) - 1 or canvas.active_point == 0
    # At the picture's edge a point stays on the picture.
    canvas.active_point = 0
    for _ in range(40):
        QTest.keyClick(canvas, Qt.Key.Key_Left, Qt.KeyboardModifier.ShiftModifier)
    assert line[0][0] == 0.0


def test_the_settings_form_saves_shapes_and_choices_together(qtbot) -> None:
    from neurowatch.dialogs.plugin_dialogs import BindingEditor, Camera

    existing = PluginBinding(
        "vcount",
        "c1",
        True,
        {
            "fps": 8,
            "classes": ["bus", "truck"],
            "lanes": [{"id": "l1", "name": "Lane A", "points": []}],
            "lines": [{"id": "x", "lane": "l1", "points": LINE}],
            "zones": [],
            "areas": [],
        },
    )
    editor = BindingEditor(_manifest(), [Camera("c1", "Road", {})], existing)
    qtbot.addWidget(editor)
    assert editor.shapes is not None and editor.zones is None, "all on one picture"
    assert editor.problem() is None
    config = editor.binding("vcount").config
    assert config["fps"] == 8 and config["classes"] == ["bus", "truck"]
    assert config["lanes"][0]["name"] == "Lane A"
    assert config["lines"][0]["lane"] == "l1" and config["lines"][0]["name"] == "Lane A"
    editor.shapes.add()
    assert editor.problem() == "Draw the crossing line of Lane 2: two different points."
    assert "crossing line of Lane 2" in editor.shapes.status.text()


def test_a_plugin_with_only_zones_keeps_the_simple_editor(qtbot) -> None:
    from neurowatch.dialogs.plugin_dialogs import BindingEditor, Camera

    manifest = Manifest.from_dict(
        {
            "id": "nw.motion",
            "name": "Motion",
            "vendor": "NeuroWatch",
            "version": "0.1.0",
            "config_schema": {
                "type": "object",
                "properties": {"zones": {"type": "array", "x-nw-widget": "nw:zones"}},
            },
        }
    )
    assert manifest is not None
    editor = BindingEditor(manifest, [Camera("c1", "Road", {})], None)
    qtbot.addWidget(editor)
    assert editor.zones is not None and editor.shapes is None


def test_the_picture_stays_put_between_tabs_and_the_list_never_scrolls_sideways(qtbot) -> None:
    from PySide6.QtWidgets import QApplication

    from neurowatch.widgets.shape_editor import ShapeEditor

    editor = ShapeEditor(_fields(), {})
    qtbot.addWidget(editor)
    editor.resize(1000, 700)
    editor.show()
    QApplication.processEvents()
    widths = set()
    for key in ("lanes", "lines", "areas", "zones"):
        editor.tabs.set_value(key)
        editor.add()
        QApplication.processEvents()
        widths.add(editor.canvas.width())
    assert len(widths) == 1, "the side column's contents must not move the picture"
    editor.tabs.set_value("lanes")
    long_name = "A very long lane name going towards the city centre and beyond"
    editor.name.setText(long_name)
    editor.name.textEdited.emit(long_name)
    QApplication.processEvents()
    assert editor.picker.horizontalScrollBar().maximum() == 0 or (
        editor.picker.horizontalScrollBarPolicy() == Qt.ScrollBarPolicy.ScrollBarAlwaysOff
    )
    rows = {editor.picker.item(i).sizeHint().height() for i in range(editor.picker.count())}
    assert len(rows) == 1, "rows the same height, finished or not"


def test_vehicle_counting_has_lanes_and_detection_areas_and_no_other_tabs(qtbot) -> None:
    """The plugin lists its lanes' crossing lines without a form of their own."""
    from neurowatch.dialogs.plugin_dialogs import BindingEditor, Camera

    schema = {
        "type": "object",
        "properties": {
            "lines": {"type": "array", "items": {"type": "object"}},
            "lanes": {
                "type": "array",
                "x-nw-widget": "nw:lanes",
                "x-nw-lines": "lines",
                "title": "Lanes",
            },
            "detection_areas": {
                "type": "array",
                "x-nw-widget": "nw:zones",
                "title": "Detection areas",
            },
        },
    }
    manifest = Manifest.from_dict(
        {
            "id": "nw.vcount",
            "name": "Vehicle counting",
            "vendor": "NeuroWatch",
            "version": "0.1.0",
            "config_schema": schema,
        }
    )
    assert manifest is not None
    saved = {
        "lanes": [{"id": "l1", "name": "To Galle", "points": []}],
        "lines": [
            {"id": "x", "lane": "l1", "points": LINE},
            {"id": "old", "name": "Gate", "points": LINE},
        ],
        # What an earlier version saved: not shown, and left as it was.
        "crossing_areas": [{"id": "a", "name": "Bay", "points": [[0, 0], [1, 0], [1, 1]]}],
    }
    editor = BindingEditor(
        manifest, [Camera("c1", "Road", {})], PluginBinding("v", "c1", True, saved)
    )
    qtbot.addWidget(editor)
    assert editor.shapes is not None
    assert [b.text() for b in editor.shapes.tabs.buttons()] == ["Lanes", "Detection areas"]
    assert [text for text, _, _ in _key_of(editor.shapes)] == [
        "Crossing line",
        "Lane area",
        "Watched",
        "Ignored",
    ]
    assert editor.problem() is None
    config = editor.binding("v").config
    assert {line["id"] for line in config["lines"]} == {"x", "old"}
    assert config["crossing_areas"] == saved["crossing_areas"]


def test_the_settings_fit_a_narrow_page_and_a_small_screen(qtbot) -> None:
    from PySide6.QtWidgets import QScrollArea

    from neurowatch.dialogs.plugin_dialogs import BindingDialog, BindingEditor, Camera

    schema = {**SCHEMA}
    layers = [
        {"id": "a", "label": "Vehicles and people"},
        {"id": "b", "label": "Counting lines, crossing areas and lanes"},
        {"id": "c", "label": "Analysis (trails)"},
        {"id": "d", "label": "Detection areas (watched, ignored)"},
    ]
    manifest = Manifest.from_dict(
        {
            "id": "nw.vcount",
            "name": "Vehicle counting",
            "vendor": "NeuroWatch",
            "version": "0.1.0",
            "config_schema": schema,
            "overlay_layers": layers,
        }
    )
    assert manifest is not None
    editor = BindingEditor(manifest, [Camera("c1", "Road", {})], None)
    qtbot.addWidget(editor)
    assert editor.minimumSizeHint().width() <= 760, "the page scrolls down, never sideways"

    # The dialog's settings scroll, rather than asking for more than a screen has:
    # a window held smaller than its contents lays them over each other.
    dialog = BindingDialog("Vehicle counting", manifest, [Camera("c1", "Road", {})])
    qtbot.addWidget(dialog)
    assert (
        isinstance(dialog.settings_area, QScrollArea)
        and dialog.settings_area.widget() is dialog.editor
    )
    assert dialog.minimumSizeHint().height() < 600


def test_checkboxes_show_their_card_rather_than_the_windows_background() -> None:
    """The main window's background reaches every child that has no rule of its own:
    each checkbox on the page was a dark patch over its card."""
    from neurowatch.theme import plugins_stylesheet

    assert "QWidget#pluginsView QCheckBox, QWidget#pluginsView QRadioButton" in (
        plugins_stylesheet("dark")
    )
