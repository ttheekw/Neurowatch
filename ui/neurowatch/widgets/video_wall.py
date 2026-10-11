"""Video wall widget — grid of CellWidgets with per-camera frame routing."""

from __future__ import annotations

import logging
import math
import time
from collections.abc import Iterable
from dataclasses import dataclass, field

from PySide6.QtCore import QPoint, QRect, QSize, Qt, QTimer, Signal
from PySide6.QtGui import (
    QCloseEvent,
    QColor,
    QCursor,
    QFont,
    QFontMetrics,
    QKeyEvent,
    QMouseEvent,
    QPainter,
    QPaintEvent,
    QPen,
    QResizeEvent,
)
from PySide6.QtWidgets import QGridLayout, QLayout, QWidget

from neurowatch.ipc.frame_reader import FrameReaderThread
from neurowatch.ipc.viewer_client import NativeViewerClient
from neurowatch.models.camera import MAIN_STREAM, SUB_STREAM, CameraConfig, connection_status
from neurowatch.models.wall_layout import (
    Placement,
    Slot,
    Span,
    clamp_span,
    move_to_cell,
    pack,
    page_slots,
    restore,
    swap,
)
from neurowatch.widgets.cell_widget import (
    AUDIO_AVAILABLE,
    AUDIO_NONE,
    AUDIO_ON,
    CellStatus,
    CellWidget,
)
from neurowatch.widgets.native_viewer_surface import BindResult, NativeViewerSurface

logger = logging.getLogger(__name__)

_HIDDEN_PAGE_FPS = 2.0
# A cached frame younger than this came from a running decoder (a page warmed
# before an automatic page change): shown as live, not "refreshing".
_FRESH_FRAME_S = 1.5
# After an outage, pictures may come back a moment before the backend's
# camera status does; for this long after they resume, fresh pictures win.
_RECOVERY_GRACE_S = 10.0
# Shape of one grid cell. Cells keep it so a camera's picture fills its tile
# and every gap between tiles is the same; space the wall has to spare is one
# black margin around the whole grid, not bars inside each tile.
_CELL_ASPECT = 16.0 / 9.0
_GRID_SPACING = 1
_GRID_MARGIN = 1
# A sidebar drag that leaves one tile usually enters the next one at once.
_EXTERNAL_PREVIEW_LINGER_MS = 120
# Holding a dragged tile this close to the wall's left or right edge, this
# long, shows the previous or next page, so the tile can go there.
_PAGE_EDGE_PX = 48
_PAGE_FLIP_DELAY_MS = 600


def _cameras_move(count: int) -> str:
    return "1 camera moves" if count == 1 else f"{count} cameras move"


@dataclass
class CameraHandover:
    """What one wall gives another when cameras move between windows: their
    running frame readers, last pictures and connection states."""

    readers: dict[str, FrameReaderThread] = field(default_factory=dict)
    frames: dict[str, tuple[bytes, int, int, str]] = field(default_factory=dict)
    statuses: dict[str, str] = field(default_factory=dict)


@dataclass
class _LayoutResult:
    """A proposed wall layout: what a drop or resize would change."""

    placements: list[Placement]  # every tile, sorted by position
    spans: dict[str, Span]
    cam_id: str
    page: int  # page the edited camera lands on
    changed: bool
    partner: str | None = None  # swap partner
    notes: list[str] = field(default_factory=list)


@dataclass
class _LayoutDrag:
    kind: str  # "move" | "resize" | "external" (camera dragged from the sidebar)
    cam_id: str
    source: CellWidget | None
    result: _LayoutResult | None = None
    # -1/+1 while held at the left/right edge with a page there, else 0.
    edge: int = 0
    last_pos: QPoint = field(default_factory=QPoint)  # global


@dataclass
class _PreviewTile:
    rect: QRect
    name: str
    role: str  # "active" | "partner" | "other"
    size: str = ""


@dataclass
class _EdgeHint:
    """A page beside this one a dragged tile can go to."""

    side: int  # -1 left, +1 right
    label: str
    active: bool  # the tile is held there: the page shows shortly


@dataclass
class _Preview:
    tiles: list[_PreviewTile]
    empty: list[QRect]
    ghost: tuple[QPoint, str] | None
    message: str
    edges: list[_EdgeHint] = field(default_factory=list)


