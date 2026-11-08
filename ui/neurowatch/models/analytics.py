"""What analytics plugins show on a camera's picture: a marker beside the
camera's name (``plugins.cameras``) and their drawings (``plugins.overlays``).

No Qt: the walls, the Plugins page and tests use it.
"""

from __future__ import annotations

import dataclasses
import math
from collections import deque
from collections.abc import Iterable
from dataclasses import dataclass, field
from typing import Any

#: Item types a tile can draw (``nw_analytics_protocol::messages::OverlayItem``).
ITEM_TYPES = ("box", "polygon", "polyline", "text")
STYLES = ("accent", "success", "warning", "danger", "muted")
ANCHORS = ("top_left", "top_right", "bottom_left", "bottom_right")
#: How long a marking stays in the key after it was last drawn.
MARKING_HOLD_S = 6.0
#: Most markings remembered per view (a plugin decides how many it names).
MAX_MARKINGS_HELD = 64


@dataclass(frozen=True)
class Layer:
    id: str
    label: str
    default_on: bool = False


@dataclass(frozen=True)
class Figure:
    """A number a plugin reports about a camera, shown beside its picture."""

    label: str
    value: str
    #: An icon the app has (``car``, ``total``, …), or "": shown as text.
    icon: str = ""
    #: The colour of what it counts in the plugin's drawings, or "".
    style: str = ""


@dataclass(frozen=True)
class Marking:
    """What a colour on the picture stands for: one entry of the key shown
    under the picture. Named areas are listed here, never written on the video."""

    label: str
    style: str
    #: Drawn filled (an ignored area), not only outlined.
    filled: bool = False


@dataclass(frozen=True)
class TileInfo:
    """What a wall tile shows of the plugins analysing its camera."""

    figures: tuple[Figure, ...] = ()
    markings: tuple[Marking, ...] = ()
    #: The tile's two buttons: on or off, or None when it has no such button.
    drawings: bool | None = None
    counts: bool | None = None


def tile_choice(ui: dict[str, Any], camera_id: str) -> tuple[bool, bool]:
    """A camera's ``(drawings, counts)`` from ``ui.tile_plugins``. Without a
    choice of its own: drawings on, counts as ``ui.show_counts`` (off)."""
    counts = ui.get("show_counts", False) is True
    tiles = ui.get("tile_plugins")
    entry = tiles.get(camera_id) if isinstance(tiles, dict) else None
    if not isinstance(entry, dict):
        return True, counts
    return entry.get("drawings", True) is not False, entry.get("counts", counts) is True


@dataclass(frozen=True)
class CameraPlugin:
    """A plugin analysing a camera."""

    plugin: str
    name: str
    #: The plugin's state for the camera: ``running``, ``waiting``,
    #: ``settling``, ``error``, ``starting`` or ``offline``.
    state: str
    layers: tuple[Layer, ...] = ()
    #: Layers drawn on the live wall.
    wall_layers: frozenset[str] = frozenset()
    #: What the plugin reports about the camera, in its order, shown beside
    #: the picture, never on it (today's counts).
    figures: tuple[Figure, ...] = ()

    @property
    def running(self) -> bool:
        return self.state in ("running", "settling")


@dataclass(frozen=True)
class Drawing:
    """One layer's drawing on one camera, until ``until`` (monotonic s)."""

    camera_id: str
    plugin: str
    layer: str
    items: tuple[dict[str, Any], ...]
    until: float
    frame: int | None = None
    time_us: int | None = None
    #: Its declared lifetime, counted from its picture (``matches_picture``).
    ttl_ms: int = 2000
    observed_us: int | None = None


def parse_cameras(result: object) -> dict[str, list[CameraPlugin]]:
    """``plugins.cameras`` by camera id."""
    raw = result.get("cameras") if isinstance(result, dict) else None
    cameras: dict[str, list[CameraPlugin]] = {}
    for camera_id, plugins in (raw if isinstance(raw, dict) else {}).items():
        for entry in plugins if isinstance(plugins, list) else []:
            if not isinstance(entry, dict) or "plugin" not in entry:
                continue
            layers = tuple(
                Layer(
                    str(layer["id"]),
                    str(layer.get("label") or layer["id"]),
                    bool(layer.get("default_on")),
                )
                for layer in entry.get("layers") or []
                if isinstance(layer, dict) and "id" in layer
            )
            wall = entry.get("wall_layers")
            cameras.setdefault(str(camera_id), []).append(
                CameraPlugin(
                    plugin=str(entry["plugin"]),
                    name=str(entry.get("name") or entry["plugin"]),
                    state=str(entry.get("state") or "offline"),
                    layers=layers,
                    wall_layers=frozenset(str(w) for w in wall)
                    if isinstance(wall, list)
                    else frozenset(),
                    figures=tuple(
                        Figure(
                            str(figure["label"]),
                            str(figure.get("value", "")),
                            str(figure.get("icon") or ""),
                            str(figure.get("style") or ""),
                        )
                        for figure in entry.get("figures") or []
                        if isinstance(figure, dict) and "label" in figure
                    ),
                )
            )
    return cameras


