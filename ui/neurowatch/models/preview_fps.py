"""Desktop live-preview frame-rate limits (``ui.preview_fps``).

They cap only what the desktop wall paints: the camera's own frame rate,
recording and browser viewing are never limited by them. Each tile's reader
also asks the recorder for its rate, so frames above it are dropped before
they are converted for the screen. No Qt here.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

#: A wall with more visible tiles than this uses the ``dense`` limit.
DENSE_WALL_TILES = 16
#: Bounds the Settings dialog and the recorder accept.
MAX_VISIBLE_FPS = 60
MAX_HIDDEN_FPS = 30


@dataclass(frozen=True)
class PreviewFps:
    """Frames per second the wall shows, by what is on screen."""

    single: int = 25  # one camera fills the wall (single view)
    wall: int = 15  # up to DENSE_WALL_TILES tiles
    dense: int = 4  # more tiles than that
    hidden: int = 2  # cameras on pages not shown, kept warm for an instant page change

    @classmethod
    def from_config(cls, ui: object) -> PreviewFps:
        """From the config's ``ui`` section; missing or odd values keep their defaults."""
        raw = ui.get("preview_fps") if isinstance(ui, dict) else None
        if not isinstance(raw, dict):
            return cls()
        default = cls()

        def value(name: str, top: int) -> int:
            try:
                return max(1, min(top, int(raw.get(name, getattr(default, name)))))
            except (TypeError, ValueError):
                return int(getattr(default, name))

        return cls(
            single=value("single", MAX_VISIBLE_FPS),
            wall=value("wall", MAX_VISIBLE_FPS),
            dense=value("dense", MAX_VISIBLE_FPS),
            hidden=value("hidden", MAX_HIDDEN_FPS),
        )

    def to_json(self) -> dict[str, Any]:
        return {
            "single": self.single,
            "wall": self.wall,
            "dense": self.dense,
            "hidden": self.hidden,
        }

    def visible(self, visible_count: int, *, single_view: bool = False) -> float:
        """The limit for the tiles on screen, from how many cameras are shown
        (not the grid's capacity: empty slots cost nothing)."""
        if single_view:
            return float(self.single)
        if visible_count <= DENSE_WALL_TILES:
            return float(self.wall)
        return float(self.dense)
