"""Custom frameless title bar — logo, tab navigation, window controls."""

from __future__ import annotations

from collections.abc import Callable

from PySide6.QtCore import QPoint, Qt, Signal
from PySide6.QtGui import QFont, QMouseEvent
from PySide6.QtWidgets import (
    QHBoxLayout,
    QLabel,
    QMenu,
    QPushButton,
    QSizePolicy,
    QWidget,
)

from neurowatch.theme import themed_menu

_HEIGHT = 38
_BG = "#0e0e18"
_ACCENT = "#4e9af1"
_TEXT = "#c8c8dc"
_TAB_ACTIVE_BG = "#1a2440"
_TAB_HOVER_BG = "#1a2233"

# No state changes a border width, so focus or activation only changes
# colours and never shifts a label or resizes a button. Keyboard focus (mouse
# clicks do not take focus) shows as a tint and an underlined label.
_TAB_STYLE = """
QPushButton {{
    background: transparent;
    color: {fg};
    border: none;
    border-bottom: 2px solid {border};
    padding: 0 18px;
    font-size: 13px;
}}
QPushButton:hover {{ background: {hover}; }}
QPushButton:focus {{ background: {hover}; text-decoration: underline; }}
"""
_WIN_BTN = """
QPushButton {
    background: transparent; color: #a0a0b8;
    border: none; font-size: 13px;
}
QPushButton:hover, QPushButton:focus { background: #2a2a40; color: white; }
QPushButton#close:hover, QPushButton#close:focus { background: #c0392b; color: white; }
"""
_PANELS_BTN = """
QPushButton {
    background: transparent; color: #a0a0b8;
    border: none; font-size: 12px; padding: 0 12px;
}
QPushButton:hover, QPushButton:focus { background: #2a2a40; color: white; }
"""

TABS = ("live", "playback", "diagnostics", "settings")


