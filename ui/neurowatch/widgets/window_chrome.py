"""Frameless window chrome matching the main window: a title strip that moves
the window (double-click maximizes), window buttons, and edge resizing.

The main window draws its own title bar; dialogs and extra windows use this
so none of them shows the desktop's window border. Moving and resizing go
through the compositor (``startSystemMove``/``startSystemResize``, X11 and
Wayland) so they feel native, with a manual move as the fallback.

Dialogs get a header carrying their heading, rounded corners and a soft
shadow when the desktop composites windows (Wayland always; X11 when a
compositing manager runs). Without one, transparent corners would show black,
so they fall back to a square frame. ``NW_WINDOW_EFFECTS=0`` forces that.
Extra video windows stay opaque and square on purpose: a translucent window
costs compositing on every video frame and breaks X11 native video overlays.

Wayland lets no application place its windows, and GNOME opens these
frameless dialogs off-centre (often top-left). There a dialog is a
transparent window covering its parent's screen with the card centred in it
(``CardOverlay``): clicks outside the card reach whatever is underneath, the
header drags the card, and a resizable dialog's edges resize it. Where the
application may place windows (X11, Windows) dialogs stay normal windows,
which Qt centres over their parent.
"""

from __future__ import annotations

import ctypes
import ctypes.util
import logging
import os
import sys
import time
from functools import cache

from PySide6.QtCore import QEvent, QObject, QPoint, QRect, QRectF, QSize, Qt, QTimer
from PySide6.QtGui import (
    QColor,
    QCursor,
    QGuiApplication,
    QMouseEvent,
    QPainter,
    QPaintEvent,
    QRegion,
    QWindow,
)
from PySide6.QtWidgets import (
    QApplication,
    QDialog,
    QFrame,
    QHBoxLayout,
    QLabel,
    QLayout,
    QPushButton,
    QVBoxLayout,
    QWidget,
)

logger = logging.getLogger(__name__)

#: Header height of a dialog and title strip height of a window.
DIALOG_HEADER_HEIGHT = 56
WINDOW_CHROME_HEIGHT = 36
#: Width of the invisible edge band that resizes a resizable window.
GRIP_WIDTH = 5
#: Space around a dialog for its shadow, and its corner radius.
SHADOW_MARGIN = 18
CORNER_RADIUS = 12


def window_effects_supported() -> bool:
    """Whether frameless windows may be translucent (rounded, shadowed)."""
    if os.environ.get("NW_WINDOW_EFFECTS", "").strip() == "0":
        return False
    return _platform_composites(QGuiApplication.platformName())


@cache
def _platform_composites(platform: str) -> bool:
    # Not Windows: a translucent (layered) window there loses ClearType and
    # its text looks blurred. Windows 11 rounds opaque dialogs itself
    # (``round_native_corners``).
    if platform.startswith("wayland") or platform == "cocoa":
        return True
    if platform == "xcb":
        running = _x11_compositor_running()
        logger.info("X11 compositing manager %s", "found" if running else "not running")
        return running
    return False  # offscreen, minimal, eglfs…


def _x11_compositor_running() -> bool:
    """True when the X11 screen has an owner of ``_NET_WM_CM_Sn`` (EWMH)."""
    try:
        x11 = ctypes.CDLL(ctypes.util.find_library("X11") or "libX11.so.6")
        x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
        x11.XOpenDisplay.restype = ctypes.c_void_p
        x11.XDefaultScreen.argtypes = [ctypes.c_void_p]
        x11.XDefaultScreen.restype = ctypes.c_int
        x11.XInternAtom.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int]
        x11.XInternAtom.restype = ctypes.c_ulong
        x11.XGetSelectionOwner.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
        x11.XGetSelectionOwner.restype = ctypes.c_ulong
        x11.XCloseDisplay.argtypes = [ctypes.c_void_p]
        display = x11.XOpenDisplay(None)
        if not display:
            return False
        try:
            screen = x11.XDefaultScreen(display)
            atom = x11.XInternAtom(display, f"_NET_WM_CM_S{screen}".encode(), 0)
            return bool(atom and x11.XGetSelectionOwner(display, atom))
        finally:
            x11.XCloseDisplay(display)
    except (OSError, AttributeError):
        return False