def markings_drawn(drawings: Iterable[Drawing]) -> tuple[Marking, ...]:
    """The named areas in ``drawings``, each once, in drawing order."""
    return tuple(
        dict.fromkeys(
            Marking(item["label"], str(item.get("style") or "accent"), bool(item.get("fill")))
            for drawing in drawings
            for item in drawing.items
            if item.get("type") == "polygon"
            and isinstance(item.get("label"), str)
            and item["label"]
        )
    )


def markings_reported(figures: Iterable[Figure], counts_shown: bool) -> tuple[Marking, ...]:
    """What a plugin's figures say its colours mean. A counted type's cell
    already shows its colour while counts are shown."""
    return tuple(
        Marking(figure.label, figure.style)
        for figure in figures
        if figure.style and not (figure.icon and counts_shown)
    )


@dataclass
class MarkingHold:
    """The markings recently drawn on each view (a camera, a playback session).

    A layer can lapse for a moment while a plugin is slow, and the key's size
    decides the picture's: held a few seconds, the wall does not jump.
    """

    hold_s: float = MARKING_HOLD_S
    _seen: dict[str, dict[Marking, float]] = field(default_factory=dict)

    def update(self, drawings: dict[str, list[Drawing]], now: float) -> None:
        """Note what is drawn now; ``now`` is the drawings' monotonic clock."""
        for view, shown in drawings.items():
            seen = self._seen.setdefault(view, {})
            for marking in markings_drawn(d for d in shown if d.until > now):
                if marking in seen or len(seen) < MAX_MARKINGS_HELD:
                    seen[marking] = now

    def shown(self, views: Iterable[str], now: float) -> tuple[Marking, ...]:
        """Those of ``views``, each once, in the order they first appeared."""
        self._seen = {
            view: kept
            for view, seen in self._seen.items()
            if (kept := {m: at for m, at in seen.items() if now - at < self.hold_s})
        }
        return tuple(
            dict.fromkeys(marking for view in views for marking in self._seen.get(view, {}))
        )

    def clear(self) -> None:
        self._seen.clear()


def marker_text(plugins: list[CameraPlugin]) -> str | None:
    """The tile's tooltip line for its plugins, or None: no marker."""
    if not plugins:
        return None
    words = {
        "running": "running",
        "settling": "paused while the camera moves",
        "waiting": "waiting for video",
        "starting": "starting",
        "error": "has a problem",
        "offline": "not connected",
    }
    parts = [f"{p.name} ({words.get(p.state, p.state)})" for p in plugins]
    return "Analysed by " + ", ".join(parts)


def _point(value: object) -> bool:
    return (
        isinstance(value, (list, tuple))
        and len(value) == 2
        and all(
            isinstance(v, (int, float)) and not isinstance(v, bool) and math.isfinite(v)
            for v in value
        )
    )


def _valid(item: object) -> bool:
    """An item a tile can draw; the backend sanitizes, the UI still checks."""
    if not isinstance(item, dict) or item.get("type") not in ITEM_TYPES:
        return False
    kind = item["type"]
    if kind == "box":
        rect = item.get("rect")
        return (
            isinstance(rect, list)
            and len(rect) == 4
            and all(isinstance(v, (int, float)) and math.isfinite(v) for v in rect)
        )
    if kind in ("polygon", "polyline"):
        points = item.get("points")
        least = 3 if kind == "polygon" else 2
        return isinstance(points, list) and len(points) >= least and all(map(_point, points))
    lines = item.get("lines")
    return isinstance(lines, list) and bool(lines) and all(isinstance(t, str) for t in lines)


