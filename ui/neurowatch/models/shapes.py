"""Everything a plugin's settings draw on the camera's picture: lanes (with their
crossing lines), counting lines, areas. The portal's ``shapes.ts`` follows the same
rules, so a shape is finished, or not, for the same reason in both.

No Qt: the desktop's drawing editor and tests use it.
"""

from __future__ import annotations

import uuid
from copy import deepcopy
from typing import Any

from neurowatch.models.lanes import Lane, LaneSet, _points
from neurowatch.models.plugins import FormField

#: What the form draws (``models.plugins.form_fields``), in the order it shows them.
DRAWN_KINDS = ("lanes", "lines", "areas", "zones")
DEFAULT_MAX_ITEMS = 32
DEFAULT_MAX_POINTS = 64

Item = Lane | dict[str, Any]


def _distinct(points: list[list[float]]) -> bool:
    return len(points) == 2 and points[0] != points[1]


class ShapeSet:
    """A binding's shapes while they are edited. What else an entry holds stays."""

    def __init__(self, fields: list[FormField], config: dict[str, Any]) -> None:
        self.fields = sorted(
            (f for f in fields if f.kind in DRAWN_KINDS), key=lambda f: DRAWN_KINDS.index(f.kind)
        )
        lanes = next((f for f in self.fields if f.kind == "lanes"), None)
        lines = next((f for f in self.fields if f.kind == "lines"), None)
        self._lanes_key = lanes.key if lanes is not None else ""
        self._lines_key = (
            lanes.lines_key if lanes is not None else lines.key if lines is not None else ""
        )
        self.lanes = LaneSet.from_config(
            config.get(self._lanes_key) if lanes is not None else [],
            config.get(self._lines_key) if self._lines_key else [],
        )
        for line in self.lanes.other_lines:
            line["points"] = _points(line.get("points"), 2)
            line.setdefault("id", uuid.uuid4().hex[:16])
        self._areas: dict[str, list[dict[str, Any]]] = {}
        for field in self.fields:
            if field.kind in ("areas", "zones"):
                raw = config.get(field.key)
                self._areas[field.key] = [
                    {**deepcopy(entry), "points": _points(entry.get("points"), _limit(field))}
                    for entry in (raw if isinstance(raw, list) else [])
                    if isinstance(entry, dict)
                ]

    def items(self, field: FormField) -> list[Any]:
        if field.kind == "lanes":
            return self.lanes.lanes
        if field.kind == "lines":
            return self.lanes.other_lines
        return self._areas.get(field.key, [])

    def can_add(self, field: FormField) -> bool:
        return len(self.items(field)) < (field.max_items or DEFAULT_MAX_ITEMS)

    def add(self, field: FormField, kind: str = "include") -> Item | None:
        """A new, empty entry; None at its limit."""
        if not self.can_add(field):
            return None
        number = len(self.items(field)) + 1
        ident = uuid.uuid4().hex[:16]
        item: Item
        if field.kind == "lanes":
            item = Lane.new(number)
        elif field.kind == "lines":
            item = {
                "id": ident,
                "name": f"Line {number}",
                "points": [],
                "in_label": "In",
                "out_label": "Out",
            }
        elif field.kind == "zones":
            word = "Ignored" if kind == "exclude" else "Watched"
            item = {"id": ident, "name": f"{word} {number}", "kind": kind, "points": []}
        else:
            item = {"id": ident, "name": f"Area {number}", "points": []}
        self.items(field).append(item)
        return item

    def remove(self, field: FormField, item: Item) -> None:
        self.items(field).remove(item)

    def problem(self) -> str | None:
        """Why these cannot be saved, or None: the first thing to finish."""
        for field in self.fields:
            if field.kind == "lanes":
                text = self.lanes.problem()
                if text is not None:
                    return text
            elif field.kind == "lines":
                for number, line in enumerate(self.lanes.other_lines, 1):
                    if not _distinct(line["points"]):
                        name = str(line.get("name") or "").strip() or f"counting line {number}"
                        return f"Draw {name}: two different points."
            else:
                for number, area in enumerate(self._areas.get(field.key, []), 1):
                    if len(area["points"]) < 3:
                        name = str(area.get("name") or "").strip() or f"{field.title} {number}"
                        return f"Complete {name} with 3 corners or more, or remove it."
        return None

    def values(self) -> dict[str, Any]:
        """The settings to save."""
        values: dict[str, Any] = {key: deepcopy(areas) for key, areas in self._areas.items()}
        if self._lanes_key:
            values[self._lanes_key] = self.lanes.lanes_config()
        if self._lines_key:
            values[self._lines_key] = self.lanes.lines_config()
        return values


def issue(field: FormField, item: Item) -> str | None:
    """What this one still needs, in a few words, or None."""
    if isinstance(item, Lane):
        if not item.name.strip():
            return "needs a name"
        if not _distinct(item.line):
            return "needs a crossing line"
        return "area needs 3 corners" if 0 < len(item.area) < 3 else None
    if field.kind == "lines":
        return None if _distinct(item["points"]) else "needs 2 points"
    return None if len(item["points"]) >= 3 else "needs 3 corners"


def lane_summary(lane: Lane) -> str:
    """What a finished lane has, so the list tells its two parts apart."""
    if not _distinct(lane.line):
        return ""
    return "Crossing line and area" if len(lane.area) >= 3 else "Crossing line, no area"


def _limit(field: FormField) -> int:
    return field.max_points or DEFAULT_MAX_POINTS
