"""The main window's title strip while its panels are hidden.

"Hide panels" (title bar, ``Ctrl+Shift+H``) leaves only the wall under this
strip, laid out like an extra window's: the window's title, the status
readings of the bottom bar (``set_status``), page navigation, "New window",
"Show panels" to bring everything back, and the window buttons.
"""

from __future__ import annotations

from PySide6.QtCore import QEvent, QObject, Qt, Signal
from PySide6.QtWidgets import QFrame, QHBoxLayout, QLabel, QSizePolicy, QToolButton, QWidget

from neurowatch.theme import apply_dialog_theme
from neurowatch.widgets.window_chrome import ChromeTitleBar


class CompactTitleBar(ChromeTitleBar):
    """Slim title strip of the main window in compact view."""

    previous_page = Signal()
    next_page = Signal()
    new_window = Signal()
    show_panels = Signal()

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__("NeuroWatch", parent, minimize=True, maximize=True)
        controls = QWidget(self)
        controls.setObjectName("compactControls")
        # The main window's root style gives every plain widget its own
        # background; the controls sit on the strip's instead.
        controls.setStyleSheet(
            "QWidget#compactControls, QWidget#compactPaging, QWidget#compactStatus"
            " { background: transparent; }"
        )
        layout = QHBoxLayout(controls)
        layout.setContentsMargins(0, 0, 8, 0)
        layout.setSpacing(2)

        # The bottom status bar's readings, moved here while the panels are hidden.
        self._status = QWidget(controls)
        self._status.setObjectName("compactStatus")
        status = QHBoxLayout(self._status)
        status.setContentsMargins(0, 0, 0, 0)
        status.setSpacing(0)
        self._status_layout = status
        layout.addWidget(self._status)
        self._status_sep = self._separator()
        layout.addWidget(self._status_sep)
        self._status.hide()
        self._status_sep.hide()

        self._paging = QWidget(controls)
        self._paging.setObjectName("compactPaging")
        paging = QHBoxLayout(self._paging)
        paging.setContentsMargins(0, 0, 0, 0)
        paging.setSpacing(2)
        self.previous = self._control("\u2039", "Previous page (PgUp)")
        self.indicator = QLabel()
        self.indicator.setObjectName("pageIndicator")
        self.next = self._control("\u203a", "Next page (PgDn)")
        for widget in (self.previous, self.indicator, self.next):
            paging.addWidget(widget)
        paging.addWidget(self._separator())
        layout.addWidget(self._paging)

        self.new_window_button = self._control("⧉ New window", "Move a page to its own window")
        layout.addWidget(self.new_window_button)
        self.show_panels_button = self._control(
            "Show panels", "Show the menu bar, toolbar and camera list (Ctrl+Shift+H)"
        )
        layout.addWidget(self.show_panels_button)
        layout.addWidget(self._separator())
        self.add_controls(controls)

        self.previous.clicked.connect(self.previous_page)
        self.next.clicked.connect(self.next_page)
        self.new_window_button.clicked.connect(self.new_window)
        self.show_panels_button.clicked.connect(self.show_panels)
        apply_dialog_theme(self, "dark")  # like an extra window's strip
        # The title gives way to the controls: it is shortened with "…".
        self._full_title = ""
        self._title.setSizePolicy(QSizePolicy.Policy.Ignored, QSizePolicy.Policy.Preferred)
        self._title.setMinimumWidth(80)
        self._title.installEventFilter(self)

    def _control(self, text: str, tip: str) -> QToolButton:
        button = QToolButton(self)
        button.setObjectName("pageControl")
        button.setText(text)
        button.setToolTip(tip)
        button.setAccessibleName(tip)
        button.setFocusPolicy(Qt.FocusPolicy.TabFocus)
        button.setCursor(Qt.CursorShape.PointingHandCursor)
        return button

    def _separator(self) -> QFrame:
        line = QFrame(self)
        line.setObjectName("chromeSeparator")
        return line

    def set_title(self, title: str) -> None:
        self._full_title = title
        self._title.setToolTip(title)
        self._elide_title()

    def _elide_title(self) -> None:
        width = max(0, self._title.width())
        self._title.setText(
            self._title.fontMetrics().elidedText(
                self._full_title, Qt.TextElideMode.ElideRight, width
            )
        )

    def eventFilter(self, watched: QObject, event: QEvent) -> bool:  # noqa: N802
        if watched is self._title and event.type() == QEvent.Type.Resize:
            self._elide_title()
        return False

    def set_status(self, widget: QWidget | None) -> None:
        """Show ``widget`` (the status bar) in the strip; None when it has
        gone back to the bottom of the window."""
        if widget is not None:
            self._status_layout.addWidget(widget)
        self._status.setVisible(widget is not None)
        self._status_sep.setVisible(widget is not None)

    def set_state(
        self, *, title: str, page: int, count: int, solo: bool, can_open_window: bool
    ) -> None:
        """``page`` is 0-based; paging shows only with two or more pages."""
        self.set_title(title)
        self._paging.setVisible(count > 1)
        self.indicator.setText(f"Page {page + 1} / {count}")
        self.indicator.setAccessibleName(f"Page {page + 1} of {count}")
        for button in (self.previous, self.next):
            button.setEnabled(not solo)
        self.new_window_button.setEnabled(can_open_window)

    # The main window leaves fullscreen before it minimizes or maximizes, as
    # its full title bar does, so a restored window can be moved again.
    def _leave_fullscreen(self) -> bool:
        win = self.window()
        if not win.isFullScreen():
            return False
        win.showNormal()
        exit_fullscreen = getattr(win, "_exit_fullscreen_mode", None)
        if callable(exit_fullscreen):
            exit_fullscreen()
        return True

    def _minimize(self) -> None:
        self._leave_fullscreen()
        self.window().showMinimized()

    def _toggle_maximize(self) -> None:
        if not self._leave_fullscreen():
            super()._toggle_maximize()
