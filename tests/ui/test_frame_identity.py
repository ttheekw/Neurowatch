from __future__ import annotations

import dataclasses
import struct

from neurowatch.ipc import frame_reader


def test_frame_identity_trailer_is_optional_and_uses_64_bit_capture_time(monkeypatch) -> None:
    monkeypatch.setattr(frame_reader, "FRAME_IDENTITIES_OFFSET", 64)
    ring = bytearray(64 + frame_reader.SLOT_COUNT * 16)
    struct.pack_into("<Qq", ring, 64 + 2 * 16, 2**40, 1_700_000_000_123_456)
    assert frame_reader.frame_identity(ring, 2) == (2**40, 1_700_000_000_123_456)
    assert frame_reader.frame_identity(ring[:64], 2) == (0, 0)
    assert frame_reader.frame_identity(ring, -1) == (0, 0)
    assert frame_reader.frame_identity(ring, frame_reader.SLOT_COUNT) == (0, 0)


def test_drawings_start_on_their_picture_and_hold_only_for_their_lifetime() -> None:
    from neurowatch.models.analytics import Drawing, matches_picture

    drawing = Drawing("c", "p", "boxes", (), 100.0, frame=5, time_us=1_000_000, ttl_ms=500)
    assert matches_picture(drawing, 5, 1_000_000)
    assert not matches_picture(drawing, 4, 960_000), "never annotate an older catch-up picture"
    assert matches_picture(drawing, 6, 1_040_000), "hold while waiting for the next analysis result"
    assert not matches_picture(drawing, 100, 1_600_000), "do not hold beyond the declared lifetime"
    assert matches_picture(drawing, 1000, 1_000_000), "a second stream aligns by capture time"
    assert matches_picture(drawing, 0, 0), "an old backend has no frame metadata"


def test_main_sub_drawings_align_on_node_observation_not_independent_stream_clocks() -> None:
    from neurowatch.models.analytics import Drawing, matches_picture, parse_overlays

    drawings = parse_overlays(
        {
            "overlays": [
                {
                    "camera_id": "c",
                    "plugin": "p",
                    "layer": "boxes",
                    "items": [{"type": "box", "rect": [0.1, 0.2, 0.3, 0.4]}],
                    "frame": 5,
                    "time_us": 90_000_000,
                    "observed_us": 10_000_000,
                    "ttl_ms": 500,
                }
            ]
        },
        0,
    )
    drawing = drawings["c"][0]
    assert matches_picture(drawing, 100, 1_000_000, 10_040_000)
    assert not matches_picture(drawing, 99, 1_000_000, 9_960_000)
    assert not matches_picture(drawing, 200, 1_000_000, 10_600_000)
    # Recorded history has no live observation clock; keep its presentation timeline.
    history = Drawing("c", "p", "boxes", (), 100, time_us=1_000_000, ttl_ms=500)
    assert matches_picture(history, 0, 1_040_000, 10_000_000)


def test_observation_trailer_keeps_older_ring_identity_layout(monkeypatch) -> None:
    monkeypatch.setattr(frame_reader, "FRAME_OBSERVED_OFFSET", 64)
    ring = bytearray(64 + frame_reader.SLOT_COUNT * 8)
    struct.pack_into("<q", ring, 64 + 2 * 8, 1_700_000_000_123_456)
    assert frame_reader.frame_observed_us(ring, 2) == 1_700_000_000_123_456
    assert frame_reader.frame_observed_us(ring[:64], 2) == 0
    assert frame_reader.frame_observed_us(ring, -1) == 0


def test_a_result_ahead_of_the_shown_picture_keeps_the_one_before_on_screen() -> None:
    """The flicker fix: a plugin's newest result names a picture the tile has
    not shown yet; the previous result stays until that picture arrives."""
    from neurowatch.models.analytics import Drawing, DrawingHistory

    def result(n: int, at_ms: int, until: float = 100.0, layer: str = "detections") -> Drawing:
        us = at_ms * 1000
        return Drawing("c", "p", layer, ({"n": n},), until, n, us, 1500, us)

    history = DrawingHistory()
    first, second = result(1, 1000), result(2, 1300)
    history.update([first, result(1, 1000, layer="counts")])
    assert [d.layer for d in history.shown((0, 1, 1_100_000), 0)] == ["detections", "counts"]
    history.update([second, result(1, 1000, layer="counts")])
    # The tile still shows a picture from 1.1 s: the first result covers it.
    assert history.shown((0, 1, 1_100_000), 0)[0] is first
    # Its own picture arrives: the second result replaces it.
    assert history.shown((2, 1, 1_300_000), 0)[0] is second
    # Re-sent unchanged results do not push older ones out.
    for n in range(10):
        history.update([dataclasses.replace(second, until=100.0 + n)])
    assert history.shown((2, 1, 1_300_000), 0)[0].until == 109.0, "the renewed lifetime counts"
    history.update([result(3, 1600)])
    assert history.shown((0, 1, 1_400_000), 0)[0].items == second.items
    assert history.shown((0, 1, 1_100_000), 0)[0] is first, "still kept after the re-sends"
    # A layer no longer drawn is gone, and expired results are never shown.
    assert [d.layer for d in history.shown((0, 1, 1_400_000), 0)] == ["detections"]
    assert history.shown((0, 1, 1_400_000), 200.0) == []
    history.prune(200.0)
    assert history.until() == []
    # An older backend has no picture identities: the newest result is shown.
    history.update([first])
    history.update([second])
    assert history.shown((0, 0, 0), 0) == [second]
    # A result replacing one for the same picture supersedes it, even expired.
    history.update([dataclasses.replace(second, until=-1.0)])
    assert history.shown((2, 1, 1_300_000), 0) == []


