"""Wall tile packing: sizes, gap filling, reflow onto later pages, restore."""

from __future__ import annotations

import random

from neurowatch.models.camera import CameraConfig, WallPosition
from neurowatch.models.wall_layout import (
    Placement,
    clamp_span,
    move_to_cell,
    pack,
    page_count,
    page_slots,
    restore,
    swap,
)


def _ids(n: int) -> list[str]:
    return [f"c{i}" for i in range(n)]


def _on_page(placements: list[Placement], page: int) -> list[str]:
    return [p.cam_id for p in placements if p.page == page]


def _free_cells(placements: list[Placement], page: int, rows: int, cols: int) -> int:
    return len(page_slots(placements, page, rows, cols)) - len(_on_page(placements, page))


def _saved(placements: list[Placement]) -> dict[str, tuple[int, int, int]]:
    return {p.cam_id: (p.page, p.row, p.col) for p in placements}


def test_single_cells_fill_pages_in_order() -> None:
    placements = pack(_ids(11), {}, 3, 3)
    assert _on_page(placements, 0) == _ids(9)
    assert _on_page(placements, 1) == ["c9", "c10"]
    assert placements[4] == Placement("c4", 0, 1, 1)
    assert page_count(placements) == 2


def test_large_first_tile_on_3x3_shows_six_and_moves_three_to_next_page() -> None:
    placements = pack(_ids(9), {"c0": (2, 2)}, 3, 3)
    assert _on_page(placements, 0) == ["c0", "c1", "c2", "c3", "c4", "c5"]
    assert _on_page(placements, 1) == ["c6", "c7", "c8"]
    by_id = {p.cam_id: p for p in placements}
    assert by_id["c0"].slot == (0, 0, 2, 2)
    assert by_id["c1"].slot == (0, 2, 1, 1)
    assert by_id["c2"].slot == (1, 2, 1, 1)
    assert [by_id[c].slot for c in ("c3", "c4", "c5")] == [(2, 0, 1, 1), (2, 1, 1, 1), (2, 2, 1, 1)]


def test_shrinking_a_big_tile_lets_the_others_flow_back() -> None:
    order = _ids(9)
    big = pack(order, {"c0": (2, 2)}, 3, 3)
    small = pack([p.cam_id for p in big], {"c0": (1, 1)}, 3, 3)
    assert small == pack(order, {}, 3, 3)
    assert _free_cells(small, 0, 3, 3) == 0


def test_tile_that_cannot_stay_in_place_leaves_no_gap_behind() -> None:
    # c2 sits in the last column; as 2x2 it no longer fits there. Later tiles
    # fill the cell it left instead of leaving it empty.
    placements = pack(_ids(9), {"c2": (2, 2)}, 3, 3)
    by_id = {p.cam_id: p for p in placements}
    assert by_id["c2"].slot == (1, 0, 2, 2)
    assert by_id["c3"].slot == (0, 2, 1, 1)
    assert _free_cells(placements, 0, 3, 3) == 0
    assert _on_page(placements, 0) == ["c0", "c1", "c3", "c2", "c4", "c5"]


def test_small_tiles_backfill_gaps_a_wide_tile_leaves() -> None:
    # c1 spans the full row, so it cannot sit beside c0 and starts row 1;
    # the single tiles after it fill the rest of row 0.
    placements = pack(_ids(4), {"c1": (1, 3)}, 2, 3)
    assert [(p.cam_id, p.page, p.row, p.col) for p in placements] == [
        ("c0", 0, 0, 0),
        ("c2", 0, 0, 1),
        ("c3", 0, 0, 2),
        ("c1", 0, 1, 0),
    ]


def test_pages_have_no_gap_a_later_tile_could_fill() -> None:
    rng = random.Random(3)
    for _ in range(300):
        rows, cols = rng.randint(1, 4), rng.randint(1, 4)
        order = _ids(rng.randint(1, 16))
        spans = {c: (rng.randint(1, 3), rng.randint(1, 3)) for c in order}
        placements = pack(order, spans, rows, cols)
        for page in range(page_count(placements)):
            cells = [cell for p in placements if p.page == page for cell in p.cells()]
            assert len(cells) == len(set(cells))
            assert all(0 <= r < rows and 0 <= c < cols for r, c in cells)
            later_1x1 = [p for p in placements if p.page > page and p.span == (1, 1)]
            if later_1x1:
                assert _free_cells(placements, page, rows, cols) == 0


