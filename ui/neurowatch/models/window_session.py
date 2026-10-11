"""Extra windows saved in the config (``desktop.windows``), so they open again.

Each window keeps its number, grid, tiles (camera, page, cell, size), the
page it showed, whether it cycled, and where it was: its screen, size and
position, and whether it was maximized or fullscreen. The main wall's own
layout is each camera's ``wall_position``.

Parsing is lenient: a window or tile that is not understood is skipped, so a
hand-edited or newer config never stops the app from starting.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

WINDOW_STATES = ("normal", "maximized", "fullscreen")


def _int(value: object, default: int = 0, minimum: int | None = None) -> int:
    if isinstance(value, bool) or not isinstance(value, int | float):
        return default
    number = int(value)
    return max(minimum, number) if minimum is not None else number


@dataclass(frozen=True)
class SavedTile:
    camera_id: str
    page: int
    row: int
    col: int
    row_span: int = 1
    col_span: int = 1

    def to_dict(self) -> dict[str, Any]:
        return {
            "camera_id": self.camera_id,
            "page": self.page,
            "row": self.row,
            "col": self.col,
            "row_span": self.row_span,
            "col_span": self.col_span,
        }

    @classmethod
    def from_dict(cls, value: object) -> SavedTile | None:
        if not isinstance(value, dict) or not isinstance(value.get("camera_id"), str):
            return None
        return cls(
            camera_id=value["camera_id"],
            page=_int(value.get("page"), minimum=0),
            row=_int(value.get("row"), minimum=0),
            col=_int(value.get("col"), minimum=0),
            row_span=_int(value.get("row_span"), 1, minimum=1),
            col_span=_int(value.get("col_span"), 1, minimum=1),
        )


@dataclass(frozen=True)
class SavedGeometry:
    screen: str
    x: int
    y: int
    width: int
    height: int
    state: str = "normal"

    def to_dict(self) -> dict[str, Any]:
        return {
            "screen": self.screen,
            "x": self.x,
            "y": self.y,
            "width": self.width,
            "height": self.height,
            "state": self.state,
        }

    @classmethod
    def from_dict(cls, value: object) -> SavedGeometry | None:
        if not isinstance(value, dict):
            return None
        width = _int(value.get("width"))
        height = _int(value.get("height"))
        if width <= 0 or height <= 0:
            return None
        state = value.get("state")
        screen = value.get("screen")
        return cls(
            screen=screen if isinstance(screen, str) else "",
            x=_int(value.get("x")),
            y=_int(value.get("y")),
            width=width,
            height=height,
            state=state if state in WINDOW_STATES else "normal",
        )


@dataclass(frozen=True)
class SavedWindow:
    number: int
    rows: int
    cols: int
    tiles: list[SavedTile] = field(default_factory=list)
    page: int = 0
    cycling: bool = True
    geometry: SavedGeometry | None = None

    def to_dict(self) -> dict[str, Any]:
        return {
            "number": self.number,
            "rows": self.rows,
            "cols": self.cols,
            "tiles": [tile.to_dict() for tile in self.tiles],
            "page": self.page,
            "cycling": self.cycling,
            "geometry": None if self.geometry is None else self.geometry.to_dict(),
        }

    @classmethod
    def from_dict(cls, value: object) -> SavedWindow | None:
        if not isinstance(value, dict):
            return None
        rows = _int(value.get("rows"))
        cols = _int(value.get("cols"))
        number = _int(value.get("number"))
        if not (1 <= rows <= 16 and 1 <= cols <= 16) or number < 2:
            return None
        raw_tiles = value.get("tiles")
        if not isinstance(raw_tiles, list):
            raw_tiles = []
        parsed = (SavedTile.from_dict(t) for t in raw_tiles)
        tiles = [tile for tile in parsed if tile is not None]
        cycling = value.get("cycling")
        return cls(
            number=number,
            rows=rows,
            cols=cols,
            tiles=tiles,
            page=_int(value.get("page"), minimum=0),
            cycling=cycling if isinstance(cycling, bool) else True,
            geometry=SavedGeometry.from_dict(value.get("geometry")),
        )


def saved_windows(config: dict) -> list[SavedWindow]:
    """The windows saved in ``config`` (as ``config.get`` returns it), each
    number once, in number order."""
    desktop = config.get("desktop")
    raw = desktop.get("windows") if isinstance(desktop, dict) else None
    windows: dict[int, SavedWindow] = {}
    for entry in raw if isinstance(raw, list) else []:
        window = SavedWindow.from_dict(entry)
        if window is not None and window.number not in windows:
            windows[window.number] = window
    return [windows[number] for number in sorted(windows)]