def test_a_drawing_resent_in_later_snapshots_keeps_its_declared_lifetime() -> None:
    """The recorder's ``ttl_ms`` is the time left; matching against it hid a
    layer re-sent unchanged half way through its life (blinking lines)."""
    from neurowatch.models.analytics import matches_picture, parse_overlays

    entry = {
        "camera_id": "c",
        "plugin": "p",
        "layer": "lines",
        "frame": 5,
        "items": [{"type": "box", "rect": [0.1, 0.2, 0.3, 0.4]}],
        "time_us": 1_000_000,
        "observed_us": 1_000_000,
    }
    # 900 ms after it arrived: 600 ms left of a 1.5 s lifetime.
    later = parse_overlays({"overlays": [{**entry, "ttl_ms": 600, "lifetime_ms": 1500}]}, 10.0)
    drawing = later["c"][0]
    assert drawing.until == 10.6, "it still lapses when its time is up"
    assert matches_picture(drawing, 30, 2_100_000, 2_100_000), "1.1 s after its picture"
    assert not matches_picture(drawing, 40, 2_600_000, 2_600_000)
    # An older recorder sends no lifetime: the time left is all there is.
    old = parse_overlays({"overlays": [{**entry, "ttl_ms": 600}]}, 10.0)["c"][0]
    assert old.ttl_ms == 600


def _analysed(n: int, until: float = 1e9, layer: str = "detections"):
    from neurowatch.models.analytics import Drawing

    us = n * 100_000
    return Drawing("c1", "p", layer, ({"n": n},), until, n, us, 2500, us)


def test_analysed_pictures_find_the_picture_a_result_describes() -> None:
    from neurowatch.models.analytics import AnalysedPictures, analysed_target

    pictures = AnalysedPictures(keep_us=1_000_000, max_bytes=1000)
    for n in range(1, 21):
        pictures.add((n, n * 100_000, n * 100_000), f"frame {n}", 10)
    # Older than keep_us (receipt clock) is gone; the rest are found by picture.
    assert pictures.find(_analysed(7)) is None
    assert pictures.find(_analysed(15)) == "frame 15"
    # Another stream of the camera: other sequence numbers, nearest receipt time.
    other = dataclasses.replace(_analysed(15), frame=900, observed_us=1_540_000)
    assert pictures.find(other) == "frame 15"
    assert pictures.find(dataclasses.replace(other, observed_us=5_000_000)) is None
    # The byte limit holds the newest pictures only.
    for n in range(21, 200):
        pictures.add((n, n * 100_000, n * 100_000), f"frame {n}", 100)
    assert pictures.find(_analysed(185)) is None and pictures.find(_analysed(195)) == "frame 195"
    # The newest current result with a picture decides; expired ones do not.
    assert analysed_target([_analysed(3), _analysed(5, layer="lines")], 0).frame == 5
    assert analysed_target([_analysed(5, until=-1.0)], 0) is None
    # An older recorder without picture identities holds nothing.
    pictures.clear()
    pictures.add((0, 0, 0), "frame", 10)
    assert pictures.find(_analysed(0)) is None


def test_a_result_never_lands_on_a_picture_before_the_one_it_describes() -> None:
    from neurowatch.models.analytics import AnalysedPictures

    pictures = AnalysedPictures()
    for n in range(1, 6):
        pictures.add((n, n * 100_000, n * 100_000), f"frame {n}", 10)
    # A fast plugin: the result is here, its picture is still on its way.
    assert pictures.find(_analysed(6)) is None, "frame 5 is near, but boxes would lead the video"
    pictures.add((6, 600_000, 600_000), "frame 6", 10)
    assert pictures.find(_analysed(6)) == "frame 6"
    # Another stream of the camera, between two pictures: nearest, once it has passed.
    other = dataclasses.replace(_analysed(6), frame=900, observed_us=660_000)
    assert pictures.find(other) is None
    pictures.add((7, 700_000, 700_000), "frame 7", 10)
    assert pictures.find(other) == "frame 7"


def test_large_pictures_are_thinned_so_they_outlast_a_slow_plugin() -> None:
    from neurowatch.models.analytics import AnalysedPictures

    delay_us = 1_250_000  # the CPU model on 1080p, as measured
    size = 4 * 1920 * 1080 * 3  # large enough to have to thin within the budget
    pictures = AnalysedPictures()
    found = []
    for n in range(1, 30 * 12):
        us = n * 33_333
        pictures.add((n, us, us), n, size)
        assert pictures._bytes <= pictures.max_bytes
        if us <= delay_us or n % 10:
            continue
        seen = us - delay_us
        result = dataclasses.replace(_analysed(0), frame=None, time_us=seen, observed_us=seen)
        pictures.note_result(result)
        found.append((n, pictures.find(result)))
    settled = [picture for n, picture in found if n > 30 * 4]
    assert settled and None not in settled, "every result finds a picture once the delay is known"
    for n, picture in found:
        if n > 30 * 4:
            seen_n = (n * 33_333 - delay_us) / 33_333
            assert abs(picture - seen_n) * 33_333 <= 200_000, "and it is close to the right one"
    pictures = AnalysedPictures()
    for n in range(1, 100):
        pictures.add((n, n * 33_333, n * 33_333), n, 1000)
    assert len(pictures._held) == 99, "small pictures are all kept: exact matches stay exact"