_MAXIMIZE_GLYPH = "□"
_RESTORE_GLYPH = "❐"


#: DwmSetWindowAttribute: DWMWA_WINDOW_CORNER_PREFERENCE = DWMWCP_ROUND.
_DWMWA_WINDOW_CORNER_PREFERENCE = 33
_DWMWCP_ROUND = 2


def round_native_corners(window: QWidget) -> bool:
    """Ask Windows 11 to round ``window``'s corners (with its border and
    shadow), as the desktop does for its own windows; Windows 10 and other
    systems are left as they are. True when Windows accepted it."""
    if sys.platform != "win32":
        return False
    try:
        hwnd = int(window.winId())
        preference = ctypes.c_int(_DWMWCP_ROUND)
        # 64-bit Windows has one calling convention, so CDLL fits dwmapi.
        dwmapi = ctypes.CDLL("dwmapi")
        result = dwmapi.DwmSetWindowAttribute(
            ctypes.c_void_p(hwnd),
            _DWMWA_WINDOW_CORNER_PREFERENCE,
            ctypes.byref(preference),
            ctypes.sizeof(preference),
        )
    except (AttributeError, OSError, ValueError):
        return False
    return result == 0


def positions_windows() -> bool:
    """Whether the application may place its windows: Wayland compositors
    ignore client window positions."""
    return not QGuiApplication.platformName().startswith("wayland")


def start_system_move(widget: QWidget) -> bool:
    """Ask the compositor to move ``widget``'s window; False if it cannot."""
    win = widget.window()
    handle = win.windowHandle()
    if handle is None or win.isFullScreen():
        return False
    try:
        return bool(handle.startSystemMove())
    except (AttributeError, RuntimeError):
        return False


