"""An extra live-view window holding one or more pages of the wall's cameras.

The main window moves pages here ("New Window", or "＋ Page" in this window's
title strip); closing this window gives all its cameras back, and "Return
page" gives back the page on screen. Like the main window it draws its own
title strip (no desktop border) and can be moved to another monitor, resized
from its edges and maximized.

Each window has its own layout: a grid (chosen when it opens, changed from
"⊞" in its title strip), and tiles that can be dragged to swap or move and
resized from their corner, as on the main wall, packed without gaps. That
layout belongs to the window only: the main wall's saved positions never
change here, so returned cameras go back where they were.

With two or more pages the window cycles through them, switching at once
with every tile's last picture already drawn (pause/play in the title strip,
PageUp/PageDown by hand). Every window changes pages on the main window's
one page clock, at the same interval and at the same moment (``MainWindow._on_page_clock``).

Each camera is shown by exactly one window, so it is read and decoded once, and
only while its page is on screen. Moving cameras between windows hands over
their running readers and last pictures (``VideoWallWidget.hand_over``/
``adopt``), so tiles never open blank. On X11 the window fades in and out.
"""

from __future__ import annotations

import logging
from dataclasses import replace

from PySide6.QtCore import (
    QEasingCurve,
    QEvent,
    QPropertyAnimation,
    QRect,
    Qt,
    QTimer,
    Signal,
)
from PySide6.QtGui import (
    QCloseEvent,
    QGuiApplication,
    QHideEvent,
    QKeyEvent,
    QMoveEvent,
    QResizeEvent,
    QShowEvent,
)
from PySide6.QtWidgets import (
    QFrame,
    QHBoxLayout,
    QLabel,
    QToolButton,
    QVBoxLayout,
    QWidget,
)

from neurowatch.ipc.viewer_client import NativeViewerClient
from neurowatch.models.camera import CameraConfig, WallPosition
from neurowatch.models.wall_layout import GRID_PRESETS, Placement, Span, grid_label, restore
from neurowatch.models.window_session import SavedGeometry, SavedTile, SavedWindow
from neurowatch.theme import active_colors, apply_dialog_theme, themed_menu
from neurowatch.widgets.video_wall import CameraHandover, VideoWallWidget
from neurowatch.widgets.window_chrome import (
    GRIP_WIDTH,
    ChromeTitleBar,
    EdgeResizer,
    guard_window_size,
)

logger = logging.getLogger(__name__)

_FADE_IN_MS = 160
_FADE_OUT_MS = 120
#: Page-cycle intervals offered in the title strip, seconds.
CYCLE_INTERVALS = (5, 10, 15, 30, 60)
DEFAULT_CYCLE_INTERVAL = 10


def _window_fades() -> bool:
    """Window opacity works on X11 (with a compositor); Wayland ignores it."""
    return QGuiApplication.platformName() == "xcb"