def test_a_tile_never_shows_a_result_on_an_older_picture(qtbot) -> None:
    from neurowatch.models.camera import CameraConfig
    from neurowatch.widgets.video_wall import VideoWallWidget

    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall.set_grid(1, 1)
    wall.set_cameras([CameraConfig(id="c1", name="Road", url="rtsp://x/1")])
    wall.set_analytics({"c1": ("Analysed by Vehicle counting (running)", True)})
    cell = wall._cells[0]
    size = 32 * 18 * 3
    shown: list[int] = []
    upload = cell.update_frame

    def record(data: bytes, width: int, height: int, fmt: str = "rgb") -> None:
        shown.append(data[0])
        upload(data, width, height, fmt)

    cell.update_frame = record

    def arrive(n: int) -> None:
        us = n * 100_000
        wall._on_identified_frame_ready("c1", bytes([n]) * size, 32, 18, "rgb", n, us, us)
        wall._flush_pending_frames()

    for n in range(1, 6):
        arrive(n)
    # Results arrive before their pictures, as with a fast plugin.
    for n in range(6, 12):
        shown.clear()
        wall.set_drawings({"c1": [_analysed(n)]})
        assert shown == [], "its picture is not here yet"
        arrive(n)
        assert shown == [n], "the very picture, not the one before it"
        assert cell._frame_identity[0] == n


def test_pictures_that_stay_behind_the_newest_result_still_show_their_own(qtbot) -> None:
    """A fast plugin on the sub stream, a tile on the main stream arriving later:
    a newer result is always here by the time a picture arrives."""
    from neurowatch.models.camera import CameraConfig
    from neurowatch.widgets.video_wall import VideoWallWidget

    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall.set_grid(1, 1)
    wall.set_cameras([CameraConfig(id="c1", name="Road", url="rtsp://x/1")])
    wall.set_analytics({"c1": ("Analysed by Vehicle counting (running)", True)})
    cell = wall._cells[0]
    size = 32 * 18 * 3
    shown: list[int] = []
    upload = cell.update_frame

    def record(data: bytes, width: int, height: int, fmt: str = "rgb") -> None:
        shown.append(data[0])
        upload(data, width, height, fmt)

    cell.update_frame = record

    def arrive(n: int) -> None:
        us = n * 100_000
        wall._on_identified_frame_ready("c1", bytes([n]) * size, 32, 18, "rgb", n, us, us)
        wall._flush_pending_frames()

    for n in range(1, 4):
        arrive(n)
    for n in range(4, 30):
        wall.set_drawings({"c1": [_analysed(n + 2)]})  # always two ahead
        shown.clear()
        arrive(n)
        if n >= 6:
            assert shown == [n], "this picture, with the result about it"
            assert cell._frame_identity[0] == n
            assert not cell._drawings_paused, "boxes stay on"
            drawn = cell._drawing_history.shown(cell._frame_identity, 0)
            assert [d.frame for d in drawn] == [n]


def _opening_wall(qtbot):
    from neurowatch.models.camera import CameraConfig
    from neurowatch.widgets.video_wall import VideoWallWidget

    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall.set_grid(1, 1)
    cams = [CameraConfig(id=i, name=i, url=f"rtsp://x/{i}") for i in ("c1", "c2")]
    wall.set_cameras(cams, page=0)
    wall.set_analytics({"c1": ("Analysed by Vehicle counting (running)", True)})
    cell = wall._cells[0]
    size = 32 * 18 * 3
    shown: list[int] = []
    upload = cell.update_frame

    def record(data: bytes, width: int, height: int, fmt: str = "rgb") -> None:
        shown.append(data[0])
        upload(data, width, height, fmt)

    cell.update_frame = record

    def arrive(n: int) -> None:
        us = n * 100_000
        wall._on_identified_frame_ready("c1", bytes([n]) * size, 32, 18, "rgb", n, us, us)
        wall._flush_pending_frames()

    return wall, cell, shown, arrive


def test_opening_a_camera_a_plugin_draws_on_shows_no_plain_video_first(qtbot) -> None:
    wall, cell, shown, arrive = _opening_wall(qtbot)
    # The plugin has been running: its newest result is about a picture this tile
    # never saw, and the ones it did see are still being analysed.
    wall.set_drawings({"c1": [_analysed(1)]})
    for n in range(50, 56):
        arrive(n)
    assert shown == [], "not one picture without its drawings"
    assert cell._waiting_for_analysis, "and the empty tile says why"
    assert not cell._drawings_paused

    wall.set_drawings({"c1": [_analysed(52)]})
    assert shown == [52], "the first picture shown is one with its result"
    assert cell._frame_identity[0] == 52
    assert not cell._waiting_for_analysis
    assert [d.frame for d in cell._drawing_history.shown(cell._frame_identity, 0)] == [52]