class ChromeTitleBar(QWidget):
    """Header of a frameless window: drag it to move the window.

    ``variant="dialog"`` is a dialog's header: its heading in large type and
    a round close button, on the dialog's own surface. ``variant="window"``
    is a compact title strip with window buttons.
    """

    def __init__(
        self,
        title: str,
        parent: QWidget | None = None,
        *,
        minimize: bool = False,
        maximize: bool = False,
        variant: str = "window",
    ) -> None:
        super().__init__(parent)
        self.setObjectName("chromeTitleBar")
        self.setProperty("variant", variant)
        self.setAttribute(Qt.WidgetAttribute.WA_StyledBackground, True)
        dialog = variant == "dialog"
        self.setFixedHeight(DIALOG_HEADER_HEIGHT if dialog else WINDOW_CHROME_HEIGHT)
        self._maximize = maximize
        self._drag_pos: QPoint | None = None
        self._native_drag = False

        layout = QHBoxLayout(self)
        if dialog:
            # The heading lines up with the dialog's content (20 px margins).
            layout.setContentsMargins(20, 6, 12, 0)
        else:
            layout.setContentsMargins(12, 0, 5, 0)
        layout.setSpacing(4)
        self._title = QLabel(title)
        self._title.setObjectName("chromeTitle")
        layout.addWidget(self._title, stretch=1)
        self._controls_at = layout.count()

        self._max_btn: QPushButton | None = None
        if minimize:
            layout.addWidget(self._button("–", "Minimise", "minimize", self._minimize))
        if maximize:
            self._max_btn = self._button(
                _MAXIMIZE_GLYPH, "Maximise / Restore", "maximize", self._toggle_maximize
            )
            layout.addWidget(self._max_btn)
        layout.addWidget(self._button("✕", "Close", "close", lambda: self.window().close()))

    def _button(self, text: str, tip: str, role: str, slot: object) -> QPushButton:
        btn = QPushButton(text)
        btn.setObjectName("chromeButton")
        btn.setProperty("role", role)
        btn.setToolTip(tip)
        btn.setAccessibleName(tip)
        btn.setFocusPolicy(Qt.FocusPolicy.TabFocus)
        btn.setCursor(Qt.CursorShape.PointingHandCursor)
        btn.clicked.connect(slot)
        return btn

    def set_title(self, title: str) -> None:
        self._title.setText(title)

    def add_controls(self, widget: QWidget) -> None:
        """Put ``widget`` between the title and the window buttons."""
        layout = self.layout()
        if isinstance(layout, QHBoxLayout):
            layout.insertWidget(self._controls_at, widget)
            self._controls_at += 1

    def sync_window_state(self) -> None:
        """Show restore or maximize on the button for the window's state."""
        if self._max_btn is not None:
            self._max_btn.setText(
                _RESTORE_GLYPH if self.window().isMaximized() else _MAXIMIZE_GLYPH
            )

    def _minimize(self) -> None:
        self.window().showMinimized()

    def _toggle_maximize(self) -> None:
        if not self._maximize:
            return
        win = self.window()
        if win.isMaximized():
            win.showNormal()
        else:
            win.showMaximized()

    def mousePressEvent(self, event: QMouseEvent) -> None:
        if event.button() == Qt.MouseButton.LeftButton:
            self._drag_pos = event.globalPosition().toPoint()
            # A card overlay's window covers the screen: the card moves instead.
            overlay = card_overlay(self.window())
            self._native_drag = overlay is None and start_system_move(self)
            event.accept()
            return
        super().mousePressEvent(event)

    def mouseMoveEvent(self, event: QMouseEvent) -> None:
        if self._drag_pos is not None and event.buttons() & Qt.MouseButton.LeftButton:
            overlay = card_overlay(self.window())
            if overlay is not None:
                pos = event.globalPosition().toPoint()
                overlay.move_card(pos - self._drag_pos)
                self._drag_pos = pos
            elif not self._native_drag:
                win = self.window()
                if win.isMaximized():
                    win.showNormal()
                pos = event.globalPosition().toPoint()
                win.move(win.pos() + pos - self._drag_pos)
                self._drag_pos = pos
            event.accept()
            return
        super().mouseMoveEvent(event)

    def mouseReleaseEvent(self, event: QMouseEvent) -> None:
        self._drag_pos = None
        self._native_drag = False
        super().mouseReleaseEvent(event)

    def mouseDoubleClickEvent(self, event: QMouseEvent) -> None:
        if event.button() == Qt.MouseButton.LeftButton:
            self._toggle_maximize()
            event.accept()
            return
        super().mouseDoubleClickEvent(event)


