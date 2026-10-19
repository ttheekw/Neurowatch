"""Pure event-to-clip presets; preparing one never starts an export."""

from __future__ import annotations

from collections.abc import Iterable
from dataclasses import dataclass

from neurowatch.models.clip_export import MAX_CLIP_MS
from neurowatch.models.events import Event


@dataclass(frozen=True)
class EventClip:
    bounds: tuple[int, int]
    adjusted: bool
    ongoing: bool


def event_clip(
    event: Event,
    *,
    now_ms: int,
    pre_sec: int = 10,
    post_sec: int = 15,
    coverage: Iterable[tuple[int, int]] | None = None,
    max_ms: int = MAX_CLIP_MS,
) -> EventClip | None:
    """Pad an event, intersect saved footage and cap length without jumping gaps.

    Ongoing events end at the request's fixed cutoff, not a moving future end.
    Instant events need at least one second for the existing trim controls.
    Internal recording gaps stay in the selection; the exporter explains them.
    """
    if pre_sec < 0 or post_sec < 0 or max_ms <= 0:
        return None
    start = max(0, event.start_ms - pre_sec * 1000)
    end = (
        now_ms
        if event.end_ms is None
        else min(now_ms, max(event.start_ms + 1000, event.end_ms) + post_sec * 1000)
    )
    requested = (start, end)
    if coverage is not None:
        overlaps = [(max(start, a), min(end, b)) for a, b in coverage if a < end and b > start]
        overlaps = [(a, b) for a, b in overlaps if b > a]
        if not overlaps:
            return None
        start = min(a for a, _ in overlaps)
        end = max(b for _, b in overlaps)
    end = min(end, start + max_ms)
    if end - start < 1000:
        return None
    return EventClip((start, end), (start, end) != requested, event.end_ms is None)
