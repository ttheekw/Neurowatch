"""Single camera cell — matches reference NVR UI design.

Each cell shows:
  - Top-left: green-background camera name label
  - Top-left (next to name): LIVE green badge or OFFLINE orange badge
  - Center (if offline): large OFFLINE overlay text
  - Bottom-center: timestamp
  - Empty state: "Assign Camera" placeholder
"""

from __future__ import annotations

import datetime
import time
from collections.abc import Callable
from typing import TYPE_CHECKING

from PySide6.QtCore import QEvent, QPoint, QRect, Qt, Signal
from PySide6.QtGui import (
    QColor,
    QContextMenuEvent,
    QDragEnterEvent,
    QDragLeaveEvent,
    QDragMoveEvent,
    QDropEvent,
    QFont,
    QFontMetrics,
    QImage,
    QMouseEvent,
    QPainter,
    QPaintEvent,
    QPen,
    QPixmap,
    QPolygon,
    QResizeEvent,
    QWheelEvent,
)
from PySide6.QtWidgets import QApplication, QMenu, QWidget

from neurowatch.models.camera import CameraConfig
from neurowatch.theme import mono_font, themed_menu, ui_font
from neurowatch.widgets.native_viewer_surface import NativeViewerSurface

if TYPE_CHECKING:
    from neurowatch.widgets.gpu_video_surface import GpuVideoSurface

_FONTS: dict[str, QFont] = {}


def _overlay_fonts() -> dict[str, QFont]:
    """Tile overlay fonts, created once (a QGuiApplication must exist)."""
    if not _FONTS:
        _FONTS.update(
            name=ui_font(8, QFont.Weight.DemiBold),
            dot=ui_font(10, QFont.Weight.Bold),
            badge=ui_font(7, QFont.Weight.Bold),
            info=mono_font(8),
        )
    return _FONTS


# Colours
_BG = QColor(18, 18, 28)
_NAME_BG = QColor(20, 60, 24)  # dark green label bg
_NAME_TEXT = QColor(240, 250, 240)
_LIVE_BG = QColor(22, 163, 74)  # bright green badge
_LIVE_TEXT = QColor(255, 255, 255)
_OFFLINE_BG = QColor(20, 20, 30, 180)  # semi-transparent dark overlay
_OFFLINE_TEXT = QColor(234, 88, 12)  # orange
_OFFLINE_BADGE_BG = QColor(180, 50, 10)
_TS_BG = QColor(0, 0, 0, 140)
_TS_TEXT = QColor(220, 220, 230)
_EMPTY_TEXT = QColor(150, 150, 175)  # >= 4.5:1 on _BG
_ASSIGN_TEXT = QColor(115, 115, 140)  # >= 3:1 on _BG
_GRID_LINE = QColor(28, 28, 44)
_BORDER = QColor(22, 22, 36)  # very subtle, barely visible
_ZOOM_TEXT = QColor(200, 200, 100)

_ZOOM_MIN = 1.0
_ZOOM_MAX = 8.0
_ZOOM_STEP = 0.25

_FALLBACK_HINT = "Scroll to zoom · middle-drag to pan · double-click for single view"
_NATIVE_HINT = "Double-click for single view"
_LAYOUT_HINT = "Drag to move · drag the corner to resize"

CAMERA_MIME_PREFIX = "neurowatch/cam_id:"

# Resize grip in a tile's bottom-right corner (hit area, logical px).
_GRIP_SIZE = 18
_GRIP = QColor(226, 232, 240, 200)
_GRIP_SHADOW = QColor(0, 0, 0, 150)

# Recording indicator colours
_REC_DOT_RED = QColor(239, 68, 68)  # #ef4444 — recording
_REC_DOT_AMBER = QColor(245, 158, 11)  # #f59e0b — scheduled


#: The camera's audio as a tile shows it (``CellWidget.set_audio``).
AUDIO_NONE, AUDIO_AVAILABLE, AUDIO_ON = "none", "available", "on"


class CellStatus:
    OFFLINE = "offline"
    CONNECTING = "connecting"
    STREAMING = "streaming"
    ERROR = "error"
    RECONNECTING = "reconnecting"
    NO_SUB = "no_sub"  # grid view but camera has no sub stream configured