class EdgeResizer(QObject):
    """Resizes a frameless window from a thin band along its edges.

    The window keeps a ``grip``-wide margin around its content (``GRIP_WIDTH``
    by default; a chrome dialog's shadow band); mouse events there reach the
    window itself, or ``surface`` (a widget covering the whole window, such
    as a dialog's shadow shell), which this filter turns into a resize cursor
    and a compositor resize. Off while maximized or fullscreen.
    """

    def __init__(
        self, window: QWidget, *, surface: QWidget | None = None, grip: int = GRIP_WIDTH
    ) -> None:
        super().__init__(window)
        self._window = window
        self._surface = surface or window
        self._grip = max(1, grip)
        self._surface.setMouseTracking(True)
        self._surface.installEventFilter(self)

    def edges_at(self, pos: QPoint) -> Qt.Edge:
        """The edges a press at ``pos`` (window coordinates) resizes."""
        win = self._window
        edges = Qt.Edge(0)
        if win.isMaximized() or win.isFullScreen():
            return edges
        grip = self._grip
        if pos.x() < grip:
            edges |= Qt.Edge.LeftEdge
        elif pos.x() >= win.width() - grip:
            edges |= Qt.Edge.RightEdge
        if pos.y() < grip:
            edges |= Qt.Edge.TopEdge
        elif pos.y() >= win.height() - grip:
            edges |= Qt.Edge.BottomEdge
        return edges

    def _window_pos(self, event: QMouseEvent) -> QPoint:
        pos = event.position().toPoint()
        if self._surface is self._window:
            return pos
        return self._surface.mapTo(self._window, pos)

    @staticmethod
    def cursor_for(edges: Qt.Edge) -> Qt.CursorShape | None:
        diagonal = {
            Qt.Edge.LeftEdge | Qt.Edge.TopEdge: Qt.CursorShape.SizeFDiagCursor,
            Qt.Edge.RightEdge | Qt.Edge.BottomEdge: Qt.CursorShape.SizeFDiagCursor,
            Qt.Edge.RightEdge | Qt.Edge.TopEdge: Qt.CursorShape.SizeBDiagCursor,
            Qt.Edge.LeftEdge | Qt.Edge.BottomEdge: Qt.CursorShape.SizeBDiagCursor,
        }
        if edges in diagonal:
            return diagonal[edges]
        if edges & (Qt.Edge.LeftEdge | Qt.Edge.RightEdge):
            return Qt.CursorShape.SizeHorCursor
        if edges & (Qt.Edge.TopEdge | Qt.Edge.BottomEdge):
            return Qt.CursorShape.SizeVerCursor
        return None

    def eventFilter(self, watched: QObject, event: QEvent) -> bool:  # noqa: N802
        if watched is not self._surface:
            return False
        if event.type() == QEvent.Type.MouseMove and isinstance(event, QMouseEvent):
            if not event.buttons():
                cursor = self.cursor_for(self.edges_at(self._window_pos(event)))
                if cursor is None:
                    self._surface.unsetCursor()
                else:
                    self._surface.setCursor(cursor)
        elif event.type() == QEvent.Type.MouseButtonPress and isinstance(event, QMouseEvent):
            edges = self.edges_at(self._window_pos(event))
            handle = self._window.windowHandle()
            if event.button() == Qt.MouseButton.LeftButton and edges and handle is not None:
                # The sizes the drag brings are the user's (strict guard).
                allow_resize(self._window)
                try:
                    handle.startSystemResize(edges)
                except (AttributeError, RuntimeError):
                    return False
                return True
        elif event.type() == QEvent.Type.Leave:
            self._surface.unsetCursor()
        return False


class _ShadowShell(QWidget):
    """Holds a dialog's frame and paints a soft shadow around it."""

    def __init__(self, parent: QWidget, margin: int) -> None:
        super().__init__(parent)
        self._margin = margin

    def paintEvent(self, _event: QPaintEvent) -> None:
        if not self._margin:
            return
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        painter.setPen(Qt.PenStyle.NoPen)
        m = self._margin
        # The shadow falls slightly downward, like light from above.
        frame = QRectF(self.rect()).adjusted(m, m, -m, -m).translated(0, 4)
        # Stacked translucent outlines: dense at the frame, fading outward.
        layers = m - 2
        for i in range(layers, 0, -1):
            spread = float(i)
            painter.setBrush(QColor(0, 0, 0, 7))
            painter.drawRoundedRect(
                frame.adjusted(-spread, -spread, spread, spread),
                CORNER_RADIUS + spread,
                CORNER_RADIUS + spread,
            )
        painter.end()


_NORMAL_SIZE = "nwNormalSize"
#: Set on a resizable chrome dialog: it changes size only when asked to.
_STRICT_SIZE = "nwStrictSize"
#: Until when (``time.monotonic()``) a size change was asked for.
_RESIZE_ALLOWED_UNTIL = "nwResizeAllowedUntil"
#: How long a resize the user started stays open; each new size during the
#: drag extends it (a drag may pause).
RESIZE_SESSION_S = 3.0


