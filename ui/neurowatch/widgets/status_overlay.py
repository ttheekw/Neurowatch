"""The status readings over the video wall while the title strip is hidden.

In fullscreen with the panels hidden (compact view) the title strip, which
carries the status bar's readings, hides after a few seconds. With
``ui.fullscreen_status`` on, ``MainWindow`` moves the status bar into this
overlay meanwhile: the bar as it looks at the bottom of the window, over the
top edge of the wall, on its own background colour at
``ui.fullscreen_status_opacity`` percent. Mouse events pass through to the
tiles underneath.
"""

from __future__ import annotations

from PySide6.QtCore import QEvent, QObject, Qt
from PySide6.QtWidgets import QHBoxLayout, QWidget

from neurowatch.widgets.status_bar_widget import BAR_RGB, StatusBarWidget

DEFAULT_OPACITY = 50  # percent
#: The bar's separator line (under the bar here: it sits at the top).
_SEP_RGB = (0x1A, 0x1A, 0x30)


def overlay_style(opacity: int) -> str:
    """The overlay's background: the bar's colour at ``opacity`` percent."""
    alpha = round(max(0, min(100, opacity)) * 255 / 100)
    r, g, b = BAR_RGB
    sr, sg, sb = _SEP_RGB
    return (
        f"QWidget#statusOverlay {{ background-color: rgba({r}, {g}, {b}, {alpha});"
        f" border-bottom: 1px solid rgba({sr}, {sg}, {sb}, {alpha}); }}"
    )


class StatusOverlay(QWidget):
    """Holds a status widget over the top edge of ``wall`` (see ``hold``)."""

    def __init__(self, wall: QWidget) -> None:
        super().__init__(wall.parentWidget())
        self.setObjectName("statusOverlay")
        self._wall = wall
        self._held: StatusBarWidget | None = None
        self._opacity = -1
        self.setAttribute(Qt.WidgetAttribute.WA_TransparentForMouseEvents)
        self.setAttribute(Qt.WidgetAttribute.WA_StyledBackground)
        self.set_opacity(DEFAULT_OPACITY)
        layout = QHBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(0)
        self._layout = layout
        wall.installEventFilter(self)
        self.hide()

    @property
    def holding(self) -> bool:
        return self._held is not None

    @property
    def opacity(self) -> int:
        return self._opacity

    def set_opacity(self, opacity: int) -> None:
        """Background opacity in percent; the readings stay fully opaque."""
        opacity = max(0, min(100, int(opacity)))
        if opacity != self._opacity:
            self._opacity = opacity
            self.setStyleSheet(overlay_style(opacity))

    def hold(self, status: StatusBarWidget) -> None:
        """Show ``status`` over the wall, laid out as at the bottom of the
        window, until ``release``."""
        if self._held is not status:
            self._layout.addWidget(status)
            self._held = status
        status.set_overlay()
        status.show()
        self._place()
        self.show()
        self.raise_()

    def release(self) -> None:
        """Hide; the caller puts the status widget back where it belongs."""
        self._held = None
        self.hide()

    def eventFilter(self, watched: QObject, event: QEvent) -> bool:  # noqa: N802
        if watched is self._wall and event.type() in (QEvent.Type.Resize, QEvent.Type.Move):
            self._place()
        return False

    def _place(self) -> None:
        wall = self._wall.geometry()
        self.setGeometry(wall.x(), wall.y(), wall.width(), self.sizeHint().height())