def parse_overlays(result: object, now: float) -> dict[str, list[Drawing]]:
    """``plugins.overlays`` by camera id, each drawing shown until its time
    to live runs out (from ``now``, monotonic seconds)."""
    raw = result.get("overlays") if isinstance(result, dict) else None
    drawings: dict[str, list[Drawing]] = {}
    for entry in raw if isinstance(raw, list) else []:
        if not isinstance(entry, dict) or "camera_id" not in entry:
            continue
        items = tuple(item for item in entry.get("items") or [] if _valid(item))
        if not items:
            continue
        ttl = entry.get("ttl_ms")
        ttl_s = float(ttl) / 1000 if isinstance(ttl, (int, float)) else 2.0
        # ``ttl_ms`` is the time left; ``lifetime_ms`` (newer recorders) the
        # declared lifetime from its picture, which picture matching needs.
        lifetime = entry.get("lifetime_ms")
        lifetime_s = float(lifetime) / 1000 if isinstance(lifetime, (int, float)) else ttl_s
        camera_id = str(entry["camera_id"])
        drawings.setdefault(camera_id, []).append(
            Drawing(
                camera_id,
                str(entry.get("plugin", "")),
                str(entry.get("layer", "")),
                items,
                now + max(0.0, ttl_s),
                entry.get("frame") if isinstance(entry.get("frame"), int) else None,
                entry.get("time_us") if isinstance(entry.get("time_us"), int) else None,
                max(0, min(10000, int(lifetime_s * 1000))),
                entry.get("observed_us") if isinstance(entry.get("observed_us"), int) else None,
            )
        )
    return drawings


def on_wall(
    drawings: dict[str, list[Drawing]],
    cameras: dict[str, list[CameraPlugin]],
    mode: str = "chosen",
) -> dict[str, list[Drawing]]:
    """The drawings the live wall shows (``ui.wall_drawings``): none
    (``off``), each binding's wall layers (``chosen``), or every layer of
    the plugins analysing the camera, debug included (``all``)."""
    if mode == "off":
        return {}
    shown: dict[str, list[Drawing]] = {}
    for camera_id, layers in drawings.items():
        plugins = cameras.get(camera_id, [])
        if mode == "all":
            wanted = {(p.plugin, d.layer) for p in plugins for d in layers}
        else:
            wanted = {(p.plugin, layer) for p in plugins for layer in p.wall_layers}
        kept = [d for d in layers if (d.plugin, d.layer) in wanted]
        if kept:
            shown[camera_id] = kept
    return shown


@dataclass
class LayerChoice:
    """Layers shown on one camera in a view of its own (the Plugins page):
    every layer unless switched off there."""

    hidden: set[tuple[str, str]] = field(default_factory=set)

    def shown(self, drawings: list[Drawing]) -> list[Drawing]:
        return [d for d in drawings if (d.plugin, d.layer) not in self.hidden]


def matches_picture(drawing: Drawing, seq: int, capture_us: int, observed_us: int = 0) -> bool:
    """A result starts on its picture, never on an earlier cached picture.

    Between analysis results it holds for its declared lifetime. A second
    camera stream has different sequences and presentation clocks; the node
    receipt clock aligns that live view without changing recorded history.
    Older backends without identities retain their existing live behavior.
    """
    if not capture_us or drawing.time_us is None:
        return True
    if seq and drawing.frame == seq:
        return True
    if observed_us > 0 and drawing.observed_us is not None and drawing.observed_us > 0:
        return 0 <= observed_us - drawing.observed_us <= drawing.ttl_ms * 1000
    return 0 <= capture_us - drawing.time_us <= drawing.ttl_ms * 1000