def test_a_quiet_camera_with_nothing_drawn_opens_at_once(qtbot) -> None:
    _wall, cell, shown, arrive = _opening_wall(qtbot)
    arrive(1)
    assert shown == [1], "a plugin with nothing to draw must not hold the video back"
    assert not cell._waiting_for_analysis


def test_results_that_never_match_give_way_to_live_video(qtbot, monkeypatch) -> None:
    import neurowatch.widgets.video_wall as module

    wall, cell, shown, arrive = _opening_wall(qtbot)
    wall.set_drawings({"c1": [_analysed(1)]})
    now = [100.0]
    monkeypatch.setattr(module.time, "monotonic", lambda: now[0])
    arrive(50)
    assert shown == [] and cell._waiting_for_analysis
    now[0] += module._ANALYSED_WAIT_S - 0.1
    arrive(51)
    assert shown == [], "still waiting"
    now[0] += 0.2
    arrive(52)
    assert shown == [52], "never a black tile for good: a plugin may be far too slow"
    assert not cell._waiting_for_analysis


def test_a_tile_waits_for_a_plugin_still_starting_when_the_app_opens(qtbot, monkeypatch) -> None:
    import neurowatch.widgets.video_wall as module

    wall, cell, shown, arrive = _opening_wall(qtbot)
    wall.set_analytics({"c1": ("Analysed by Vehicle counting (starting)", False)})
    wall.set_awaited_cameras({"c1"})
    arrive(10)
    arrive(11)
    assert shown == [], "results are on their way: no plain video first"
    assert cell._waiting_for_analysis
    # Later than a result's own wait: the time starting does not count against it.
    now = [module.time.monotonic() + module._ANALYSED_WAIT_S + 1]
    monkeypatch.setattr(module.time, "monotonic", lambda: now[0])
    arrive(12)
    assert shown == []
    wall.set_drawings({"c1": [_analysed(1)]})
    arrive(13)
    assert shown == [], "its first result is about a picture from before this tile's"
    wall.set_drawings({"c1": [_analysed(13)]})
    assert shown == [13]
    assert not cell._waiting_for_analysis


def test_a_plugin_that_never_finishes_starting_gives_way_to_live_video(qtbot, monkeypatch) -> None:
    import neurowatch.widgets.video_wall as module

    wall, cell, shown, arrive = _opening_wall(qtbot)
    wall.set_awaited_cameras({"c1"})
    now = [100.0]
    monkeypatch.setattr(module.time, "monotonic", lambda: now[0])
    arrive(1)
    assert shown == []
    now[0] += module._PLUGIN_START_WAIT_S + 0.1
    arrive(2)
    assert shown == [2], "never blank for good"
    assert not cell._waiting_for_analysis
    arrive(3)
    assert shown == [2, 3]


def test_switching_streams_keeps_the_picture_until_the_new_one_has_its_result(qtbot) -> None:
    from neurowatch.models.camera import CameraConfig
    from neurowatch.widgets.video_wall import VideoWallWidget

    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall.set_grid(2, 1)
    camera = CameraConfig(id="c1", name="c1", url="rtsp://x/main", sub_url="rtsp://x/sub")
    wall.set_cameras([camera, CameraConfig(id="c2", name="c2", url="rtsp://x/c2")], page=0)
    wall.set_analytics({"c1": ("Analysed by Vehicle counting (running)", True)})
    cell = wall._cell_for_cam_id("c1")
    shown: list[int] = []
    upload = cell.update_frame

    def record(data: bytes, width: int, height: int, fmt: str = "rgb") -> None:
        shown.append(data[0])
        upload(data, width, height, fmt)

    cell.update_frame = record

    def arrive(n: int) -> None:
        us = n * 100_000
        wall._on_identified_frame_ready("c1", bytes([n]) * (32 * 18 * 3), 32, 18, "rgb", n, us, us)
        wall._flush_pending_frames()

    wall.set_drawings({"c1": [_analysed(2)]})
    for n in (1, 2, 3):
        arrive(n)
    assert shown == [2]
    shown.clear()
    wall.set_solo_camera("c1")  # full size: from the sub stream to the main
    assert wall._cell_for_cam_id("c1") is cell
    assert wall._analysed["c1"].opened is False, "it starts again on the new stream"
    arrive(5)
    assert shown == [], "not one plain picture on the way"
    wall.set_drawings({"c1": [_analysed(5)]})
    assert shown == [5]
    shown.clear()

    # The main stream takes seconds to connect, and results move on meanwhile.
    wall.set_drawings({"c1": [_analysed(5), _analysed(40)]})
    us = 41 * 100_000
    big = bytes([41]) * (64 * 36 * 3)
    wall._on_identified_frame_ready("c1", big, 64, 36, "rgb", 41, us, us)
    wall._flush_pending_frames()
    assert shown == [], "the first main picture: not plain video, whatever the gap"
    assert wall._analysed["c1"].size == (64, 36)
    wall.set_drawings({"c1": [_analysed(41)]})
    assert shown == [41]


