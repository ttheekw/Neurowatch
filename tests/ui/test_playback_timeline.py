"""The desktop playback timeline: long ranges, wheel zoom, panning, speeds."""

from __future__ import annotations

import datetime

from PySide6.QtCore import QPoint, QPointF, Qt
from PySide6.QtGui import QWheelEvent

from neurowatch.widgets.playback_timeline import PlaybackTimeline, ruler_ticks, tick_label

H = 3_600_000
M = 60_000


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


def test_labels_name_the_day_at_midnight_and_show_seconds_close_up():
    midnight = _local_ms(2026, 10, 3)
    assert tick_label(midnight, 6 * H) == datetime.datetime(2026, 10, 3).strftime("%b %d")
    assert tick_label(midnight + 6 * H, 6 * H) == "06:00"
    assert tick_label(midnight + 6 * H + 10_000, 10_000) == "06:00:10"


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