class _PageControls(QWidget):
    """Page navigation, auto-cycle and page moves, in the title strip."""

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        layout = QHBoxLayout(self)
        layout.setContentsMargins(0, 0, 8, 0)
        layout.setSpacing(2)

        self.grid = self._button("", "Grid of this window")
        self.grid.setPopupMode(QToolButton.ToolButtonPopupMode.InstantPopup)
        self.grid_menu = themed_menu(self.grid)
        self.grid.setMenu(self.grid_menu)
        layout.addWidget(self.grid)
        self.add_page = self._button("＋ Page", "Move another page of the main window here")
        layout.addWidget(self.add_page)

        self._paging = QWidget(self)
        paging = QHBoxLayout(self._paging)
        paging.setContentsMargins(0, 0, 0, 0)
        paging.setSpacing(2)
        paging.addWidget(self._separator())
        # Single angle quotation marks read better than < and > as page arrows.
        self.previous = self._button("\u2039", "Previous page (PgUp)")
        self.indicator = QLabel()
        self.indicator.setObjectName("pageIndicator")
        self.next = self._button("\u203a", "Next page (PgDn)")
        self.cycle = self._button("", "")
        self.cycle.setCheckable(True)
        self.cycle.setPopupMode(QToolButton.ToolButtonPopupMode.MenuButtonPopup)
        self.interval_menu = themed_menu(self.cycle)
        self.cycle.setMenu(self.interval_menu)
        self.return_page = self._button("⤺ Return page", "Give the page on screen back")
        for widget in (self.previous, self.indicator, self.next, self.cycle, self.return_page):
            paging.addWidget(widget)
        layout.addWidget(self._paging)
        layout.addWidget(self._separator())

    def _button(self, text: str, tip: str) -> QToolButton:
        button = QToolButton(self)
        button.setObjectName("pageControl")
        button.setText(text)
        button.setToolTip(tip)
        button.setAccessibleName(tip or text)
        button.setFocusPolicy(Qt.FocusPolicy.TabFocus)
        button.setCursor(Qt.CursorShape.PointingHandCursor)
        return button

    def _separator(self) -> QFrame:
        line = QFrame(self)
        line.setObjectName("chromeSeparator")
        return line

    def set_state(
        self,
        *,
        page: int,
        count: int,
        cycling: bool,
        interval: int,
        can_add: bool,
        solo: bool,
        grid: tuple[int, int],
    ) -> None:
        label = grid_label(*grid)
        self.grid.setText(f"⊞ {label}")
        self.grid.setAccessibleName(f"Grid {label}")
        self.grid.setEnabled(not solo)
        for action in self.grid_menu.actions():
            action.setChecked(action.data() == grid)
        self.add_page.setEnabled(can_add)
        self.add_page.setToolTip(
            "Move another page of the main window here"
            if can_add
            else "The main window has no page to spare: it keeps one"
        )
        self._paging.setVisible(count > 1)
        self.indicator.setText(f"Page {page + 1} / {count}")
        self.indicator.setAccessibleName(f"Page {page + 1} of {count}")
        for button in (self.previous, self.next, self.cycle):
            button.setEnabled(not solo)
        self.cycle.setChecked(cycling)
        self.cycle.setText(f"{'⏸' if cycling else '⏵'} {interval} s")
        if solo:
            tip = "Leave single view to change pages"
        elif cycling:
            tip = (
                f"Pages change every {interval} s, in step with the other windows. "
                "Click to pause; arrow for the interval (all windows)."
            )
        else:
            tip = "Paused. Click to cycle pages; arrow for the interval."
        self.cycle.setToolTip(tip)
        self.cycle.setAccessibleName("Pause page cycling" if cycling else "Cycle pages")
        for action in self.interval_menu.actions():
            action.setChecked(action.data() == interval)