class CellWidget(QWidget):
    gpu_failed = Signal()
    expand_requested = Signal(str)  # cam_id (double-click)
    single_click_requested = Signal(str)  # cam_id (single left-click)
    add_camera_requested = Signal()
    camera_drop_requested = Signal(str)
    edit_requested = Signal(str)  # cam_id
    remove_requested = Signal(str)  # cam_id
    stream_requested = Signal(str, object)  # cam_id, stream id or None for Auto
    # The speaker (or its menu item) was clicked: hear this camera, or stop.
    audio_toggle_requested = Signal(str)  # cam_id
    # Wall layout editing. Positions are global so the wall can follow a
    # drag that leaves this tile (the tile keeps the mouse grab meanwhile).
    move_drag_started = Signal(str, QPoint)  # cam_id, global pos
    resize_drag_started = Signal(str, QPoint)  # cam_id, global pos
    layout_drag_moved = Signal(QPoint)  # global pos
    layout_drag_finished = Signal(QPoint)  # global pos
    tile_size_requested = Signal(str, int, int)  # cam_id, row_span, col_span
    # A camera dragged from the sidebar hovers over / leaves this tile.
    external_drag_moved = Signal(str, QPoint)  # cam_id, global pos
    external_drag_left = Signal()

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self._refreshing = False
        self._camera: CameraConfig | None = None
        self._pixmap: QPixmap | None = None
        self._gpu_surface: GpuVideoSurface | None = None
        self._gpu_disabled = False
        self._zoom: float = 1.0
        self._pan: QPoint = QPoint(0, 0)
        self._pan_origin: QPoint = QPoint(0, 0)
        self._pan_start: QPoint = QPoint(0, 0)
        self._zoom_reset_rect: QRect = QRect()
        self._status: str = CellStatus.OFFLINE
        self._encoding: str = "--"
        self._fps: float | None = None
        self._aspect: str = "free"  # "free" | "16:9" | "4:3"
        self._recording_state: str = "idle"  # "idle" | "recording" | "scheduled"
        self._mode: str = "live"  # "live" | "playback"
        # Stream picked for this view from the context menu; None = Auto.
        self._stream_choice: str | None = None
        # slot -> (what it shows, rendered strip); see _cached_strip().
        self._overlay_cache: dict[str, tuple[tuple[object, ...], QPixmap]] = {}
        self._overlay_ts_ms: int | None = None
        # Optional native VideoOverlay surface — populated by VideoWallWidget
        # when the backend's native viewer runtime is available.  When set,
        # this widget skips its own pixmap-based frame draw (the sub-widget
        # renders the frame natively) but continues to draw overlays.
        self._native_surface: NativeViewerSurface | None = None
        # Layout editing (set by the wall): off in solo view and playback.
        self._layout_editable = False
        self._tile_span: tuple[int, int] = (1, 1)
        self._grid_size: tuple[int, int] = (1, 1)
        # "move" (armed, starts past the drag distance), "moving" or "resizing".
        self._gesture: str | None = None
        self._press_pos = QPoint()
        self._hover_grip = False
        # What an empty slot invites the user to do.
        self._empty_hint = "Click to Add Camera"
        self.setMinimumSize(120, 68)
        self.setStyleSheet("background-color: #12121c;")
        self.setAcceptDrops(True)
        self.setMouseTracking(True)  # corner-grip hover
        # The camera's audio: "none", "available" (muted) or "on" (heard).
        self._audio = AUDIO_NONE
        self._audio_rect = QRect()

    def set_mode(self, mode: str) -> None:
        """Set display mode: 'live' (default) or 'playback' (changes badge label/color)."""
        self._mode = mode
        self.update()

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    @property
    def zoom_supported(self) -> bool:
        """Zoom/pan only exists where this widget paints the video itself.

        A native overlay draws its own pixels, which a Qt-side transform
        cannot follow, so zoom is never offered in that mode.
        """
        return self._native_surface is None

    def set_camera(self, cam: CameraConfig | None) -> None:
        if cam is not None and self._camera is not None and cam.id == self._camera.id:
            # Same camera (e.g. an unrelated wall refresh): keep its last
            # frame and zoom instead of flashing the placeholder.
            self._camera = cam
            self.update()
            return
        # Keep the GL surface for the next camera: recreating it on every
        # page change flashes the tile black while the new context starts.
        # Hidden, it no longer shows the previous camera's picture.
        self._hide_gpu_surface()
        self._camera = cam
        self._stream_choice = None
        self._end_gesture()
        self._refresh_tooltip()
        self._pixmap = None
        self._zoom = 1.0
        self._pan = QPoint(0, 0)
        self._status = CellStatus.OFFLINE if cam is None else CellStatus.CONNECTING
        self._encoding = "--"
        self._fps = None
        self._overlay_ts_ms = None
        self._audio = AUDIO_NONE
        self.update()

    def set_audio(self, state: str) -> None:
        """Show the speaker: ``AUDIO_NONE`` (camera without audio, no
        speaker), ``AUDIO_AVAILABLE`` (muted) or ``AUDIO_ON`` (heard)."""
        if state not in (AUDIO_NONE, AUDIO_AVAILABLE, AUDIO_ON) or state == self._audio:
            return
        self._audio = state
        self._refresh_tooltip()
        self.update()

    @property
    def audio_state(self) -> str:
        return self._audio

    def update_frame(self, data: bytes, width: int, height: int, pixel_format: str = "rgb") -> None:
        """Upload a paced frame to GPU YUV textures or the RGB pixmap fallback."""
        if not data or width == 0 or height == 0:
            return
        if pixel_format.startswith("nv12:"):
            if self._gpu_disabled:
                return
            try:
                from neurowatch.widgets.gpu_video_surface import GpuVideoSurface

                if self._gpu_surface is None:
                    self._gpu_surface = GpuVideoSurface(
                        self, self._frame_rect, self._paint_gpu_overlays
                    )
                    self._gpu_surface.failed.connect(self._on_gpu_failed)
                    self._gpu_surface.setGeometry(self.rect())
                self._gpu_surface.set_frame(data, width, height, int(pixel_format.split(":")[1]))
                self._status = CellStatus.STREAMING
                self._gpu_surface.show()
                self._gpu_surface.raise_()
                self.update()
            except Exception:
                self._on_gpu_failed()
            return
        self._hide_gpu_surface()
        try:
            if pixel_format == "gray8":
                img = QImage(data, width, height, width, QImage.Format.Format_Grayscale8)
            else:
                # RGB24 from GStreamer: [R][G][B] per pixel.
                # GStreamer "RGB" format matches Qt Format_RGB888 byte-for-byte.
                # stride = width * 3 bytes/pixel.
                img = QImage(data, width, height, width * 3, QImage.Format.Format_RGB888)
            self._pixmap = QPixmap.fromImage(img)
        except Exception:
            self._pixmap = None
        self._status = CellStatus.STREAMING
        self.update()

    def update(self, *args) -> None:
        surface = getattr(self, "_gpu_surface", None)
        if surface is not None and surface.isVisible():
            surface.update()
        else:
            super().update(*args)

    def _hide_gpu_surface(self) -> None:
        if self._gpu_surface is not None:
            self._gpu_surface.hide()

    def release_gpu_surface(self) -> None:
        if self._gpu_surface is not None:
            surface, self._gpu_surface = self._gpu_surface, None
            surface.hide()
            surface.cleanup()
            surface.deleteLater()

    def _on_gpu_failed(self) -> None:
        self._gpu_disabled = True
        self.release_gpu_surface()
        self.gpu_failed.emit()
        self.update()

    def _paint_gpu_overlays(self, painter: QPainter) -> None:
        self._paint_contents(painter, gpu=True)

    def set_status(self, status: str) -> None:
        self._status = status
        self.update()

    def set_stream_info(self, encoding: str | None = None, fps: float | None = None) -> None:
        if encoding:
            self._encoding = encoding
        if fps is not None:
            self._fps = fps

    def set_aspect(self, mode: str) -> None:
        """Set aspect ratio lock: 'free', '16:9', or '4:3'."""
        self._aspect = mode
        self.update()

    def set_recording(self, state: str) -> None:
        """Update recording indicator. state: 'idle' | 'recording' | 'scheduled'."""
        if self._recording_state != state:
            self._recording_state = state
            self.update()

    def set_layout_state(
        self, editable: bool, span: tuple[int, int], grid: tuple[int, int]
    ) -> None:
        """Whether this tile can be moved/resized, its size and the grid's."""
        changed = (editable, span, grid) != (
            self._layout_editable,
            self._tile_span,
            self._grid_size,
        )
        self._layout_editable = editable
        self._tile_span = span
        self._grid_size = grid
        if not editable:
            self._end_gesture()
        if changed:
            self._refresh_tooltip()
            self.update()

    def set_empty_hint(self, hint: str) -> None:
        """Second line shown on an empty slot."""
        if hint != self._empty_hint:
            self._empty_hint = hint
            self.update()

    @property
    def layout_editable(self) -> bool:
        return self._layout_editable and self._camera is not None

    def grip_rect(self) -> QRect:
        """Hit area of the corner resize grip (bottom-right)."""
        rect = self.rect()
        return QRect(
            rect.right() - _GRIP_SIZE + 1, rect.bottom() - _GRIP_SIZE + 1, _GRIP_SIZE, _GRIP_SIZE
        )

    def _can_resize(self) -> bool:
        rows, cols = self._grid_size
        return self.layout_editable and rows * cols > 1

    def _end_gesture(self) -> None:
        if self._gesture is not None:
            self._gesture = None
            self.unsetCursor()

    def attach_native_surface(self, surface: NativeViewerSurface) -> None:
        """Install a native VideoOverlay surface as a child of this cell.

        The surface is placed to cover the video area (a strip is reserved
        at the top for the name/badge overlay so the two do not overlap on
        X11, where native subwindows composite above Qt-painted content).
        """
        if self._native_surface is surface:
            return
        self.detach_native_surface()
        self.release_gpu_surface()
        surface.setParent(self)
        self._native_surface = surface
        self._zoom = _ZOOM_MIN
        self._pan = QPoint(0, 0)
        self._zoom_reset_rect = QRect()
        self._refresh_tooltip()
        self._layout_native_surface()
        surface.show()
        self.update()

    def detach_native_surface(self) -> None:
        surface = self._native_surface
        if surface is None:
            return
        self._native_surface = None
        surface.release()
        surface.hide()
        surface.setParent(None)
        surface.deleteLater()
        self._refresh_tooltip()
        self.update()

    def _refresh_tooltip(self) -> None:
        if self._camera is None:
            self.setToolTip("")
            return
        hint = _FALLBACK_HINT if self.zoom_supported else _NATIVE_HINT
        if self.layout_editable:
            hint = f"{hint}\n{_LAYOUT_HINT}"
        if getattr(self, "_audio", AUDIO_NONE) != AUDIO_NONE:
            hint = f"{hint}\nSpeaker (top right): hear this camera"
        self.setToolTip(hint)

    def _layout_native_surface(self) -> None:
        if self._native_surface is None:
            return
        rect = self.rect()
        # Reserve a slim top strip (badge/name) and a bottom strip
        # (timestamp/info) so Qt-painted overlays remain visible.
        top_reserved = 24
        bottom_reserved = 22
        avail_h = max(1, rect.height() - top_reserved - bottom_reserved)
        self._native_surface.setGeometry(
            rect.left(), rect.top() + top_reserved, rect.width(), avail_h
        )

    def set_overlay_timestamp(self, ts_ms: int | None) -> None:
        if self._overlay_ts_ms == ts_ms:
            return
        self._overlay_ts_ms = ts_ms
        self.update()

    # ------------------------------------------------------------------
    # Qt events
    # ------------------------------------------------------------------

    def paintEvent(self, _event: QPaintEvent) -> None:
        if self._gpu_surface is not None and self._gpu_surface.isVisible():
            return
        painter = QPainter(self)
        self._paint_contents(painter)
        painter.end()

    def _paint_contents(self, painter: QPainter, gpu: bool = False) -> None:
        # Smooth scaling on every frame is expensive on dense walls.
        painter.setRenderHint(QPainter.RenderHint.SmoothPixmapTransform, self._zoom > 1.0)
        rect = self.rect()

        # GPU video has already cleared and drawn the background.
        if not gpu:
            painter.fillRect(rect, _BG)

        if self._camera is None:
            self._draw_empty(painter, rect)
        else:
            if gpu:
                pass
            elif self._native_surface is not None:
                # The native subwindow draws the video content itself.  Fill
                # the reserved overlay strips so they don't flash the grid
                # placeholder while native rendering is warming up.
                painter.fillRect(rect, _BG)
            elif self._pixmap and not self._pixmap.isNull():
                self._draw_frame(painter, rect)
            else:
                self._draw_grid_placeholder(painter, rect)

            # Dim overlay for OFFLINE / RECONNECTING / NO_SUB states
            if self._status in (CellStatus.OFFLINE, CellStatus.ERROR):
                painter.fillRect(rect, QColor(0, 0, 0, 100))
                self._draw_offline_text(painter, rect)
            elif self._status == CellStatus.RECONNECTING:
                painter.fillRect(rect, QColor(0, 0, 0, 80))
                painter.setPen(QColor(250, 180, 30))
                painter.setFont(ui_font(11, QFont.Weight.Bold))
                painter.drawText(rect, Qt.AlignmentFlag.AlignCenter, "Reconnecting…")
            elif self._status == CellStatus.NO_SUB:
                painter.fillRect(rect, QColor(0, 0, 0, 140))
                painter.setPen(QColor(220, 110, 30))
                painter.setFont(ui_font(11, QFont.Weight.Bold))
                painter.drawText(
                    rect.adjusted(0, -14, 0, 0),
                    Qt.AlignmentFlag.AlignCenter,
                    "No Sub Stream",
                )
                painter.setPen(QColor(160, 80, 20))
                painter.setFont(ui_font(8))
                painter.drawText(
                    rect.adjusted(0, 14, 0, 0),
                    Qt.AlignmentFlag.AlignCenter,
                    "Double-click to view in full screen",
                )

            self._draw_name_label(painter, rect)
            self._draw_speaker(painter, rect)
            self._draw_bottom_info(painter, rect)
            if self.zoom_supported and self._zoom > 1.0:
                self._draw_zoom_badge(painter, rect)
                self._zoom_reset_rect = self._draw_zoom_reset_btn(painter, rect)
            else:
                self._zoom_reset_rect = QRect()
            if self._can_resize() and (self._hover_grip or self._gesture == "resizing"):
                self._draw_resize_grip(painter)

        # Outer border
        painter.setPen(QColor(_BORDER))
        painter.setBrush(Qt.BrushStyle.NoBrush)
        painter.drawRect(rect.adjusted(0, 0, -1, -1))

    def resizeEvent(self, event: QResizeEvent) -> None:
        super().resizeEvent(event)
        self._layout_native_surface()
        if self._gpu_surface is not None:
            self._gpu_surface.setGeometry(self.rect())

    def wheelEvent(self, event: QWheelEvent) -> None:
        delta = event.angleDelta().y()
        if delta != 0 and self.zoom_supported:
            step = _ZOOM_STEP if delta > 0 else -_ZOOM_STEP
            self._zoom = max(_ZOOM_MIN, min(_ZOOM_MAX, self._zoom + step))
            if self._zoom == _ZOOM_MIN:
                self._pan = QPoint(0, 0)
            self.update()
            event.accept()
        else:
            super().wheelEvent(event)

    def mousePressEvent(self, event: QMouseEvent) -> None:
        if (
            event.button() == Qt.MouseButton.LeftButton
            and self._camera is not None
            and self._audio != AUDIO_NONE
            and self._audio_rect.contains(event.position().toPoint())
        ):
            self.audio_toggle_requested.emit(self._camera.id)
            event.accept()
            return
        if event.button() == Qt.MouseButton.LeftButton:
            # Check zoom reset badge first
            if (
                self._zoom > 1.0
                and not self._zoom_reset_rect.isNull()
                and self._zoom_reset_rect.contains(event.position().toPoint())
            ):
                self._zoom = _ZOOM_MIN
                self._pan = QPoint(0, 0)
                self._zoom_reset_rect = QRect()
                self.update()
                event.accept()
                return
            if (
                self._camera
                and self._can_resize()
                and self.grip_rect().contains(event.position().toPoint())
            ):
                # Resizing starts at once: the grip is an explicit handle.
                self._gesture = "resizing"
                self.setCursor(Qt.CursorShape.SizeFDiagCursor)
                self.resize_drag_started.emit(self._camera.id, event.globalPosition().toPoint())
                event.accept()
                return
            if self._camera:
                self.single_click_requested.emit(self._camera.id)
                if self.layout_editable:
                    # A move starts only past the drag distance, so clicks and
                    # double-clicks keep working.
                    self._gesture = "move"
                    self._press_pos = event.position().toPoint()
            else:
                self.add_camera_requested.emit()
                event.accept()
                return
        elif event.button() == Qt.MouseButton.MiddleButton and self.zoom_supported:
            self._pan_origin = event.position().toPoint()
            self._pan_start = QPoint(self._pan.x(), self._pan.y())
            event.accept()
            return
        super().mousePressEvent(event)

    def dragEnterEvent(self, event: QDragEnterEvent) -> None:
        text = event.mimeData().text()
        if text.startswith(CAMERA_MIME_PREFIX):
            event.acceptProposedAction()
            self._emit_external_hover(text, event.position().toPoint())
        else:
            super().dragEnterEvent(event)

    def dragMoveEvent(self, event: QDragMoveEvent) -> None:
        text = event.mimeData().text()
        if text.startswith(CAMERA_MIME_PREFIX):
            event.acceptProposedAction()
            self._emit_external_hover(text, event.position().toPoint())
        else:
            super().dragMoveEvent(event)

    def dragLeaveEvent(self, event: QDragLeaveEvent) -> None:
        self.external_drag_left.emit()
        super().dragLeaveEvent(event)

    def _emit_external_hover(self, text: str, pos: QPoint) -> None:
        cam_id = text.removeprefix(CAMERA_MIME_PREFIX)
        if cam_id:
            self.external_drag_moved.emit(cam_id, self.mapToGlobal(pos))

    def dropEvent(self, event: QDropEvent) -> None:
        text = event.mimeData().text()
        if text.startswith(CAMERA_MIME_PREFIX):
            cam_id = text.removeprefix(CAMERA_MIME_PREFIX)
            self.external_drag_left.emit()
            if cam_id:
                self.camera_drop_requested.emit(cam_id)
            event.acceptProposedAction()
            return
        super().dropEvent(event)

    def mouseMoveEvent(self, event: QMouseEvent) -> None:
        if (
            event.buttons() & Qt.MouseButton.MiddleButton
            and self.zoom_supported
            and self._zoom > 1.0
        ):
            delta = event.position().toPoint() - self._pan_origin
            self._pan = self._pan_start + delta
            self.update()
            event.accept()
            return
        if event.buttons() & Qt.MouseButton.LeftButton and self._gesture is not None:
            self._continue_gesture(event)
            event.accept()
            return
        if not event.buttons():
            self._update_grip_hover(event.position().toPoint())
        super().mouseMoveEvent(event)

    def _continue_gesture(self, event: QMouseEvent) -> None:
        global_pos = event.globalPosition().toPoint()
        if self._gesture == "move":
            moved = (event.position().toPoint() - self._press_pos).manhattanLength()
            if moved < QApplication.startDragDistance() or self._camera is None:
                return
            self._gesture = "moving"
            self.setCursor(Qt.CursorShape.ClosedHandCursor)
            self.move_drag_started.emit(self._camera.id, global_pos)
        self.layout_drag_moved.emit(global_pos)

    def _update_grip_hover(self, pos: QPoint) -> None:
        hover = self._can_resize() and self.grip_rect().contains(pos)
        if hover == self._hover_grip:
            return
        self._hover_grip = hover
        if hover:
            self.setCursor(Qt.CursorShape.SizeFDiagCursor)
        else:
            self.unsetCursor()
        self.update()

    def leaveEvent(self, event: QEvent) -> None:
        if self._hover_grip and self._gesture is None:
            self._hover_grip = False
            self.unsetCursor()
            self.update()
        super().leaveEvent(event)

    def mouseReleaseEvent(self, event: QMouseEvent) -> None:
        if event.button() == Qt.MouseButton.MiddleButton:
            event.accept()
            return
        if event.button() == Qt.MouseButton.LeftButton and self._gesture is not None:
            dragging = self._gesture in ("moving", "resizing")
            self._end_gesture()
            self._update_grip_hover(event.position().toPoint())
            if dragging:
                self.layout_drag_finished.emit(event.globalPosition().toPoint())
                event.accept()
                return
        super().mouseReleaseEvent(event)

    def mouseDoubleClickEvent(self, event: QMouseEvent) -> None:
        if self._camera and event.button() == Qt.MouseButton.LeftButton:
            self.expand_requested.emit(self._camera.id)
            event.accept()
            return
        super().mouseDoubleClickEvent(event)

    def contextMenuEvent(self, event: QContextMenuEvent) -> None:
        if self._camera is None:
            return
        menu = self._build_context_menu()
        menu.exec(event.globalPos())
        menu.deleteLater()
        event.accept()

    def _build_context_menu(self) -> QMenu:
        assert self._camera is not None
        cam_id = self._camera.id
        menu = themed_menu(self)
        if self._audio != AUDIO_NONE:
            live = self._mode != "playback"
            if self._audio == AUDIO_ON:
                text = "Stop Listening" if live else "Mute Audio"
            else:
                text = "Listen" if live else "Play Audio"
            menu.addAction(text, lambda: self.audio_toggle_requested.emit(cam_id))
            menu.addSeparator()
        menu.addAction("Edit Camera", lambda: self.edit_requested.emit(cam_id))
        menu.addAction("Remove Camera", lambda: self.remove_requested.emit(cam_id))
        menu.addSeparator()
        self._add_stream_menu(menu, cam_id)
        self._add_tile_size_menu(menu, cam_id)
        if self.zoom_supported:
            reset = menu.addAction("Reset Zoom", self._reset_zoom)
            reset.setEnabled(self._zoom > 1.0)
        else:
            # Aspect lock is also a Qt-side transform the overlay cannot follow.
            return menu
        aspect_menu = menu.addMenu("Aspect Ratio")
        for mode, label in [("free", "Free"), ("16:9", "16:9"), ("4:3", "4:3")]:
            action = aspect_menu.addAction(label)
            action.setCheckable(True)
            action.setChecked(self._aspect == mode)
            action.triggered.connect(lambda _c, m=mode: self.set_aspect(m))
        return menu

    def set_stream_choice(self, stream_id: str | None) -> None:
        """Mark which stream this view picked (None = Auto) for the menu."""
        self._stream_choice = stream_id

    def _add_stream_menu(self, menu: QMenu, cam_id: str) -> None:
        assert self._camera is not None
        options = self._camera.stream_options()
        if len(options) < 2:
            return
        stream_menu = menu.addMenu("Stream")
        choices: list[tuple[str | None, str]] = [(None, "Auto")]
        choices += [(option.id, option.label()) for option in options]
        for stream_id, label in choices:
            action = stream_menu.addAction(label)
            action.setCheckable(True)
            action.setChecked(self._stream_choice == stream_id)
            action.triggered.connect(
                lambda _c, sid=stream_id: self.stream_requested.emit(cam_id, sid)
            )

    def _add_tile_size_menu(self, menu: QMenu, cam_id: str) -> None:
        if not self._can_resize():
            return
        rows, cols = self._grid_size
        size_menu = menu.addMenu("Tile Size")
        # (row_span, col_span, label); sizes are shown as width × height.
        choices: list[tuple[int, int, str]] = [
            (n, n, f"{n} × {n}") for n in range(1, min(rows, cols) + 1) if n <= 4
        ]
        if cols >= 2:
            choices.append((1, 2, "2 × 1  (wide)"))
        if rows >= 2:
            choices.append((2, 1, "1 × 2  (tall)"))
        for rs, cs, label in choices:
            action = size_menu.addAction(label)
            action.setCheckable(True)
            action.setChecked(self._tile_span == (rs, cs))
            action.triggered.connect(
                lambda _c, r=rs, c=cs: self.tile_size_requested.emit(cam_id, r, c)
            )

    def _reset_zoom(self) -> None:
        self._zoom = _ZOOM_MIN
        self._pan = QPoint(0, 0)
        self.update()

    # ------------------------------------------------------------------
    # Drawing helpers
    # ------------------------------------------------------------------

    def _draw_frame(self, painter: QPainter, rect: QRect) -> None:
        assert self._pixmap is not None
        painter.drawPixmap(
            self._frame_rect(self._pixmap.width(), self._pixmap.height()), self._pixmap
        )

    def _frame_rect(self, iw: int, ih: int) -> QRect:
        rect = self.rect()
        if not iw or not ih or rect.height() == 0:
            return QRect()
        if self._aspect == "free":
            scale = min(rect.width() / iw, rect.height() / ih) * self._zoom
            dw, dh = int(iw * scale), int(ih * scale)
        else:
            target = 16.0 / 9.0 if self._aspect == "16:9" else 4.0 / 3.0
            cell_ratio = rect.width() / rect.height()
            if cell_ratio >= target:
                # cell wider: pillarbox — fit height, shrink width
                dh = rect.height()
                dw = int(dh * target)
            else:
                # cell taller: letterbox — fit width, shrink height
                dw = rect.width()
                dh = int(dw / target)
            dw = int(dw * self._zoom)
            dh = int(dh * self._zoom)

        x = rect.x() + (rect.width() - dw) // 2 + self._pan.x()
        y = rect.y() + (rect.height() - dh) // 2 + self._pan.y()
        return QRect(x, y, dw, dh)

    def _draw_grid_placeholder(self, painter: QPainter, rect: QRect) -> None:
        painter.setPen(_GRID_LINE)
        step = 24
        for x in range(rect.left(), rect.right(), step):
            painter.drawLine(x, rect.top(), x, rect.bottom())
        for y in range(rect.top(), rect.bottom(), step):
            painter.drawLine(rect.left(), y, rect.right(), y)

    def _draw_empty(self, painter: QPainter, rect: QRect) -> None:
        self._draw_grid_placeholder(painter, rect)
        painter.setPen(_EMPTY_TEXT)
        painter.setFont(ui_font(10))
        painter.drawText(
            rect.adjusted(0, -16, 0, 0),
            Qt.AlignmentFlag.AlignCenter,
            "Empty Slot",
        )
        painter.setPen(_ASSIGN_TEXT)
        painter.setFont(ui_font(8))
        painter.drawText(
            rect.adjusted(0, 10, 0, 0),
            Qt.AlignmentFlag.AlignCenter,
            self._empty_hint,
        )

    def _draw_offline_text(self, painter: QPainter, rect: QRect) -> None:
        painter.setPen(_OFFLINE_TEXT)
        painter.setFont(ui_font(14, QFont.Weight.Bold))
        text = "ERROR" if self._status == CellStatus.ERROR else "OFFLINE"
        painter.drawText(rect, Qt.AlignmentFlag.AlignCenter, text)

    def set_refreshing(self, refreshing: bool) -> None:
        """Mark the shown picture as an old one, kept while this camera's
        decoder restarts after it was off-screen."""
        if refreshing == self._refreshing:
            return
        self._refreshing = refreshing
        self.update()

    def _badge(self) -> tuple[str, QColor]:
        if self._mode == "playback" and self._status == CellStatus.STREAMING:
            return "PLAY", QColor(80, 60, 160)  # purple for playback
        if self._status == CellStatus.STREAMING and self._refreshing:
            return "REFRESHING", QColor(70, 90, 120)
        if self._status == CellStatus.STREAMING:
            return "LIVE", _LIVE_BG
        if self._status == CellStatus.CONNECTING:
            return "CONNECTING", QColor(100, 100, 40)
        if self._status == CellStatus.RECONNECTING:
            return "RECONNECTING", QColor(140, 90, 10)
        if self._status == CellStatus.NO_SUB:
            return "NO SUB", QColor(120, 55, 10)
        if self._status == CellStatus.ERROR:
            return "ERROR", _OFFLINE_BADGE_BG
        return "OFFLINE", _OFFLINE_BADGE_BG

    def _cached_strip(
        self, slot: str, key: tuple[object, ...], render: Callable[[], QPixmap]
    ) -> QPixmap:
        """Text overlays are laid out once per change, not once per video frame."""
        cached = self._overlay_cache.get(slot)
        if cached is None or cached[0] != key:
            cached = (key, render())
            self._overlay_cache[slot] = cached
        return cached[1]

    def _draw_name_label(self, painter: QPainter, rect: QRect) -> None:
        if not self._camera:
            return
        name = self._camera.name or "Camera"
        badge_label, badge_bg = self._badge()
        dpr = self.devicePixelRatioF()
        key = (name, self._recording_state, badge_label, badge_bg.rgba(), dpr)
        strip = self._cached_strip(
            "label",
            key,
            lambda: self._render_label_strip(
                name, self._recording_state, badge_label, badge_bg, dpr
            ),
        )
        painter.drawPixmap(rect.left() + 2, rect.top() + 2, strip)

    def _render_label_strip(
        self, name: str, recording_state: str, badge_label: str, badge_bg: QColor, dpr: float
    ) -> QPixmap:
        fonts = _overlay_fonts()
        padding_x, padding_y = 6, 3
        fm = QFontMetrics(fonts["name"])
        text_w = fm.horizontalAdvance(name) + padding_x * 2
        text_h = fm.height() + padding_y * 2
        dot = {"recording": _REC_DOT_RED, "scheduled": _REC_DOT_AMBER}.get(recording_state)
        dot_w = text_h + 1 if dot is not None else 0
        bw = QFontMetrics(fonts["badge"]).horizontalAdvance(badge_label) + 8
        width = dot_w + text_w + 3 + bw

        strip = QPixmap(max(1, round(width * dpr)), max(1, round(text_h * dpr)))
        strip.setDevicePixelRatio(dpr)
        strip.fill(Qt.GlobalColor.transparent)
        painter = QPainter(strip)
        # Recording indicator dot — drawn before the name label
        if dot is not None:
            painter.setPen(dot)
            painter.setFont(fonts["dot"])
            painter.drawText(QRect(0, 0, text_h, text_h), Qt.AlignmentFlag.AlignCenter, "●")
        # Name label background
        name_rect = QRect(dot_w, 0, text_w, text_h)
        painter.fillRect(name_rect, _NAME_BG)
        painter.setPen(_NAME_TEXT)
        painter.setFont(fonts["name"])
        painter.drawText(
            name_rect.adjusted(padding_x, padding_y, -padding_x, -padding_y),
            Qt.AlignmentFlag.AlignVCenter | Qt.AlignmentFlag.AlignLeft,
            name,
        )
        # Status badge right of name
        badge_rect = QRect(dot_w + text_w + 3, 0, bw, text_h)
        painter.fillRect(badge_rect, badge_bg)
        painter.setPen(_LIVE_TEXT)
        painter.setFont(fonts["badge"])
        painter.drawText(badge_rect, Qt.AlignmentFlag.AlignCenter, badge_label)
        painter.end()
        return strip

    def _draw_speaker(self, painter: QPainter, rect: QRect) -> None:
        """The audio button at the top right: a muted speaker (struck through)
        on a camera with audio, a green one with sound waves while it is
        heard. Click to hear this camera, or to stop."""
        if self._audio == AUDIO_NONE or rect.width() < 90:
            self._audio_rect = QRect()
            return
        scale = 1.2  # drawn on a 24×20 grid
        box = QRect(rect.right() - 32, rect.top() + 3, round(24 * scale), round(20 * scale))
        self._audio_rect = box
        on = self._audio == AUDIO_ON
        painter.save()
        painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        painter.translate(box.topLeft())
        painter.scale(scale, scale)
        painter.setPen(Qt.PenStyle.NoPen)
        painter.setBrush(QColor(22, 128, 61, 235) if on else QColor(0, 0, 0, 170))
        painter.drawRoundedRect(QRect(0, 0, 24, 20), 4, 4)
        colour = QColor(255, 255, 255) if on else QColor(225, 225, 235, 220)
        x, y = 5, 10
        body = QPolygon(
            [
                QPoint(x, y - 3),
                QPoint(x + 3, y - 3),
                QPoint(x + 7, y - 7),
                QPoint(x + 7, y + 7),
                QPoint(x + 3, y + 3),
                QPoint(x, y + 3),
            ]
        )
        painter.setBrush(colour)
        painter.drawPolygon(body)
        pen = QPen(colour, 1.6)
        pen.setCapStyle(Qt.PenCapStyle.RoundCap)
        painter.setPen(pen)
        painter.setBrush(Qt.BrushStyle.NoBrush)
        if on:
            painter.drawArc(QRect(x + 6, y - 4, 6, 8), -60 * 16, 120 * 16)
            painter.drawArc(QRect(x + 6, y - 7, 10, 14), -60 * 16, 120 * 16)
        else:
            painter.drawLine(x + 10, y - 3, x + 15, y + 3)
            painter.drawLine(x + 15, y - 3, x + 10, y + 3)
        painter.restore()

    def _draw_zoom_badge(self, painter: QPainter, rect: QRect) -> None:
        text = f"{self._zoom:.1f}×"
        font = mono_font(8, QFont.Weight.Bold)
        painter.setFont(font)
        fm = painter.fontMetrics()
        tw = fm.horizontalAdvance(text) + 8
        th = fm.height() + 4
        x = rect.right() - tw - 4
        y = rect.top() + 4
        painter.fillRect(QRect(x, y, tw, th), QColor(0, 0, 0, 160))
        painter.setPen(_ZOOM_TEXT)
        painter.drawText(QRect(x + 4, y, tw - 8, th), Qt.AlignmentFlag.AlignVCenter, text)

    def _draw_zoom_reset_btn(self, painter: QPainter, rect: QRect) -> QRect:
        text = "↺ 1×"
        font = ui_font(8, QFont.Weight.Bold)
        painter.setFont(font)
        fm = painter.fontMetrics()
        tw = fm.horizontalAdvance(text) + 10
        th = fm.height() + 4
        x = rect.right() - tw - 4
        # Place below the zoom badge (badge height ≈ fm.height()+4, +2 gap)
        badge_th = painter.fontMetrics().height() + 4
        y = rect.top() + 4 + badge_th + 2
        btn_rect = QRect(x, y, tw, th)
        painter.fillRect(btn_rect, QColor(60, 30, 10, 200))
        painter.setPen(QColor(255, 200, 80))
        painter.drawText(btn_rect, Qt.AlignmentFlag.AlignCenter, text)
        return btn_rect

    def _draw_resize_grip(self, painter: QPainter) -> None:
        """Three diagonal ridges in the bottom-right corner."""
        r = self.grip_rect().adjusted(3, 3, -3, -3)
        painter.save()
        painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        painter.setPen(Qt.PenStyle.NoPen)
        painter.setBrush(_GRIP_SHADOW)
        painter.drawPolygon(
            QPolygon(
                [
                    r.topRight() + QPoint(1, -1),
                    r.bottomRight() + QPoint(1, 1),
                    r.bottomLeft() + QPoint(-1, 1),
                ]
            )
        )
        painter.setBrush(Qt.BrushStyle.NoBrush)
        painter.setPen(_GRIP)
        for offset in (3, 7, 11):
            painter.drawLine(r.right() - offset, r.bottom(), r.right(), r.bottom() - offset)
        painter.restore()

    def _draw_bottom_info(self, painter: QPainter, rect: QRect) -> None:
        if self._mode == "playback" and self._overlay_ts_ms is not None:
            try:
                ts = datetime.datetime.fromtimestamp(self._overlay_ts_ms / 1000.0).strftime(
                    "%H:%M:%S"
                )
            except (OverflowError, OSError, ValueError):
                ts = "--:--:--"
        else:
            ts = time.strftime("%H:%M:%S")
        fps_text = f"{self._fps:.1f} fps" if self._fps and self._fps > 0 else "-- fps"
        info = f"{ts}  |  {self._encoding}  |  {fps_text}"
        max_w = rect.width() - 8
        dpr = self.devicePixelRatioF()
        strip = self._cached_strip(
            "info", (info, max_w, dpr), lambda: self._render_info_strip(info, max_w, dpr)
        )
        tw = round(strip.width() / dpr)
        th = round(strip.height() / dpr)
        painter.drawPixmap(rect.center().x() - tw // 2, rect.bottom() - th - 2, strip)

    def _render_info_strip(self, info: str, max_w: int, dpr: float) -> QPixmap:
        font = _overlay_fonts()["info"]
        fm = QFontMetrics(font)
        tw = max(1, min(max_w, fm.horizontalAdvance(info) + 12))
        th = fm.height() + 4
        strip = QPixmap(round(tw * dpr), round(th * dpr))
        strip.setDevicePixelRatio(dpr)
        strip.fill(Qt.GlobalColor.transparent)
        painter = QPainter(strip)
        painter.fillRect(QRect(0, 0, tw, th), _TS_BG)
        painter.setPen(_TS_TEXT)
        painter.setFont(font)
        painter.drawText(
            QRect(6, 0, tw - 12, th),
            Qt.AlignmentFlag.AlignVCenter | Qt.AlignmentFlag.AlignLeft,
            info,
        )
        painter.end()
        return strip