def test_a_tile_with_its_drawings_off_opens_at_once(qtbot) -> None:
    wall, _cell, shown, arrive = _opening_wall(qtbot)
    wall.set_plain_cameras({"c1"})
    wall.set_drawings({"c1": [_analysed(1)]})
    arrive(50)
    assert shown == [50]
    wall.set_plain_cameras(set())
    wall.set_drawings_on_analysed(False)
    arrive(51)
    assert shown[-1] == 51, "nor when Settings turns analysed pictures off"


def test_coming_back_to_a_page_does_not_flash_a_cached_picture(qtbot) -> None:
    from neurowatch.models.camera import CameraConfig

    wall, cell, shown, arrive = _opening_wall(qtbot)
    wall.set_drawings({"c1": [_analysed(1)]})
    wall.set_drawings({"c1": [_analysed(5)]})
    for n in range(3, 7):
        arrive(n)
    assert shown == [5]
    other = CameraConfig(id="c2", name="c2", url="rtsp://x/c2")
    first = CameraConfig(id="c1", name="c1", url="rtsp://x/c1")
    wall.set_cameras([other, first], page=0)  # another camera takes the tile
    assert cell.camera is not None and cell.camera.id == "c2"
    wall.set_drawings({"c1": [_analysed(6)]})
    shown.clear()
    wall.set_cameras([first, other], page=0)
    assert shown == [], "its newest frame has no drawings on it"
    assert cell.camera is not None and cell.camera.id == "c1"
    arrive(7)
    assert shown == [], "nor does it fall back to plain video on the way in"
    assert cell._waiting_for_analysis
    wall.set_drawings({"c1": [_analysed(7)]})
    assert shown == [7]


def test_a_tile_with_plugin_drawings_shows_only_the_analysed_pictures(qtbot) -> None:
    from neurowatch.models.camera import CameraConfig
    from neurowatch.widgets.video_wall import VideoWallWidget

    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall.set_grid(1, 1)
    wall.set_cameras([CameraConfig(id="c1", name="Road", url="rtsp://x/1")])
    wall.set_analytics({"c1": ("Analysed by Vehicle counting (running)", True)})
    cell = wall._cells[0]
    picture = bytes(32 * 18 * 3)

    def arrive(n: int) -> None:
        us = n * 100_000
        wall._on_identified_frame_ready("c1", picture, 32, 18, "rgb", n, us, us)
        wall._flush_pending_frames()

    for n in range(1, 11):
        arrive(n)
    assert cell._frame_identity[0] == 10, "live video before any result"
    wall.set_drawings({"c1": [_analysed(4)]})
    assert cell._frame_identity[0] == 4, "the analysed picture, with its result"
    arrive(11)
    arrive(12)
    assert cell._frame_identity[0] == 4, "held until the next result"
    wall.set_drawings({"c1": [_analysed(8), _analysed(8, layer="lines")]})
    assert cell._frame_identity[0] == 8
    assert {d.layer for d in cell._drawing_history.shown(cell._frame_identity, 0)} == {
        "detections",
        "lines",
    }
    # Results whose pictures are long gone: live video, not a frozen picture.
    wall.set_drawings({"c1": [_analysed(8), _analysed(400)]})
    arrive(13)
    assert cell._frame_identity[0] == 13
    # No current result: live again; switched off: always live.
    wall.set_drawings({})
    arrive(14)
    assert cell._frame_identity[0] == 14
    wall.set_drawings_on_analysed(False)
    wall.set_drawings({"c1": [_analysed(13)]})
    arrive(15)
    assert cell._frame_identity[0] == 15


def _motion_wall(qtbot):
    from neurowatch.models.camera import CameraConfig
    from neurowatch.widgets.video_wall import VideoWallWidget

    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall.set_grid(1, 1)
    wall.set_cameras([CameraConfig(id="c1", name="Road", url="rtsp://x/1")])
    wall.set_analytics({"c1": ("Analysed by Motion (running)", True)})
    return wall


class _Clock:
    now = 1000.0

    def monotonic(self) -> float:
        return self.now


def _replay_sparse(qtbot, monkeypatch, *, layers: str, picture=(1920, 1080), delay_frames=37):
    """Motion at 25 fps with an analysis every 5th picture (5 fps), results
    ``delay_frames`` later, each living 1.5 s from its arrival."""
    import types

    from neurowatch.models.analytics import Drawing
    from neurowatch.widgets import cell_widget, video_wall

    clock = _Clock()
    fake = types.SimpleNamespace(monotonic=clock.monotonic, time=video_wall.time.time)
    monkeypatch.setattr(video_wall, "time", fake)
    monkeypatch.setattr(cell_widget, "time", fake)
    wall = _motion_wall(qtbot)
    cell = wall._cells[0]
    pw, ph = picture
    size = pw * ph * 3
    step = 40_000
    quiet, burst = 250, 100
    got: list[int] = []
    for n in range(1, quiet + burst + delay_frames + 60):
        clock.now += step / 1e6
        us = n * step
        wall._on_identified_frame_ready("c1", bytes([n % 250 + 1]) * size, pw, ph, "rgb", n, us, us)
        wall._flush_pending_frames()
        d = n - delay_frames
        if d >= 1 and d % 5 == 0:
            dus = d * step
            items = []
            until = clock.now + 1.5
            if layers == "both":
                items.append(
                    Drawing("c1", "motion", "debug", ({"n": 1},), until, d, dus, 1500, dus)
                )
            if quiet < d <= quiet + burst:
                items.append(
                    Drawing("c1", "motion", "detections", ({"n": 2},), until, d, dus, 1500, dus)
                )
            wall.set_drawings({"c1": items} if items else {})
        if n > quiet + delay_frames:
            drawn = cell._drawing_history.shown(cell._frame_identity, clock.now)
            if (
                cell._drawings
                and not cell._drawings_paused
                and any(
                    x.layer == "detections" and x.frame == cell._frame_identity[0] for x in drawn
                )
            ):
                got.append(n)
    return got