class _LayoutPreviewOverlay(QWidget):
    """Shows the page a drop or resize would produce, above the live tiles.

    Painted by Qt, so on X11 a native video overlay window still draws over
    it; the tile outlines and the message strip remain visible there.
    """

    _WASH = QColor(6, 8, 16, 215)
    _ACCENT = QColor(96, 165, 250)
    _ACCENT_FILL = QColor(59, 130, 246, 70)
    _PARTNER = QColor(245, 158, 11)
    _PARTNER_FILL = QColor(245, 158, 11, 45)
    _OTHER = QColor(148, 163, 184, 130)
    _OTHER_FILL = QColor(255, 255, 255, 14)
    _TEXT = QColor(226, 232, 240)
    _MUTED = QColor(148, 163, 184)
    _PILL = QColor(15, 23, 42, 235)

    def __init__(self, parent: QWidget) -> None:
        super().__init__(parent)
        self.setAttribute(Qt.WidgetAttribute.WA_TransparentForMouseEvents, True)
        self.setAttribute(Qt.WidgetAttribute.WA_NoSystemBackground, True)
        self.setStyleSheet("background: transparent;")
        self._preview: _Preview | None = None
        self.hide()

    @property
    def preview(self) -> _Preview | None:
        return self._preview

    def set_preview(self, preview: _Preview | None) -> None:
        self._preview = preview
        self.update()

    def paintEvent(self, _event: QPaintEvent) -> None:
        preview = self._preview
        if preview is None:
            return
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        painter.fillRect(self.rect(), self._WASH)

        dashed = QPen(self._OTHER, 1, Qt.PenStyle.DashLine)
        painter.setBrush(Qt.BrushStyle.NoBrush)
        painter.setPen(dashed)
        for rect in preview.empty:
            painter.drawRoundedRect(rect.adjusted(4, 4, -4, -4), 6, 6)

        name_font = QFont("Sans", 10, QFont.Weight.DemiBold)
        size_font = QFont("Sans", 9)
        for tile in preview.tiles:
            line, fill, width = {
                "active": (self._ACCENT, self._ACCENT_FILL, 2),
                "partner": (self._PARTNER, self._PARTNER_FILL, 2),
            }.get(tile.role, (self._OTHER, self._OTHER_FILL, 1))
            rect = tile.rect.adjusted(3, 3, -3, -3)
            painter.setPen(QPen(line, width))
            painter.setBrush(fill)
            painter.drawRoundedRect(rect, 6, 6)
            painter.setPen(self._TEXT if tile.role != "other" else self._MUTED)
            painter.setFont(name_font)
            text_rect = rect.adjusted(6, 0, -6, -14 if tile.size else 0)
            name = QFontMetrics(name_font).elidedText(
                tile.name, Qt.TextElideMode.ElideRight, text_rect.width()
            )
            painter.drawText(text_rect, Qt.AlignmentFlag.AlignCenter, name)
            if tile.size:
                painter.setFont(size_font)
                painter.drawText(
                    rect.adjusted(6, 18, -6, 0), Qt.AlignmentFlag.AlignCenter, tile.size
                )

        for edge in preview.edges:
            self._draw_edge(painter, edge)

        if preview.ghost is not None:
            pos, name = preview.ghost
            self._draw_pill(
                painter, name, QPoint(pos.x() + 14, pos.y() + 14), self._ACCENT, anchor="top-left"
            )
        if preview.message:
            bottom = QPoint(self.rect().center().x(), self.rect().bottom() - 14)
            self._draw_pill(painter, preview.message, bottom, None, anchor="bottom-center")
        painter.end()

    def _draw_edge(self, painter: QPainter, edge: _EdgeHint) -> None:
        band = QRect(0, 0, _PAGE_EDGE_PX, self.height()).adjusted(3, 3, -3, -3)
        if edge.side > 0:
            band.moveRight(self.width() - 4)
        painter.setPen(QPen(self._ACCENT, 2 if edge.active else 1, Qt.PenStyle.DashLine))
        painter.setBrush(self._ACCENT_FILL if edge.active else self._OTHER_FILL)
        painter.drawRoundedRect(band, 6, 6)
        painter.setPen(self._TEXT if edge.active else self._MUTED)
        painter.setFont(QFont("Sans", 18, QFont.Weight.DemiBold))
        arrow = band.adjusted(0, 0, 0, -24)
        glyph = "\u2039" if edge.side < 0 else "\u203a"
        painter.drawText(arrow, Qt.AlignmentFlag.AlignCenter, glyph)
        painter.setFont(QFont("Sans", 8, QFont.Weight.DemiBold))
        painter.drawText(band.adjusted(0, 24, 0, 0), Qt.AlignmentFlag.AlignCenter, edge.label)

    def _draw_pill(
        self, painter: QPainter, text: str, at: QPoint, border: QColor | None, *, anchor: str
    ) -> None:
        font = QFont("Sans", 9, QFont.Weight.DemiBold)
        fm = QFontMetrics(font)
        max_w = max(80, self.width() - 32)
        text = fm.elidedText(text, Qt.TextElideMode.ElideRight, max_w - 24)
        w, h = fm.horizontalAdvance(text) + 24, fm.height() + 12
        if anchor == "bottom-center":
            rect = QRect(at.x() - w // 2, at.y() - h, w, h)
        else:
            rect = QRect(at.x(), at.y(), w, h)
            rect.moveRight(min(rect.right(), self.width() - 4))
            rect.moveBottom(min(rect.bottom(), self.height() - 4))
        painter.setPen(QPen(border, 1) if border is not None else Qt.PenStyle.NoPen)
        painter.setBrush(self._PILL)
        painter.drawRoundedRect(rect, h / 2, h / 2)
        painter.setPen(self._TEXT)
        painter.setFont(font)
        painter.drawText(rect, Qt.AlignmentFlag.AlignCenter, text)


class VideoWallWidget(QWidget):
    """Displays a page of cameras in a configurable rows x cols grid."""

    cell_expand_requested = Signal(str)  # cam_id (double-click)
    cell_single_click = Signal(str)  # cam_id (single-click)
    cell_add_camera_requested = Signal()
    camera_status_changed = Signal(str, str)  # cam_id, status
    # A drag, drop or resize changed the wall layout: (placements:
    # list[Placement] sorted by position, chosen spans: dict[str, (row_span,
    # col_span)], page to show). The owner stores them in each camera's
    # wall_position, applies them with set_cameras() and saves them.
    layout_edit_requested = Signal(object, object, int)
    camera_quality_requested = Signal(str, str)  # cam_id, stream: "main", "sub" or a stream id
    # Cameras the backend must decode for this wall (visible, not native).
    view_demand_changed = Signal(list)
    # A tile's speaker was clicked: hear this camera live, or stop (cam_id).
    listen_toggled = Signal(str)
    cell_edit_requested = Signal(str)  # cam_id (right-click → Edit)
    cell_remove_requested = Signal(str)  # cam_id (right-click → Remove)
    # A tile held at the wall's edge brought another page into view (0-based);
    # the drag goes on there.
    drag_page_changed = Signal(int)

    def __init__(
        self,
        parent: QWidget | None = None,
        viewer_client: NativeViewerClient | None = None,
    ) -> None:
        super().__init__(parent)
        from neurowatch.widgets.gpu_video_surface import gpu_video_available, reserve_gl_window

        self._gpu_enabled = gpu_video_available()
        if self._gpu_enabled:
            # Video tiles are GL widgets created on the first frame, after the
            # window is shown; without this the window is recreated then.
            reserve_gl_window(self)
        self._rows = 3
        self._cols = 3
        self._stretch_rows = 0
        self._stretch_cols = 0
        self._cameras: list[CameraConfig] = []
        self._page = 0
        self._solo_camera_id: str | None = None
        self._cells: list[CellWidget] = []
        self._demand: tuple[str, ...] = ()
        self._readers: dict[str, FrameReaderThread] = {}
        # Hidden pages keep a low-rate reader and the last frame per camera.
        # Their decoders stop (see desktop_demand), so the cached frame is shown
        # marked "refreshing" until the camera's decoder delivers again.
        self._last_frame_by_id: dict[str, tuple[bytes, int, int, str]] = {}
        self._last_frame_at: dict[str, float] = {}  # cam_id -> monotonic time
        # Cameras shown as not streaming, and when their pictures came back.
        self._down_ids: set[str] = set()
        self._resumed_at: dict[str, float] = {}
        # FrameReaderThreads can collectively deliver hundreds of queued Qt
        # signals per second on a dense wall.  Keep only the newest frame for
        # each visible camera and upload pixmaps in one paced GUI-thread pass;
        # otherwise RGB->QPixmap work inside every signal handler lets stale
        # frame events build up and a nominal 15 FPS wall can collapse to 3 FPS.
        self._pending_frames: dict[str, tuple[bytes, int, int, str]] = {}
        self._streaming_ids: set[str] = set()
        # Camera connectivity outlives a page's local preview reader.
        self._status_by_id: dict[str, str] = {}
        # cam_id -> (last_frame_ts, smoothed_fps, last_ui_emit_ts)
        self._fps_state: dict[str, tuple[float, float, float]] = {}
        self._encoding_by_id: dict[str, str] = {}
        # cam_id -> the camera's audio track ("AAC", …), when it has one.
        self._audio_by_id: dict[str, str] = {}
        self._listening: str | None = None
        self._auto_fit: bool = True
        self._no_sub_ids: set[str] = set()  # cameras in grid view with no sub stream
        self._quality_by_id: dict[str, str] = {}  # cam_id -> stream (last emitted)
        # Streams picked from a tile's menu for the current view only; cleared
        # whenever the view changes (page, grid or solo).
        self._view_stream: dict[str, str] = {}
        self._cell_by_cam_id: dict[str, int] = {}
        self._cam_by_id: dict[str, CameraConfig] = {}
        # Tiles of every page (wall order), and the slots laid out right now.
        self._placement_list: list[Placement] = []
        self._placements: dict[str, Placement] = {}
        self._slot_geometry: list[Slot] = []
        self._layout_drag: _LayoutDrag | None = None
        self._layout_editing_enabled = True
        self._empty_hint = "Click to Add Camera"

        # Native viewer client (lazy — never instantiated at import time so
        # unit tests can inject a stub without hitting the backend socket).
        self._viewer_client = viewer_client
        # cam_id -> NativeViewerSurface currently rendering that camera.
        self._native_surfaces: dict[str, NativeViewerSurface] = {}
        # cam_ids that failed native attach and fell back to raw frames.
        self._native_fallback: set[str] = set()
        # Tri-state cache of capability probe: None = not yet probed,
        # True = backend reports native available, False = unavailable.
        self._native_available: bool | None = None

        self._grid = QGridLayout(self)
        self._grid.setSpacing(_GRID_SPACING)
        self._grid.setContentsMargins(_GRID_MARGIN, _GRID_MARGIN, _GRID_MARGIN, _GRID_MARGIN)
        # The spare margins _fit_cells adds follow the wall's size; they must
        # not become its minimum (see minimumSizeHint).
        self._grid.setSizeConstraint(QLayout.SizeConstraint.SetNoConstraint)
        self.setStyleSheet("background-color: #080810;")

        self._render_timer = QTimer(self)
        self._render_timer.setTimerType(Qt.TimerType.PreciseTimer)
        self._render_timer.timeout.connect(self._flush_pending_frames)
        self._render_timer.start(33)

        self._overlay = _LayoutPreviewOverlay(self)
        self._external_linger = QTimer(self)
        self._external_linger.setSingleShot(True)
        self._external_linger.setInterval(_EXTERNAL_PREVIEW_LINGER_MS)
        self._external_linger.timeout.connect(self._end_external_preview)
        self._page_flip = QTimer(self)
        self._page_flip.setSingleShot(True)
        self._page_flip.setInterval(_PAGE_FLIP_DELAY_MS)
        self._page_flip.timeout.connect(self._flip_page_under_drag)

        self._rebuild_cells()

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def set_grid(self, rows: int, cols: int) -> None:
        """Rebuild the grid with the given dimensions."""
        if rows == self._rows and cols == self._cols:
            return
        self._end_layout_drag()
        self._rows = rows
        self._cols = cols
        self._page = 0
        self._view_stream.clear()
        self._update_placements()
        self._rebuild_cells()
        self._assign_cameras()

    def set_cameras(self, cameras: list[CameraConfig], page: int | None = None) -> None:
        """Replace the full camera list and show ``page`` (default: the first).

        Cameras are ordered by their saved wall position; each one's tile
        covers ``CameraConfig.tile_span`` grid cells.
        """
        self._end_layout_drag()
        self._cameras = self._sorted_cameras(list(cameras))
        self._cam_by_id = {cam.id: cam for cam in self._cameras}
        self._update_placements()
        self._encoding_by_id = {cam.id: self._infer_encoding(cam) for cam in self._cameras}
        # Prune quality state only for cameras that are gone.  Preserving entries
        # for existing cameras prevents needless pipeline restarts (and the SHM
        # race they trigger) when set_cameras() is called for an unrelated reason
        # such as adding/editing another camera.
        current_ids = {cam.id for cam in cameras}
        self._quality_by_id = {k: v for k, v in self._quality_by_id.items() if k in current_ids}
        self._status_by_id = {k: v for k, v in self._status_by_id.items() if k in current_ids}
        target_page = 0 if page is None else max(0, min(page, self.page_count - 1))
        if self._page != target_page:
            self._view_stream.clear()  # another page is a new view
        options = {cam.id: {o.id for o in cam.stream_options()} for cam in cameras}
        self._view_stream = {
            k: v for k, v in self._view_stream.items() if v in options.get(k, set())
        }
        self._last_frame_by_id = {
            k: v for k, v in self._last_frame_by_id.items() if k in current_ids
        }
        self._last_frame_at = {k: v for k, v in self._last_frame_at.items() if k in current_ids}
        self._resumed_at = {k: v for k, v in self._resumed_at.items() if k in current_ids}
        self._down_ids &= current_ids
        if self._solo_camera_id not in current_ids:
            self._solo_camera_id = None
        self._page = target_page
        self._assign_cameras()

    def hand_over(self, cam_ids: Iterable[str]) -> CameraHandover:
        """Give up ``cam_ids`` to another wall (see ``adopt``) before they are
        removed from this one. Their readers keep running, so the other wall
        shows their last picture at once instead of waiting for a new reader
        and a first frame."""
        handover = CameraHandover()
        for cam_id in set(cam_ids):
            reader = self._readers.pop(cam_id, None)
            if reader is not None:
                reader.frame_ready.disconnect(self._on_frame_ready)
                handover.readers[cam_id] = reader
            frame = self._last_frame_by_id.get(cam_id)
            if frame is not None:
                handover.frames[cam_id] = frame
            status = self._status_by_id.get(cam_id)
            if status is not None:
                handover.statuses[cam_id] = status
            self._pending_frames.pop(cam_id, None)
            self._fps_state.pop(cam_id, None)
            self._streaming_ids.discard(cam_id)
        return handover

    def adopt(self, handover: CameraHandover) -> None:
        """Take cameras another wall handed over. Call right before the
        set_cameras() that lists them: any camera assignment in between stops
        readers of cameras this wall does not list. Their tiles open on the
        last picture, marked refreshing until the next frame."""
        for cam_id, reader in handover.readers.items():
            previous = self._readers.pop(cam_id, None)
            if previous is not None and previous is not reader:
                previous.stop()
            if hasattr(reader, "set_accept_nv12"):
                reader.set_accept_nv12(self._gpu_enabled)
            reader.frame_ready.connect(self._on_frame_ready)
            self._readers[cam_id] = reader
        for cam_id, frame in handover.frames.items():
            if self._gpu_enabled or not frame[3].startswith("nv12:"):
                self._last_frame_by_id[cam_id] = frame
        self._status_by_id.update(handover.statuses)

    def set_solo_camera(self, cam_id: str | None) -> None:
        """Show one camera without discarding the wall's hidden-page streams."""
        if cam_id is not None and not any(cam.id == cam_id for cam in self._cameras):
            return
        if self._solo_camera_id == cam_id:
            return
        self._end_layout_drag()
        self._solo_camera_id = cam_id
        self._page = 0
        self._view_stream.clear()
        self._assign_cameras()

    def set_page(self, page: int) -> None:
        page = max(0, min(page, self.page_count - 1))
        if page == self._page:
            return
        self._end_layout_drag()
        self._page = page
        self._view_stream.clear()
        self._assign_cameras()

    def set_view_stream(self, cam_id: str, stream_id: str | None) -> None:
        """Show ``stream_id`` for ``cam_id`` in the current view; None = Auto.

        The pick lasts until the view changes (page, grid or solo).
        """
        cam = next((c for c in self._cameras if c.id == cam_id), None)
        if cam is None:
            return
        if stream_id is not None and all(o.id != stream_id for o in cam.stream_options()):
            return
        if self._view_stream.get(cam_id) == stream_id:
            return
        if stream_id is None:
            self._view_stream.pop(cam_id, None)
        else:
            self._view_stream[cam_id] = stream_id
        logger.info("View stream for cam_id=%s: %s", cam_id, stream_id or "auto")
        if cam_id in self._native_surfaces:
            # A native overlay decodes its own stream: re-attach with the pick.
            self._release_native_surface(cam_id)
            self._native_fallback.discard(cam_id)
        self._assign_cameras()

    def _stream_for(self, cam: CameraConfig, use_sub: bool) -> str:
        picked = self._view_stream.get(cam.id)
        if picked is not None:
            return picked
        return SUB_STREAM if use_sub and cam.sub_url else MAIN_STREAM

    @property
    def page(self) -> int:
        return self._page

    @property
    def page_count(self) -> int:
        if self._solo_camera_id is not None or not self._placement_list:
            return 1
        return self._placement_list[-1].page + 1

    @property
    def capacity(self) -> int:
        """Grid cells per page (the most tiles a page can show)."""
        return self._rows * self._cols

    @property
    def visible_count(self) -> int:
        """Cameras shown right now."""
        return len(self._page_cameras())

    def placements(self) -> list[Placement]:
        """Every camera's tile, in wall order, for the current grid."""
        return list(self._placement_list)

    def desktop_demand(self) -> list[str]:
        """Cameras the backend must decode for this wall: those visible in a
        cell that is not drawn by a native overlay (which decodes on its own).
        Hidden pages are not decoded."""
        return sorted(
            cam.id
            for cam in self._page_cameras()
            if cam.id not in self._native_surfaces and cam.id not in self._no_sub_ids
        )

    def page_demand(self, page: int) -> list[str]:
        """Cameras ``page`` decodes once shown. Decoding them shortly before
        an automatic page change lets that page open on current pictures.
        None in solo view, or while native overlays draw this wall (they
        decode on their own once attached)."""
        if self._solo_camera_id is not None or self._native_surfaces:
            return []
        return sorted(p.cam_id for p in self._placement_list if p.page == page)

    def _publish_demand(self) -> None:
        demand = tuple(self.desktop_demand())
        if demand != self._demand:
            self._demand = demand
            self.view_demand_changed.emit(list(demand))

    @property
    def has_native_surfaces(self) -> bool:
        """True while any visible cell is drawn by a native overlay window."""
        return bool(self._native_surfaces)

    def _page_cameras(self) -> list[CameraConfig]:
        if self._solo_camera_id is not None:
            cam = self._cam_by_id.get(self._solo_camera_id)
            return [cam] if cam is not None else []
        return [self._cam_by_id[p.cam_id] for p in self._placement_list if p.page == self._page]

    def _update_placements(self) -> None:
        order, spans = self._layout_order_spans()
        saved = {
            c.id: (c.wall_position.page, c.wall_position.row, c.wall_position.col)
            for c in self._cameras
            if c.wall_position is not None
        }
        self._placement_list = restore(order, saved, spans, self._rows, self._cols)
        self._placements = {p.cam_id: p for p in self._placement_list}

    def _layout_order_spans(self) -> tuple[list[str], dict[str, Span]]:
        return [c.id for c in self._cameras], {c.id: c.tile_span for c in self._cameras}

    def _page_slots(self) -> list[Slot]:
        """Slots of the shown page: its tiles (in _page_cameras order), then
        one empty slot per free grid cell."""
        if self._solo_camera_id is not None:
            return [(0, 0, self._rows, self._cols)]
        return page_slots(self._placement_list, self._page, self._rows, self._cols)

    def _apply_geometry(self) -> None:
        """Lay the cell pool out as the shown page's slots (tiles may span
        several grid cells). Cells are reused, never recreated, here."""
        self._fit_cells()
        slots = self._page_slots()[: len(self._cells)]
        if slots == self._slot_geometry:
            return
        for cell in self._cells:
            self._grid.removeWidget(cell)
        for i, cell in enumerate(self._cells):
            if i < len(slots):
                row, col, row_span, col_span = slots[i]
                self._grid.addWidget(cell, row, col, row_span, col_span)
                cell.setVisible(True)
            else:
                cell.setVisible(False)
        self._slot_geometry = slots
        self._grid.invalidate()
        self._grid.activate()

    # ------------------------------------------------------------------
    # Private helpers
    # ------------------------------------------------------------------

    def _rebuild_cells(self) -> None:
        """Tear down and recreate the grid of CellWidgets."""
        # Clear old stretch factors so removed rows/cols stop consuming space.
        for r in range(self._stretch_rows):
            self._grid.setRowStretch(r, 0)
        for c in range(self._stretch_cols):
            self._grid.setColumnStretch(c, 0)

        # Frame readers are independent of CellWidgets. Keep them alive while
        # rebuilding the visible grid so layout and solo-mode changes do not
        # throw away already-warmed hidden-page streams.
        self._fps_state.clear()

        # A camera that keeps the same cell index in the new layout (e.g. a
        # grid geometry change that doesn't reorder the page) must keep its
        # native session alive — releasing/reattaching tears down the
        # backend GStreamer pipeline and stalls that stream for seconds on
        # X11. self._rows/_cols/_page already reflect the new geometry here,
        # so _page_cameras() predicts the post-rebuild assignment.
        next_cell = {cam.id: i for i, cam in enumerate(self._page_cameras())}
        preserved_surfaces: dict[str, NativeViewerSurface] = {}
        for cam_id, surface in list(self._native_surfaces.items()):
            if next_cell.get(cam_id) == self._cell_by_cam_id.get(cam_id):
                preserved_surfaces[cam_id] = surface
                # Orphan now so the old CellWidget's teardown below does not
                # cascade-delete it as a child widget.
                surface.setParent(None)
            else:
                self._release_native_surface(cam_id)
        self._native_fallback.clear()
        self._no_sub_ids.clear()
        self._cell_by_cam_id.clear()

        # Remove old widgets now: a hidden-but-parented cell would linger
        # (with its last frame) until the event loop runs deleteLater.
        for widget in self._cells:
            self._grid.removeWidget(widget)
            widget.release_gpu_surface()
            widget.hide()
            widget.setParent(None)
            widget.deleteLater()
        self._cells.clear()
        self._slot_geometry = []

        count = self._rows * self._cols
        for i in range(count):
            cell = CellWidget()
            cell.gpu_failed.connect(self._disable_gpu)
            cell.expand_requested.connect(self.cell_expand_requested)
            cell.single_click_requested.connect(self.cell_single_click)
            cell.add_camera_requested.connect(self.cell_add_camera_requested)
            cell.camera_drop_requested.connect(lambda cam_id, idx=i: self.drop_camera(cam_id, idx))
            cell.edit_requested.connect(self.cell_edit_requested)
            cell.remove_requested.connect(self.cell_remove_requested)
            cell.stream_requested.connect(self.set_view_stream)
            cell.audio_toggle_requested.connect(self.listen_toggled)
            cell.tile_size_requested.connect(self.set_tile_size)
            cell.move_drag_started.connect(
                lambda cam_id, pos, c=cell: self._begin_layout_drag("move", cam_id, c, pos)
            )
            cell.resize_drag_started.connect(
                lambda cam_id, pos, c=cell: self._begin_layout_drag("resize", cam_id, c, pos)
            )
            cell.layout_drag_moved.connect(lambda pos, c=cell: self._update_layout_drag(c, pos))
            cell.layout_drag_finished.connect(lambda pos, c=cell: self._finish_layout_drag(c, pos))
            cell.external_drag_moved.connect(self._on_external_drag_moved)
            cell.external_drag_left.connect(self._external_linger.start)
            cell.set_empty_hint(self._empty_hint)
            cell.hide()  # _apply_geometry shows the cells a page uses
            self._cells.append(cell)

        # Reattach preserved surfaces to their (same-index) new cell. This is
        # a pure Qt reparent — no viewer.detach/viewer.attach call — so the
        # backend pipeline keeps running uninterrupted.
        for cam_id, surface in preserved_surfaces.items():
            idx = next_cell[cam_id]
            self._cells[idx].attach_native_surface(surface)
            self._cell_by_cam_id[cam_id] = idx

        # Row/col stretch controlled by auto_fit flag
        stretch = 1 if self._auto_fit else 0
        for r in range(self._rows):
            self._grid.setRowStretch(r, stretch)
        for c in range(self._cols):
            self._grid.setColumnStretch(c, stretch)

        self._stretch_rows = self._rows
        self._stretch_cols = self._cols
        self._apply_geometry()

    def _assign_cameras(self) -> None:
        """Map the current page and keep every other page warm at low FPS."""
        cap = self._rows * self._cols
        self._apply_geometry()
        page_cams = self._page_cameras()
        editable = self._layout_editable()
        grid = (self._rows, self._cols)
        # Every grid preset prefers substreams. Only an explicitly expanded
        # camera uses main; selecting a 1x1 grid must not promote every camera.
        use_sub = self._solo_camera_id is None

        logger.debug(
            "Assigning cameras: page=%d, cam_count=%d, use_sub=%s",
            self._page,
            len(page_cams),
            use_sub,
        )
        # Empty grid slots cost nothing.  Base the preview rate on cameras that
        # are actually visible, not the preset's theoretical capacity (for
        # example, the default 4x8 layout with only 16 cameras).
        target_fps = self._target_ui_fps(len(page_cams), single_view=cap <= 1)
        # Sample at twice the delivery rate.  This avoids phase-locking a 15
        # FPS producer to a 15 FPS GUI timer and accidentally rendering every
        # other frame, while the reader remains the actual FPS limiter.
        self._render_timer.setInterval(max(8, round(500.0 / target_fps)))
        self._pending_frames.clear()

        current_ids = {cam.id for cam in self._cameras}

        # A removed camera is the only reason a warm reader should be stopped.
        for cam_id in list(self._readers.keys()):
            if cam_id not in current_ids:
                logger.debug("Stopping frame reader for removed cam_id=%s", cam_id)
                self._readers[cam_id].stop()
                del self._readers[cam_id]
                self._streaming_ids.discard(cam_id)
                self._fps_state.pop(cam_id, None)

        next_cell = {cam.id: i for i, cam in enumerate(page_cams)}
        for cam_id in list(self._native_surfaces.keys()):
            # A surface is a child of its cell, so a camera that moves to
            # another cell must re-attach there instead of keeping video in
            # the cell that now shows a different camera.
            if cam_id not in next_cell or next_cell[cam_id] != self._cell_by_cam_id.get(cam_id):
                self._release_native_surface(cam_id)
                self._streaming_ids.discard(cam_id)
                self._fps_state.pop(cam_id, None)
        # Cameras that fell back last cycle: reset so we re-probe fresh.
        self._native_fallback = {cid for cid in self._native_fallback if cid in next_cell}

        self._no_sub_ids = set()

        # Configure every camera for the economical sub stream in the wall.
        # In explicit solo mode only the selected camera is
        # promoted to main quality; hidden cameras remain warm on sub quality.
        quality_cams = page_cams if self._solo_camera_id is not None else self._cameras
        for quality_cam in quality_cams:
            target = self._stream_for(quality_cam, use_sub)
            if self._quality_by_id.get(quality_cam.id) == target:
                continue
            self._quality_by_id[quality_cam.id] = target
            logger.info("Stream switch for cam_id=%s: %s", quality_cam.id, target)
            self.camera_quality_requested.emit(quality_cam.id, target)

        # Assign cameras to cells
        self._cell_by_cam_id.clear()
        for i, cell in enumerate(self._cells):
            cam = page_cams[i] if i < len(page_cams) else None
            cell.set_camera(cam)
            if cam is None:
                cell.set_layout_state(False, (1, 1), grid)
                continue
            placement = self._placements.get(cam.id)
            span = placement.span if placement is not None else (1, 1)
            cell.set_layout_state(editable, span, grid)
            cell.set_stream_choice(self._view_stream.get(cam.id))
            self._cell_by_cam_id[cam.id] = i
            cell.set_status(self._status_by_id.get(cam.id, CellStatus.CONNECTING))

            if cam.id in self._no_sub_ids:
                # No sub stream configured — show error overlay, no reader
                cell.set_status(CellStatus.NO_SUB)
                continue

            cell.set_stream_info(self._encoding_by_id.get(cam.id, "AUTO"), None)
            cell.set_audio(self._audio_state(cam.id))
            cached = self._last_frame_by_id.get(cam.id)
            if cached is not None:
                data, width, height, pixel_format = cached
                cell.update_frame(data, width, height, pixel_format)
                cell.set_status(self._status_by_id.get(cam.id, CellStatus.STREAMING))
                # An old picture until its decoder delivers, unless the page
                # was warmed and the picture is current.
                age = time.monotonic() - self._last_frame_at.get(cam.id, -math.inf)
                cell.set_refreshing(age > _FRESH_FRAME_S)

            # Prefer the native VideoOverlay path whenever the backend
            # reports it is available and the previous attempt did not
            # already fail.  Otherwise fall through to the raw-frame reader.
            if cam.id not in self._native_fallback and self._native_ready():
                if self._try_start_native(cam, cell):
                    continue
                # attach failed — remember so we don't retry on every reassign.
                self._native_fallback.add(cam.id)

            if cam.id in self._native_surfaces:
                # Already streaming natively — nothing to do.
                continue

        # Native surfaces cover only visible cells. All other cameras retain a
        # low-rate raw reader so their first frame is already cached when their
        # page becomes visible. Fallback-visible cameras run at wall FPS.
        native_ids = set(self._native_surfaces)
        visible_ids = set(next_cell)
        desired_reader_ids = current_ids - native_ids
        for cam_id in list(self._readers):
            if cam_id not in desired_reader_ids:
                self._readers[cam_id].stop()
                del self._readers[cam_id]
                self._fps_state.pop(cam_id, None)

        for cam in self._cameras:
            if cam.id not in desired_reader_ids:
                continue
            reader_fps = target_fps if cam.id in visible_ids else _HIDDEN_PAGE_FPS
            if cam.id not in self._readers:
                logger.debug(
                    "Starting frame reader for cam_id=%s at %.1f FPS",
                    cam.id,
                    reader_fps,
                )
                reader = FrameReaderThread(cam.id, max_emit_fps=reader_fps)
                if hasattr(reader, "set_accept_nv12"):
                    reader.set_accept_nv12(self._gpu_enabled)
                reader.frame_ready.connect(self._on_frame_ready)
                reader.start()
                self._readers[cam.id] = reader
            else:
                self._readers[cam.id].set_max_emit_fps(reader_fps)
        self._publish_demand()

    # ------------------------------------------------------------------
    # Layout editing: drag a tile to swap/move it, drag its corner to resize
    # ------------------------------------------------------------------

    def set_empty_slot_hint(self, hint: str) -> None:
        """What empty slots say under "Empty Slot"."""
        self._empty_hint = hint
        for cell in self._cells:
            cell.set_empty_hint(hint)

    def set_layout_editing_enabled(self, enabled: bool) -> None:
        """Allow moving and resizing tiles with the mouse (default on)."""
        if enabled == self._layout_editing_enabled:
            return
        self._layout_editing_enabled = enabled
        self._end_layout_drag()
        self._assign_cameras()

    def _layout_editable(self) -> bool:
        return (
            self._layout_editing_enabled
            and self._solo_camera_id is None
            and self._rows * self._cols > 1
        )

    @property
    def layout_drag_active(self) -> bool:
        return self._layout_drag is not None

    def drop_camera(self, cam_id: str, slot_index: int) -> None:
        """A camera dropped on a slot (from the sidebar): swap it with the
        slot's camera, or move it into an empty slot."""
        self._end_external_preview()
        if not self._layout_editable():
            return
        result = self._move_result(cam_id, slot_index)
        if result is not None and result.changed:
            self._request_layout(result)

    def set_tile_size(self, cam_id: str, row_span: int, col_span: int) -> None:
        """Resize ``cam_id``'s tile; later tiles reflow and may change page."""
        if not self._layout_editable() or cam_id not in self._placements:
            return
        span = clamp_span((row_span, col_span), self._rows, self._cols)
        result = self._resize_result(cam_id, span)
        if result.changed:
            self._request_layout(result)

    def _request_layout(self, result: _LayoutResult) -> None:
        logger.info("Wall layout edit: cam_id=%s lands on page %d", result.cam_id, result.page)
        self.layout_edit_requested.emit(result.placements, result.spans, result.page)

    def _resize_result(self, cam_id: str, span: Span) -> _LayoutResult:
        """Give ``cam_id`` a new size and pack the wall densely again: tiles
        fill the space a smaller tile frees, or move on for a larger one."""
        order = [p.cam_id for p in self._placement_list]
        spans = self._layout_order_spans()[1]
        spans[cam_id] = span
        return self._layout_result(pack(order, spans, self._rows, self._cols), spans, cam_id)

    def _move_result(self, cam_id: str, slot_index: int) -> _LayoutResult | None:
        if cam_id not in self._placements or not 0 <= slot_index < len(self._slot_geometry):
            return None
        page_cams = self._page_cameras()
        if slot_index < len(page_cams):
            target = page_cams[slot_index].id
            if target == cam_id:
                return None
            spans = self._layout_order_spans()[1]
            placements, spans = swap(self._placement_list, spans, cam_id, target)
            return self._layout_result(placements, spans, cam_id, partner=target)
        row, col = self._slot_geometry[slot_index][:2]
        order = move_to_cell(self._placement_list, cam_id, self._page, row, col, self._cols)
        spans = self._layout_order_spans()[1]
        return self._layout_result(pack(order, spans, self._rows, self._cols), spans, cam_id)

    def _layout_result(
        self,
        placements: list[Placement],
        spans: dict[str, Span],
        cam_id: str,
        partner: str | None = None,
    ) -> _LayoutResult:
        by_id = {p.cam_id: p for p in placements}
        changed = placements != self._placement_list or spans != self._layout_order_spans()[1]
        landing = by_id[cam_id].page
        notes: list[str] = []
        if landing != self._page:
            notes.append(f"{self._camera_name(cam_id)} moves to page {landing + 1}")
        before = {c.id for c in self._page_cameras()}
        after = {p.cam_id for p in placements if p.page == self._page}
        left = sorted(before - after - {cam_id})
        if left:
            pages = {by_id[i].page for i in left}
            where = "the next page" if pages == {self._page + 1} else "other pages"
            notes.append(f"{_cameras_move(len(left))} to {where}")
        arrived = len(after - before - {cam_id})
        if arrived:
            notes.append(f"{_cameras_move(arrived)} onto this page")
        return _LayoutResult(
            placements=placements,
            spans=spans,
            cam_id=cam_id,
            page=landing,
            changed=changed,
            partner=partner,
            notes=notes,
        )

    def _slot_rect(self, slot: Slot) -> QRect:
        row, col, row_span, col_span = slot
        first = self._grid.cellRect(row, col)
        last = self._grid.cellRect(row + row_span - 1, col + col_span - 1)
        return first.united(last)

    def _slot_at(self, pos: QPoint) -> int | None:
        for i, slot in enumerate(self._slot_geometry):
            if self._slot_rect(slot).contains(pos):
                return i
        return None

    def _span_at(self, cam_id: str, pos: QPoint) -> Span:
        """Tile size whose bottom-right corner is the grid cell under ``pos``
        (clamped to the grid); the tile's top-left stays put."""
        placement = self._placements[cam_id]
        row = max(
            [r for r in range(self._rows) if self._grid.cellRect(r, 0).top() <= pos.y()],
            default=0,
        )
        col = max(
            [c for c in range(self._cols) if self._grid.cellRect(0, c).left() <= pos.x()],
            default=0,
        )
        return (
            max(1, min(row - placement.row + 1, self._rows - placement.row)),
            max(1, min(col - placement.col + 1, self._cols - placement.col)),
        )

    def _begin_layout_drag(
        self, kind: str, cam_id: str, source: CellWidget | None, global_pos: QPoint
    ) -> None:
        if not self._layout_editable() or cam_id not in self._placements:
            return
        if self._layout_drag is not None:
            self._end_layout_drag()
        self._layout_drag = _LayoutDrag(kind, cam_id, source)
        self._overlay.setGeometry(self.rect())
        self._overlay.show()
        self._overlay.raise_()
        if kind != "external":
            self.grabKeyboard()  # Esc cancels
        self._update_layout_drag(source, global_pos)

    def _update_layout_drag(self, source: CellWidget | None, global_pos: QPoint) -> None:
        drag = self._layout_drag
        if drag is None or drag.source is not source:
            return
        drag.last_pos = global_pos
        pos = self.mapFromGlobal(global_pos)
        if drag.kind == "resize":
            span = self._span_at(drag.cam_id, pos)
            drag.result = self._resize_result(drag.cam_id, span)
        else:
            slot = self._slot_at(pos) if self.rect().contains(pos) else None
            drag.result = None if slot is None else self._move_result(drag.cam_id, slot)
            self._track_page_edge(drag, pos)
        self._overlay.set_preview(self._build_preview(drag, pos))

    # A tile held at the left or right edge goes to the page on that side.

    def _edge_at(self, pos: QPoint) -> int:
        """-1/+1 when ``pos`` is at the left/right edge and a page lies there."""
        if not 0 <= pos.y() < self.height():
            return 0
        if pos.x() < _PAGE_EDGE_PX and self._page > 0:
            return -1
        if pos.x() >= self.width() - _PAGE_EDGE_PX and self._page < self.page_count - 1:
            return 1
        return 0

    def _track_page_edge(self, drag: _LayoutDrag, pos: QPoint) -> None:
        edge = self._edge_at(pos)
        if edge == drag.edge:
            return
        drag.edge = edge
        if edge:
            self._page_flip.start()
        else:
            self._page_flip.stop()

    def _flip_page_under_drag(self) -> None:
        drag = self._layout_drag
        if drag is None or not drag.edge:
            return
        page = max(0, min(self._page + drag.edge, self.page_count - 1))
        if page == self._page:
            return
        self._page = page
        self._view_stream.clear()
        self._assign_cameras()
        if drag.kind != "external" and QWidget.mouseGrabber() is not self:
            # The dragged tile's cell now shows another camera (or none) and
            # stops following the mouse: the wall carries the drag on.
            self.grabMouse(QCursor(Qt.CursorShape.ClosedHandCursor))
        self._overlay.raise_()
        logger.debug("Drag of cam_id=%s shows page %d", drag.cam_id, page)
        self.drag_page_changed.emit(page)
        drag.edge = 0  # held there still: the next page follows after a pause
        self._update_layout_drag(drag.source, drag.last_pos)

    def mouseMoveEvent(self, event: QMouseEvent) -> None:
        drag = self._layout_drag
        if drag is not None and QWidget.mouseGrabber() is self:
            self._update_layout_drag(drag.source, event.globalPosition().toPoint())
            event.accept()
            return
        super().mouseMoveEvent(event)

    def mouseReleaseEvent(self, event: QMouseEvent) -> None:
        drag = self._layout_drag
        if (
            drag is not None
            and QWidget.mouseGrabber() is self
            and event.button() == Qt.MouseButton.LeftButton
        ):
            self._finish_layout_drag(drag.source, event.globalPosition().toPoint())
            event.accept()
            return
        super().mouseReleaseEvent(event)

    def _finish_layout_drag(self, source: CellWidget | None, global_pos: QPoint) -> None:
        drag = self._layout_drag
        if drag is None or drag.source is not source:
            return
        self._update_layout_drag(source, global_pos)
        result = drag.result
        self._end_layout_drag()
        if result is not None and result.changed:
            self._request_layout(result)

    def _end_layout_drag(self) -> None:
        self._external_linger.stop()
        self._page_flip.stop()
        if self._layout_drag is None:
            return
        self._layout_drag = None
        if QWidget.keyboardGrabber() is self:
            self.releaseKeyboard()
        if QWidget.mouseGrabber() is self:
            self.releaseMouse()
        self._overlay.set_preview(None)
        self._overlay.hide()

    def _on_external_drag_moved(self, cam_id: str, global_pos: QPoint) -> None:
        self._external_linger.stop()
        drag = self._layout_drag
        if drag is None:
            self._begin_layout_drag("external", cam_id, None, global_pos)
        elif drag.kind == "external" and drag.cam_id == cam_id:
            self._update_layout_drag(None, global_pos)

    def _end_external_preview(self) -> None:
        if self._layout_drag is not None and self._layout_drag.kind == "external":
            self._end_layout_drag()

    def _build_preview(self, drag: _LayoutDrag, pos: QPoint) -> _Preview:
        result = drag.result
        placements = result.placements if result is not None else self._placement_list
        tiles: list[_PreviewTile] = []
        for p in placements:
            if p.page != self._page:
                continue
            role = "other"
            if p.cam_id == drag.cam_id:
                role = "active"
            elif result is not None and p.cam_id == result.partner:
                role = "partner"
            size = (
                f"{p.col_span} × {p.row_span}" if drag.kind == "resize" and role == "active" else ""
            )
            tiles.append(
                _PreviewTile(self._slot_rect(p.slot), self._camera_name(p.cam_id), role, size)
            )
        empty = [
            self._slot_rect(slot)
            for slot in page_slots(placements, self._page, self._rows, self._cols)[len(tiles) :]
        ]
        name = self._camera_name(drag.cam_id)
        ghost = None if drag.kind == "resize" else (pos, name)
        if drag.kind == "resize":
            span = result.spans[drag.cam_id] if result is not None else (1, 1)
            parts = [f"{name}: {span[1]} × {span[0]}"]
        elif result is None:
            parts = ["Drop on a tile to swap, or on an empty slot to move"]
        elif result.partner is not None:
            parts = [f"Swap {name} ↔ {self._camera_name(result.partner)}"]
        else:
            parts = [f"Move {name} here"]
        if result is not None:
            parts += result.notes
        edges: list[_EdgeHint] = []
        if drag.kind != "resize":
            if drag.edge:
                parts.append(f"Hold to show page {self._page + drag.edge + 1}")
            if self._page > 0:
                edges.append(_EdgeHint(-1, f"Page {self._page}", drag.edge < 0))
            if self._page < self.page_count - 1:
                edges.append(_EdgeHint(1, f"Page {self._page + 2}", drag.edge > 0))
        if drag.kind != "external":
            parts.append("Esc to cancel")
        return _Preview(tiles, empty, ghost, "  ·  ".join(parts), edges)

    def _camera_name(self, cam_id: str) -> str:
        cam = self._cam_by_id.get(cam_id)
        return (cam.name if cam is not None else "") or "Camera"

    def keyPressEvent(self, event: QKeyEvent) -> None:
        if self._layout_drag is not None and event.key() == Qt.Key.Key_Escape:
            self._end_layout_drag()
            event.accept()
            return
        super().keyPressEvent(event)

    def _fit_cells(self) -> None:
        """Size the grid so each cell is _CELL_ASPECT, centred in the wall.

        In solo view the one tile spans the whole grid, so it is the tile
        that keeps the shape."""
        rows, cols = (1, 1) if self._solo_camera_id is not None else (self._rows, self._cols)
        width = self.width() - 2 * _GRID_MARGIN - (cols - 1) * _GRID_SPACING
        height = self.height() - 2 * _GRID_MARGIN - (rows - 1) * _GRID_SPACING
        if width <= 0 or height <= 0:
            return
        cell_w = min(width / cols, height / rows * _CELL_ASPECT)
        spare_w = max(0, round(width - cell_w * cols))
        spare_h = max(0, round(height - cell_w / _CELL_ASPECT * rows))
        left, top = spare_w // 2, spare_h // 2
        margins = (
            _GRID_MARGIN + left,
            _GRID_MARGIN + top,
            _GRID_MARGIN + spare_w - left,
            _GRID_MARGIN + spare_h - top,
        )
        current = self._grid.contentsMargins()
        if margins != (current.left(), current.top(), current.right(), current.bottom()):
            self._grid.setContentsMargins(*margins)

    def minimumSizeHint(self) -> QSize:  # noqa: N802
        hint = super().minimumSizeHint()
        m = self._grid.contentsMargins()
        spare_w = m.left() + m.right() - 2 * _GRID_MARGIN
        spare_h = m.top() + m.bottom() - 2 * _GRID_MARGIN
        return QSize(max(0, hint.width() - spare_w), max(0, hint.height() - spare_h))

    def resizeEvent(self, event: QResizeEvent) -> None:
        super().resizeEvent(event)
        self._fit_cells()
        if self._overlay.isVisible():
            self._overlay.setGeometry(self.rect())

    # ------------------------------------------------------------------
    # Native viewer helpers
    # ------------------------------------------------------------------

    def _ensure_viewer_client(self) -> NativeViewerClient | None:
        """Lazily construct the viewer client — never at import/test time."""
        if self._viewer_client is None:
            try:
                self._viewer_client = NativeViewerClient()
            except Exception as e:  # pragma: no cover — defensive
                logger.info("NativeViewerClient unavailable: %s", e)
                self._viewer_client = None
        return self._viewer_client

    def _native_ready(self) -> bool:
        """Return True if the backend reports the native viewer is usable."""
        if self._native_available is not None:
            return self._native_available
        client = self._ensure_viewer_client()
        if client is None:
            self._native_available = False
            return False
        try:
            self._native_available = client.is_available()
        except Exception as e:  # pragma: no cover — defensive
            logger.info("viewer.capabilities probe failed: %s", e)
            self._native_available = False
        return bool(self._native_available)

    def _try_start_native(self, cam: CameraConfig, cell: CellWidget) -> bool:
        """Attempt to install a native viewer surface for ``cam`` in ``cell``.

        Returns True when the surface has been installed (either ATTACHED
        immediately or PENDING until the cell is mapped).  Returns False
        only on a hard FAILED result from ``bind()`` — the caller must
        then fall back to the raw-frame reader path.
        """
        client = self._ensure_viewer_client()
        if client is None:
            return False
        surface = self._native_surfaces.get(cam.id)
        created = surface is None
        if surface is None:
            surface = NativeViewerSurface(client)
            cell.attach_native_surface(surface)
        # Native overlays decode independently of the SHM supervisor. Always
        # pass the resolved stream, including Auto's sub/main choice. bind()
        # preserves an unchanged session and rebinds when its stream changes,
        # even if a grid rebuild kept this camera in the same cell.
        stream = self._stream_for(cam, self._solo_camera_id is None)
        result = surface.bind(cam.id, stream)
        if result is BindResult.FAILED:
            # Hard error from the backend — tear the surface back out so
            # the legacy raw-frame path can own the cell instead.
            self._native_surfaces.pop(cam.id, None)
            cell.detach_native_surface()
            return False
        self._native_surfaces[cam.id] = surface
        if created:
            logger.info(
                "Native viewer surface installed for cam_id=%s (%s)",
                cam.id,
                result.value,
            )
        return True

    def _release_native_surface(self, cam_id: str) -> None:
        surface = self._native_surfaces.pop(cam_id, None)
        if surface is None:
            return
        cell = self._cell_for_cam_id(cam_id)
        if cell is not None:
            cell.detach_native_surface()
        else:
            surface.release()
            surface.setParent(None)
            surface.deleteLater()

    def _cell_for_cam_id(self, cam_id: str) -> CellWidget | None:
        idx = self._cell_by_cam_id.get(cam_id)
        if idx is None or idx >= len(self._cells):
            return None
        return self._cells[idx]

    def _on_frame_ready(
        self, cam_id: str, data: bytes, width: int, height: int, pixel_format: str
    ) -> None:
        """Cache the latest frame; defer repeated pixmap uploads to one GUI tick."""
        if cam_id not in self._encoding_by_id:
            return  # Ignore a queued frame from a camera that was removed.
        if pixel_format.startswith("nv12:") and not self._gpu_enabled:
            return  # Ignore queued GPU frames after requesting RGB fallback.
        first_frame = cam_id not in self._last_frame_by_id
        self._last_frame_by_id[cam_id] = (data, width, height, pixel_format)
        self._last_frame_at[cam_id] = time.monotonic()
        if cam_id in self._down_ids:
            self._down_ids.discard(cam_id)
            self._resumed_at[cam_id] = self._last_frame_at[cam_id]
        self._status_by_id[cam_id] = CellStatus.STREAMING
        if cam_id not in self._streaming_ids:
            self._streaming_ids.add(cam_id)
            self.camera_status_changed.emit(cam_id, "streaming")

        cell_idx = self._cell_by_cam_id.get(cam_id)
        if cell_idx is None or cell_idx >= len(self._cells):
            return
        frame = (data, width, height, pixel_format)
        if first_frame:
            # Do not leave a newly connected camera blank until the next tick.
            self._render_frame(cam_id, frame)
            return
        # Assignment replaces any frame that the GUI has not painted yet.
        # This is the backpressure boundary between decoder threads and Qt.
        self._pending_frames[cam_id] = frame

    def _disable_gpu(self) -> None:
        self._gpu_enabled = False
        for reader in self._readers.values():
            if hasattr(reader, "set_accept_nv12"):
                reader.set_accept_nv12(False)
        # A queued/cached YUV frame must not re-enable a failed renderer.
        self._pending_frames = {
            key: value
            for key, value in self._pending_frames.items()
            if not value[3].startswith("nv12:")
        }
        self._last_frame_by_id = {
            key: value
            for key, value in self._last_frame_by_id.items()
            if not value[3].startswith("nv12:")
        }
        for cell in self._cells:
            cell._gpu_disabled = True
            cell.release_gpu_surface()
            cell.update()

    def _flush_pending_frames(self) -> None:
        """Render at most one current frame per visible camera per timer pass."""
        if not self._pending_frames:
            return
        pending = self._pending_frames
        self._pending_frames = {}
        for cam_id in tuple(self._cell_by_cam_id):
            frame = pending.get(cam_id)
            if frame is not None:
                self._render_frame(cam_id, frame)

    def _render_frame(self, cam_id: str, frame: tuple[bytes, int, int, str]) -> None:
        """Upload one already-coalesced frame to its currently assigned cell."""
        cell_idx = self._cell_by_cam_id.get(cam_id)
        if cell_idx is None or cell_idx >= len(self._cells):
            return
        data, width, height, pixel_format = frame
        fps = self._update_fps(cam_id)
        self._cells[cell_idx].set_stream_info(self._encoding_by_id.get(cam_id, "AUTO"), fps)
        self._cells[cell_idx].update_frame(data, width, height, pixel_format)
        self._cells[cell_idx].set_refreshing(False)

    def set_auto_fit(self, enabled: bool) -> None:
        """Toggle equal-stretch auto-fit mode for all grid cells."""
        if self._auto_fit == enabled:
            return
        self._auto_fit = enabled
        stretch = 1 if enabled else 0
        for r in range(self._rows):
            self._grid.setRowStretch(r, stretch)
        for c in range(self._cols):
            self._grid.setColumnStretch(c, stretch)
        self._grid.invalidate()

    def set_cell_status(self, cam_id: str, status: str) -> str:
        """Update the status of the cell showing the given camera, and return
        the status applied.

        While a camera recovers (its pictures resumed after it was shown as not
        streaming, less than ``_RECOVERY_GRACE_S`` ago), fresh pictures win: a
        poll saying otherwise is the backend's status lagging behind the
        reconnect, and showing it would flip the tile and the camera list
        between offline and live. A camera that was live and stops is shown as
        it is at once."""
        status = connection_status(status)
        if status != CellStatus.STREAMING and self._recovering(cam_id):
            status = CellStatus.STREAMING
        if status != CellStatus.STREAMING:
            self._down_ids.add(cam_id)
        self._status_by_id[cam_id] = status
        if status == CellStatus.STREAMING:
            self._streaming_ids.add(cam_id)
        else:
            self._streaming_ids.discard(cam_id)
            # Do not let a frame queued just before an outage repaint the cell
            # and change CellWidget's local state back to STREAMING.
            self._pending_frames.pop(cam_id, None)
        if cam_id in self._no_sub_ids:
            return status  # keep the NO_SUB overlay — don't let the poll override it
        cell = self._cell_for_cam_id(cam_id)
        if cell is not None:
            cell.set_status(status)
        return status

    def _recovering(self, cam_id: str) -> bool:
        """Pictures resumed after an outage, recently, and are still arriving."""
        now = time.monotonic()
        resumed = self._resumed_at.get(cam_id)
        last = self._last_frame_at.get(cam_id)
        return (
            resumed is not None
            and last is not None
            and now - resumed <= _RECOVERY_GRACE_S
            and now - last <= _FRESH_FRAME_S
        )

    def set_cell_recording(self, cam_id: str, state: str) -> None:
        """Update recording indicator on the cell showing the given camera."""
        cell = self._cell_for_cam_id(cam_id)
        if cell is not None:
            cell.set_recording(state)

    def set_camera_audio(self, cam_id: str, audio: str | None) -> None:
        """The camera's audio track (``AAC``, …), or None: its tile shows a
        speaker only when it has one."""
        if audio:
            if self._audio_by_id.get(cam_id) == audio:
                return
            self._audio_by_id[cam_id] = audio
        elif self._audio_by_id.pop(cam_id, None) is None:
            return
        cell = self._cell_for_cam_id(cam_id)
        if cell is not None:
            cell.set_audio(self._audio_state(cam_id))

    def set_listening(self, cam_id: str | None) -> None:
        """The camera heard live right now (one for the whole app), or None."""
        if cam_id == self._listening:
            return
        previous, self._listening = self._listening, cam_id
        for changed in (previous, cam_id):
            if changed is not None and (cell := self._cell_for_cam_id(changed)) is not None:
                cell.set_audio(self._audio_state(changed))

    def has_audio(self, cam_id: str) -> bool:
        return cam_id in self._audio_by_id

    def visible_camera_ids(self) -> set[str]:
        """Cameras with a tile on screen in this wall (its shown page or the
        solo camera)."""
        return {cam.id for cam in self._page_cameras()}

    def _audio_state(self, cam_id: str) -> str:
        if cam_id not in self._audio_by_id:
            return AUDIO_NONE
        return AUDIO_ON if cam_id == self._listening else AUDIO_AVAILABLE

    def set_camera_codec(
        self, cam_id: str, codec: str, width: object = None, height: object = None
    ) -> None:
        """Update the detected codec (and input resolution, when known) label."""
        if not codec:
            return
        label = codec
        if isinstance(width, int) and isinstance(height, int) and width > 0 and height > 0:
            label = f"{codec} {width}×{height}"
        if self._encoding_by_id.get(cam_id) == label:
            return  # re-sent on every poll
        self._encoding_by_id[cam_id] = label
        cell = self._cell_for_cam_id(cam_id)
        if cell is not None:
            cell.set_stream_info(label, None)
            cell.update()

    def _sorted_cameras(self, cameras: list[CameraConfig]) -> list[CameraConfig]:
        def sort_key(cam: CameraConfig) -> tuple[int, int, int, int]:
            if cam.wall_position is None:
                return (1, 0, 0, 0)
            wp = cam.wall_position
            return (0, wp.page, wp.row, wp.col)

        return sorted(cameras, key=sort_key)

    def _update_fps(self, cam_id: str) -> float | None:
        now = time.monotonic()
        last = self._fps_state.get(cam_id)
        if last is None:
            self._fps_state[cam_id] = (now, 0.0, now)
            return None

        last_ts, smoothed, last_emit = last
        dt = now - last_ts
        if dt <= 0.001:
            return None

        if dt > 2.0:
            # Stream resumed after a pause; keep previous UI value until new stable samples.
            self._fps_state[cam_id] = (now, smoothed, last_emit)
            return None

        # Time-based low-pass filter (approx. 2.5 s time constant).
        instant = 1.0 / dt
        alpha = math.exp(-dt / 2.5)
        next_smoothed = instant if smoothed <= 0 else (smoothed * alpha + instant * (1.0 - alpha))
        self._fps_state[cam_id] = (now, next_smoothed, last_emit)

        # Rate-limit UI updates to reduce visual jitter.
        if now - last_emit < 0.35:
            return None

        self._fps_state[cam_id] = (now, next_smoothed, now)
        return next_smoothed

    def _infer_encoding(self, cam: CameraConfig) -> str:
        protocol = (cam.protocol or "").strip().lower()
        url = (cam.url or "").strip().lower()

        # Prefer explicit hints in URL/path when available.
        if any(token in url for token in ("h265", "hevc")):
            return "H265"
        if any(token in url for token in ("h264", "avc")):
            return "H264"
        if any(token in url for token in ("mjpeg", "mjpg", "jpeg")):
            return "MJPEG"

        if protocol and protocol != "auto":
            mapped = {
                "rtsp": "CODEC?",
                "mjpeg": "MJPEG",
                "rtmp": "CODEC?",
                "file": "CODEC?",
                "v4l2": "CODEC?",
            }
            return mapped.get(protocol, protocol.upper())

        if url.startswith(("rtsp://", "rtsps://")):
            return "CODEC?"
        if url.startswith(("http://", "https://")):
            return "MJPEG"
        if url.startswith("rtmp://"):
            return "CODEC?"
        if url.startswith("/dev/video"):
            return "CODEC?"
        if url.startswith("/"):
            return "CODEC?"
        return "CODEC?"

    def _target_ui_fps(self, visible_count: int, *, single_view: bool = False) -> float:
        """Scale preview FPS based on visible cell count to keep the UI responsive.

        Keep standard walls (up to 16 cameras) at 15 FPS. This limits preview
        delivery only; it does not change the source or recording frame rate.
        """
        if single_view:
            return 25.0
        if visible_count <= 16:
            return 15.0
        return 4.0

    def mark_quality_unsynced(self, cam_id: str) -> None:
        """Force a quality re-sync on next camera assignment."""
        self._quality_by_id.pop(cam_id, None)

    def shutdown(self) -> None:
        """Stop readers and detach native sessions before Qt destroys the cells."""
        self._end_layout_drag()
        self._render_timer.stop()
        for cell in self._cells:
            cell.release_gpu_surface()
        self._pending_frames.clear()
        for reader in self._readers.values():
            reader.stop()
        self._readers.clear()
        for cam_id in list(self._native_surfaces):
            self._release_native_surface(cam_id)

    def closeEvent(self, event: QCloseEvent) -> None:
        self.shutdown()
        super().closeEvent(event)