def allow_resize(window: QWidget, seconds: float = RESIZE_SESSION_S) -> None:
    """Let a strictly guarded window (see ``WindowSizeGuard``) take new sizes
    for ``seconds``: the user is resizing it, or the app is."""
    window.setProperty(_RESIZE_ALLOWED_UNTIL, time.monotonic() + seconds)


def resize_window(window: QWidget, size: QSize) -> None:
    """Resize ``window`` from code, also when it is strictly guarded; a card
    overlay's card takes the size instead."""
    overlay = card_overlay(window)
    if overlay is not None:
        overlay.resize_card(size)
        return
    allow_resize(window)
    window.resize(size)


class WindowSizeGuard(QObject):
    """Undoes the size GNOME forces on a frameless window when it opens another.

    GNOME on Wayland, with displays of different scales in its physical
    layout mode, tells a frameless window on the 1x display to become exactly
    twice its size whenever a window opens over it (a dialog, a message box
    or a file chooser). Qt applies a compositor's size even past a window's
    maximum. A floating window may pick its own size, so it goes back at
    once, before anything is painted at the wrong size:

    * a fixed-size window (minimum = maximum, e.g. a chrome dialog) to that
      size, whatever the window system asks;
    * a strictly guarded window (a resizable chrome dialog) to its last size
      whenever it changes size without being asked (``allow_resize``: the
      user dragging its edges, or ``resize_window``), and back to normal when
      it is maximized or made fullscreen (it has no button for either; GNOME
      also auto-maximizes windows that open nearly as large as the screen).
      The doubled size is not always exactly double: one taller than the
      screen comes back clamped;
    * any other window to its last size when it is suddenly exactly twice
      that. A size the user drags or snaps to, maximizing and fullscreen are
      left alone.

    Keeps no state of its own (the window is its Qt parent; the last size is
    a property of the window), so it works even if PySide drops and
    recreates this object's Python wrapper.
    """

    def __init__(self, window: QWidget) -> None:
        super().__init__(window)
        window.installEventFilter(self)

    def eventFilter(self, watched: QObject, event: QEvent) -> bool:  # noqa: N802
        if not isinstance(watched, QWidget) or watched is not self.parent():
            return False
        strict = bool(watched.property(_STRICT_SIZE))
        if (
            strict
            and event.type() == QEvent.Type.WindowStateChange
            and (watched.isMaximized() or watched.isFullScreen())
        ):
            logger.debug("Window system maximized %s; restoring it", type(watched).__name__)
            # Not from inside the state change itself.
            QTimer.singleShot(0, watched, lambda w=watched: self._restore(w))
            return False
        if event.type() == QEvent.Type.Resize and not (
            watched.isMaximized() or watched.isFullScreen() or watched.isMinimized()
        ):
            keep = self._size_to_keep(watched)
            if keep is not None:
                logger.debug(
                    "Window system resized %s to %sx%s; keeping %sx%s",
                    type(watched).__name__,
                    watched.width(),
                    watched.height(),
                    keep.width(),
                    keep.height(),
                )
                watched.resize(keep)
            else:
                watched.setProperty(_NORMAL_SIZE, watched.size())
        return False

    @staticmethod
    def _restore(window: QWidget) -> None:
        window.showNormal()
        last = window.property(_NORMAL_SIZE)
        if isinstance(last, QSize) and not last.isEmpty():
            resize_window(window, last)

    @staticmethod
    def _size_to_keep(window: QWidget) -> QSize | None:
        size = window.size()
        fixed = window.minimumSize()
        if fixed == window.maximumSize():
            return fixed if size != fixed else None
        last = window.property(_NORMAL_SIZE)
        if window.property(_STRICT_SIZE):
            until = window.property(_RESIZE_ALLOWED_UNTIL)
            # A popup opening (list, calendar, menu) is when GNOME doubles a
            # window: never a size the user asked for.
            asked = (
                isinstance(until, float)
                and time.monotonic() <= until
                and QApplication.activePopupWidget() is None
            )
            if asked:
                allow_resize(window)  # a drag goes on as long as sizes come
                return None
            if isinstance(last, QSize) and not last.isEmpty() and size != last:
                return last
            return None
        if isinstance(last, QSize) and not last.isEmpty() and size == last * 2:
            return last
        return None