def test_spans_larger_than_the_grid_are_clamped() -> None:
    assert clamp_span((5, 0), 3, 4) == (3, 1)
    assert pack(["c0"], {"c0": (9, 9)}, 2, 2) == [Placement("c0", 0, 0, 0, 2, 2)]


def test_swap_trades_places_and_keeps_the_wall_shape() -> None:
    before = pack(_ids(9), {"c0": (2, 2)}, 3, 3)
    after, spans = swap(before, {"c0": (2, 2)}, "c4", "c0")
    assert [(p.page, p.slot) for p in after] == [(p.page, p.slot) for p in before]
    assert after[0].cam_id == "c4" and after[0].span == (2, 2)
    assert spans["c4"] == (2, 2) and spans["c0"] == (1, 1)


def test_move_to_free_cell_goes_after_the_tiles_before_it() -> None:
    placements = pack(_ids(4), {}, 3, 3)
    assert move_to_cell(placements, "c0", 0, 2, 2, 3) == ["c1", "c2", "c3", "c0"]
    placements = pack(_ids(12), {}, 3, 3)
    assert move_to_cell(placements, "c11", 0, 0, 1, 3) == [
        "c0", "c11", "c1", "c2", "c3", "c4", "c5", "c6", "c7", "c8", "c9", "c10",
    ]  # fmt: skip


def test_restore_keeps_a_saved_layout_that_repacking_would_change() -> None:
    # Packing this order backfills c3 next to c1; sorting the result by
    # position and packing again would give a different wall.
    spans = {"c1": (2, 1), "c2": (1, 2), "c4": (2, 1)}
    shown = pack(_ids(5), spans, 3, 3)
    by_position = [p.cam_id for p in shown]
    assert pack(by_position, spans, 3, 3) != shown
    assert restore(by_position, _saved(shown), spans, 3, 3) == shown


def test_restore_round_trips_any_packed_layout() -> None:
    rng = random.Random(11)
    for _ in range(300):
        rows, cols = rng.randint(1, 4), rng.randint(1, 4)
        order = _ids(rng.randint(0, 14))
        spans = {c: (rng.randint(1, 3), rng.randint(1, 3)) for c in order}
        shown = pack(order, spans, rows, cols)
        assert restore([p.cam_id for p in shown], _saved(shown), spans, rows, cols) == shown


def test_restore_fills_the_gap_a_removed_camera_leaves() -> None:
    shown = pack(_ids(9), {}, 3, 3)
    saved = _saved(shown)
    del saved["c1"]
    order = [c for c in _ids(9) if c != "c1"]
    assert restore(order, saved, {}, 3, 3) == pack(order, {}, 3, 3)


def test_restore_puts_a_new_camera_in_the_first_free_spot() -> None:
    spans = {"c0": (2, 2)}
    shown = pack(_ids(6), spans, 3, 3)  # page 0 full
    restored = restore([*_ids(6), "new"], _saved(shown), spans, 3, 3)
    assert restored[:6] == shown
    assert restored[6] == Placement("new", 1, 0, 0)


def test_restore_repacks_when_the_grid_changes() -> None:
    shown = pack(_ids(10), {}, 3, 3)
    bigger = restore(_ids(10), _saved(shown), {}, 4, 4)
    assert bigger == pack(_ids(10), {}, 4, 4)  # page 1's camera moves up
    smaller = restore(_ids(10), _saved(shown), {}, 2, 2)
    assert smaller == pack(_ids(10), {}, 2, 2)


def test_restore_repacks_overlapping_saved_positions() -> None:
    saved = {"c0": (0, 0, 0), "c1": (0, 0, 0)}
    assert restore(_ids(2), saved, {}, 2, 2) == pack(_ids(2), {}, 2, 2)


def test_camera_tile_span_round_trips_through_config() -> None:
    cam = CameraConfig.from_dict(
        {"id": "x", "wall_position": {"page": 1, "row": 0, "col": 2, "row_span": 2, "extra": 1}}
    )
    assert cam.wall_position == WallPosition(page=1, row=0, col=2, row_span=2, col_span=1)
    assert cam.tile_span == (2, 1)
    assert CameraConfig(id="y").tile_span == (1, 1)
    cam.set_tile_span((3, 3))
    assert cam.to_dict()["wall_position"]["col_span"] == 3