def test_motion_boxes_alone_find_their_pictures_from_the_first_result(qtbot, monkeypatch) -> None:
    """Reported: with only the plugin's boxes layer chosen the wall showed
    nothing (with its analysis layer too, it did). The boxes come only during
    motion, so no result had been seen to measure the delay by, and the byte
    budget held every picture of a large stream: the first results of a burst
    described pictures already dropped. Each of the burst's 100 pictures shows
    its boxes, with or without the analysis layer."""
    for picture, delay_frames in (((1920, 1080), 50), ((3840, 2160), 37)):
        both = _replay_sparse(
            qtbot, monkeypatch, layers="both", picture=picture, delay_frames=delay_frames
        )
        only = _replay_sparse(
            qtbot, monkeypatch, layers="boxes", picture=picture, delay_frames=delay_frames
        )
        assert len(both) == 100, f"{picture}: both layers {both[:3]}"
        assert len(only) == 100, f"{picture}: boxes only, first at {only[:3]}"


def _replay_steady(qtbot, monkeypatch, *, results: str, delay_frames: int = 37):
    """Motion at 25 fps, an analysis of every 5th picture, results
    ``delay_frames`` later (each living 1.5 s from arrival), the user showing
    only the boxes layer: 10 s quiet, a 4 s burst, 6 s quiet. ``results``:

    - ``every``: the plugin answers every picture on every layer, an empty
      boxes layer in the quiet (the newer contract).
    - ``hidden``: the older plugin: the debug layer (not chosen) every
      picture, the boxes layer only in the burst.

    Returns, per tick after the first result, the picture shown and whether
    boxes are painted on it."""
    import types

    from neurowatch.models.analytics import Drawing
    from neurowatch.widgets import cell_widget, video_wall

    clock = _Clock()
    fake = types.SimpleNamespace(monotonic=clock.monotonic, time=video_wall.time.time)
    monkeypatch.setattr(video_wall, "time", fake)
    monkeypatch.setattr(cell_widget, "time", fake)
    wall = _motion_wall(qtbot)
    cell = wall._cells[0]
    step = 40_000
    quiet, burst = 250, 100
    box = {"type": "box", "rect": [0.2, 0.3, 0.1, 0.1], "label": "Motion"}
    shown: list[tuple[int, bool]] = []
    started = False
    for n in range(1, quiet + burst + 150 + delay_frames):
        clock.now += step / 1e6
        us = n * step
        wall._on_identified_frame_ready(
            "c1", bytes([n % 250 + 1]) * (64 * 36 * 3), 64, 36, "rgb", n, us, us
        )
        d = n - delay_frames
        if d >= 1 and d % 5 == 0:
            started = True
            dus = d * step
            until = clock.now + 1.5
            in_burst = quiet < d <= quiet + burst

            def result(layer: str, items: tuple, d=d, dus=dus, until=until) -> Drawing:
                return Drawing("c1", "motion", layer, items, until, d, dus, 1500, dus)

            boxes = (box,) if in_burst else ()
            if results == "every":
                chosen = [result("detections", boxes)]
                wall.set_drawings(
                    {"c1": chosen}, {"c1": [result("debug", ({"type": "text"},)), *chosen]}
                )
            else:
                chosen = [result("detections", boxes)] if in_burst else []
                clock_all = [result("debug", ({"type": "text"},)), *chosen]
                wall.set_drawings({"c1": chosen} if chosen else {}, {"c1": clock_all})
        wall._flush_pending_frames()
        if started:
            seen = cell._drawing_history.shown(cell._frame_identity, clock.now)
            painted = bool(
                cell._drawings
                and not cell._drawings_paused
                and any(x.items and x.frame == cell._frame_identity[0] for x in seen)
            )
            shown.append((cell._frame_identity[0], painted))
    return shown


def test_a_tile_with_plugin_drawings_keeps_the_plugins_rate_in_the_quiet_too(
    qtbot, monkeypatch
) -> None:
    """Reported: with only Motion's boxes chosen the tile was live until
    motion started, then dropped to the analysed pictures (5 fps, behind
    live), which showed as lag. With a result for every analysed picture, also
    the empty ones and the layers not chosen, the tile shows only analysed
    pictures from the first result on, and the boxes sit on their pictures."""
    for results in ("every", "hidden"):
        shown = _replay_steady(qtbot, monkeypatch, results=results)
        assert len(shown) > 400, results
        assert all(n % 5 == 0 for n, _p in shown), f"{results}: a live picture in {shown[:50]}"
        for second in range(0, len(shown) - 25, 25):
            window = shown[second : second + 25]
            assert len({n for n, _p in window}) <= 6, f"{results}: {window}"
        painted = {n for n, p in shown if p}
        burst = {n for n in range(255, 355) if n % 5 == 0}
        assert painted == burst, f"{results}: boxes on their own pictures only: {sorted(painted)}"