class TitleBar(QWidget):
    """Draggable title bar with tab navigation and window controls."""

    tab_changed = Signal(str)  # "live" | "playback" | "settings"
    export_config_requested = Signal()
    export_video_requested = Signal()
    import_config_requested = Signal()
    settings_requested = Signal()
    about_requested = Signal()
    # "Hide panels": show only the wall under a slim strip (compact view).
    hide_panels_requested = Signal()

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setFixedHeight(_HEIGHT)
        self.setAttribute(Qt.WidgetAttribute.WA_StyledBackground)
        self.setStyleSheet(f"background-color: {_BG};")

        self._drag_pos: QPoint | None = None
        self._native_drag_active = False
        self._current_tab = "live"
        # Extra ≡ menu entries (text, callback) added by extensions.
        self._extra_actions: list[tuple[str, Callable[[], object]]] = []

        layout = QHBoxLayout(self)
        layout.setContentsMargins(12, 0, 0, 0)
        layout.setSpacing(0)

        self._menu_btn = QPushButton("≡")
        self._menu_btn.setFixedSize(32, _HEIGHT)
        self._menu_btn.setStyleSheet(_WIN_BTN)
        # Keyboard focus only: a mouse click must not leave a focus box behind.
        self._menu_btn.setFocusPolicy(Qt.FocusPolicy.TabFocus)
        self._menu_btn.setToolTip("File Menu")
        self._menu_btn.setAccessibleName("Application menu")
        self._menu_btn.clicked.connect(self._show_file_menu)
        layout.addWidget(self._menu_btn)

        # Logo
        logo = QLabel("⬡ NeuroWatch")
        logo.setFont(QFont("Sans", 12, QFont.Weight.Bold))
        logo.setStyleSheet(f"color: {_ACCENT}; padding-right: 20px;")
        layout.addWidget(logo)

        self._site_label = QLabel("My Site")
        self._site_label.setStyleSheet(f"color: {_TEXT}; font-size: 11px; padding-left: 8px;")
        layout.addWidget(self._site_label)

        # Tab buttons
        self._tab_btns: dict[str, QPushButton] = {}
        for key in TABS:
            btn = QPushButton(key.capitalize())
            btn.setFixedHeight(_HEIGHT)
            btn.setCursor(Qt.CursorShape.PointingHandCursor)
            btn.setAccessibleName(f"{key.capitalize()} tab")
            btn.setFocusPolicy(Qt.FocusPolicy.TabFocus)
            btn.clicked.connect(lambda _c, k=key: self._activate(k))
            layout.addWidget(btn)
            self._tab_btns[key] = btn

        self._update_tab_styles()

        # Flexible spacer
        spacer = QWidget()
        spacer.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Preferred)
        layout.addWidget(spacer)

        self._hide_panels_btn = QPushButton("Hide panels")
        self._hide_panels_btn.setFixedHeight(_HEIGHT)
        self._hide_panels_btn.setStyleSheet(_PANELS_BTN)
        self._hide_panels_btn.setFocusPolicy(Qt.FocusPolicy.TabFocus)
        self._hide_panels_btn.setCursor(Qt.CursorShape.PointingHandCursor)
        self._hide_panels_btn.setToolTip(
            "Hide this bar, the toolbar and the camera list (Ctrl+Shift+H)"
        )
        self._hide_panels_btn.setAccessibleName("Hide panels")
        self._hide_panels_btn.clicked.connect(self.hide_panels_requested)
        layout.addWidget(self._hide_panels_btn)

        # Window controls
        layout.setSpacing(0)
        for name, symbol, obj_name in [
            ("min", "─", "min"),
            ("max", "□", "max"),
            ("close", "✕", "close"),
        ]:
            btn = QPushButton(symbol)
            btn.setObjectName(obj_name)
            btn.setFixedSize(42, _HEIGHT)
            btn.setStyleSheet(_WIN_BTN)
            btn.setFocusPolicy(Qt.FocusPolicy.TabFocus)
            btn.setCursor(Qt.CursorShape.PointingHandCursor)
            if name == "min":
                btn.setToolTip("Minimise")
            elif name == "max":
                btn.setToolTip("Maximise / Restore")
            else:
                btn.setToolTip("Close")
            btn.setAccessibleName(btn.toolTip())
            layout.addWidget(btn)
            if name == "min":
                btn.clicked.connect(self._minimize)
            elif name == "max":
                btn.clicked.connect(self._toggle_maximize)
            else:
                btn.clicked.connect(lambda: self.window().close())

    # ------------------------------------------------------------------
    # Drag to move
    # ------------------------------------------------------------------

    def mousePressEvent(self, event: QMouseEvent) -> None:
        if event.button() == Qt.MouseButton.LeftButton:
            self._drag_pos = event.globalPosition().toPoint()
            self._native_drag_active = self._start_system_move()
            event.accept()
            return
        super().mousePressEvent(event)

    def mouseMoveEvent(self, event: QMouseEvent) -> None:
        if self._drag_pos and (event.buttons() & Qt.MouseButton.LeftButton):
            if self._native_drag_active:
                event.accept()
                return
            win = self.window()
            if win.isFullScreen() or win.isMaximized():
                win.showNormal()
                exit_fullscreen = getattr(win, "_exit_fullscreen_mode", None)
                if callable(exit_fullscreen):
                    exit_fullscreen()
                self._drag_pos = event.globalPosition().toPoint()
                # Some compositors only permit a native move after the window
                # has left its fullscreen/maximized state.
                self._native_drag_active = self._start_system_move()
                event.accept()
                return
            delta = event.globalPosition().toPoint() - self._drag_pos
            win.move(win.pos() + delta)
            self._drag_pos = event.globalPosition().toPoint()
            event.accept()
            return
        super().mouseMoveEvent(event)

    def mouseReleaseEvent(self, event: QMouseEvent) -> None:
        self._drag_pos = None
        self._native_drag_active = False
        super().mouseReleaseEvent(event)

    def mouseDoubleClickEvent(self, event: QMouseEvent) -> None:
        self._toggle_maximize()

    # ------------------------------------------------------------------

    def _start_system_move(self) -> bool:
        """Ask the desktop compositor to move this frameless window."""
        win = self.window()
        if win.isFullScreen():
            return False
        handle = win.windowHandle()
        if handle is None:
            return False
        try:
            return bool(handle.startSystemMove())
        except (AttributeError, RuntimeError):
            return False

    def _minimize(self) -> None:
        win = self.window()
        if win.isFullScreen():
            # A minimized fullscreen window normally restores back to
            # fullscreen, where desktop window managers deliberately disable
            # moving. Restore first so taskbar activation returns a movable
            # normal window.
            win.showNormal()
            exit_fullscreen = getattr(win, "_exit_fullscreen_mode", None)
            if callable(exit_fullscreen):
                exit_fullscreen()
        win.showMinimized()

    def _toggle_maximize(self) -> None:
        win = self.window()
        if win.isFullScreen():
            win.showNormal()
            exit_fullscreen = getattr(win, "_exit_fullscreen_mode", None)
            if callable(exit_fullscreen):
                exit_fullscreen()
        elif win.isMaximized():
            win.showNormal()
        else:
            win.showMaximized()

    def _activate(self, key: str) -> None:
        # "settings" opens a dialog — don't change the active tab highlight.
        if key == "settings":
            self.tab_changed.emit(key)
            return
        if key == self._current_tab:
            return
        self._current_tab = key
        self._update_tab_styles()
        self.tab_changed.emit(key)

    def show_tab(self, key: str) -> None:
        """Switch to tab ``key`` as if it were clicked."""
        self._activate(key)

    def set_site_name(self, name: str) -> None:
        self._site_label.setText(name or "My Site")

    def add_menu_action(self, text: str, callback: Callable[[], object]) -> None:
        """Add an entry to the ≡ menu, after Settings (extensions)."""
        self._extra_actions.append((text, callback))

    def _show_file_menu(self) -> None:
        menu = self._build_file_menu()
        menu.exec(self._menu_btn.mapToGlobal(self._menu_btn.rect().bottomLeft()))
        menu.deleteLater()

    def _build_file_menu(self) -> QMenu:
        menu = themed_menu(self)
        menu.addAction("Export Config…", lambda: self.export_config_requested.emit())
        menu.addAction("Export Video…", lambda: self.export_video_requested.emit())
        menu.addAction("Import Config…", lambda: self.import_config_requested.emit())
        menu.addSeparator()
        menu.addAction("Settings", lambda: self.settings_requested.emit())
        for text, callback in self._extra_actions:
            menu.addAction(text, callback)
        menu.addSeparator()
        menu.addAction("About Neurowatch", lambda: self.about_requested.emit())
        return menu

    def _update_tab_styles(self) -> None:
        for key, btn in self._tab_btns.items():
            active = key == self._current_tab
            btn.setStyleSheet(
                _TAB_STYLE.format(
                    fg=_ACCENT if active else _TEXT,
                    border=_ACCENT if active else "transparent",
                    hover=_TAB_HOVER_BG,
                )
            )