class _NativeCorners(QObject):
    """Rounds a dialog's corners the native way once its window exists."""

    def __init__(self, dialog: QWidget) -> None:
        super().__init__(dialog)
        dialog.installEventFilter(self)

    def eventFilter(self, watched: QObject, event: QEvent) -> bool:  # noqa: N802
        if event.type() == QEvent.Type.Show and isinstance(watched, QWidget):
            round_native_corners(watched)
            watched.removeEventFilter(self)
        return False


def chrome_size_hint(dialog: QWidget) -> QSize:
    """The size a chrome dialog's content asks for: its card's on Wayland
    (``CardOverlay``; the window itself covers the screen), else the window's."""
    overlay = card_overlay(dialog)
    if overlay is not None:
        return overlay._shell.sizeHint()
    return dialog.sizeHint()


def card_overlay(window: QWidget) -> CardOverlay | None:
    """The card overlay ``window`` is shown as, if any."""
    return window.findChild(CardOverlay, options=Qt.FindChildOption.FindDirectChildrenOnly)


class CardOverlay(QObject):
    """Shows a chrome dialog as a card centred over its parent's screen, where
    windows cannot be placed (Wayland; see the module notes).

    The dialog becomes a transparent window covering that screen (maximized),
    masked to the card, so clicks outside the card reach the windows below
    (a modal dialog still blocks its own application). The card (``shell``)
    keeps the size its content asks for (or, ``resizable``, the size dragged
    from its ``grip``-wide edge band or set with ``resize_window``) and is
    centred, offset by however far its header was dragged.
    """

    def __init__(
        self, dialog: QDialog, shell: QWidget, *, resizable: bool = False, grip: int = GRIP_WIDTH
    ) -> None:
        super().__init__(dialog)
        self._dialog = dialog
        self._shell = shell
        self._resizable = resizable
        self._grip = max(1, grip)
        self._offset = QPoint()
        self._sized = False  # a resizable card's size was set (resize_card)
        self._resize_edges = Qt.Edge(0)
        self._resize_from: tuple[QPoint, QRect] | None = None
        dialog.setAttribute(Qt.WidgetAttribute.WA_TranslucentBackground, True)
        # Its size is the screen's: no size guard (see WindowSizeGuard).
        dialog.setProperty(_GUARDED, True)
        dialog.installEventFilter(self)
        shell.installEventFilter(self)
        if resizable:
            shell.setMouseTracking(True)

    # -- placement ---------------------------------------------------------

    def card_rect(self) -> QRect:
        return self._shell.geometry()

    def move_card(self, delta: QPoint) -> None:
        """Drag the card; it stops where its header would leave the screen."""
        self._offset += delta
        self._offset = self._card_at(self._offset).center() - self._dialog.rect().center()
        self._place()

    def resize_card(self, size: QSize) -> None:
        bounded = size.expandedTo(self._shell.minimumSizeHint()).expandedTo(
            self._shell.minimumSize()
        )
        self._shell.resize(bounded)
        self._sized = True
        self._place()

    def _cover_screen(self) -> None:
        parent = self._dialog.parentWidget()
        screen = parent.window().screen() if parent is not None else None
        screen = (
            screen or QGuiApplication.screenAt(QCursor.pos()) or QGuiApplication.primaryScreen()
        )
        if screen is not None:
            self._dialog.setScreen(screen)
            self._dialog.setGeometry(screen.availableGeometry())
        if not self._resizable:
            self._shell.adjustSize()
        elif not self._sized:
            self._shell.resize(self._shell.sizeHint())
        self._dialog.setWindowState(Qt.WindowState.WindowMaximized)
        self._place()

    def _card_at(self, offset: QPoint) -> QRect:
        """The card centred, moved by ``offset``, kept on the screen (its
        header first, when the card is larger than the screen)."""
        area = self._dialog.rect()
        card = QRect(QPoint(0, 0), self._shell.size())
        card.moveCenter(area.center() + offset)
        card.moveLeft(max(area.left(), min(card.left(), area.right() - card.width() + 1)))
        card.moveTop(max(area.top(), min(card.top(), area.bottom() - card.height() + 1)))
        return card

    def _place(self) -> None:
        # Only a drag changes the offset: the window may still be growing to
        # the screen's size here, and keeping the card inside an older, smaller
        # window must not stick.
        card = self._card_at(self._offset)
        if self._shell.pos() != card.topLeft():
            self._shell.move(card.topLeft())
        self._dialog.setMask(QRegion(card))

    # -- events ------------------------------------------------------------

    def eventFilter(self, watched: QObject, event: QEvent) -> bool:  # noqa: N802
        kind = event.type()
        if watched is self._dialog:
            if kind == QEvent.Type.Show:
                self._cover_screen()
            elif kind == QEvent.Type.Resize:
                self._place()
            return False
        if watched is not self._shell:
            return False
        if kind == QEvent.Type.Resize:
            self._place()
        elif self._resizable and isinstance(event, QMouseEvent):
            return self._edge_resize(event)
        elif kind == QEvent.Type.Leave and self._resize_from is None:
            self._shell.unsetCursor()
        return False

    def _edges_at(self, pos: QPoint) -> Qt.Edge:
        edges = Qt.Edge(0)
        width, height, grip = self._shell.width(), self._shell.height(), self._grip
        if pos.x() < grip:
            edges |= Qt.Edge.LeftEdge
        elif pos.x() >= width - grip:
            edges |= Qt.Edge.RightEdge
        if pos.y() < grip:
            edges |= Qt.Edge.TopEdge
        elif pos.y() >= height - grip:
            edges |= Qt.Edge.BottomEdge
        return edges

    def _edge_resize(self, event: QMouseEvent) -> bool:
        pos = event.position().toPoint()
        kind = event.type()
        if kind == QEvent.Type.MouseButtonPress and event.button() == Qt.MouseButton.LeftButton:
            edges = self._edges_at(pos)
            if not edges:
                return False
            self._resize_edges = edges
            self._resize_from = (event.globalPosition().toPoint(), self.card_rect())
            return True
        if kind == QEvent.Type.MouseMove:
            if self._resize_from is None:
                cursor = EdgeResizer.cursor_for(self._edges_at(pos))
                if cursor is None:
                    self._shell.unsetCursor()
                else:
                    self._shell.setCursor(cursor)
                return False
            start, rect = self._resize_from
            delta = event.globalPosition().toPoint() - start
            self._drag_edges(QRect(rect), delta)
            return True
        if kind == QEvent.Type.MouseButtonRelease and self._resize_from is not None:
            self._resize_from = None
            return True
        return False

    def _drag_edges(self, rect: QRect, delta: QPoint) -> None:
        """The card with its dragged edges moved by ``delta``; the opposite
        edges stay where they are."""
        minimum = self._shell.minimumSizeHint().expandedTo(self._shell.minimumSize())
        edges = self._resize_edges
        if edges & Qt.Edge.LeftEdge:
            rect.setLeft(min(rect.left() + delta.x(), rect.right() - minimum.width() + 1))
        if edges & Qt.Edge.RightEdge:
            rect.setRight(max(rect.right() + delta.x(), rect.left() + minimum.width() - 1))
        if edges & Qt.Edge.TopEdge:
            rect.setTop(min(rect.top() + delta.y(), rect.bottom() - minimum.height() + 1))
        if edges & Qt.Edge.BottomEdge:
            rect.setBottom(max(rect.bottom() + delta.y(), rect.top() + minimum.height() - 1))
        rect = rect.intersected(self._dialog.rect())
        self._shell.resize(rect.size())
        self._offset = rect.center() - self._dialog.rect().center()
        self._place()


