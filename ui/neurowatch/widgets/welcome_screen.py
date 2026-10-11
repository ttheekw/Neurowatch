"""Startup welcome screen shown while the main window is constructed.

Main-window construction is synchronous, so the screen does not animate on a
timer. ``present`` gives the splash its first exposure and paint before
construction blocks; each ``set_stage`` call reports a real startup step,
repaints synchronously and pumps the Qt event loop once.
"""

from __future__ import annotations

import logging
import math
from enum import Enum

from PySide6.QtCore import QEvent, QObject, QPointF, Qt, Signal
from PySide6.QtGui import (
    QColor,
    QCursor,
    QGuiApplication,
    QPainter,
    QPainterPath,
    QPaintEvent,
    QPen,
    QRegion,
    QResizeEvent,
)
from PySide6.QtWidgets import (
    QApplication,
    QFrame,
    QHBoxLayout,
    QLabel,
    QProgressBar,
    QVBoxLayout,
    QWidget,
)

from neurowatch import build_info
from neurowatch.theme import COLORS, METRICS

logger = logging.getLogger(__name__)


def _can_position_windows() -> bool:
    """Wayland compositors ignore client window positioning (move())."""
    return not QGuiApplication.platformName().startswith("wayland")


class StartupStage(Enum):
    """Ordered startup steps; ``label`` is the user-facing text."""

    STARTING = "Starting NeuroWatch"
    CONNECTING = "Connecting to backend"
    PREPARING_WORKSPACE = "Preparing workspace"
    LOADING_CAMERAS = "Loading cameras"
    OPENING = "Opening control room"

    @property
    def label(self) -> str:
        return str(self.value)


class _BrandMark(QWidget):
    """Hexagonal NeuroWatch mark with an accent lens."""

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setFixedSize(56, 56)

    def paintEvent(self, event: QPaintEvent) -> None:
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing)
        center = QPointF(self.width() / 2, self.height() / 2)
        radius = min(self.width(), self.height()) / 2 - 2
        hexagon = QPainterPath()
        for i in range(6):
            angle = math.pi / 3 * i - math.pi / 2
            point = QPointF(
                center.x() + radius * math.cos(angle), center.y() + radius * math.sin(angle)
            )
            if i == 0:
                hexagon.moveTo(point)
            else:
                hexagon.lineTo(point)
        hexagon.closeSubpath()
        painter.fillPath(hexagon, QColor(COLORS.accent_subtle))
        painter.setPen(QPen(QColor(COLORS.accent), 2))
        painter.drawPath(hexagon)
        painter.setPen(Qt.PenStyle.NoPen)
        painter.setBrush(QColor(COLORS.focus_ring))
        painter.drawEllipse(center, radius * 0.32, radius * 0.32)
        painter.setBrush(QColor(COLORS.window))
        painter.drawEllipse(center, radius * 0.13, radius * 0.13)


def _stylesheet() -> str:
    c, m = COLORS, METRICS
    return f"""
QWidget#welcomeScreen {{
    background: transparent;
}}
QFrame#welcomeCard {{
    background-color: {c.surface};
    border: 1px solid {c.border};
    border-top: 2px solid {c.accent};
}}
QLabel {{
    background: transparent;
    color: {c.text};
}}
QLabel#welcomeTitle {{
    font-size: 26px;
    font-weight: 600;
    letter-spacing: 0.5px;
}}
QLabel#welcomeTagline {{
    color: {c.text_muted};
    font-size: {m.font_size}px;
}}
QLabel#welcomeStage {{
    font-size: {m.font_size + 1}px;
    font-weight: 600;
}}
QLabel#welcomeDetail, QLabel#welcomeStep, QLabel#welcomeVersion {{
    color: {c.text_muted};
    font-size: {m.font_size_small}px;
}}
QProgressBar#welcomeProgress {{
    background-color: {c.surface_sunken};
    border: none;
    border-radius: 2px;
}}
QProgressBar#welcomeProgress::chunk {{
    background-color: {c.accent};
    border-radius: 2px;
}}
"""


