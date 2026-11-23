"""Plugin settings form: x-nw-order, long choice lists that scroll, area fields."""

from __future__ import annotations

from typing import Any

from PySide6.QtWidgets import QCheckBox

from neurowatch.dialogs.plugin_dialogs import BindingEditor
from neurowatch.models.plugins import (
    apply_order,
    chosen_first,
    extra_fields,
    form_fields,
    needs_scroll,
)
from neurowatch.widgets.choice_list import ChoiceList
from neurowatch.widgets.zone_editor import ZoneEditor

SQUARE = [[0.1, 0.1], [0.9, 0.1], [0.9, 0.9], [0.1, 0.9]]
PENTAGON = [[0.1, 0.1], [0.9, 0.1], [0.9, 0.9], [0.5, 0.95], [0.1, 0.9]]


def test_apply_order_puts_named_keys_first() -> None:
    assert apply_order(["a", "b", "c", "d"], ["c", "x", "a", "c"]) == ["c", "a", "b", "d"]
    assert apply_order(["a", "b"], []) == ["a", "b"]


def test_form_fields_follow_x_nw_order_at_the_top_level() -> None:
    schema: dict[str, Any] = {
        "x-nw-order": ["zeta", "alpha"],
        "properties": {
            "alpha": {"type": "integer"},
            "mid": {"type": "integer"},
            "zeta": {"type": "boolean"},
        },
    }
    assert [f.key for f in form_fields(schema)] == ["zeta", "alpha", "mid"]
    del schema["x-nw-order"]
    assert [f.key for f in form_fields(schema)] == ["alpha", "mid", "zeta"]


def test_item_fields_follow_x_nw_order() -> None:
    item = {
        "x-nw-order": ["b"],
        "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}},
    }
    assert [f.key for f in extra_fields(item, frozenset())] == ["b", "a"]


def test_scroll_threshold_and_chosen_first() -> None:
    assert not needs_scroll(8) and needs_scroll(9)
    assert chosen_first(["a", "b", "c", "d"], ["d", "b"]) == ["b", "d", "a", "c"]


def test_a_long_choice_list_scrolls_and_searches(qtbot) -> None:
    names = [f"item{i}" for i in range(20)]
    short = ChoiceList(names[:5])
    long = ChoiceList(names, chosen=["item7"])
    qtbot.addWidget(short)
    qtbot.addWidget(long)
    assert not short.scrolls and long.scrolls
    assert long.summary.text() == "1 of 20 chosen"
    long.show()
    long.search.setText("item1")
    assert long.boxes["item1"].isVisible() and not long.boxes["item2"].isVisible()
    long.search.clear()
    long.chosen_only.setChecked(True)
    assert [c for c, b in long.boxes.items() if b.isVisible()] == ["item7"]
    assert long.values() == ["item7"]


def _zones_schema() -> dict[str, Any]:
    return {
        "type": "array",
        "x-nw-widget": "nw:zones",
        "items": {
            "x-nw-order": ["sides_m", "frames"],
            "properties": {
                "frames": {"type": "integer", "title": "Frames", "minimum": 1, "default": 3},
                "sides_m": {
                    "type": "array",
                    "title": "Side lengths (metres)",
                    "items": {"type": "number", "minimum": 0},
                    "minItems": 4,
                    "maxItems": 4,
                },
            },
        },
    }


def test_side_lengths_only_for_a_four_corner_area(qtbot) -> None:
    fields = extra_fields(_zones_schema()["items"], frozenset({"id", "name", "kind", "points"}))
    assert [f.key for f in fields] == ["sides_m", "frames"]
    editor = ZoneEditor(
        [
            {"id": "a", "name": "Four", "kind": "include", "points": SQUARE},
            {"id": "b", "name": "Five", "kind": "include", "points": PENTAGON},
        ],
        area_fields=fields,
    )
    qtbot.addWidget(editor)
    editor.picker.setCurrentRow(0)
    assert editor.area_field_shown("sides_m") and editor.area_field_shown("frames")
    assert editor.sides_note.isHidden()
    editor.picker.setCurrentRow(1)
    assert not editor.area_field_shown("sides_m") and editor.area_field_shown("frames")
    assert not editor.sides_note.isHidden()


def test_binding_editor_shows_every_description(qtbot) -> None:
    from neurowatch.models.plugins import Manifest

    schema = {
        "properties": {
            "n": {"type": "integer", "title": "N", "description": "How many"},
            "pick": {
                "type": "array",
                "title": "Pick",
                "description": "Which ones",
                "items": {"enum": [f"o{i}" for i in range(12)]},
            },
        }
    }
    manifest = Manifest.from_dict({"id": "p", "name": "P", "config_schema": schema})
    editor = BindingEditor(manifest, [], None)
    qtbot.addWidget(editor)
    from PySide6.QtWidgets import QLabel

    texts = {label.text() for label in editor.findChildren(QLabel)}
    assert {"How many", "Which ones"} <= texts


