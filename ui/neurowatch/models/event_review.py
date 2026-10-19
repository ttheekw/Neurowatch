"""Pure, cached events-only review intervals. No video or RPC work."""

from __future__ import annotations

from bisect import bisect_right
from collections import defaultdict
from collections.abc import Iterable, Mapping
from dataclasses import dataclass, field

from neurowatch.models.events import PLAYBACK_KINDS, Event

Interval = tuple[int, int]


def _merge(intervals: Iterable[Interval]) -> list[Interval]:
    merged: list[Interval] = []
    for start, end in sorted((a, b) for a, b in intervals if b > a):
        if merged and start <= merged[-1][1]:
            merged[-1] = (merged[-1][0], max(end, merged[-1][1]))
        else:
            merged.append((start, end))
    return merged


@dataclass(frozen=True)
class EventReviewPlan:
    intervals: tuple[Interval, ...]
    _ends: tuple[int, ...] = field(init=False, repr=False)

    def __post_init__(self) -> None:
        object.__setattr__(self, "_ends", tuple(end for _, end in self.intervals))

    def next_at(self, at_ms: int) -> int | None:
        """Stay inside an interval, jump to the next start, or finish. O(log n)."""
        index = bisect_right(self._ends, at_ms)
        return max(at_ms, self.intervals[index][0]) if index < len(self.intervals) else None


def event_review_plan(
    events: Iterable[Event],
    coverage: Mapping[str, Iterable[Interval]],
    *,
    now_ms: int,
    bounds: Interval | None = None,
    padding: Mapping[str, tuple[int, int]] | None = None,
) -> EventReviewPlan:
    """Merge event windows before intersecting coverage, avoiding events × files scans.

    Each event intersects only its own camera's recorded footage; resulting
    windows are combined for synchronized multi-camera review. Acknowledgement
    and sidebar filters do not remove events from this playback mode.
    """
    low, high = bounds or (0, now_ms)
    low, high = max(0, low), min(now_ms, high)
    by_camera: dict[str, list[Interval]] = defaultdict(list)
    for event in events:
        camera = event.camera_id
        if camera is None or camera not in coverage or event.kind not in PLAYBACK_KINDS:
            continue
        pre, post = (padding or {}).get(camera, (10, 15))
        end = (
            high
            if event.end_ms is None
            else max(event.start_ms + 1000, event.end_ms) + max(0, post) * 1000
        )
        start = max(low, event.start_ms - max(0, pre) * 1000)
        if min(end, high) > start:
            by_camera[camera].append((start, min(end, high)))
    windows: list[Interval] = []
    for camera, events_for_camera in by_camera.items():
        activity = _merge(events_for_camera)
        recordings = _merge(coverage[camera])
        i, j = 0, 0
        while i < len(activity) and j < len(recordings):
            start = max(activity[i][0], recordings[j][0])
            end = min(activity[i][1], recordings[j][1])
            if end > start:
                windows.append((start, end))
            if activity[i][1] <= recordings[j][1]:
                i += 1
            else:
                j += 1
    return EventReviewPlan(tuple(_merge(windows)))