class WelcomeScreen(QWidget):
    """Branded startup surface reporting real startup stages.

    Lifecycle: ``present()`` → ``set_stage()`` per step → ``finish(main_window)``
    (closes once that window is shown) or ``dismiss()`` on failure.
    """

    dismissed = Signal()

    def __init__(self, version: str = "", parent: QWidget | None = None) -> None:
        super().__init__(parent, Qt.WindowType.SplashScreen | Qt.WindowType.FramelessWindowHint)
        self.setObjectName("welcomeScreen")
        self.setWindowTitle("NeuroWatch — Starting")
        # Wayland ignores move(), so a small splash lands wherever the
        # compositor puts it (top-left on GNOME). There the splash is a
        # transparent maximized window with the card centred; see present().
        self.setAttribute(Qt.WidgetAttribute.WA_TranslucentBackground, True)
        self.setStyleSheet(_stylesheet())

        self._current_stage: StartupStage | None = None
        self._dismissed = False
        self._pending_window: QWidget | None = None

        outer = QVBoxLayout(self)
        outer.setContentsMargins(0, 0, 0, 0)
        card = QFrame(self)
        card.setObjectName("welcomeCard")
        card.setAttribute(Qt.WidgetAttribute.WA_StyledBackground, True)
        card.setFixedSize(520, 300)
        outer.addWidget(card, alignment=Qt.AlignmentFlag.AlignCenter)
        self._card = card

        layout = QVBoxLayout(card)
        layout.setContentsMargins(36, 32, 36, 24)
        layout.setSpacing(0)

        brand = QHBoxLayout()
        brand.setSpacing(16)
        brand.addWidget(_BrandMark(card), alignment=Qt.AlignmentFlag.AlignVCenter)
        names = QVBoxLayout()
        names.setSpacing(2)
        self._title = QLabel("NeuroWatch", card)
        self._title.setObjectName("welcomeTitle")
        names.addWidget(self._title)
        tagline = QLabel("Network Video Recorder · Control Room", card)
        tagline.setObjectName("welcomeTagline")
        names.addWidget(tagline)
        brand.addLayout(names)
        brand.addStretch(1)
        layout.addLayout(brand)

        layout.addStretch(1)

        stage_row = QHBoxLayout()
        self._stage = QLabel("", card)
        self._stage.setObjectName("welcomeStage")
        self._stage.setAccessibleName("Startup stage")
        stage_row.addWidget(self._stage, stretch=1)
        self._step = QLabel("", card)
        self._step.setObjectName("welcomeStep")
        stage_row.addWidget(self._step, alignment=Qt.AlignmentFlag.AlignRight)
        layout.addLayout(stage_row)
        layout.addSpacing(10)

        self._progress = QProgressBar(card)
        self._progress.setObjectName("welcomeProgress")
        self._progress.setRange(0, len(StartupStage))
        self._progress.setValue(0)
        self._progress.setTextVisible(False)
        self._progress.setFixedHeight(4)
        self._progress.setAccessibleName("Startup progress")
        layout.addWidget(self._progress)
        layout.addSpacing(8)

        self._detail = QLabel("", card)
        self._detail.setObjectName("welcomeDetail")
        self._detail.setMinimumHeight(16)
        layout.addWidget(self._detail)
        layout.addSpacing(14)

        label = version or build_info.build_identity().label
        self._version = QLabel(f"Version {label}", card)
        self._version.setTextFormat(Qt.TextFormat.PlainText)
        self._version.setObjectName("welcomeVersion")
        layout.addWidget(self._version, alignment=Qt.AlignmentFlag.AlignRight)

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    @property
    def current_stage(self) -> StartupStage | None:
        return self._current_stage

    @property
    def is_dismissed(self) -> bool:
        return self._dismissed

    def title_text(self) -> str:
        return self._title.text()

    def version_text(self) -> str:
        return self._version.text()

    def stage_text(self) -> str:
        return self._stage.text()

    def detail_text(self) -> str:
        return self._detail.text()

    def step_text(self) -> str:
        return self._step.text()

    def progress(self) -> tuple[int, int]:
        return self._progress.value(), self._progress.maximum()

    def resizeEvent(self, event: QResizeEvent) -> None:
        super().resizeEvent(event)
        # Clicks outside the card fall through to the desktop underneath.
        self.setMask(QRegion(self._card.geometry()))

    def present(self) -> None:
        """Centre the card on the active screen, expose it, and paint it now.

        Never fullscreen: GNOME fills behind fullscreen windows with black.
        Where move() works (X11) the window is just the card, moved to the
        centre. On Wayland it is a transparent maximized window, which the
        compositor sizes to the work area, and the layout centres the card.
        """
        screen = QGuiApplication.screenAt(QCursor.pos()) or QGuiApplication.primaryScreen()
        if screen is not None:
            self.setScreen(screen)
        if _can_position_windows():
            self.resize(self._card.size())
            if screen is not None:
                frame = self.frameGeometry()
                frame.moveCenter(screen.availableGeometry().center())
                self.move(frame.topLeft())
            self.show()
        else:
            if screen is not None:
                self.setGeometry(screen.availableGeometry())
            self.showMaximized()
        QApplication.processEvents()
        self.repaint()

    def set_stage(self, stage: StartupStage, detail: str = "") -> None:
        """Show ``stage``, repaint synchronously, then pump pending events."""
        if self._dismissed:
            return
        self._current_stage = stage
        index = list(StartupStage).index(stage) + 1
        total = len(StartupStage)
        self._stage.setText(stage.label)
        self._detail.setText(detail)
        self._step.setText(f"Step {index} of {total}")
        self._progress.setValue(index)
        logger.debug("Startup stage %d/%d: %s %s", index, total, stage.label, detail)
        self.repaint()
        QApplication.processEvents()

    def finish(self, window: QWidget) -> None:
        """Dismiss once ``window`` is visible (immediately if it already is)."""
        if self._dismissed:
            return
        if window.isVisible():
            self.dismiss()
            return
        self._pending_window = window
        window.installEventFilter(self)

    def dismiss(self) -> None:
        """Close the screen; safe to call more than once."""
        if self._dismissed:
            return
        self._dismissed = True
        if self._pending_window is not None:
            self._pending_window.removeEventFilter(self)
            self._pending_window = None
        self.close()
        self.dismissed.emit()

    def eventFilter(self, watched: QObject, event: QEvent) -> bool:  # noqa: N802
        if watched is self._pending_window and event.type() == QEvent.Type.Show:
            self.dismiss()
        return super().eventFilter(watched, event)
