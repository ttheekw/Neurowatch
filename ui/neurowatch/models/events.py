"""Events (``events.*``): names, times, and which cameras show activity now.

No Qt: the desktop Events tab, tile badges and the headless commands use it.
"""

from __future__ import annotations

import math
import time
from collections.abc import Iterable
from dataclasses import dataclass, field
from typing import Any

EVENT_LABELS: dict[str, str] = {
    "motion": "Motion",
    "intrusion": "Intrusion",
    "line_crossing": "Line crossed",
    "tamper": "Tampering",
    "digital_input": "Alarm input",
    "camera_event": "Camera event",
    "camera_offline": "Camera offline",
    "recording_failed": "Recording failed",
    "disk_low": "Recording drive almost full",
    "storage_missing": "Recording storage missing",
    "backend_started": "NeuroWatch started",
    "bookmark": "Bookmark",
}
#: How an event's end reads, for the kinds whose end is news.
END_LABELS: dict[str, str] = {
    "camera_offline": "Camera back online",
    "recording_failed": "Recording recovered",
    "disk_low": "Recording drive has space again",
    "storage_missing": "Recording storage is back",
}
#: Kinds a tile shows as movement in the picture.
ACTIVITY_KINDS = frozenset({"motion", "intrusion", "line_crossing"})
PLAYBACK_KINDS = frozenset(EVENT_LABELS) - {"disk_low", "storage_missing", "backend_started"}
EVENT_PRIORITY = {
    "camera_offline": 5,
    "recording_failed": 5,
    "storage_missing": 5,
    "tamper": 4,
    "digital_input": 3,
    "bookmark": 2,
}
EVENT_COLORS = {
    "motion": "#f97316",
    "intrusion": "#f97316",
    "line_crossing": "#f97316",
    "tamper": "#a855f7",
    "digital_input": "#06b6d4",
    "camera_event": "#64748b",
    "camera_offline": "#ef4444",
    "recording_failed": "#ef4444",
    "bookmark": "#3b82f6",
    "disk_low": "#f59e0b",
    "storage_missing": "#ef4444",
    "backend_started": "#64748b",
}
#: Kinds about the system rather than the picture.
SYSTEM_KINDS = frozenset(
    {"camera_offline", "recording_failed", "disk_low", "storage_missing", "backend_started"}
)
#: An event without duration (a line crossed) shows as activity this long.
INSTANT_ACTIVITY_S = 5.0


def label(kind: str) -> str:
    return EVENT_LABELS.get(kind, kind.replace("_", " ").capitalize())


#: Kinds as nouns in a sentence ("It reports motion and line crossing").
_KIND_NOUNS: dict[str, str] = {
    "line_crossing": "line crossing",
    "tamper": "tampering",
    "digital_input": "alarm inputs",
    "camera_event": "other camera events",
}


def kinds_text(kinds: Iterable[str]) -> str:
    """``["motion", "line_crossing"]`` → ``"motion and line crossing"``."""
    names = [_KIND_NOUNS.get(k, label(k).lower()) for k in kinds]
    if len(names) <= 1:
        return "".join(names)
    return ", ".join(names[:-1]) + " and " + names[-1]


@dataclass(frozen=True)
class Event:
    """One stored event (``EventRecord``); times in ms since the epoch."""

    id: int
    kind: str
    camera_id: str | None
    start_ms: int
    end_ms: int | None
    source: str = ""
    topic: str | None = None
    detail: str | None = None
    acknowledged: bool = False
    score: float | None = None
    zones: tuple[str, ...] = ()
    bbox: tuple[float, float, float, float] | None = None
    #: The recorder saved a picture of the event (``events.thumbnail``).
    has_image: bool = False
    #: A bookmark whose footage cleanup never deletes.
    kept: bool = False

    @classmethod
    def from_dict(cls, raw: dict[str, Any]) -> Event | None:
        try:
            return cls(
                id=int(raw["id"]),
                kind=str(raw.get("kind") or ""),
                camera_id=raw["camera_id"] if isinstance(raw.get("camera_id"), str) else None,
                start_ms=int(raw.get("start_ms") or 0),
                end_ms=int(raw["end_ms"]) if isinstance(raw.get("end_ms"), (int, float)) else None,
                source=str(raw.get("source") or ""),
                topic=raw["topic"] if isinstance(raw.get("topic"), str) else None,
                detail=raw["detail"] if isinstance(raw.get("detail"), str) else None,
                acknowledged=bool(raw.get("acknowledged", False)),
                score=_fraction(raw.get("score")),
                zones=tuple(
                    dict.fromkeys(
                        zone
                        for zone in raw.get("zones", [])[:32]
                        if isinstance(zone, str) and 0 < len(zone) <= 64
                    )
                )
                if isinstance(raw.get("zones"), list)
                else (),
                bbox=_bounds(raw.get("bbox")),
                has_image=isinstance(raw.get("thumbnail"), str) and bool(raw["thumbnail"]),
                kept=raw.get("kept") is True,
            )
        except (KeyError, TypeError, ValueError):
            return None

    @property
    def going_on(self) -> bool:
        return self.end_ms is None

    @property
    def label(self) -> str:
        return label(self.kind)

    def describe(self, camera_name: str | None = None) -> str:
        """What happened, with the detail when it adds something."""
        if self.kind == "backend_started" and self.detail:
            return f"{self.detail} started"  # "NeuroWatch 0.1.0 started"
        parts = [self.label]
        if self.detail and self.detail != camera_name:
            parts.append(self.detail)
        elif self.kind == "camera_event" and self.topic:
            parts.append(self.topic)
        return " · ".join(parts)


