"""Events-only review intersects each camera's footage and never bridges gaps."""

from __future__ import annotations

from neurowatch.models.event_review import event_review_plan
from neurowatch.models.events import Event


def test_event_review_merges_padding_and_intersects_only_the_event_camera():
    events = [
        Event(1, "motion", "a", 20_000, 25_000),
        Event(2, "bookmark", "a", 35_000, 35_000),
        Event(3, "motion", "missing", 0, 200_000),
    ]
    plan = event_review_plan(
        events, {"a": [(0, 30_000), (40_000, 60_000)], "b": [(30_000, 40_000)]}, now_ms=100_000
    )
    assert plan.intervals == ((10_000, 30_000), (40_000, 51_000))
    assert plan.next_at(0) == 10_000
    assert plan.next_at(20_000) == 20_000
    assert plan.next_at(30_000) == 40_000
    assert plan.next_at(51_000) is None


def test_zero_padding_ongoing_bounds_and_system_events():
    events = [
        Event(1, "bookmark", "a", 10_000, 10_000),
        Event(2, "motion", "a", 50_000, None),
        Event(3, "backend_started", "a", 0, 100_000),
    ]
    plan = event_review_plan(
        events, {"a": [(0, 100_000)]}, now_ms=80_000, bounds=(0, 60_000), padding={"a": (0, 0)}
    )
    assert plan.intervals == ((10_000, 11_000), (50_000, 60_000))
    assert plan.next_at(11_000) == 50_000


def test_unsorted_overlapping_coverage_has_no_duplicate_jumps():
    plan = event_review_plan(
        [Event(1, "motion", "a", 20_000, 50_000)],
        {"a": [(40_000, 60_000), (0, 30_000), (25_000, 45_000)]},
        now_ms=100_000,
    )
    assert plan.intervals == ((10_000, 60_000),)
    assert event_review_plan([], {"a": [(0, 100_000)]}, now_ms=100_000).next_at(0) is None
