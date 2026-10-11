"""Wall tile layout: cameras in wall order, each tile covering one or more
grid cells, packed densely onto pages.

Packing walks the cameras in order and puts each tile at the earliest free
spot (page, then row-major cell) where it fits, so smaller tiles fill the gaps
a larger one leaves and a page only ends with free cells no later tile fits.
A tile that fits no existing page starts a new one.

Example: a 3x3 wall whose first tile is 2x2 shows six tiles; the rest move
to the next page. Shrinking it back lets them flow back.

A saved layout is restored exactly (see ``restore``): re-packing the saved
positions in sorted order can, rarely, give a different wall, so positions
are only recomputed when the saved ones no longer fit the grid or leave a gap
a later tile could fill.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from dataclasses import dataclass, replace

Span = tuple[int, int]  # (row_span, col_span)

#: Grid choices offered in the UI: (label, rows, cols). Labels are COLS×ROWS,
#: the NVR convention ("3×2" is 3 columns by 2 rows).
GRID_PRESETS: list[tuple[str, int, int]] = [
    ("1×1", 1, 1),
    ("2×2", 2, 2),
    ("3×3", 3, 3),
    ("4×4", 4, 4),
    ("3×2", 2, 3),
    ("4×3", 3, 4),
    ("5×4", 4, 5),
    ("8×4", 4, 8),
]


def grid_label(rows: int, cols: int) -> str:
    """``rows`` x ``cols`` as the UI writes it (COLS×ROWS)."""
    return f"{cols}×{rows}"


Slot = tuple[int, int, int, int]  # (row, col, row_span, col_span)
Cell = tuple[int, int]


@dataclass(frozen=True)
class Placement:
    cam_id: str
    page: int
    row: int
    col: int
    row_span: int = 1
    col_span: int = 1

    @property
    def span(self) -> Span:
        return (self.row_span, self.col_span)

    @property
    def slot(self) -> Slot:
        return (self.row, self.col, self.row_span, self.col_span)

    def cells(self) -> list[Cell]:
        return [
            (self.row + dr, self.col + dc)
            for dr in range(self.row_span)
            for dc in range(self.col_span)
        ]


def clamp_span(span: Span, rows: int, cols: int) -> Span:
    """A tile is at least one cell and never larger than the grid."""
    return (max(1, min(span[0], rows)), max(1, min(span[1], cols)))


def _sort_key(p: Placement) -> tuple[int, int, int]:
    return (p.page, p.row, p.col)


def pack(order: Sequence[str], spans: Mapping[str, Span], rows: int, cols: int) -> list[Placement]:
    """Place every camera of ``order`` onto pages of a ``rows`` x ``cols``
    grid. Returns tiles sorted by position (page, row, col)."""
    if rows <= 0 or cols <= 0:
        return []
    pages: list[set[Cell]] = []
    placements = [_place(pages, cam_id, spans, rows, cols) for cam_id in order]
    return sorted(placements, key=_sort_key)


def _place(
    pages: list[set[Cell]], cam_id: str, spans: Mapping[str, Span], rows: int, cols: int
) -> Placement:
    """Put ``cam_id`` at the earliest spot that fits and mark it occupied."""
    rs, cs = clamp_span(spans.get(cam_id, (1, 1)), rows, cols)
    spot = _first_fit(pages, rs, cs, rows, cols)
    if spot is None:
        pages.append(set())
        spot = (len(pages) - 1, 0)  # a clamped tile always fits an empty page
    page, index = spot
    row, col = divmod(index, cols)
    placement = Placement(cam_id, page, row, col, rs, cs)
    pages[page].update(placement.cells())
    return placement


def _first_fit(
    pages: Sequence[set[Cell]],
    rs: int,
    cs: int,
    rows: int,
    cols: int,
    before: tuple[int, int] | None = None,
) -> tuple[int, int] | None:
    """Earliest (page, cell index) where an rs x cs tile fits, optionally
    only spots strictly before ``before``."""
    for page, occupied in enumerate(pages):
        for index in range(rows * cols):
            if before is not None and (page, index) >= before:
                return None
            row, col = divmod(index, cols)
            if row + rs > rows or col + cs > cols:
                continue
            if all((row + dr, col + dc) not in occupied for dr in range(rs) for dc in range(cs)):
                return (page, index)
    return None


def restore(
    order: Sequence[str],
    saved: Mapping[str, tuple[int, int, int]],
    spans: Mapping[str, Span],
    rows: int,
    cols: int,
) -> list[Placement]:
    """The saved layout (``saved``: cam_id -> (page, row, col)) when it still
    fits this grid without overlaps or fillable gaps; cameras without a saved
    position then take the first free spots. Otherwise everything is packed
    afresh in ``order``."""
    if rows <= 0 or cols <= 0:
        return []
    pages: list[set[Cell]] = []
    kept: list[Placement] = []
    for cam_id in order:
        position = saved.get(cam_id)
        if position is None:
            continue
        page, row, col = position
        rs, cs = clamp_span(spans.get(cam_id, (1, 1)), rows, cols)
        placement = Placement(cam_id, page, row, col, rs, cs)
        if min(page, row, col) < 0 or row + rs > rows or col + cs > cols:
            return pack(order, spans, rows, cols)
        while len(pages) <= page:
            pages.append(set())
        if any(cell in pages[page] for cell in placement.cells()):
            return pack(order, spans, rows, cols)
        pages[page].update(placement.cells())
        kept.append(placement)
    if not _is_dense(kept, pages, rows, cols):
        return pack(order, spans, rows, cols)
    kept += [_place(pages, cam_id, spans, rows, cols) for cam_id in order if cam_id not in saved]
    return sorted(kept, key=_sort_key)


def _is_dense(
    placements: Sequence[Placement], pages: list[set[Cell]], rows: int, cols: int
) -> bool:
    """No tile could move to an earlier free spot (and no page is empty).
    Every result of ``pack`` has this property."""
    if any(not occupied for occupied in pages):
        return False
    for p in placements:
        cells = p.cells()
        pages[p.page].difference_update(cells)
        earlier = _first_fit(
            pages, p.row_span, p.col_span, rows, cols, before=(p.page, p.row * cols + p.col)
        )
        pages[p.page].update(cells)
        if earlier is not None:
            return False
    return True


def page_count(placements: Sequence[Placement]) -> int:
    return max((p.page for p in placements), default=0) + 1


def page_slots(placements: Sequence[Placement], page: int, rows: int, cols: int) -> list[Slot]:
    """Grid slots of ``page``: its tiles in wall order, then each free cell."""
    tiles = [p for p in placements if p.page == page]
    covered = {cell for p in tiles for cell in p.cells()}
    slots = [p.slot for p in tiles]
    slots += [(r, c, 1, 1) for r in range(rows) for c in range(cols) if (r, c) not in covered]
    return slots


def swap(
    placements: Sequence[Placement], spans: Mapping[str, Span], a: str, b: str
) -> tuple[list[Placement], dict[str, Span]]:
    """Exchange two cameras' tiles. Sizes stay with the slots, so the wall
    keeps its shape exactly and only the two pictures trade places."""
    names = {a: b, b: a}
    swapped = [replace(p, cam_id=names.get(p.cam_id, p.cam_id)) for p in placements]
    new_spans = dict(spans)
    new_spans[a], new_spans[b] = spans.get(b, (1, 1)), spans.get(a, (1, 1))
    return swapped, new_spans


def move_to_cell(
    placements: Sequence[Placement], cam_id: str, page: int, row: int, col: int, cols: int
) -> list[str]:
    """Order with ``cam_id`` moved to the free cell (page, row, col): it goes
    before the first other tile whose top-left comes after that cell."""
    target = (page, row * cols + col)
    rest = [p for p in placements if p.cam_id != cam_id]
    index = sum(1 for p in rest if (p.page, p.row * cols + p.col) < target)
    order = [p.cam_id for p in rest]
    order.insert(index, cam_id)
    return order