@dataclass
class DrawingHistory:
    """A tile's recent drawings per (plugin, layer), so its picture always
    carries the newest result it has reached.

    A plugin's result reaches the UI while the tile still shows an older
    picture (the result names the picture it analysed, and decoding,
    pacing and the overlay poll each add delay). The newest drawing alone
    then matches nothing until that picture is shown: the layer blinks off
    on every result. Kept here, the drawing before it covers the gap.
    """

    depth: int = 4
    _layers: dict[tuple[str, str], list[Drawing]] = field(default_factory=dict)

    def update(self, drawings: list[Drawing]) -> None:
        """The layers drawn now; a layer no longer drawn is forgotten."""
        layers: dict[tuple[str, str], list[Drawing]] = {}
        for drawing in drawings:
            key = (drawing.plugin, drawing.layer)
            past = layers[key] if key in layers else self._layers.get(key, [])
            if past and dataclasses.replace(past[-1], until=drawing.until) == drawing:
                # Re-sent unchanged (each snapshot renews its lifetime).
                layers[key] = [*past[:-1], drawing]
                continue
            # Only results about earlier pictures may stand in for it: one
            # that replaces a result for the same picture (or carries no
            # picture time) supersedes it.
            older = [d for d in past if picture_time(d) < picture_time(drawing)]
            layers[key] = [*older, drawing][-self.depth :]
        self._layers = layers

    def shown(self, frame_identity: tuple[int, ...], now: float) -> list[Drawing]:
        """Per layer, the newest drawing the picture has reached, while it
        lasts (an expired or outlived one leaves the layer empty)."""
        out: list[Drawing] = []
        for history in self._layers.values():
            for drawing in reversed(history):
                if _ahead_of(drawing, *frame_identity):
                    continue
                if drawing.until > now and matches_picture(drawing, *frame_identity):
                    out.append(drawing)
                break
        return out

    def prune(self, now: float) -> None:
        """Forget expired drawings."""
        layers = {key: [d for d in h if d.until > now] for key, h in self._layers.items()}
        self._layers = {key: h for key, h in layers.items() if h}

    def until(self) -> list[float]:
        return [d.until for history in self._layers.values() for d in history]

    def clear(self) -> None:
        self._layers = {}


def picture_time(drawing: Drawing) -> tuple[int, int]:
    """When the analysed picture was seen (node clock), then its capture time."""
    if drawing.time_us is None:
        return (0, 0)
    return (drawing.observed_us or 0, drawing.time_us)


def _ahead_of(drawing: Drawing, seq: int, capture_us: int, observed_us: int = 0) -> bool:
    """The drawing is about a picture after the one shown (see ``matches_picture``)."""
    if not capture_us or drawing.time_us is None or (seq and drawing.frame == seq):
        return False
    if observed_us > 0 and drawing.observed_us is not None and drawing.observed_us > 0:
        return observed_us < drawing.observed_us
    return capture_us < drawing.time_us


#: How far a held picture may be from the one a result describes (another
#: stream of the camera, or a tile reading fewer pictures than the plugin).
PICTURE_MATCH_US = 200_000
#: Held beyond the measured delay, for its variation between results.
_HOLD_MARGIN_US = 500_000


def picture_gap_us(later: tuple[int, int], earlier: tuple[int, int]) -> int:
    """How much later one ``picture_time`` is than another (receipt clock
    when both have it)."""
    if later[0] > 0 and earlier[0] > 0:
        return later[0] - earlier[0]
    return later[1] - earlier[1]


def analysed_target(drawings: list[Drawing], now: float) -> Drawing | None:
    """The newest current drawing that names its picture: the picture an
    analysed-pictures tile shows (None: the tile shows live video)."""
    named = [d for d in drawings if d.time_us is not None and d.until > now]
    return max(named, key=picture_time, default=None)


