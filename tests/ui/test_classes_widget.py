"""The ``nw:classes`` setting, and events of a plugin's own classes."""

from __future__ import annotations

from typing import Any

from neurowatch.dialogs.plugin_dialogs import _ClassesPicker
from neurowatch.models.alerts import KIND_LABELS
from neurowatch.models.events import ACTIVITY_KINDS, Event, kinds_text, label
from neurowatch.models.plugins import form_fields

IDS = ["person", "bicycle", "car", "forklift"]


def _schema(*, confidence: bool = True) -> dict[str, Any]:
    props: dict[str, Any] = {
        "id": {
            "enum": IDS,
            "x-nw-labels": {"person": "Person", "car": "Car", "forklift": "Forklift"},
        }
    }
    if confidence:
        props["confidence"] = {
            "type": "integer",
            "minimum": 10,
            "maximum": 95,
            "default": 50,
            "x-nw-defaults": {"car": 60},
        }
    return {
        "properties": {
            "classes": {
                "type": "array",
                "x-nw-widget": "nw:classes",
                "title": "Detect",
                "default": [{"id": "person", "confidence": 50}],
                "items": {"type": "object", "properties": props},
            }
        }
    }


def test_the_schema_reads_as_a_classes_field() -> None:
    (field,) = form_fields(_schema())
    assert field.kind == "classes"
    assert field.choices == IDS
    assert field.choice_labels["car"] == "Car"
    assert field.has_confidence
    assert (field.confidence_min, field.confidence_max, field.confidence_default) == (10, 95, 50)
    assert field.class_defaults == {"car": 60}
    (plain,) = form_fields(_schema(confidence=False))
    assert not plain.has_confidence


def test_values_round_trip_keep_unknown_ids_and_restore_confidence(qtbot) -> None:
    (field,) = form_fields(_schema())
    saved = [{"id": "person", "confidence": 70}, {"id": "gone", "confidence": 33}]
    picker = _ClassesPicker(field, saved)
    qtbot.addWidget(picker)
    assert picker.values() == [{"id": "person", "confidence": 70}, {"id": "gone", "confidence": 33}]
    picker.boxes["car"].setChecked(True)
    assert picker.spins["car"].value() == 60  # its own default
    picker.boxes["forklift"].setChecked(True)
    assert picker.spins["forklift"].value() == 50  # the field's
    picker.spins["car"].setValue(80)
    picker.boxes["car"].setChecked(False)
    assert not picker.spins["car"].isEnabled()
    picker.boxes["car"].setChecked(True)
    assert picker.spins["car"].value() == 80
    assert picker.values() == [
        {"id": "person", "confidence": 70},
        {"id": "car", "confidence": 80},
        {"id": "forklift", "confidence": 50},
        {"id": "gone", "confidence": 33},
    ]


def test_without_per_class_confidence_only_ids_are_saved(qtbot) -> None:
    (field,) = form_fields(_schema(confidence=False))
    picker = _ClassesPicker(field, field.default)
    qtbot.addWidget(picker)
    assert not picker.spins
    assert picker.values() == [{"id": "person"}]


def test_search_and_chosen_only_filter_the_rows(qtbot) -> None:
    (field,) = form_fields(_schema())
    picker = _ClassesPicker(field, [{"id": "person"}])
    qtbot.addWidget(picker)
    picker.show()
    picker.search.setText("fork")
    assert [c for c, b in picker.boxes.items() if b.isVisible()] == ["forklift"]
    picker.search.setText("")
    picker.chosen_only.setChecked(True)
    assert [c for c, b in picker.boxes.items() if b.isVisible()] == ["person"]


def test_object_events_read_as_their_class() -> None:
    event = Event.from_dict(
        {"id": 1, "kind": "object", "start_ms": 1, "detail": "Car", "source": "plugin"}
    )
    assert event is not None
    assert event.label == "Object" == label("object")
    assert event.describe("Front door") == "Car"
    assert "object" in ACTIVITY_KINDS
    assert kinds_text(["person", "object"]) == "people and objects"
    assert "object" in KIND_LABELS
