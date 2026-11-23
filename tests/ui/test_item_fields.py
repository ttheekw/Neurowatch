"""Extra per-area (``nw:zones``) and per-class (``nw:classes``) settings fields,
and the Detection tag on events a plugin marked as detections."""

from __future__ import annotations

from typing import Any

from PySide6.QtGui import QImage

from neurowatch.dialogs.plugin_dialogs import BindingEditor, _ClassesPicker
from neurowatch.models.events import Event
from neurowatch.models.plugins import form_fields
from neurowatch.widgets.zone_editor import ZoneEditor

CLASS_IDS = ["person", "car", "dog"]
LABELS = {"person": "Person", "car": "Car", "dog": "Dog"}
SQUARE = [[0.1, 0.1], [0.9, 0.1], [0.9, 0.9], [0.1, 0.9]]


def _classes_prop() -> dict[str, Any]:
    return {
        "type": "array",
        "x-nw-widget": "nw:classes",
        "title": "Objects",
        "items": {
            "type": "object",
            "properties": {
                "id": {"enum": CLASS_IDS, "x-nw-labels": LABELS},
                "confidence": {"type": "integer", "minimum": 10, "maximum": 95, "default": 50},
                "confirm": {
                    "type": "integer",
                    "title": "Confirmed (%)",
                    "description": "Median of recent scores",
                    "minimum": 10,
                    "maximum": 99,
                    "default": 70,
                },
                "min_size": {"type": "number", "title": "Smallest", "minimum": 0, "maximum": 100},
                "max_size": {
                    "type": "number",
                    "title": "Largest",
                    "minimum": 0,
                    "maximum": 100,
                    "default": 100,
                },
                "min_ratio": {"type": "number", "minimum": 0, "maximum": 20},
                "max_ratio": {"type": "number", "minimum": 0, "maximum": 20, "default": 20},
                "note": {"type": "string"},
            },
        },
    }


def _zones_prop() -> dict[str, Any]:
    return {
        "type": "array",
        "x-nw-widget": "nw:zones",
        "title": "Areas",
        "items": {
            "type": "object",
            "properties": {
                "id": {"type": "string"},
                "name": {"type": "string"},
                "kind": {"enum": ["include", "exclude"]},
                "points": {"type": "array", "maxItems": 32},
                "classes": {
                    "type": "array",
                    "title": "Objects",
                    "description": "Empty: all",
                    "x-nw-zone-kinds": ["include"],
                    "items": {"enum": CLASS_IDS, "x-nw-labels": LABELS},
                },
                "frames": {
                    "type": "integer",
                    "title": "Pictures in a row",
                    "minimum": 1,
                    "maximum": 30,
                    "default": 3,
                    "x-nw-zone-kinds": ["include"],
                },
                "loiter_sec": {
                    "type": "number",
                    "minimum": 0,
                    "maximum": 3600,
                    "default": 0,
                    "x-nw-zone-kinds": ["include"],
                },
                "sides_m": {
                    "type": "array",
                    "title": "Sides (m)",
                    "minItems": 4,
                    "maxItems": 4,
                    "items": {"type": "number", "minimum": 0},
                    "default": [0, 0, 0, 0],
                    "x-nw-zone-kinds": ["include"],
                },
                "events": {"type": "boolean", "default": True, "x-nw-zone-kinds": ["include"]},
                "alerts": {"type": "boolean", "default": True, "x-nw-zone-kinds": ["include"]},
                "odd": {"type": "object"},
            },
        },
    }


def _schema() -> dict[str, Any]:
    return {"properties": {"classes": _classes_prop(), "zones": _zones_prop()}}


def _fields() -> dict[str, Any]:
    return {f.key: f for f in form_fields(_schema())}


def test_class_fields_are_parsed_with_bounds_and_defaults() -> None:
    extras = {f.key: f for f in _fields()["classes"].class_fields}
    assert list(extras) == ["confirm", "min_size", "max_size", "min_ratio", "max_ratio"]
    assert extras["confirm"].kind == "int"
    assert (extras["confirm"].minimum, extras["confirm"].maximum) == (10, 99)
    assert extras["confirm"].default == 70
    assert extras["confirm"].description == "Median of recent scores"
    assert extras["min_size"].kind == "float"
    assert extras["min_size"].initial() == 0
    assert extras["max_ratio"].initial() == 20
    assert extras["min_ratio"].title == "Min ratio"


def test_area_fields_are_parsed_by_type_and_kind() -> None:
    extras = {f.key: f for f in _fields()["zones"].area_fields}
    assert list(extras) == ["classes", "frames", "loiter_sec", "sides_m", "events", "alerts"]
    assert "odd" not in extras  # unknown types are not shown
    assert extras["classes"].kind == "many"
    assert extras["classes"].choices == CLASS_IDS
    assert extras["classes"].choice_labels["car"] == "Car"
    assert extras["classes"].initial() == []
    assert extras["frames"].initial() == 3
    assert extras["sides_m"].kind == "numbers"
    assert extras["sides_m"].length == 4
    assert extras["sides_m"].initial() == [0, 0, 0, 0]
    assert extras["events"].kind == "bool" and extras["events"].initial() is True
    assert all(f.applies_to("include") and not f.applies_to("exclude") for f in extras.values())


def test_a_wrong_stored_value_falls_back_to_the_default() -> None:
    extras = {f.key: f for f in _fields()["zones"].area_fields}
    assert extras["frames"].value_of("many") == 3
    assert extras["sides_m"].value_of([1, 2]) == [0, 0, 0, 0]
    assert extras["classes"].value_of(["car", "gone"]) == ["car"]
    assert extras["events"].value_of(None) is True