def test_empty_results_with_a_picture_are_results_without_drawing_anything() -> None:
    from neurowatch.models.analytics import (
        CameraPlugin,
        analysed_target,
        analysing_clock,
        markings_drawn,
        on_wall,
        parse_overlays,
    )

    def entry(layer: str, items: list, **extra) -> dict:
        return {
            "camera_id": "c",
            "plugin": "motion",
            "layer": layer,
            "items": items,
            "ttl_ms": 1500,
            "lifetime_ms": 1500,
            **extra,
        }

    named = {"frame": 7, "time_us": 700_000, "observed_us": 701_000}
    parsed = parse_overlays(
        {
            "overlays": [
                entry("detections", [], **named),
                entry("debug", [{"type": "text", "lines": ["x"]}], **named),
                entry("lines", []),  # no picture named: cleared, as before
            ]
        },
        10.0,
    )["c"]
    assert [(d.layer, d.items == ()) for d in parsed] == [("detections", True), ("debug", False)]
    assert analysed_target(parsed[:1], 10.0) is not None, "an empty result is a current result"
    assert markings_drawn(parsed[:1]) == ()
    plugin = CameraPlugin("motion", "Motion", "running", wall_layers=frozenset({"detections"}))
    shown = on_wall({"c": parsed}, {"c": [plugin]}, "chosen")["c"]
    assert [d.layer for d in shown] == ["detections"]
    clock = analysing_clock({"c": parsed}, {"c": [plugin]}, "chosen")["c"]
    assert {d.layer for d in clock} == {"detections", "debug"}, "hidden layers pace the tile"
    assert analysing_clock({"c": parsed}, {"c": []}, "chosen") == {}
    assert analysing_clock({"c": parsed}, {"c": [plugin]}, "off") == {}


