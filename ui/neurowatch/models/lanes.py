"""Lanes as a plugin's settings hold them (``x-nw-widget: nw:lanes``): each
lane named, with its crossing line (an entry of the companion lines field,
``x-nw-lines``, whose ``lane`` is the lane's id) and, optionally, its area.

No Qt: the desktop lane editor and tests use it.
"""

from __future__ import annotations

import math
import uuid
from copy import deepcopy
from dataclasses import dataclass, field
from typing import Any

MAX_LANES = 32
MAX_AREA_POINTS = 64


def _point(value: object) -> list[float] | None:
    if (
        isinstance(value, (list, tuple))
        and len(value) == 2
        and all(
            isinstance(v, (int, float)) and not isinstance(v, bool) and math.isfinite(v)
            for v in value
        )
    ):
        return [min(1.0, max(0.0, float(value[0]))), min(1.0, max(0.0, float(value[1])))]
    return None


def _points(value: object, limit: int) -> list[list[float]]:
    if not isinstance(value, list):
        return []
    return [p for p in (_point(v) for v in value[:limit]) if p is not None]


@dataclass
class Lane:
    """One lane: its name, area corners (may be empty) and crossing line."""

    id: str
    name: str
    area: list[list[float]] = field(default_factory=list)
    #: The crossing line's two points (fewer while it is being drawn).
    line: list[list[float]] = field(default_factory=list)
    line_id: str = ""
    #: What else the saved line held (kept as it was).
    line_extra: dict[str, Any] = field(default_factory=dict)

    @staticmethod
    def new(number: int) -> Lane:
        return Lane(id=uuid.uuid4().hex[:16], name=f"Lane {number}", line_id=uuid.uuid4().hex[:16])


@dataclass
class LaneSet:
    """A binding's lanes and the lines that are not theirs (kept as they are)."""

    lanes: list[Lane] = field(default_factory=list)
    other_lines: list[dict[str, Any]] = field(default_factory=list)
    #: What else each saved lane held, by id (kept as it was).
    lane_extra: dict[str, dict[str, Any]] = field(default_factory=dict)

    @staticmethod
    def from_config(lanes: object, lines: object) -> LaneSet:
        """Read the two fields; a lane's crossing line is the first line naming it."""
        result = LaneSet()
        raw_lines = (
            [dict(line) for line in lines if isinstance(line, dict)]
            if isinstance(lines, list)
            else []
        )
        used: set[int] = set()
        for raw in lanes if isinstance(lanes, list) else []:
            if not isinstance(raw, dict) or not isinstance(raw.get("id"), str) or not raw["id"]:
                continue
            lane = Lane(
                id=raw["id"],
                name=str(raw.get("name") or ""),
                area=_points(raw.get("points"), MAX_AREA_POINTS),
            )
            result.lane_extra[lane.id] = {
                k: deepcopy(v) for k, v in raw.items() if k not in ("id", "name", "points")
            }
            index = next(
                (i for i, line in enumerate(raw_lines) if line.get("lane") == lane.id), None
            )
            if index is not None:
                used.add(index)
                line = raw_lines[index]
                lane.line = _points(line.get("points"), 2)
                lane.line_id = str(line.get("id") or uuid.uuid4().hex[:16])
                lane.line_extra = {
                    k: deepcopy(v)
                    for k, v in line.items()
                    # A lane's traffic goes one way: its line has no direction names.
                    if k not in ("id", "name", "points", "lane", "in_label", "out_label")
                }
            else:
                lane.line_id = uuid.uuid4().hex[:16]
            result.lanes.append(lane)
        result.other_lines = [line for i, line in enumerate(raw_lines) if i not in used]
        return result

    def lanes_config(self) -> list[dict[str, Any]]:
        return [
            {
                **deepcopy(self.lane_extra.get(lane.id, {})),
                "id": lane.id,
                "name": lane.name.strip(),
                "points": deepcopy(lane.area),
            }
            for lane in self.lanes
        ]

    def lines_config(self) -> list[dict[str, Any]]:
        """The lines to save: the others as they were, then each lane's own
        (named after its lane)."""
        own = [
            {
                **deepcopy(lane.line_extra),
                "id": lane.line_id,
                "name": lane.name.strip(),
                "lane": lane.id,
                "points": deepcopy(lane.line),
            }
            for lane in self.lanes
            if lane.line
        ]
        return deepcopy(self.other_lines) + own

    def problem(self) -> str | None:
        """Why these lanes cannot be saved, or None."""
        if len(self.lanes) > MAX_LANES:
            return f"Use at most {MAX_LANES} lanes."
        for number, lane in enumerate(self.lanes, 1):
            label = lane.name.strip() or f"Lane {number}"
            if not lane.name.strip():
                return f"Give lane {number} a name."
            if len(lane.line) != 2 or lane.line[0] == lane.line[1]:
                return f"Draw the crossing line of {label}: two different points."
            if lane.area and len(lane.area) < 3:
                return f"Complete the area of {label} with 3 corners or more, or clear it."
        names = [lane.name.strip().casefold() for lane in self.lanes]
        if len(set(names)) != len(names):
            return "Give each lane a different name."
        return None
