"""The desktop playback timeline: long ranges, wheel zoom, panning, speeds."""

from __future__ import annotations

import datetime
from itertools import pairwise

import pytest
from PySide6.QtCore import QPoint, QPointF, Qt
from PySide6.QtGui import QWheelEvent

from neurowatch.widgets.playback_timeline import PlaybackTimeline, ruler_ticks, tick_label

H = 3_600_000
M = 60_000


def test_timeline_supports_bookmarks_health_and_alarm_kinds_with_distinct_colours(qtbot):
    from neurowatch.models.events import EVENT_COLORS, Event

    timeline = PlaybackTimeline()
    qtbot.addWidget(timeline)
    timeline.set_range(0, 100000)
    timeline.set_camera_rows(
        [{"camera_id": "cam", "label": "Gate", "segments": [{"start_ms": 0, "end_ms": 100000}]}]
    )
    timeline.set_motion_events(
        [
            Event(i, kind, "cam", i * 1000, i * 1000 + 500)
            for i, kind in enumerate(
                ("motion", "tamper", "bookmark", "digital_input", "recording_failed"), 1
            )
        ]
    )
    assert len(timeline._motion_events["cam"]) == 5
    assert (
        len(
            {
                EVENT_COLORS[kind]
                for kind in ("motion", "tamper", "bookmark", "digital_input", "recording_failed")
            }
        )
        == 5
    )
    assert "bookmark" in timeline._motion_legend.toolTip().lower()


def test_overlapping_marker_severity_is_preserved_when_new_motion_arrives(qtbot):
    from neurowatch.models.events import Event

    timeline = PlaybackTimeline()
    qtbot.addWidget(timeline)
    timeline.set_range(0, 100000)
    timeline.set_camera_rows([{"camera_id": "cam", "label": "Gate", "segments": []}])
    timeline.set_motion_events(
        [Event(1, "recording_failed", "cam", 1000, 5000), Event(2, "motion", "cam", 2000, 4000)]
    )
    assert timeline._motion_events["cam"][-1].kind == "recording_failed"
    timeline.update_motion_event(Event(3, "motion", "cam", 3000, 3500))
    assert timeline._motion_events["cam"][-1].kind == "recording_failed"


def _local_ms(*args: int) -> int:
    return int(datetime.datetime(*args).timestamp() * 1000)


def _wheel(
    widget: PlaybackTimeline,
    x: float,
    dy: int = 0,
    dx: int = 0,
    modifiers: Qt.KeyboardModifier = Qt.KeyboardModifier.NoModifier,
) -> None:
    point = QPointF(x, 40)
    event = QWheelEvent(
        point,
        widget.mapToGlobal(point),
        QPoint(0, 0),
        QPoint(dx, dy),
        Qt.MouseButton.NoButton,
        modifiers,
        Qt.ScrollPhase.NoScrollPhase,
        False,
    )
    widget.wheelEvent(event)


def _timeline(qtbot, start: int, end: int) -> PlaybackTimeline:
    timeline = PlaybackTimeline()
    qtbot.addWidget(timeline)
    timeline.resize(1000, 160)
    timeline.set_range(start, end)
    timeline.set_camera_rows([{"label": "Gate", "segments": [{"start_ms": start, "end_ms": end}]}])
    return timeline