def test_every_layer_of_a_result_is_drawn_on_a_main_stream_tile(qtbot, monkeypatch) -> None:
    """Reported: on a main-stream (1080p) tile parts of a drawing were missing.
    Layers of the same analysed picture reach the tile at different moments
    (the boxes first, the zones and score text a few ticks later, or the other
    way round); once both are here the picture carries both, and a layer that
    was missing never shows one of an earlier picture instead."""
    import types

    from neurowatch.models.analytics import Drawing
    from neurowatch.widgets import cell_widget, video_wall

    clock = _Clock()
    fake = types.SimpleNamespace(monotonic=clock.monotonic, time=video_wall.time.time)
    monkeypatch.setattr(video_wall, "time", fake)
    monkeypatch.setattr(cell_widget, "time", fake)
    wall = _motion_wall(qtbot)
    cell = wall._cells[0]
    step = 40_000
    size = 1920 * 1080 * 3
    pending: list[tuple[int, list[Drawing]]] = []  # (arrives at picture, results)
    results: dict[str, Drawing] = {}
    complete: list[int] = []
    for n in range(1, 400):
        clock.now += step / 1e6
        us = n * step
        wall._on_identified_frame_ready(
            "c1", bytes([n % 250 + 1]) * size, 1920, 1080, "rgb", n, us, us
        )
        d = n - 30
        if d >= 1 and d % 5 == 0:
            dus = d * step
            until = clock.now + 1.5
            box = {"type": "box", "rect": [0.2, 0.3, 0.1, 0.1], "label": "Motion"}
            zone = {"type": "polygon", "points": [[0, 0], [1, 0], [1, 1]], "label": "Zone"}
            text = {"type": "text", "anchor": "bottom_left", "lines": ["Score"]}
            first = Drawing("c1", "motion", "detections", (box,), until, d, dus, 1500, dus)
            rest = Drawing("c1", "motion", "debug", (zone, text), until, d, dus, 1500, dus)
            order = (first, rest) if (d // 5) % 2 else (rest, first)
            pending.append((n, [order[0]]))
            pending.append((n + 3, [order[1]]))
        for _due, layers in [p for p in pending if p[0] <= n]:
            for layer in layers:
                results[layer.layer] = layer
        pending = [p for p in pending if p[0] > n]
        if results:
            wall.set_drawings({"c1": list(results.values())})
        wall._flush_pending_frames()
        if n > 100:
            seen = cell._drawing_history.shown(cell._frame_identity, clock.now)
            on_picture = {x.layer for x in seen if x.frame == cell._frame_identity[0] and x.items}
            if len(on_picture) == 2:
                complete.append(cell._frame_identity[0])
    pictures = {n for n in range(105, 370) if n % 5 == 0}
    assert pictures <= set(complete), (
        f"pictures without both layers: {sorted(pictures - set(complete))}"
    )


def _replay_main_tile(qtbot, monkeypatch, *, offset_us: int, chosen: frozenset[str]):
    """Motion on the sub stream (its own sequence numbers and capture clock,
    25 fps, every 5th picture analysed, results 0.3-1.5 s late, every layer
    each time: the zones and score always, the box in a 4 s burst), the tile on
    the main stream (1080p, 25 fps, its pictures reaching the recorder
    ``offset_us`` after the sub's, decoded 60 ms later). Returns, per 10 ms
    tick showing an analysed picture, its receipt time and the layers painted."""
    import random
    import types

    from neurowatch.models.analytics import CameraPlugin, Drawing, analysing_clock, on_wall
    from neurowatch.widgets import cell_widget, video_wall

    clock = _Clock()
    fake = types.SimpleNamespace(monotonic=clock.monotonic, time=video_wall.time.time)
    monkeypatch.setattr(video_wall, "time", fake)
    monkeypatch.setattr(cell_widget, "time", fake)
    wall = _motion_wall(qtbot)
    cell = wall._cells[0]
    cameras = {"c1": [CameraPlugin("motion", "Motion", "running", wall_layers=chosen)]}
    rng = random.Random(7)
    box = {"type": "box", "rect": [0.2, 0.3, 0.1, 0.1], "label": "Motion"}
    zone = {"type": "polygon", "points": [[0.1, 0.1], [0.9, 0.1], [0.9, 0.9]], "label": "ROI"}
    text = {"type": "text", "anchor": "bottom_left", "lines": ["Score"]}
    base = 1_000_000_000
    size = 1920 * 1080 * 3
    seq = 0
    sub_pictures = 0
    results: list[tuple[int, list[Drawing]]] = []  # (arrives at, layers)
    tile: list[tuple[int, tuple[int, int, int]]] = []  # (decoded at, identity)
    store: dict[str, Drawing] = {}
    arrived_last = 0
    shown: list[tuple[int, set[str]]] = []
    for t in range(0, 16_000_000, 10_000):
        clock.now = 1000.0 + t / 1e6
        if t % 40_000 == 0:  # a sub picture: the plugin analyses every 5th
            seq += 1
            sub_pictures += 1
            if sub_pictures % 5 == 0:
                observed = base + t
                arrives = max(arrived_last + 1, t + rng.randint(300_000, 1_500_000))
                arrived_last = arrives
                moving = 6_000_000 <= t < 10_000_000

                def result(layer: str, items: tuple, n=seq, at=observed) -> Drawing:
                    return Drawing("c1", "motion", layer, items, 0, n, at - 5_000_000, 1500, at)

                boxes = (box,) if moving else ()
                results.append(
                    (arrives, [result("detections", boxes), result("debug", (zone, text))])
                )
        if t % 40_000 == 20_000:  # a main picture, numbered among the camera's
            seq += 1
            observed = base + t + offset_us
            tile.append((t + offset_us + 60_000, (seq, observed + 3_000_000, observed)))
        for _due, identity in [p for p in tile if p[0] <= t]:
            wall._on_identified_frame_ready(
                "c1", bytes([identity[0] % 250 + 1]) * size, 1920, 1080, "rgb", *identity
            )
        tile = [p for p in tile if p[0] > t]
        due = [r for r in results if r[0] <= t]
        results = [r for r in results if r[0] > t]
        for arrives, layers in due:
            for layer in layers:
                store[layer.layer] = dataclasses.replace(layer, until=1000.0 + arrives / 1e6 + 1.5)
        if due:
            current = {"c1": [d for d in store.values() if d.until > clock.now]}
            wall.set_drawings(
                on_wall(current, cameras, "chosen"), analysing_clock(current, cameras, "chosen")
            )
        wall._flush_pending_frames()
        pictures = wall._analysed.get("c1")
        if pictures is not None and pictures.shown is not None and cell._frame_size == (1920, 1080):
            seen = cell._drawing_history.shown(cell._frame_identity, clock.now)
            painted = (
                {d.layer for d in seen if d.items}
                if cell._drawings and not cell._drawings_paused
                else set()
            )
            shown.append((cell._frame_identity[2] - base, painted))
    return shown


def test_a_main_stream_tile_shows_the_boxes_of_a_plugin_on_the_sub_stream(
    qtbot, monkeypatch
) -> None:
    """Reported: the boxes showed on the sub stream's tile, only the zone (the
    analysis layer) on the main stream's. The wall is not why: a 1080p main
    tile shows sub-stream results (other sequence numbers, another capture
    clock, matched on the receipt clock) with their boxes on every picture of
    the burst, whatever the delay between the streams. (The cause was the
    plugin being moved to the desktop's main pictures: Motion's manifest.)"""
    for offset_us in (0, 150_000, 300_000):
        for chosen in (frozenset({"detections", "debug"}), frozenset({"detections"})):
            shown = _replay_main_tile(qtbot, monkeypatch, offset_us=offset_us, chosen=chosen)
            what = f"main {offset_us // 1000} ms later, {sorted(chosen)}"
            assert len(shown) > 1000, f"{what}: analysed pictures shown ({len(shown)})"
            burst = [layers for at, layers in shown if 6_200_000 <= at < 9_800_000]
            assert len(burst) > 300, what
            missing = sum("detections" not in layers for layers in burst)
            assert missing == 0, f"{what}: {missing} of {len(burst)} burst ticks without the box"
            quiet = [layers for at, layers in shown if at < 5_900_000 or at >= 10_100_000]
            assert all("detections" not in layers for layers in quiet), what
            if "debug" in chosen:
                assert all("debug" in layers for _at, layers in shown), f"{what}: the zone"
