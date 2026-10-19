"""Event export presets do not create jobs and never select unrelated footage."""

from neurowatch.models.event_clips import event_clip
from neurowatch.models.events import Event

T0 = 1_800_000_000_000


def test_padding_defaults_zero_and_instant_events():
    event = Event(1, "motion", "gate", T0, T0 + 5000)
    assert event_clip(event, now_ms=T0 + 60_000).bounds == (T0 - 10_000, T0 + 20_000)
    assert event_clip(event, pre_sec=0, post_sec=0, now_ms=T0 + 60_000).bounds == (T0, T0 + 5000)
    instant = Event(2, "bookmark", "gate", T0, T0)
    assert event_clip(instant, pre_sec=0, post_sec=0, now_ms=T0 + 60_000).bounds == (T0, T0 + 1000)


def test_ongoing_clips_stop_at_now_and_clamp_to_overlapping_coverage():
    event = Event(1, "motion", "gate", T0, None)
    clip = event_clip(event, now_ms=T0 + 30_000, coverage=[(T0 - 5000, T0 + 25_000)])
    assert clip.bounds == (T0 - 5000, T0 + 25_000)
    assert clip.adjusted and clip.ongoing
    assert event_clip(event, now_ms=T0 + 30_000).bounds[1] == T0 + 30_000
    assert event_clip(event, now_ms=T0 + 30_000, coverage=[(T0 + 60_000, T0 + 70_000)]) is None


def test_limits_gaps_unsorted_coverage_and_bad_padding():
    event = Event(1, "motion", "gate", T0, T0 + 60_000)
    clip = event_clip(
        event,
        now_ms=T0 + 80_000,
        max_ms=20_000,
        coverage=[(T0 + 50_000, T0 + 70_000), (T0 - 3000, T0 + 3000)],
    )
    assert clip.bounds == (T0 - 3000, T0 + 17_000)
    assert clip.adjusted
    assert event_clip(event, pre_sec=-1, now_ms=T0 + 80_000) is None
    assert event_clip(event, max_ms=0, now_ms=T0 + 80_000) is None


def test_too_short_coverage_is_not_expanded_into_unrelated_footage():
    event = Event(1, "motion", "gate", T0, T0 + 5000)
    assert event_clip(event, now_ms=T0 + 60_000, coverage=[(T0, T0 + 200)]) is None