def test_ruler_marks_fit_the_width_and_fall_on_local_clock_boundaries():
    start = _local_ms(2026, 10, 2, 0, 30)
    step, ticks = ruler_ticks(start, start + 48 * H, 800)
    assert step >= 6 * H
    assert 2 <= len(ticks) <= 10
    for at in ticks:
        local = datetime.datetime.fromtimestamp(at / 1000)
        assert local.minute == 0 and local.hour % (step // H) == 0

    step, ticks = ruler_ticks(start, start + 10 * M, 800)
    assert step <= M
    assert all(datetime.datetime.fromtimestamp(at / 1000).second == 0 for at in ticks)


@pytest.mark.parametrize("days", [7, 30, 90, 365, 1095])
@pytest.mark.parametrize("track_width", [80, 200, 532, 1000])
def test_long_ranges_never_generate_more_daily_labels_than_the_width_allows(days, track_width):
    start = _local_ms(2026, 10, 1, 12, 30)
    step, ticks = ruler_ticks(start, start + days * 24 * H, track_width)
    assert 1 <= len(ticks) <= max(1, track_width // 80) + 1
    if days >= 30:
        assert step > 24 * H, "month/year ranges must use wider spacing than daily marks"


@pytest.mark.parametrize("width", [240, 520, 1000])
def test_month_and_year_ruler_labels_do_not_overlap_or_cross_the_camera_gutter(qtbot, width):
    start = _local_ms(2026, 10, 1)
    timeline = _timeline(qtbot, start, start + 365 * 24 * H)
    timeline.resize(width, 180)
    left, right, _track_width = timeline._track_bounds()
    labels = timeline._ruler_labels()
    assert labels, "even a very narrow timeline must show the date"
    for _time, rect, text in labels:
        assert left <= rect.left() <= rect.right() < right
        assert text, "a label must not be silently truncated to empty"
    assert all(a[1].right() < b[1].left() for a, b in pairwise(labels))
    # Zooming in restores the more detailed hour/second ruler.
    timeline._view_start_ms = start + H
    timeline._view_end_ms = start + H + 10 * M
    assert all(":" in text for _time, _rect, text in timeline._ruler_labels())


def test_long_date_and_motion_status_wrap_below_buttons_instead_of_overlapping(qtbot):
    start = _local_ms(2026, 10, 1)
    timeline = _timeline(qtbot, start, start + 90 * 24 * H)
    timeline.resize(480, 180)
    timeline.set_playhead(start + 45 * 24 * H)
    timeline.set_motion_status("Motion history unavailable")
    timeline.show()
    qtbot.waitExposed(timeline)
    assert timeline._controls_wrapped
    assert timeline._buttons.geometry().bottom() < timeline._details.geometry().top()
    for control in [
        timeline._play_btn,
        *timeline._speed_buttons.values(),
        timeline._fit_btn,
        timeline._motion_legend,
        timeline._time_label,
    ]:
        pos = control.mapTo(timeline, QPoint())
        assert timeline.rect().contains(pos)
        assert timeline.rect().contains(pos + control.rect().bottomRight())
    speeds: list[float] = []
    timeline.speed_changed.connect(speeds.append)
    timeline._speed_buttons[2.0].click()
    assert speeds == [2.0]
    timeline.resize(1000, 180)
    assert not timeline._controls_wrapped
    timeline._fit_btn.click()
    assert timeline._view_end_ms == start + 90 * 24 * H


def test_labels_name_the_day_at_midnight_and_show_seconds_close_up():
    midnight = _local_ms(2026, 10, 3)
    assert tick_label(midnight, 6 * H) == datetime.datetime(2026, 10, 3).strftime("%b %d")
    assert tick_label(midnight + 6 * H, 6 * H) == "06:00"
    assert tick_label(midnight + 6 * H + 10_000, 10_000) == "06:00:10"


def test_multi_day_dates_are_grouped_above_their_time_marks(qtbot):
    from neurowatch.widgets.playback_timeline import _DATE_BAND_H, _RULER_H

    start = _local_ms(2026, 10, 13, 6)
    timeline = _timeline(qtbot, start, _local_ms(2026, 10, 15, 18))
    bands = timeline._day_bands()
    assert timeline.ruler_height() == _RULER_H + _DATE_BAND_H
    assert [text for _s, _e, _box, text in bands] == [
        datetime.datetime(2026, 10, day).strftime("%a, %b %d") for day in (13, 14, 15)
    ]
    for at, box, text in timeline._ruler_labels():
        assert text == datetime.datetime.fromtimestamp(at / 1000).strftime("%H:%M")
        assert box.top() >= _DATE_BAND_H
        assert any(s <= at < e for s, e, _r, _text in bands), "every time belongs to a named day"
    assert all(a[2].right() < b[2].left() for a, b in pairwise(bands))


def test_zoom_and_pan_keep_the_visible_date_even_without_a_midnight_tick(qtbot):
    start = _local_ms(2026, 10, 13, 6)
    timeline = _timeline(qtbot, start, _local_ms(2026, 10, 15, 18))
    timeline._view_start_ms = _local_ms(2026, 10, 14, 13, 15)
    timeline._view_end_ms = timeline._view_start_ms + H
    assert len(timeline._day_bands()) == 1
    assert "Oct 14" in timeline._day_bands()[0][3]
    timeline._pan_to(_local_ms(2026, 10, 15, 13, 15))
    assert "Oct 15" in timeline._day_bands()[0][3]
    timeline.fit_view()
    assert len(timeline._day_bands()) == 3


def test_midnight_labels_never_shift_under_the_previous_day_on_partial_ranges(qtbot):
    start = _local_ms(2026, 10, 11, 0, 52)
    timeline = _timeline(qtbot, start, _local_ms(2026, 10, 14, 0, 52))
    left, _right, width = timeline._track_bounds()
    duration = timeline._view_end_ms - timeline._view_start_ms
    for at, box, _text in timeline._ruler_labels():
        day_start, day_end, _rect, _name = next(
            band for band in timeline._day_bands() if band[0] <= at < band[1]
        )
        start_x = left + (day_start - timeline._view_start_ms) / duration * width
        end_x = left + (day_end - timeline._view_start_ms) / duration * width
        assert box.left() >= start_x
        assert box.right() < end_x, "a time label must stay inside its actual day"


def test_new_year_dates_include_the_year_and_single_day_ranges_stay_compact(qtbot):
    from neurowatch.widgets.playback_timeline import _RULER_H

    start = _local_ms(2025, 12, 29)
    timeline = _timeline(qtbot, start, _local_ms(2026, 1, 2))
    names = [text for _s, _e, _box, text in timeline._day_bands()]
    assert "2025" in names[0] and "2026" in names[-1]
    timeline.set_range(_local_ms(2026, 10, 14, 10), _local_ms(2026, 10, 14, 11))
    assert timeline.ruler_height() == _RULER_H
    assert not timeline._day_bands()


def test_hovering_the_ruler_shows_the_full_date_and_time(qtbot, monkeypatch):
    from PySide6.QtCore import QEvent
    from PySide6.QtGui import QHelpEvent
    from PySide6.QtWidgets import QApplication

    from neurowatch.widgets import playback_timeline

    start = _local_ms(2026, 10, 13, 6)
    timeline = _timeline(qtbot, start, _local_ms(2026, 10, 15, 18))
    texts: list[str] = []
    monkeypatch.setattr(
        playback_timeline.QToolTip, "showText", lambda _p, text, _w: texts.append(text)
    )
    left, _, width = timeline._track_bounds()
    point = QPoint(left + width // 2, timeline.ruler_height() - 8)
    event = QHelpEvent(QEvent.Type.ToolTip, point, timeline.mapToGlobal(point))
    QApplication.sendEvent(timeline, event)
    local = datetime.datetime.fromtimestamp(timeline._x_to_ms(point.x()) / 1000)
    assert local.strftime("%a, %d %b %Y") in texts[0]
    assert local.strftime("%H:%M:%S") in texts[0]


def test_a_long_range_is_shown_whole_then_zooms_around_the_pointer(qtbot):
    start = _local_ms(2026, 10, 1)
    timeline = _timeline(qtbot, start, start + 3 * 24 * H)
    assert (timeline._view_start_ms, timeline._view_end_ms) == (start, start + 72 * H)

    left, _, width = timeline._track_bounds()
    x = left + width / 4
    under = timeline._x_to_ms(x)
    _wheel(timeline, x, dy=120)
    view = timeline._view_end_ms - timeline._view_start_ms
    assert view < 72 * H
    # The time under the pointer stays under the pointer.
    assert abs(timeline._x_to_ms(x) - under) < view / width * 2


def test_sideways_scroll_and_the_scrollbar_pan_and_fit_shows_everything(qtbot):
    start = _local_ms(2026, 10, 1)
    timeline = _timeline(qtbot, start, start + 24 * H)
    assert not timeline._scrollbar.isVisibleTo(timeline)

    for _ in range(6):
        _wheel(timeline, 500, dy=120)
    width = timeline._view_end_ms - timeline._view_start_ms
    assert timeline._scrollbar.isVisibleTo(timeline)

    before = timeline._view_start_ms
    _wheel(timeline, 500, dx=-120)  # a sideways swipe
    assert timeline._view_start_ms > before
    assert timeline._view_end_ms - timeline._view_start_ms == width
    shifted = timeline._view_start_ms
    _wheel(timeline, 500, dy=-120, modifiers=Qt.KeyboardModifier.ShiftModifier)
    assert timeline._view_start_ms != shifted

    timeline._scrollbar.setValue(timeline._scrollbar.minimum())
    assert timeline._view_start_ms == start

    timeline._fit_btn.click()
    assert (timeline._view_start_ms, timeline._view_end_ms) == (start, start + 24 * H)
    assert not timeline._scrollbar.isVisibleTo(timeline)


def test_the_speed_buttons_show_the_current_speed_and_one_x_restores_it(qtbot):
    start = _local_ms(2026, 10, 1)
    timeline = _timeline(qtbot, start, start + H)
    speeds: list[float] = []
    timeline.speed_changed.connect(speeds.append)

    timeline._speed_buttons[4.0].click()
    assert speeds == [4.0]
    assert timeline._speed_buttons[4.0].isChecked()
    assert not timeline._speed_buttons[1.0].isChecked()

    timeline._speed_buttons[1.0].click()
    assert speeds == [4.0, 1.0]
    assert timeline._speed_buttons[1.0].isChecked()

    # Loading recordings again starts at normal speed, without a request.
    timeline._speed_buttons[2.0].click()
    timeline.set_range(start, start + H)
    assert timeline._speed_buttons[1.0].isChecked()
    assert speeds == [4.0, 1.0, 2.0]


def test_running_out_of_footage_pauses_the_video_too(qtbot):
    start = _local_ms(2026, 10, 1)
    timeline = _timeline(qtbot, start, start + H)
    toggles: list[bool] = []
    timeline.play_pause_toggled.connect(toggles.append)

    timeline._play_btn.click()
    assert toggles == [True]
    timeline.set_playhead(start + H - 100)
    timeline._on_tick()

    # The timeline says stopped, so the sessions must not keep playing.
    assert toggles == [True, False]
    assert not timeline._is_playing
    assert "Play" in timeline._play_btn.text()


def test_motion_markers_show_duration_on_the_correct_camera_and_hover_details(qtbot, monkeypatch):
    from PySide6.QtCore import QEvent
    from PySide6.QtGui import QHelpEvent
    from PySide6.QtWidgets import QApplication

    from neurowatch.models.events import Event
    from neurowatch.widgets import playback_timeline

    start = _local_ms(2026, 10, 13, 10)
    timeline = _timeline(qtbot, start, start + H)
    timeline.set_camera_rows(
        [
            {
                "camera_id": camera,
                "label": label,
                "segments": [{"start_ms": start, "end_ms": start + H}],
            }
            for camera, label in [("cam-a", "Gate"), ("cam-b", "Yard")]
        ]
    )
    motion = Event(1, "motion", "cam-a", start + 10 * M, start + 20 * M)
    crossed = Event(2, "line_crossing", "cam-b", start + 30 * M, start + 30 * M)
    timeline.set_motion_events([motion, crossed, Event(3, "motion", "other", start, None)])
    timeline.show()
    qtbot.waitExposed(timeline)
    track_h = timeline.height() - timeline._ctrl.height() - playback_timeline._SCROLLBAR_H - 2
    rows = timeline._row_geometry(track_h)
    first = timeline._motion_rect(motion, *rows[0][1:])
    second = timeline._motion_rect(crossed, *rows[1][1:])
    assert first.width() > second.width() == 3, "an instant event is still visible"
    image = timeline.grab().toImage()
    assert image.pixelColor(first.center()) == playback_timeline._MOTION_FILL
    assert image.pixelColor(second.center()) == playback_timeline._MOTION_FILL
    assert timeline._motion_at(first.center())[0]["label"] == "Gate"
    assert timeline._motion_at(second.center())[0]["label"] == "Yard"
    assert "other" not in timeline._motion_events

    tooltips: list[str] = []
    monkeypatch.setattr(
        playback_timeline.QToolTip, "showText", lambda _p, text, _w: tooltips.append(text)
    )
    event = QHelpEvent(QEvent.Type.ToolTip, first.center(), timeline.mapToGlobal(first.center()))
    QApplication.sendEvent(timeline, event)
    assert "Gate · Motion" in tooltips[0]
    assert "10:10:00" in tooltips[0] and "10:20:00" in tooltips[0]
    assert "10 min" in tooltips[0]

    timeline._pan_to(start + 25 * M)
    # At full zoom panning is clamped; zoom in before selecting a later interval.
    timeline._view_start_ms, timeline._view_end_ms = start + 25 * M, start + 35 * M
    assert timeline._motion_rect(motion, *rows[0][1:]).isNull()
    assert not timeline._motion_rect(crossed, *rows[1][1:]).isNull()


def test_motion_crossing_range_boundaries_is_clipped_and_clear_does_not_erase_history(qtbot):
    from neurowatch.models.events import Event
    from neurowatch.widgets import playback_timeline

    start = _local_ms(2026, 10, 13, 10)
    timeline = _timeline(qtbot, start, start + H)
    timeline.set_camera_rows(
        [
            {
                "camera_id": "cam-a",
                "label": "Gate",
                "segments": [{"start_ms": start, "end_ms": start + H}],
            }
        ]
    )
    event = Event(1, "motion", "cam-a", start - M, start + 2 * M, acknowledged=True)
    timeline.set_motion_events([event])
    assert timeline._motion_events["cam-a"] == [event], "clearing a tile alert keeps the timeline"
    track_h = timeline.height() - timeline._ctrl.height() - playback_timeline._SCROLLBAR_H - 2
    row, y, height = timeline._row_geometry(track_h)[0]
    marker = timeline._motion_rect(event, y, height)
    assert marker.left() == timeline._track_bounds()[0]
    assert not marker.isNull()
    ongoing = Event(2, "motion", "cam-a", start + 20 * M, None)
    assert timeline._motion_rect(ongoing, y, height).right() <= timeline._track_bounds()[1]
    timeline.set_camera_rows([{**row, "camera_id": "cam-b"}])
    assert not timeline._motion_events, "a new selection cannot inherit old markers"


def test_events_only_skips_quiet_footage_and_pauses_at_the_last_event(qtbot):
    from neurowatch.models.events import Event

    timeline = PlaybackTimeline()
    qtbot.addWidget(timeline)
    timeline.set_range(0, 120_000)
    timeline.set_camera_rows([{"camera_id": "a", "segments": [{"start_ms": 0, "end_ms": 120_000}]}])
    timeline.set_motion_events(
        [Event(1, "motion", "a", 10_000, 20_000), Event(2, "bookmark", "a", 60_000, 60_000)]
    )
    seeks, playing = [], []
    timeline.seek_requested.connect(seeks.append)
    timeline.play_pause_toggled.connect(playing.append)
    timeline.set_event_review(True, padding={"a": (0, 0)})
    timeline.set_playhead(0)
    timeline.toggle_play()
    assert seeks == [10_000] and playing == [True]
    timeline.set_playhead(19_900)
    timeline._on_tick()
    assert seeks == [10_000, 60_000]
    timeline.set_playhead(60_900)
    timeline._on_tick()
    assert not timeline._is_playing and playing[-1] is False
    assert timeline._playhead_ms == 61_000
    timeline.set_event_review(False)
    timeline.set_playhead(30_000)
    timeline.toggle_play()
    timeline._on_tick()
    assert timeline._playhead_ms == 30_250
    timeline.stop_playback()


def test_empty_event_review_does_not_start_or_loop_and_ignores_metadata_only_changes(qtbot):
    from neurowatch.models.events import Event

    timeline = PlaybackTimeline()
    qtbot.addWidget(timeline)
    timeline.set_range(0, 100_000)
    timeline.set_camera_rows([{"camera_id": "a", "segments": [{"start_ms": 0, "end_ms": 100_000}]}])
    timeline.set_motion_events([])
    seeks = []
    timeline.seek_requested.connect(seeks.append)
    timeline.set_event_review(True)
    timeline.toggle_play()
    assert not timeline._is_playing and seeks == []
    event = Event(1, "motion", "a", 10_000, 20_000)
    timeline.update_motion_event(event)
    timeline.toggle_play()
    assert timeline._is_playing
    assert not timeline._event_plan_dirty
    timeline.update_motion_event(Event(1, "motion", "a", 10_000, 20_000, acknowledged=True))
    assert not timeline._event_plan_dirty
    timeline.stop_playback()
