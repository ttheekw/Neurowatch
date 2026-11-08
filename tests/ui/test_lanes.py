"""Lanes for vehicle counting: each named, with its own crossing line."""

from __future__ import annotations

from neurowatch.models.lanes import LaneSet

LINE = [[0.1, 0.6], [0.5, 0.6]]


def test_lanes_read_their_own_crossing_lines_and_keep_everything_else() -> None:
    lanes = [{"id": "l1", "name": "To Colombo", "points": [], "colour": "blue"}]
    lines = [
        {"id": "free", "name": "Gate", "points": [[0, 0.8], [1, 0.8]]},
        {
            "id": "x1",
            "lane": "l1",
            "points": LINE,
            "in_label": "In",
            "out_label": "Out",
            "note": "kept",
        },
    ]
    parsed = LaneSet.from_config(lanes, lines)
    assert [lane.name for lane in parsed.lanes] == ["To Colombo"]
    assert parsed.lanes[0].line == LINE and parsed.lanes[0].line_id == "x1"
    assert parsed.problem() is None
    saved_lanes, saved_lines = parsed.lanes_config(), parsed.lines_config()
    assert saved_lanes == [{"colour": "blue", "id": "l1", "name": "To Colombo", "points": []}]
    assert saved_lines[0] == lines[0], "a line of no lane is kept as it was"
    assert saved_lines[1] == {
        "note": "kept",
        "id": "x1",
        "name": "To Colombo",
        "lane": "l1",
        "points": LINE,
    }, "a lane's traffic goes one way: its line has no direction names"
    # What is missing, in plain words.
    parsed.lanes[0].name = " "
    assert parsed.problem() == "Give lane 1 a name."
    parsed.lanes[0].name = "To Colombo"
    parsed.lanes[0].line = [[0.1, 0.6]]
    assert "crossing line of To Colombo" in (parsed.problem() or "")
    parsed.lanes[0].line = LINE
    parsed.lanes[0].area = [[0, 0], [1, 0]]
    assert "area of To Colombo" in (parsed.problem() or "")
    assert LaneSet.from_config("junk", None).lanes == []