def _fraction(value: Any) -> float | None:
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        if 0 <= value <= 1 and math.isfinite(value):
            return float(value)
    return None


def _bounds(value: Any) -> tuple[float, float, float, float] | None:
    if not isinstance(value, (list, tuple)) or len(value) != 4:
        return None
    x, y, width, height = (_fraction(part) for part in value)
    if x is None or y is None or width is None or height is None:
        return None
    if width <= 0 or height <= 0 or x + width > 1.000001 or y + height > 1.000001:
        return None
    return x, y, width, height


def zone_names(motion: dict[str, Any] | None) -> dict[str, str]:
    """Current configured names; removed/historical IDs keep their own label."""
    zones = (motion or {}).get("zones", [])
    if not isinstance(zones, list):
        return {}
    return {
        zone["id"]: zone["name"]
        for zone in zones[:32]
        if isinstance(zone, dict)
        and isinstance(zone.get("id"), str)
        and isinstance(zone.get("name"), str)
        and zone["name"].strip()
    }


def metadata_lines(event: Event, names: dict[str, str]) -> list[str]:
    """No inferred confidence/objects: score and area are analysis metadata."""
    lines = []
    if event.score is not None:
        kind = "motion" if event.kind == "motion" else "analytics"
        lines.append(f"Peak {kind} score: {event.score * 100:.1f}%".replace(".0%", "%"))
    if event.zones:
        lines.append("Zones: " + ", ".join(names.get(zone, zone) for zone in event.zones))
    if event.bbox is not None:
        coordinates = ", ".join(
            f"{key} {value * 100:.0f}%"
            for key, value in zip(("x", "y", "width", "height"), event.bbox, strict=True)
        )
        lines.append(f"Changed area: {coordinates} (analysis image)")
    return lines


def when_text(ms: int, now_ms: int | None = None) -> str:
    """``14:03:21`` today, ``Mon 14:03`` this week, else ``2026-10-01 14:03``."""
    now_ms = int(time.time() * 1000) if now_ms is None else now_ms
    now = time.localtime(now_ms / 1000)
    then = time.localtime(ms / 1000)
    if (then.tm_year, then.tm_yday) == (now.tm_year, now.tm_yday):
        return time.strftime("%H:%M:%S", then)
    age_days = (now_ms - ms) / 86_400_000
    if 0 <= age_days < 6:
        return time.strftime("%a %H:%M", then)
    return time.strftime("%Y-%m-%d %H:%M", then)


def clock_text(ms: int) -> str:
    """``14:03:21`` in local time."""
    return time.strftime("%H:%M:%S", time.localtime(ms / 1000))


def day_text(ms: int, now_ms: int | None = None) -> str:
    """``Today``, ``Yesterday``, ``Monday 12 October``, or with the year when
    it is not this year: the heading over a day's events."""
    now_ms = int(time.time() * 1000) if now_ms is None else now_ms
    now = time.localtime(now_ms / 1000)
    then = time.localtime(ms / 1000)
    today = time.mktime((now.tm_year, now.tm_mon, now.tm_mday, 0, 0, 0, 0, 0, -1))
    day = time.mktime((then.tm_year, then.tm_mon, then.tm_mday, 0, 0, 0, 0, 0, -1))
    days = round((today - day) / 86400)
    if days == 0:
        return "Today"
    if days == 1:
        return "Yesterday"
    text = f"{time.strftime('%A', then)} {then.tm_mday} {time.strftime('%B', then)}"
    return text if then.tm_year == now.tm_year else f"{text} {then.tm_year}"