class DetachedWallWindow(QWidget):
    """A movable, maximizable window showing pages of cameras."""

    # This window is closing; its cameras are still running, ready to be
    # handed back (wall.hand_over) before the wall stops.
    closed = Signal(object)
    # Its visible cameras changed, or it was minimized/restored.
    view_demand_changed = Signal()
    # "＋ Page": the owner should offer a page of the main wall for this window.
    add_page_requested = Signal(object)  # this window
    # "Return page": give page ``int`` (0-based) back to the main wall.
    return_page_requested = Signal(object, int)  # this window, page
    # The page clock is shared: these ask the owner to adjust it.
    cycling_changed = Signal()  # paused/resumed, pages added/removed, solo
    navigated_by_hand = Signal()  # restart the clock for every window
    interval_chosen = Signal(int)  # seconds, for every window
    # What the owner saves changed: cameras, pages, grid, tiles, cycling, or
    # where the window is (see ``session``).
    layout_changed = Signal()

    def __init__(
        self,
        number: int,
        cameras: list[CameraConfig],
        placements: list[Placement],
        rows: int,
        cols: int,
        *,
        auto_fit: bool = True,
        viewer_client: NativeViewerClient | None = None,
        handover: CameraHandover | None = None,
        cycle_interval: int = DEFAULT_CYCLE_INTERVAL,
    ) -> None:
        super().__init__(None, Qt.WindowType.Window | Qt.WindowType.FramelessWindowHint)
        self.number = number
        self._rows = rows
        self._cols = cols
        # This window's own layout, independent of the main wall's: camera
        # order, known positions (page, row, col) and chosen tile sizes.
        first = sorted(placements, key=lambda p: (p.row, p.col))
        self._order: list[str] = [p.cam_id for p in first]
        # Kept as on the main page; re-packed if they do not suit this grid.
        self._positions: dict[str, tuple[int, int, int]] = {
            p.cam_id: (0, p.row, p.col) for p in first
        }
        self._spans: dict[str, Span] = {p.cam_id: p.span for p in first}
        self._placements: list[Placement] = []
        self._by_id: dict[str, CameraConfig] = {}
        self._pre_solo: tuple[int, int, int] | None = None  # rows, cols, page
        self._closing = False
        self._close_done = False
        self.animate_close = True
        self._fade: QPropertyAnimation | None = None
        self._can_add_page = False
        # Page cycling: on by default; runs while there are two or more pages.
        self._cycling = True
        self._cycle_interval = cycle_interval  # shown; the owner's clock paces it

        self.setObjectName("detachedWindow")
        self.setAttribute(Qt.WidgetAttribute.WA_StyledBackground, True)
        # One dark frame: the edge band (resize grips) matches the title strip
        # and ends in a hairline outline.
        dark = active_colors("dark")
        self.setStyleSheet(
            f"#detachedWindow {{ background-color: {dark.surface_sunken};"
            f" border: 1px solid {dark.border_strong}; }}"
        )
        self.setMinimumSize(560, 320)

        self._layout = QVBoxLayout(self)
        self._layout.setSpacing(0)
        self._chrome = ChromeTitleBar("", self, minimize=True, maximize=True)
        self._controls = _PageControls(self._chrome)
        self._chrome.add_controls(self._controls)
        apply_dialog_theme(self._chrome, "dark")  # like the main title bar
        self._layout.addWidget(self._chrome)
        self._resizer = EdgeResizer(self)
        # GNOME doubles a frameless window when a dialog opens over it.
        self._size_guard = guard_window_size(self)

        self.wall = VideoWallWidget(self, viewer_client=viewer_client)
        self.wall.layout_edit_requested.connect(self._on_layout_edit)
        self.wall.drag_page_changed.connect(lambda _page: self._refresh_chrome())
        # Cameras are added in the main window; here a free slot takes a tile.
        self.wall.set_empty_slot_hint("Drag a tile here")
        self.wall.set_auto_fit(auto_fit)
        self.wall.set_grid(rows, cols)
        self.wall.cell_expand_requested.connect(self._toggle_solo)
        self.wall.cell_single_click.connect(self._on_single_click)
        self.wall.view_demand_changed.connect(self.view_demand_changed)
        self.wall.setFocusPolicy(Qt.FocusPolicy.ClickFocus)
        self._layout.addWidget(self.wall, stretch=1)

        self._wire_controls()
        self._sync_frame()
        if handover is not None:
            # Only now: any camera assignment before set_cameras() would stop
            # readers of cameras the wall does not list yet.
            self.wall.adopt(handover)
        self.update_cameras(cameras)

    def _wire_controls(self) -> None:
        c = self._controls
        c.add_page.clicked.connect(lambda: self.add_page_requested.emit(self))
        c.previous.clicked.connect(lambda: self.go_to_page(self.page - 1, user=True))
        c.next.clicked.connect(lambda: self.go_to_page(self.page + 1, user=True))
        c.cycle.clicked.connect(self.set_cycling)
        c.return_page.clicked.connect(lambda: self.return_page_requested.emit(self, self.page))
        for label, rows, cols in GRID_PRESETS:
            action = c.grid_menu.addAction(label)
            action.setCheckable(True)
            action.setData((rows, cols))
            action.triggered.connect(lambda _checked, r=rows, k=cols: self.set_grid(r, k))
        c.grid_menu.addSeparator()
        c.grid_menu.addAction("Custom…", self._choose_custom_grid)
        for seconds in CYCLE_INTERVALS:
            action = c.interval_menu.addAction(f"Every {seconds} s")
            action.setCheckable(True)
            action.setData(seconds)
            action.triggered.connect(lambda _checked, s=seconds: self._choose_interval(s))

    # ------------------------------------------------------------------
    # Cameras and pages
    # ------------------------------------------------------------------

    @property
    def camera_ids(self) -> list[str]:
        """Cameras this window shows, page by page in tile order."""
        return [p.cam_id for p in self._placements]

    @property
    def page_count(self) -> int:
        return max((p.page for p in self._placements), default=0) + 1

    @property
    def page(self) -> int:
        """The page on screen (the one solo view returns to)."""
        if self._pre_solo is not None:
            return self._pre_solo[2]
        return self.wall.page

    @property
    def grid(self) -> tuple[int, int]:
        return (self._rows, self._cols)

    def pages(self) -> list[dict[str, WallPosition]]:
        """Each page's tiles in this window's grid (page field 0), even while
        one camera is shown alone."""
        pages: list[dict[str, WallPosition]] = [{} for _ in range(self.page_count)]
        for p in self._placements:
            pages[p.page][p.cam_id] = WallPosition(0, p.row, p.col, p.row_span, p.col_span)
        return pages

    def page_camera_ids(self, page: int) -> list[str]:
        return [p.cam_id for p in self._placements if p.page == page]

    def add_page(
        self,
        placements: list[Placement],
        cameras: list[CameraConfig],
        handover: CameraHandover | None = None,
    ) -> None:
        """Take another page of the main wall (``cameras``: all cameras, as
        for update_cameras). Its cameras follow this window's, filling free
        cells first; the page where they start is shown."""
        self._exit_solo()
        self._by_id = {cam.id: cam for cam in cameras}
        added = sorted(placements, key=lambda p: (p.row, p.col))
        for p in added:
            self._order.append(p.cam_id)
            self._spans[p.cam_id] = p.span
        if handover is not None:
            self.wall.adopt(handover)  # right before set_cameras lists them
        self._relayout()
        first = next((p for p in self._placements if p.cam_id == added[0].cam_id), None)
        self._apply(show_page=first.page if first else 0)
        self._cycling_changed()
        self.layout_changed.emit()
        logger.info("Window %d now holds %d pages", self.number, self.page_count)

    def remove_page(self, page: int) -> None:
        """Drop ``page``'s cameras; hand them over (``wall.hand_over``) first."""
        gone = set(self.page_camera_ids(page))
        if not gone:
            return
        self._exit_solo()
        current = self.wall.page
        self._order = [cam_id for cam_id in self._order if cam_id not in gone]
        for cam_id in gone:
            self._positions.pop(cam_id, None)
            self._spans.pop(cam_id, None)
        shown = current - 1 if current > page else current
        self._apply(show_page=max(0, shown))
        self._cycling_changed()
        self.layout_changed.emit()

    def set_grid(self, rows: int, cols: int) -> None:
        """Lay this window out in ``rows`` x ``cols``, keeping its order and
        tile sizes; the cameras on screen stay on screen."""
        if (rows, cols) == (self._rows, self._cols) or rows < 1 or cols < 1:
            return
        self._exit_solo()
        on_screen = self.page_camera_ids(self.wall.page)
        self._rows, self._cols = rows, cols
        self._positions.clear()  # re-pack in the new grid
        self.wall.set_grid(rows, cols)
        self._relayout()
        anchor = next((p for p in self._placements if on_screen and p.cam_id == on_screen[0]), None)
        self._apply(show_page=anchor.page if anchor else 0)
        self._cycling_changed()
        self.layout_changed.emit()
        logger.info("Window %d grid: %s", self.number, grid_label(rows, cols))

    def _choose_custom_grid(self) -> None:
        from neurowatch.dialogs.custom_grid_dialog import CustomGridDialog

        dialog = CustomGridDialog(self._rows, self._cols, self)
        if dialog.exec() == CustomGridDialog.DialogCode.Accepted:
            self.set_grid(dialog.rows, dialog.cols)

    def _on_layout_edit(self, placements: object, spans: object, page: int) -> None:
        """A tile was moved or resized here: keep it in this window's layout."""
        if not isinstance(placements, list) or not isinstance(spans, dict):
            return
        tiles = [p for p in placements if isinstance(p, Placement) and p.cam_id in self._spans]
        self._order = [p.cam_id for p in tiles]
        self._positions = {p.cam_id: (p.page, p.row, p.col) for p in tiles}
        for p in tiles:
            span = spans.get(p.cam_id, p.span)
            self._spans[p.cam_id] = (int(span[0]), int(span[1]))
        self._apply(show_page=page)
        self._cycling_changed()
        self.layout_changed.emit()

    def update_cameras(self, all_cameras: list[CameraConfig]) -> None:
        """Pick up edits to this window's cameras and drop removed ones. A
        window left with no camera closes."""
        self._by_id = {cam.id: cam for cam in all_cameras}
        self._apply(show_page=self.page)
        if not self._placements and not self._closing:
            QTimer.singleShot(0, self.close)

    def _relayout(self) -> None:
        """Pack this window's cameras in its grid: known positions stay while
        they still fit without gaps; others take the first free cells."""
        self._order = [cam_id for cam_id in self._order if cam_id in self._by_id]
        known = set(self._order)
        self._positions = {k: v for k, v in self._positions.items() if k in known}
        self._spans = {k: v for k, v in self._spans.items() if k in known}
        self._placements = restore(
            self._order, self._positions, self._spans, self._rows, self._cols
        )
        self._order = [p.cam_id for p in self._placements]
        self._positions = {p.cam_id: (p.page, p.row, p.col) for p in self._placements}

    def _apply(self, show_page: int) -> None:
        """Give the wall copies of the cameras at their place here. Copies,
        because their position here must never reach the main wall's saved
        layout."""
        self._relayout()
        copies = [
            replace(
                self._by_id[p.cam_id],
                wall_position=WallPosition(p.page, p.row, p.col, *self._spans[p.cam_id]),
            )
            for p in self._placements
        ]
        last_page = max(0, self.page_count - 1)
        self.wall.set_cameras(copies, page=min(max(0, show_page), last_page))
        if self._pre_solo is not None and self.wall._solo_camera_id is None:
            rows, cols, _page = self._pre_solo
            self._pre_solo = None
            self.wall.set_grid(rows, cols)
        self._refresh_chrome()

    # ------------------------------------------------------------------
    # Saved in the config, so the window opens again with the app
    # ------------------------------------------------------------------

    def session(self) -> SavedWindow:
        """This window as the config keeps it (``desktop.windows``)."""
        tiles = [
            SavedTile(p.cam_id, p.page, p.row, p.col, *self._spans.get(p.cam_id, p.span))
            for p in self._placements
        ]
        if self.isFullScreen():
            state = "fullscreen"
        elif self.isMaximized():
            state = "maximized"
        else:
            state = "normal"
        normal = self.normalGeometry()
        rect = normal if state != "normal" and normal.isValid() else self.geometry()
        screen = self.screen()
        geometry = SavedGeometry(
            screen.name() if screen is not None else "",
            rect.x(),
            rect.y(),
            rect.width(),
            rect.height(),
            state,
        )
        return SavedWindow(
            self.number, self._rows, self._cols, tiles, self.page, self._cycling, geometry
        )

    def restore_session(self, saved: SavedWindow) -> None:
        """Lay this window out as ``saved`` had it: its tiles where they were
        (cameras that no longer exist are left out), the page it showed and
        whether it cycled."""
        tiles = sorted(
            (t for t in saved.tiles if t.camera_id in self._by_id),
            key=lambda t: (t.page, t.row, t.col),
        )
        self._order = [t.camera_id for t in tiles]
        self._positions = {t.camera_id: (t.page, t.row, t.col) for t in tiles}
        self._spans = {t.camera_id: (t.row_span, t.col_span) for t in tiles}
        self._cycling = saved.cycling
        self._apply(show_page=saved.page)
        self._cycling_changed()

    def place_saved(self, geometry: SavedGeometry | None, main_window: QWidget) -> str:
        """Put the window where it was. Returns the state to show it in
        ("normal", "maximized" or "fullscreen"). A screen that is gone puts
        it beside ``main_window`` instead. Wayland desktops choose positions
        themselves; the screen and size still apply."""
        screens = QGuiApplication.screens()
        screen = next((s for s in screens if geometry and s.name() == geometry.screen), None)
        if geometry is None or screen is None:
            self.place_near(main_window)
            return "normal"
        self.setScreen(screen)
        area = screen.availableGeometry()
        rect = QRect(
            geometry.x,
            geometry.y,
            max(self.minimumWidth(), min(geometry.width, area.width())),
            max(self.minimumHeight(), min(geometry.height, area.height())),
        )
        if not area.intersects(rect):
            rect.moveCenter(area.center())
        self.setGeometry(rect)
        return geometry.state

    def desktop_demand(self) -> list[str]:
        """Cameras to decode for this window: none while it is minimized."""
        if not self.isVisible() or self.isMinimized():
            return []
        return self.wall.desktop_demand()

    # ------------------------------------------------------------------
    # Page navigation and cycling
    # ------------------------------------------------------------------

    def go_to_page(self, page: int, *, user: bool = False) -> None:
        """Show ``page`` (wrapping around) at once. A page chosen by hand
        restarts the countdown to the next automatic one.

        No fade: fading the wall dims it towards black and composites every
        tile (GL readbacks included) per animation frame, so the page visibly
        drew tile by tile. The wall paints its cached pictures in one pass."""
        if self.page_count < 2 or self._pre_solo is not None:
            return
        page %= self.page_count
        if page != self.wall.page:
            self.wall.set_page(page)
            self._refresh_chrome()
        if user:
            # After the change: restarting the countdown drops the warmed
            # next page, which may be the one just shown.
            self.navigated_by_hand.emit()

    def set_cycling(self, enabled: bool) -> None:
        """Pause (False) or resume this window's page changes."""
        self._cycling = bool(enabled)
        self._cycling_changed()
        self.layout_changed.emit()

    def set_cycle_interval(self, seconds: int) -> None:
        """Show the shared interval (the owner's clock does the pacing)."""
        self._cycle_interval = max(1, int(seconds))
        self._refresh_chrome()

    def _choose_interval(self, seconds: int) -> None:
        self._cycling = True  # picking an interval means "cycle at this pace"
        self._cycling_changed()
        self.layout_changed.emit()
        self.interval_chosen.emit(seconds)

    @property
    def cycling(self) -> bool:
        """Whether this window changes pages on the clock right now."""
        return self._cycling and self.page_count > 1 and self._pre_solo is None

    def advance(self) -> None:
        """One tick of the shared page clock: show the next page, unless
        paused, showing one camera alone, minimized, or on a single page."""
        if self.cycling and not self.isMinimized() and not self.wall.layout_drag_active:
            # Never flip the page under a tile being dragged.
            self.go_to_page(self.wall.page + 1)

    def upcoming_demand(self) -> list[str]:
        """Cameras of the page the next ``advance`` shows, to decode shortly
        before it so the page opens on current pictures; none when it will
        not change page."""
        if not self.cycling or not self.isVisible() or self.isMinimized():
            return []
        return self.wall.page_demand((self.wall.page + 1) % self.page_count)

    def _cycling_changed(self) -> None:
        self._refresh_chrome()
        self.cycling_changed.emit()

    # ------------------------------------------------------------------
    # Chrome
    # ------------------------------------------------------------------

    def set_can_add_page(self, can_add: bool) -> None:
        """Whether the main window can spare a page for "＋ Page"."""
        self._can_add_page = can_add
        self._refresh_chrome()

    def _refresh_chrome(self) -> None:
        count = len(self.camera_ids)
        pages = self.page_count
        title = f"NeuroWatch — Window {self.number} · {count} camera{'s' if count != 1 else ''}"
        self.setWindowTitle(title)
        self._chrome.set_title(title)
        self._controls.set_state(
            page=self.page if pages else 0,
            count=max(1, pages),
            cycling=self._cycling,
            interval=self._cycle_interval,
            can_add=self._can_add_page,
            solo=self._pre_solo is not None,
            grid=(self._rows, self._cols),
        )

    def _sync_frame(self) -> None:
        """Edge grips and the title strip follow the window state: no grips
        while maximized, no chrome at all in fullscreen."""
        full = self.isFullScreen()
        grip = 0 if full or self.isMaximized() else GRIP_WIDTH
        self._layout.setContentsMargins(grip, grip, grip, grip)
        self._chrome.setVisible(not full)
        self._chrome.sync_window_state()

    # ------------------------------------------------------------------
    # Placement on screen
    # ------------------------------------------------------------------

    def place_near(self, main_window: QWidget) -> None:
        """Open on another monitor when there is one, else beside ``main_window``."""
        screens = QGuiApplication.screens()
        current = main_window.screen()
        others = [s for s in screens if s is not current]
        if others:
            area = others[(self.number - 2) % len(others)].availableGeometry()
        else:
            area = (current or QGuiApplication.primaryScreen()).availableGeometry()
        width = max(self.minimumWidth(), int(area.width() * 0.8))
        height = max(self.minimumHeight(), int(area.height() * 0.8))
        rect = QRect(0, 0, width, height)
        rect.moveCenter(area.center())
        if not others:
            # Cascade so several windows on one monitor do not stack exactly.
            offset = 32 * ((self.number - 2) % 6)
            rect.translate(offset, offset)
        self.setGeometry(rect)

    def present(self) -> None:
        """Show the window, fading it in where the platform supports it."""
        fade = _window_fades() and not self.isVisible()
        if fade:
            self.setWindowOpacity(0.0)
        if self.isMinimized():
            self.showNormal()
        self.show()
        self.raise_()
        self.activateWindow()
        # Keys (F11, Esc, PgUp/PgDn) go to the video; no title button starts focused.
        self.wall.setFocus(Qt.FocusReason.ActiveWindowFocusReason)
        if fade:
            self._animate_opacity(1.0, _FADE_IN_MS)

    def _animate_opacity(self, end: float, duration_ms: int, then: object = None) -> None:
        if self._fade is not None:
            self._fade.stop()
        fade = QPropertyAnimation(self, b"windowOpacity", self)
        fade.setDuration(duration_ms)
        fade.setStartValue(self.windowOpacity())
        fade.setEndValue(end)
        fade.setEasingCurve(QEasingCurve.Type.OutCubic)
        if callable(then):
            fade.finished.connect(then)
        self._fade = fade
        fade.start()

    # ------------------------------------------------------------------
    # Single view inside this window
    # ------------------------------------------------------------------

    def _toggle_solo(self, cam_id: str) -> None:
        if self._pre_solo is not None:
            self._exit_solo()
            return
        self._pre_solo = (self._rows, self._cols, self.wall.page)
        self.wall.set_solo_camera(cam_id)
        self.wall.set_grid(1, 1)
        self._cycling_changed()  # cycling pauses while one camera is shown

    def _exit_solo(self) -> None:
        if self._pre_solo is None:
            return
        rows, cols, page = self._pre_solo
        self._pre_solo = None
        # Grid first, then clear solo, then the page: see MainWindow._exit_solo.
        self.wall.set_grid(rows, cols)
        self.wall.set_solo_camera(None)
        self.wall.set_page(page)
        self._cycling_changed()

    def _on_single_click(self, _cam_id: str) -> None:
        self._exit_solo()

    # ------------------------------------------------------------------
    # Qt events
    # ------------------------------------------------------------------

    def keyPressEvent(self, event: QKeyEvent) -> None:
        key = event.key()
        if key == Qt.Key.Key_F11:
            if self.isFullScreen():
                self._exit_solo()
                self.showNormal()
            else:
                self.showFullScreen()
        elif key == Qt.Key.Key_Escape:
            if self._pre_solo is not None:
                self._exit_solo()
            elif self.isFullScreen():
                self.showNormal()
        elif key == Qt.Key.Key_PageDown:
            self.go_to_page(self.page + 1, user=True)
        elif key == Qt.Key.Key_PageUp:
            self.go_to_page(self.page - 1, user=True)
        elif key == Qt.Key.Key_Home:
            self.go_to_page(0, user=True)
        elif key == Qt.Key.Key_End:
            self.go_to_page(self.page_count - 1, user=True)
        else:
            super().keyPressEvent(event)

    def changeEvent(self, event: QEvent) -> None:
        if event.type() == QEvent.Type.WindowStateChange:
            self._sync_frame()
            self.view_demand_changed.emit()
            self.layout_changed.emit()  # maximized or fullscreen is saved
        super().changeEvent(event)

    def moveEvent(self, event: QMoveEvent) -> None:
        super().moveEvent(event)
        self.layout_changed.emit()

    def resizeEvent(self, event: QResizeEvent) -> None:
        super().resizeEvent(event)
        self.layout_changed.emit()

    def showEvent(self, event: QShowEvent) -> None:
        super().showEvent(event)
        self.view_demand_changed.emit()

    def hideEvent(self, event: QHideEvent) -> None:
        super().hideEvent(event)
        self.view_demand_changed.emit()

    def closeEvent(self, event: QCloseEvent) -> None:
        if self._close_done:
            super().closeEvent(event)
            return
        if not self._closing:
            self._closing = True
            logger.info("Window %d closed: its cameras return to the main window", self.number)
            # The owner takes the cameras (and their running readers) first.
            self.closed.emit(self)
            if (
                self.animate_close
                and _window_fades()
                and self.isVisible()
                and not self.wall.has_native_surfaces
            ):
                # Fade out, then stop what is left. The returned cameras are
                # already live in the main window meanwhile.
                event.ignore()
                self._animate_opacity(0.0, _FADE_OUT_MS, self._finish_close)
                return
        if self._fade is not None and self._fade.state() == QPropertyAnimation.State.Running:
            event.ignore()  # a second close while fading out
            return
        self._finish_close()

    def _finish_close(self) -> None:
        if self._close_done:
            return
        self._close_done = True
        self.wall.shutdown()
        self.close()
        self.deleteLater()