def _editor(qtbot, zones: list[dict[str, Any]]) -> ZoneEditor:
    editor = ZoneEditor(zones, area_fields=_fields()["zones"].area_fields)
    qtbot.addWidget(editor)
    editor.show()
    return editor


def test_zone_editor_saves_extra_fields_into_each_area(qtbot) -> None:
    editor = _editor(
        qtbot,
        [
            {"id": "a", "name": "Gate", "kind": "include", "points": SQUARE},
            {"id": "b", "name": "Wall", "kind": "exclude", "points": SQUARE},
        ],
    )
    # A missing value saves the default; an excluded area gets no extras.
    saved = editor.zones()
    assert saved[0]["frames"] == 3 and saved[0]["classes"] == []
    assert saved[0]["sides_m"] == [0, 0, 0, 0]
    assert saved[0]["events"] is True and saved[0]["alerts"] is True
    assert "frames" not in saved[1]

    editor.picker.setCurrentRow(0)
    inputs = editor._area_inputs
    inputs["frames"].setValue(7)
    inputs["loiter_sec"].setValue(12.5)
    inputs["alerts"].setChecked(False)
    inputs["classes"].boxes["car"].setChecked(True)
    for spin, value in zip(inputs["sides_m"].spins, (4, 3, 4, 3), strict=True):
        spin.setValue(value)
    editor.picker.setCurrentRow(1)
    assert editor.area_field_shown("frames") is False
    editor.picker.setCurrentRow(0)
    assert inputs["frames"].value() == 7  # shown again from the area's own values
    first, second = editor.zones()
    assert first["frames"] == 7 and first["loiter_sec"] == 12.5
    assert first["alerts"] is False and first["events"] is True
    assert first["classes"] == ["car"]
    assert first["sides_m"] == [4, 3, 4, 3]
    assert "frames" not in second


def test_zone_editor_without_extra_fields_leaves_areas_alone(qtbot) -> None:
    zone = {"id": "a", "name": "Gate", "kind": "include", "points": SQUARE, "odd": {"x": 1}}
    editor = ZoneEditor([zone])
    qtbot.addWidget(editor)
    assert editor.zones() == [zone]


def test_binding_editor_hands_area_fields_to_the_zone_editor(qtbot) -> None:
    from neurowatch.models.plugins import Manifest

    manifest = Manifest.from_dict(
        {"id": "p", "name": "P", "config_schema": _schema()}  # type: ignore[arg-type]
    )
    editor = BindingEditor(manifest, [], None)
    qtbot.addWidget(editor)
    assert editor.zones is not None
    assert "frames" in editor.zones._area_inputs


def test_classes_picker_saves_per_class_values(qtbot) -> None:
    field = _fields()["classes"]
    picker = _ClassesPicker(
        field,
        [{"id": "person", "confidence": 60, "confirm": 80, "min_size": 2.5, "note": "keep"}],
    )
    qtbot.addWidget(picker)
    assert picker.extra_input("person", "confirm").value() == 80
    assert picker.extra_input("car", "confirm").value() == 70  # missing: the default
    assert picker.extra_input("car", "max_ratio").value() == 20
    assert picker.extra_input("car", "confirm").toolTip() == "Median of recent scores"
    picker.boxes["car"].setChecked(True)
    picker.extra_input("car", "max_size").setValue(40)
    picker.extra_input("person", "confirm").setValue(90)
    values = picker.values()
    assert values[0] == {
        "id": "person",
        "confidence": 60,
        "confirm": 90,
        "min_size": 2.5,
        "max_size": 100,
        "min_ratio": 0,
        "max_ratio": 20,
        "note": "keep",
    }
    assert values[1]["id"] == "car" and values[1]["max_size"] == 40
    assert values[1]["confirm"] == 70 and values[1]["confidence"] == 50


def test_event_detection_flag_defaults_to_false() -> None:
    raw = {"id": 1, "kind": "object", "start_ms": 1000}
    event = Event.from_dict(raw)
    assert event is not None and event.detection is False
    event = Event.from_dict({**raw, "detection": True})
    assert event is not None and event.detection is True


def _row_image(qtbot, detection: bool) -> QImage:
    from neurowatch.widgets.event_list import EventImageLoader, EventListView

    view = EventListView(EventImageLoader(None))
    qtbot.addWidget(view)
    view.resize(520, 120)
    raw = {"id": 1, "kind": "object", "start_ms": 1_000_000, "end_ms": 2_000_000, "detail": "Car"}
    event = Event.from_dict({**raw, "detection": detection})
    assert event is not None
    view.set_events([event])
    view.show()
    return view.grab().toImage()


def test_a_detection_row_carries_a_tag(qtbot) -> None:
    from PySide6.QtGui import QFont

    from neurowatch.widgets.event_list import detection_tag_width

    assert detection_tag_width(QFont()) > 20
    assert _row_image(qtbot, True) != _row_image(qtbot, False)


def test_the_alert_rules_say_detections_never_send_alerts(qtbot, monkeypatch) -> None:
    from neurowatch.dialogs import alerts_dialog

    class Client:
        def __init__(self, endpoint: str | None = None) -> None:
            pass

        def call(self, method: str, params: Any = None) -> Any:
            return {"deliveries": []} if method == "alerts.history" else {}

    monkeypatch.setattr(alerts_dialog, "SyncIpcClient", Client)
    dialog = alerts_dialog.AlertsDialog(None, [("cam-1", "Gate")])
    qtbot.addWidget(dialog)
    assert "never send alerts" in dialog._detections_note.text()