def day_key(ms: int) -> tuple[int, int, int]:
    then = time.localtime(ms / 1000)
    return then.tm_year, then.tm_mon, then.tm_mday


def span_text(milliseconds: int) -> str:
    """``45 s``, ``3 min``, ``2 h 5 min`` for a span of time."""
    seconds = max(0, milliseconds // 1000)
    if seconds < 60:
        return f"{max(seconds, 1)} s"
    minutes = seconds // 60
    if minutes < 60:
        return f"{minutes} min"
    hours = minutes // 60
    if hours < 48:
        return f"{hours} h {minutes % 60} min" if minutes % 60 else f"{hours} h"
    return f"{hours // 24} d {hours % 24} h" if hours % 24 else f"{hours // 24} d"


def elapsed_text(event: Event, now_ms: int | None = None) -> str:
    """How long it has lasted: so far for an event going on, else its
    duration; empty for an instant event."""
    if event.end_ms is None:
        now_ms = int(time.time() * 1000) if now_ms is None else now_ms
        return span_text(max(0, now_ms - event.start_ms))
    return duration_text(event)


def full_time_text(ms: int) -> str:
    return time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(ms / 1000))


def duration_text(event: Event) -> str:
    """``now`` while it goes on, ``45 s``, ``3 min``, ``2 h 5 min``; empty for
    events without duration."""
    if event.end_ms is None:
        return "now"
    if event.end_ms == event.start_ms:
        return ""
    return span_text(event.end_ms - event.start_ms)


@dataclass
class ActivityTracker:
    """Which cameras show activity (motion, intrusion, a line crossed) now,
    from the events going on and notices as they arrive."""

    _open: dict[int, Event] = field(default_factory=dict)
    # Instant events: camera → shown until this time (monotonic seconds).
    _flash: dict[str, float] = field(default_factory=dict)
    # Stored separately from current activity: ending motion never clears an alert.
    _pending: dict[str, set[int]] = field(default_factory=dict)
    _acknowledged_through: dict[str, int] = field(default_factory=dict)

    def reset(self, open_events: Iterable[Event]) -> None:
        self._open = {e.id: e for e in open_events if e.kind in ACTIVITY_KINDS and e.camera_id}
        self._flash.clear()

    def restore_pending(self, events: Iterable[Event]) -> None:
        """Restore retained, unacknowledged activity after reconnecting."""
        self._pending.clear()
        for event in events:
            self._remember(event)

    def _remember(self, event: Event) -> None:
        if event.kind not in ACTIVITY_KINDS or not event.camera_id:
            return
        if event.acknowledged:
            pending = self._pending.get(event.camera_id)
            if pending is not None:
                pending.discard(event.id)
                if not pending:
                    self._pending.pop(event.camera_id)
        elif event.id > self._acknowledged_through.get(event.camera_id, -1):
            self._pending.setdefault(event.camera_id, set()).add(event.id)

    def pending(self) -> set[str]:
        """Cameras with activity the user has not acknowledged."""
        return set(self._pending)

    def pending_ids(self, camera_id: str) -> list[int]:
        return sorted(self._pending.get(camera_id, ()))

    def acknowledge(self, camera_id: str, ids: Iterable[int]) -> None:
        """Clear only the events the user saw; newer activity remains pending."""
        for id_ in ids:
            self._acknowledged_through[camera_id] = max(
                id_, self._acknowledged_through.get(camera_id, -1)
            )
            pending = self._pending.get(camera_id)
            if pending is not None:
                pending.discard(id_)
                if not pending:
                    self._pending.pop(camera_id)

    def apply(self, phase: str, event: Event, now: float | None = None) -> None:
        if event.kind not in ACTIVITY_KINDS or not event.camera_id:
            return
        self._remember(event)
        now = time.monotonic() if now is None else now
        if phase == "end":
            started = self._open.pop(event.id, None)
            if started is None or event.end_ms == event.start_ms:
                self._flash[event.camera_id] = now + INSTANT_ACTIVITY_S
        else:
            self._open[event.id] = event

    def active(self, now: float | None = None) -> set[str]:
        now = time.monotonic() if now is None else now
        self._flash = {cam: until for cam, until in self._flash.items() if until > now}
        cameras = {e.camera_id for e in self._open.values() if e.camera_id}
        return cameras | set(self._flash)