_GUARDED = "nwSizeGuarded"


def guard_window_size(window: QWidget) -> WindowSizeGuard | None:
    """Protect ``window`` from GNOME's doubled sizes (see ``WindowSizeGuard``).
    Returns None when it already is."""
    if window.property(_GUARDED):
        return None
    window.setProperty(_GUARDED, True)
    return WindowSizeGuard(window)


def guard_windows_on_focus(app: QGuiApplication) -> None:
    """Guard every window when it first takes focus, so message boxes, file
    dialogs and the other dialogs are covered without each call site."""
    app.focusWindowChanged.connect(guard_window_of)


def guard_window_of(window: QWindow | None) -> None:
    if window is None:
        return
    for widget in QApplication.topLevelWidgets():
        if widget.windowHandle() is window:
            guard_window_size(widget)
            return


def install_dialog_chrome(dialog: QDialog, title: str, *, resizable: bool = False) -> QVBoxLayout:
    """Make ``dialog`` frameless: a header with ``title`` (drag to move,
    round close button) on its own surface; rounded and shadowed where the
    desktop composites. Returns the layout for the dialog's content, which
    should not repeat the heading.

    By default the dialog keeps the size its content asks for (it has no
    resize grips), including a minimum size set before this call, and returns
    to it when the compositor resizes it anyway (``WindowSizeGuard``).
    ``resizable``: the dialog resizes from its edges (the shadow band, or a
    ``GRIP_WIDTH`` band without window effects), down to its content's
    minimum size; give the content stretch so it uses the space."""
    effects = window_effects_supported()
    overlay = not positions_windows()
    dialog.setWindowFlag(Qt.WindowType.FramelessWindowHint, True)
    if effects:
        dialog.setAttribute(Qt.WidgetAttribute.WA_TranslucentBackground, True)
    dialog.setProperty("chromeEffects", effects)
    dialog.setAccessibleName(title)
    style = dialog.style()
    style.unpolish(dialog)
    style.polish(dialog)

    margin = SHADOW_MARGIN if effects else 0
    if resizable:
        # The band around the frame is where it is resized from.
        margin = max(margin, GRIP_WIDTH)
    shell = _ShadowShell(dialog, margin if effects else 0)
    # The dialog's minimum size is its content's: the shell carries it.
    shell.setMinimumSize(dialog.minimumSize())
    dialog.setMinimumSize(0, 0)
    shell_layout = QVBoxLayout(shell)
    shell_layout.setContentsMargins(margin, margin, margin, margin)
    constraint = (
        QLayout.SizeConstraint.SetMinimumSize if resizable else QLayout.SizeConstraint.SetFixedSize
    )
    if overlay:
        # Wayland: a transparent window over the screen, the card centred.
        shell_layout.setSizeConstraint(constraint)
        CardOverlay(dialog, shell, resizable=resizable, grip=margin)
    else:
        outer = QVBoxLayout(dialog)
        outer.setContentsMargins(0, 0, 0, 0)
        outer.setSpacing(0)
        outer.setSizeConstraint(constraint)
        guard_window_size(dialog)
        if not effects:
            # Windows 11: native rounded corners instead of a translucent window.
            _NativeCorners(dialog)
        outer.addWidget(shell)
        if resizable:
            EdgeResizer(dialog, surface=shell, grip=margin)
            dialog.setProperty(_STRICT_SIZE, True)
    frame = QFrame(shell)
    frame.setObjectName("chromeFrame")
    frame.setProperty("rounded", effects)
    shell_layout.addWidget(frame)
    inner = QVBoxLayout(frame)
    inner.setContentsMargins(1, 1, 1, 1)
    inner.setSpacing(0)
    inner.addWidget(ChromeTitleBar(title, frame, variant="dialog"))
    body = QWidget(frame)
    inner.addWidget(body, stretch=1)
    content = QVBoxLayout(body)
    content.setContentsMargins(20, 2, 20, 16)
    content.setSpacing(10)
    return content