@dataclass
class AnalysedPictures:
    """Recent pictures of one camera, held so a tile can show the very
    picture an analytics result describes, with that result on it: the
    video then advances at the plugin's frame rate, the drawings exactly on
    what they describe. Results usually reach the UI after their picture (the
    plugin decodes and analyses it), so pictures are kept for ``keep_us``
    (receipt clock), within ``max_bytes``. A fast plugin's result can also
    arrive first: its picture is then still on its way to the tile.

    Large pictures would fill ``max_bytes`` in less time than a slow plugin
    takes (1080p: under a second), and its results would find nothing. So
    only as many are kept as the budget allows over the delay measured.
    """

    keep_us: int = 4_000_000
    #: Reached by a camera's main stream only: a sub stream's pictures run out
    #: of ``keep_us`` first.
    max_bytes: int = 256 * 1024 * 1024
    _held: deque[tuple[tuple[int, int, int], object, int]] = field(default_factory=deque)
    _bytes: int = 0
    shown: tuple[int, int] | None = None
    #: Monotonic time since playing live while results are current, or None.
    live_since: float | None = None
    #: How long results take, newest picture to the picture they describe.
    _delay_us: int = 0
    _noted: tuple[int, int] | None = None
    #: The tile has shown a picture since it opened on this camera.
    opened: bool = False
    #: Monotonic time since waiting for a plugin that is still starting, or None.
    starting_since: float | None = None
    #: The size of the pictures held: another size is another of the camera's streams.
    size: tuple[int, int] | None = None
    #: Results not shown yet. Another stream's pictures can stay behind the
    #: newest result for good (a new one arrives before they catch up), so
    #: each picture goes with the newest result it has reached, not the newest.
    _waiting: deque[Drawing] = field(default_factory=lambda: deque(maxlen=64))

    def add(self, identity: tuple[int, int, int], frame: object, size: int) -> None:
        """A decoded picture (``(seq, capture_us, observed_us)``) as it arrives."""
        if not identity[1]:
            return  # an older recorder: nothing to match a result against
        if self._held and _clock(identity) - _clock(self._held[-1][0]) < self._spacing_us(size):
            return
        self._held.append((identity, frame, size))
        self._bytes += size
        newest = _clock(identity)
        while self._held and (
            self._bytes > self.max_bytes or newest - _clock(self._held[0][0]) > self.keep_us
        ):
            self._bytes -= self._held.popleft()[2]

    def note_result(self, drawing: Drawing) -> None:
        """A result is current: how far behind the newest picture it is."""
        if not self._held or self._noted == picture_time(drawing):
            return
        self._noted = picture_time(drawing)
        identity = self._held[-1][0]
        if identity[2] > 0 and drawing.observed_us:
            delay = identity[2] - drawing.observed_us
        else:
            delay = identity[1] - (drawing.time_us or 0)
        # Up at once, down slowly: one quick result must not thin the pictures
        # the next slow one needs.
        self._delay_us = max(delay, self._delay_us * 19 // 20, 0)

    def next_result(self, drawing: Drawing | None) -> tuple[Drawing, object] | None:
        """Note the newest result; the newest one not yet shown whose picture
        is here, with that picture. Never one older than the picture shown."""
        if drawing is not None and all(
            picture_time(drawing) != picture_time(seen) for seen in self._waiting
        ):
            self._waiting.append(drawing)
        ready = None
        for result in self._waiting:
            if self.shown is not None and picture_gap_us(picture_time(result), self.shown) <= 0:
                continue
            frame = self.find(result)
            if frame is not None and (
                ready is None or picture_gap_us(picture_time(result), picture_time(ready[0])) > 0
            ):
                ready = (result, frame)
        if ready is not None:
            newest = picture_time(ready[0])
            self._waiting = deque(
                (d for d in self._waiting if picture_gap_us(picture_time(d), newest) > 0),
                maxlen=self._waiting.maxlen,
            )
        return ready

    def _spacing_us(self, size: int) -> int:
        """The time between pictures that lets the budget cover the delay."""
        span = min(self.keep_us, self._delay_us * 5 // 4 + _HOLD_MARGIN_US)
        return span * size // max(1, self.max_bytes)

    def find(self, drawing: Drawing) -> object | None:
        """The held picture ``drawing`` describes, or the nearest one within
        ``PICTURE_MATCH_US`` once held pictures lie on both sides of its moment.
        None while it is not here yet (an earlier picture would carry boxes from
        the future), or was before the first one held (just after a tile opens:
        a later picture would carry them from the past)."""
        best: tuple[int, object] | None = None
        passed = False
        reached = False
        for (seq, capture_us, observed_us), frame, _size in self._held:
            if observed_us > 0 and drawing.observed_us:
                gap = abs(observed_us - drawing.observed_us)
                passed = passed or observed_us >= drawing.observed_us
                reached = reached or observed_us <= drawing.observed_us
            else:
                gap = abs(capture_us - (drawing.time_us or 0))
                passed = passed or capture_us >= (drawing.time_us or 0)
                reached = reached or capture_us <= (drawing.time_us or 0)
            if seq and seq == drawing.frame and gap <= PICTURE_MATCH_US:
                return frame  # the same stream's very picture
            if gap <= PICTURE_MATCH_US and (best is None or gap < best[0]):
                best = (gap, frame)
        return best[1] if best and passed and reached else None

    def clear(self) -> None:
        self._held.clear()
        self._bytes = 0
        self.shown = None
        self.live_since = None
        self._delay_us = 0
        self._noted = None
        self._waiting.clear()
        self.opened = False
        self.starting_since = None
        self.size = None


def _clock(identity: tuple[int, int, int]) -> int:
    return identity[2] if identity[2] > 0 else identity[1]