def test_a_lists_labels_kept_with_its_options_are_shown() -> None:
    """Detect's "Alerts for" keeps its labels in ``items``: shown as "Traffic
    light", not the id ``traffic_light`` (the desktop showed ids, 2026-11-23)."""
    from neurowatch.models.plugins import form_fields

    schema = {
        "properties": {
            "alert_classes": {
                "type": "array",
                "items": {
                    "enum": ["person", "traffic_light"],
                    "x-nw-labels": {"person": "Person", "traffic_light": "Traffic light"},
                },
            }
        }
    }
    (field,) = form_fields(schema)
    assert field.kind == "many"
    assert field.choice_labels == {"person": "Person", "traffic_light": "Traffic light"}


def test_shape_summary_says_any_size_until_limited() -> None:
    from neurowatch.models.plugins import shape_summary

    defaults = {"min_size": 0, "max_size": 100, "min_ratio": 0, "max_ratio": 20}
    assert shape_summary(dict(defaults), defaults) == "Any size"
    assert shape_summary({**defaults, "min_size": 1, "max_size": 20}, defaults) == "1–20%"
    both = {**defaults, "min_size": 1, "max_size": 20, "min_ratio": 0.2, "max_ratio": 1.0}
    assert shape_summary(both, defaults) == "1–20%, 0.2–1"


def _detect_editor():
    from neurowatch.models.plugins import Manifest, PluginBinding

    classes = [f"class{i}" for i in range(80)]
    props = {
        "classes": {
            "type": "array",
            "x-nw-widget": "nw:classes",
            "title": "Detect",
            "items": {
                "x-nw-order": ["confidence", "confirm", "min_size", "max_size"],
                "properties": {
                    "id": {"enum": classes},
                    "confidence": {"type": "integer", "minimum": 10, "maximum": 95},
                    "confirm": {"type": "integer", "title": "Confirmed", "default": 70},
                    "min_size": {"type": "number", "title": "Smallest", "default": 0},
                    "max_size": {"type": "number", "title": "Largest", "default": 100},
                },
            },
        },
        "alert_classes": {
            "type": "array",
            "title": "Alerts for",
            "items": {"enum": classes},
        },
        "zones": {
            "type": "array",
            "x-nw-widget": "nw:zones",
            "items": {
                "properties": {
                    "classes": {
                        "type": "array",
                        "title": "Applies to",
                        "items": {"enum": classes},
                    }
                }
            },
        },
    }
    manifest = Manifest.from_dict({"id": "d", "name": "D", "config_schema": {"properties": props}})
    square = [[0.1, 0.1], [0.5, 0.1], [0.5, 0.5], [0.1, 0.5]]
    binding = PluginBinding(
        "d",
        "c1",
        True,
        {
            "classes": [{"id": "class1"}, {"id": "class2"}],
            "zones": [{"id": "a", "name": "A", "kind": "include", "points": square}] * 2,
        },
    )
    return BindingEditor(
        manifest, [], binding, endpoint="unix:/nonexistent/none.sock"
    )  # a dead endpoint: nothing reaches a real service


def test_opening_the_form_with_80_classes_is_quick(qtbot) -> None:
    """The form once built every class's inputs up front (seconds of freeze)."""
    import time

    start = time.perf_counter()
    editor = _detect_editor()
    elapsed = time.perf_counter() - start
    qtbot.addWidget(editor)
    assert elapsed < 1.0, f"building the form took {elapsed:.2f} s"
    picker = editor._inputs["classes"]
    assert picker._boxes is None, "rows wait until the list appears"
    assert len(editor.findChildren(QCheckBox)) < 60, "no 80-box lists built up front"
    assert picker.values()[0]["id"] == "class1"


def test_a_class_panel_is_made_only_when_opened(qtbot) -> None:
    editor = _detect_editor()
    qtbot.addWidget(editor)
    picker = editor._inputs["classes"]
    assert picker.boxes  # rows made
    assert picker._panels == {}
    picker._shape_buttons["class1"].setChecked(True)
    assert list(picker._panels) == ["class1"]
    picker.extra_input("class1", "min_size").setValue(5)
    assert picker._shape_buttons["class1"].text().endswith("5–100%")
    entry = picker.values()[0]
    assert entry["min_size"] == 5 and entry["confirm"] == 70
