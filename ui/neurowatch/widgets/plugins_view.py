"""The Plugins tab: analytics plugins, the cameras each one analyses, and
for each camera what the plugin sees and finds.

Left, the plugins with their cameras under them (a dot for each one's
state). Right, the selection:

* a plugin: its connection, its cameras and today's events; On, Remove,
  Bind Camera…;
* a camera: **Live** (the camera's live picture with every drawing the
  plugin sends, each layer switchable: what the plugin sees), **Output**
  (what it found: counts and recent events, double-click to play one) and
  **Settings** (which video it gets, what shows on the live wall, and the
  plugin's own settings).

Changes apply at once (``plugins.set``). Recorder calls run on worker
threads; the status refreshes every 3 s while the tab shows. The live
picture decodes only while the Live tab shows (``desktop_demand``).
"""

from __future__ import annotations

import dataclasses
import functools
import hashlib
import logging
import time
from collections.abc import Callable
from datetime import datetime
from typing import Any

from PySide6.QtCore import QSize, Qt, QTimer, Signal
from PySide6.QtGui import QColor, QFont, QIcon, QPainter, QPixmap
from PySide6.QtWidgets import (
    QCheckBox,
    QDialog,
    QFrame,
    QGridLayout,
    QHBoxLayout,
    QLabel,
    QListWidget,
    QListWidgetItem,
    QPushButton,
    QScrollArea,
    QSizePolicy,
    QStackedWidget,
    QTabWidget,
    QTreeWidget,
    QTreeWidgetItem,
    QVBoxLayout,
    QWidget,
)

from neurowatch.dialogs.message import ask
from neurowatch.dialogs.plugin_dialogs import (
    AddPluginDialog,
    BindingDialog,
    BindingEditor,
    CredentialDialog,
    Job,
    PluginsRpc,
    Snapshot,
    client_rpc,
    load,
)
from neurowatch.models.analytics import (
    CameraPlugin,
    Drawing,
    LayerChoice,
    MarkingHold,
    markings_reported,
)
from neurowatch.models.camera import CameraConfig
from neurowatch.models.events import Event, clock_text, duration_text, label
from neurowatch.models.plugins import (
    PluginBinding,
    PluginConnectionNotice,
    PluginsConfig,
    PluginServer,
    binding_text,
    connection_notices,
    parse_status,
    server_text,
    without_builtin_motion,
)
from neurowatch.theme import (
    mono_font,
    plugins_stylesheet,
    set_button_variant,
    set_label_role,
    set_status_tone,
)
from neurowatch.widgets.analytics_paint import marker_pixmap
from neurowatch.widgets.form_parts import settings_card

logger = logging.getLogger(__name__)

_POLL_MS = 3000
#: How long a "connected" banner stays.
_NOTICE_MS = 6000
_NOTICE_MAX = 144
_OUTPUT_DAYS = 7
_RECENT = 50
_TONE_DOT = {
    "success": QColor(60, 207, 110),
    "info": QColor(77, 163, 255),
    "warning": QColor(255, 176, 32),
    "error": QColor(255, 90, 90),
    "muted": QColor(130, 130, 145),
}


def _dot(tone: str) -> QIcon:
    picture = QPixmap(12, 12)
    picture.fill(Qt.GlobalColor.transparent)
    painter = QPainter(picture)
    painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
    painter.setPen(Qt.PenStyle.NoPen)
    painter.setBrush(_TONE_DOT.get(tone, _TONE_DOT["muted"]))
    painter.drawEllipse(2, 2, 8, 8)
    painter.end()
    return QIcon(picture)


def _hexagon(size: int, dpr: float) -> QPixmap:
    """The wall's plugin marker, ``size`` logical pixels square."""
    scale = max(1.0, dpr)
    picture = marker_pixmap(size, dpr, True).scaled(
        round(size * scale),
        round(size * scale),
        Qt.AspectRatioMode.KeepAspectRatio,
        Qt.TransformationMode.SmoothTransformation,
    )
    picture.setDevicePixelRatio(scale)
    return picture


def _events_today(count: int) -> str:
    if count == 0:
        return "No events today"
    return f"{count} event{'s' if count != 1 else ''} today"


def _midnight_ms(now: float) -> int:
    today = datetime.fromtimestamp(now).replace(hour=0, minute=0, second=0, microsecond=0)
    return int(today.timestamp() * 1000)


def output_counts(events: list[Event], now: float) -> tuple[int, int, int]:
    """Events in the last hour, today (local) and the last 7 days."""
    now_ms = int(now * 1000)
    hour = sum(1 for e in events if e.start_ms >= now_ms - 3_600_000)
    today = sum(1 for e in events if e.start_ms >= _midnight_ms(now))
    week = sum(1 for e in events if e.start_ms >= now_ms - _OUTPUT_DAYS * 86_400_000)
    return hour, today, week


class PluginsView(QWidget):
    """Plugins and their cameras. ``set_cameras`` gives the cameras (for the
    live picture); the window passes the analytics feed on with
    ``set_analytics`` and ``set_drawings``."""

    figures_choice_changed = Signal(bool)
    play_requested = Signal(object)  # Event
    view_demand_changed = Signal()

    def __init__(
        self,
        endpoint: str | None,
        *,
        rpc: PluginsRpc | None = None,
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        self.setObjectName("pluginsView")
        self._endpoint = endpoint
        self._rpc = rpc or client_rpc(endpoint)
        self._snapshot: Snapshot | None = None
        self._job: Job | None = None
        self._poll_job: Job | None = None
        self._output_job: Job | None = None
        #: (plugin id, camera id or None): what the right side shows.
        self._selection: tuple[str, str | None] | None = None
        self._camera_configs: list[CameraConfig] = []
        self._analytics: dict[str, list[CameraPlugin]] = {}
        self._drawings: dict[str, list[Drawing]] = {}
        self._layers = LayerChoice()
        self._marking_hold = MarkingHold()
        #: A camera's counts choice, one with its tile's button (``MainWindow``).
        self.counts_choice: Callable[[str], bool] = lambda _camera_id: False
        self._editor: BindingEditor | None = None
        self._live_ids: list[str] = []
        self._status_labels: list[tuple[QLabel, Callable[[], tuple[str, str]]]] = []
        #: The plugin page's camera rows: their status dots and today's counts.
        self._row_dots: list[tuple[QLabel, Callable[[], tuple[str, str]]]] = []
        self._today_labels: dict[str, QLabel] = {}
        self._connection_notices: list[PluginConnectionNotice] = []
        self.connection_messages: dict[str, QLabel] = {}
        #: Plugins already connected on the first read get no "connected" banner.
        self._notices_started = False

        root = QHBoxLayout(self)
        root.setContentsMargins(20, 16, 20, 16)
        root.setSpacing(20)

        # ── Plugins and their cameras ───────────────────────────────────
        self.side = QWidget()
        self.side.setFixedWidth(260)
        side = QVBoxLayout(self.side)
        side.setContentsMargins(0, 0, 0, 0)
        side.setSpacing(10)
        heading_row = QHBoxLayout()
        heading = QLabel("Plugins")
        heading.setObjectName("pluginsSideTitle")
        heading_row.addWidget(heading, 1)
        self.add_button = QPushButton("Add Plugin…")
        set_button_variant(self.add_button, "primary")
        self.add_button.clicked.connect(self._on_add)
        heading_row.addWidget(self.add_button)
        side.addLayout(heading_row)
        self.tree = QTreeWidget()
        self.tree.setObjectName("pluginsTree")
        self.tree.setHeaderHidden(True)
        self.tree.setAccessibleName("Plugins and their cameras")
        self.tree.setRootIsDecorated(False)
        self.tree.setIndentation(18)
        self.tree.setIconSize(QSize(12, 12))
        self.tree.currentItemChanged.connect(self._on_tree_selection)
        side.addWidget(self.tree, 1)
        root.addWidget(self.side)

        # ── The selection ───────────────────────────────────────────────
        content = QVBoxLayout()
        content.setSpacing(10)
        self.connection_problem = QLabel("")
        self.connection_problem.setObjectName("pluginNotice")
        self.connection_problem.setTextFormat(Qt.TextFormat.PlainText)
        self.connection_problem.setWordWrap(True)
        set_status_tone(self.connection_problem, "error")
        self.connection_problem.hide()
        content.addWidget(self.connection_problem)
        self._connection_area = QScrollArea()
        self._connection_area.setFrameShape(QFrame.Shape.NoFrame)
        self._connection_area.setWidgetResizable(True)
        self._connection_area.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Fixed)
        self._connection_area.setAccessibleName("Plugin connections")
        messages = QWidget()
        self._connection_layout = QVBoxLayout(messages)
        self._connection_layout.setContentsMargins(0, 0, 0, 0)
        self._connection_layout.setSpacing(6)
        self._connection_area.setWidget(messages)
        self._connection_area.hide()
        content.addWidget(self._connection_area)
        self.pages = QStackedWidget()
        content.addWidget(self.pages, 1)
        root.addLayout(content, 1)
        self.empty_page = self._build_empty_page()
        self.pages.addWidget(self.empty_page)
        self.plugin_page = QScrollArea()
        self.plugin_page.setWidgetResizable(True)
        self.plugin_page.setFrameShape(QFrame.Shape.NoFrame)
        self.pages.addWidget(self.plugin_page)
        self.camera_page = self._build_camera_page()
        self.pages.addWidget(self.camera_page)
        self.side.hide()  # until the recorder says there are plugins

        self._timer = QTimer(self)
        self._timer.setInterval(_POLL_MS)
        self._timer.timeout.connect(self._poll)
        self.setStyleSheet(plugins_stylesheet())

    def apply_theme(self, theme_name: str | None = None) -> None:
        """Follow the app's theme (startup and Settings changes)."""
        self.live_figures.apply_theme(theme_name)
        stylesheet = plugins_stylesheet(theme_name)
        if self.styleSheet() != stylesheet:
            self.setStyleSheet(stylesheet)

    # ------------------------------------------------------------------
    # Building
    # ------------------------------------------------------------------

    def _build_empty_page(self) -> QWidget:
        page = QWidget()
        layout = QVBoxLayout(page)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.addStretch(1)
        card = QFrame()
        card.setObjectName("pluginsEmpty")
        card.setFixedWidth(520)  # wrapped labels need a known width for their height
        body = QVBoxLayout(card)
        body.setContentsMargins(32, 32, 32, 32)
        body.setSpacing(10)
        icon = QLabel()
        icon.setPixmap(_hexagon(40, self.devicePixelRatioF()))
        icon.setAlignment(Qt.AlignmentFlag.AlignCenter)
        body.addWidget(icon)
        title = QLabel("Analytics plugins")
        title.setObjectName("pluginsEmptyTitle")
        title.setAlignment(Qt.AlignmentFlag.AlignCenter)
        body.addWidget(title)
        about = QLabel(
            "Plugins are separate programs that analyse cameras for this recorder: motion "
            "detection, licence plates, counting and more. The recorder sends each one the "
            "cameras you choose and keeps the events it reports."
        )
        about.setWordWrap(True)
        about.setAlignment(Qt.AlignmentFlag.AlignCenter)
        set_label_role(about, "hint")
        body.addWidget(about)
        self.empty_label = QLabel("Reading the recorder's plugins…")
        self.empty_label.setWordWrap(True)
        self.empty_label.setAlignment(Qt.AlignmentFlag.AlignCenter)
        body.addWidget(self.empty_label)
        self.empty_add = QPushButton("Add Plugin…")
        set_button_variant(self.empty_add, "primary")
        self.empty_add.clicked.connect(self._on_add)
        self.empty_add.hide()
        body.addSpacing(6)
        body.addWidget(self.empty_add, 0, Qt.AlignmentFlag.AlignHCenter)
        # Centred by stretches, not alignment: an aligned widget gets its size
        # hint's height, which is too short for its wrapped text.
        row = QHBoxLayout()
        row.addStretch(1)
        row.addWidget(card)
        row.addStretch(1)
        layout.addLayout(row)
        layout.addStretch(2)
        return page

    def _build_camera_page(self) -> QWidget:
        from neurowatch.widgets.figures_legend import FiguresLegend
        from neurowatch.widgets.video_wall import VideoWallWidget

        page = QWidget()
        layout = QVBoxLayout(page)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(12)
        header = QHBoxLayout()
        header.setSpacing(12)
        titles = QVBoxLayout()
        titles.setSpacing(4)
        self.camera_title = QLabel("")
        self.camera_title.setObjectName("pluginsTitle")
        titles.addWidget(self.camera_title)
        self.camera_status = QLabel("")
        self.camera_status.setWordWrap(True)
        titles.addWidget(self.camera_status)
        header.addLayout(titles, 1)
        self.unbind_button = QPushButton("Stop Analysing…")
        self.unbind_button.setToolTip("Unbind this camera from the plugin")
        set_button_variant(self.unbind_button, "secondary")
        self.unbind_button.clicked.connect(self._on_unbind_selected)
        header.addWidget(self.unbind_button, 0, Qt.AlignmentFlag.AlignTop)
        layout.addLayout(header)

        self.camera_tabs = QTabWidget()
        self.camera_tabs.setObjectName("pluginCameraTabs")
        self.camera_tabs.currentChanged.connect(self._on_camera_tab)
        layout.addWidget(self.camera_tabs, 1)

        # Live: what the plugin sees.
        live = QWidget()
        live_layout = QVBoxLayout(live)
        live_layout.setContentsMargins(0, 14, 0, 0)
        live_layout.setSpacing(10)
        self.live_wall = VideoWallWidget(live)
        self.live_wall.set_grid(1, 1)
        self.live_wall.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Expanding)
        self.live_wall.view_demand_changed.connect(lambda *_: self.view_demand_changed.emit())
        live_layout.addWidget(self.live_wall, 1)
        # What the plugin reports about the camera: beside the picture, not on it.
        self.live_figures = FiguresLegend()
        live_layout.addWidget(self.live_figures)
        layers_row = QHBoxLayout()
        layers_row.setSpacing(14)
        shown = QLabel("Show drawings")
        set_label_role(shown, "sectionLabel")
        layers_row.addWidget(shown)
        self._layers_box = QHBoxLayout()
        self._layers_box.setSpacing(14)
        layers_row.addLayout(self._layers_box)
        layers_row.addStretch(1)
        # Off until asked for; there only when the plugin reports any.
        self.figures_shown = QCheckBox("Show counts")
        self.figures_shown.setToolTip(
            "Show what the plugin reports about this camera, such as today's counts"
        )
        self.figures_shown.hide()
        self.figures_shown.toggled.connect(lambda _on: self._show_figures())
        self.figures_shown.clicked.connect(self.figures_choice_changed)
        layers_row.addWidget(self.figures_shown)
        live_layout.addLayout(layers_row)
        self.live_note = QLabel(
            "Every drawing the plugin sends shows here, so you can check what it sees. "
            "The Settings tab chooses which ones the video wall shows."
        )
        self.live_note.setWordWrap(True)
        set_label_role(self.live_note, "hint")
        live_layout.addWidget(self.live_note)
        self.camera_tabs.addTab(live, "Live")

        # Output: what the plugin found.
        output = QWidget()
        output_layout = QVBoxLayout(output)
        output_layout.setContentsMargins(0, 14, 0, 0)
        output_layout.setSpacing(12)
        tiles = QHBoxLayout()
        tiles.setSpacing(12)
        self.count_labels: dict[str, QLabel] = {}
        for key, title in (("hour", "Last hour"), ("today", "Today"), ("week", "Last 7 days")):
            card = QFrame()
            card.setObjectName("pluginStat")
            body = QVBoxLayout(card)
            body.setContentsMargins(16, 12, 16, 14)
            body.setSpacing(2)
            name = QLabel(title)
            set_label_role(name, "hint")
            body.addWidget(name)
            value = QLabel("—")
            value.setObjectName("pluginStatValue")
            body.addWidget(value)
            self.count_labels[key] = value
            tiles.addWidget(card, 1)
        output_layout.addLayout(tiles)
        recent_row = QHBoxLayout()
        recent_title = QLabel("Recent events")
        recent_title.setObjectName("cardTitle")
        recent_row.addWidget(recent_title)
        recent_hint = QLabel("Double-click one to play its recording")
        set_label_role(recent_hint, "hint")
        recent_row.addWidget(recent_hint)
        recent_row.addStretch(1)
        refresh = QPushButton("Refresh")
        set_button_variant(refresh, "secondary")
        refresh.clicked.connect(self._load_output)
        recent_row.addWidget(refresh)
        output_layout.addLayout(recent_row)
        self.recent = QListWidget()
        self.recent.setObjectName("pluginEvents")
        self.recent.setAccessibleName("Recent events from this plugin")
        self.recent.setSpacing(1)
        self.recent.itemDoubleClicked.connect(self._on_recent_activated)
        output_layout.addWidget(self.recent, 1)
        self._output_tab = self.camera_tabs.addTab(output, "Output")

        # Settings: the camera's settings for the plugin.
        settings = QWidget()
        settings_layout = QVBoxLayout(settings)
        settings_layout.setContentsMargins(0, 14, 0, 0)
        settings_layout.setSpacing(0)
        self.settings_scroll = QScrollArea()
        self.settings_scroll.setWidgetResizable(True)
        self.settings_scroll.setFrameShape(QFrame.Shape.NoFrame)
        settings_layout.addWidget(self.settings_scroll, 1)
        bar = QFrame()
        bar.setObjectName("pluginSaveBar")
        buttons = QHBoxLayout(bar)
        buttons.setContentsMargins(0, 12, 0, 0)
        buttons.setSpacing(8)
        self.settings_problem = QLabel("")
        self.settings_problem.setWordWrap(True)
        set_status_tone(self.settings_problem, "error")
        buttons.addWidget(self.settings_problem, 1)
        self.revert_button = QPushButton("Revert")
        set_button_variant(self.revert_button, "secondary")
        self.revert_button.clicked.connect(self._fill_settings)
        buttons.addWidget(self.revert_button)
        self.save_button = QPushButton("Save")
        set_button_variant(self.save_button, "primary")
        self.save_button.clicked.connect(self._save_settings)
        buttons.addWidget(self.save_button)
        settings_layout.addWidget(bar)
        self.camera_tabs.addTab(settings, "Settings")
        return page

    # ------------------------------------------------------------------
    # The window's feeds
    # ------------------------------------------------------------------

    def set_cameras(self, cameras: list[CameraConfig]) -> None:
        self._camera_configs = list(cameras)
        self._show_live_camera()

    def set_analytics(self, cameras: dict[str, list[CameraPlugin]]) -> None:
        self._analytics = cameras
        if self._selection is not None and self._selection[1] is not None:
            self._fill_layer_boxes()
        self._show_figures()

    def set_drawings(self, drawings: dict[str, list[Drawing]]) -> None:
        self._drawings = drawings
        self._show_drawings()

    def set_preview_fps(self, fps: Any) -> None:
        self.live_wall.set_preview_fps(fps)

    def desktop_demand(self) -> list[str]:
        """The camera to decode: the one on the Live tab, while it shows."""
        if (
            not self.isVisible()
            or self.pages.currentWidget() is not self.camera_page
            or self.camera_tabs.currentIndex() != 0
        ):
            return []
        return self.live_wall.desktop_demand()

    def activate(self) -> None:
        """The tab is shown: read everything, then keep the status fresh."""
        self._timer.start()
        self._run(load, self._on_loaded)

    def stop(self) -> None:
        """The tab is left or the window closes."""
        self._timer.stop()

    def hideEvent(self, event: Any) -> None:
        super().hideEvent(event)
        self._timer.stop()
        self.view_demand_changed.emit()

    def showEvent(self, event: Any) -> None:
        super().showEvent(event)
        self.view_demand_changed.emit()

    # ------------------------------------------------------------------
    # Reading
    # ------------------------------------------------------------------

    def _run(self, job: Callable[[PluginsRpc], Any], on_done: Callable[[object], None]) -> None:
        if self._job is not None:
            return
        self.add_button.setEnabled(False)
        self.empty_add.setEnabled(False)
        worker = Job(self._rpc, job)
        worker.done.connect(on_done)
        worker.finished.connect(worker.deleteLater)
        self._job = worker
        worker.start()

    def _poll(self) -> None:
        if not self.isVisible() or self._job is not None or self._poll_job is not None:
            return
        worker = Job(self._rpc, lambda rpc: parse_status(rpc("plugins.status", {})))
        worker.done.connect(self._on_status)
        worker.finished.connect(worker.deleteLater)
        self._poll_job = worker
        worker.start()

    def _on_loaded(self, result: object) -> None:
        self._job = None
        self.add_button.setEnabled(True)
        self.empty_add.setEnabled(True)
        if isinstance(result, Exception):
            logger.warning("Plugins could not be read: %s", result)
            self._connection_read_failed(result)
            if self._snapshot is None:
                self.empty_label.setText(f"The recorder's plugins could not be read: {result}")
            else:
                self.settings_problem.setText(f"Not saved: {result}")
            return
        if isinstance(result, Snapshot):
            self.settings_problem.setText("")
            self._snapshot = result
            self._update_connection_messages()
            self._rebuild_tree()

    def _on_status(self, result: object) -> None:
        self._poll_job = None
        if isinstance(result, Exception):
            self._connection_read_failed(result)
            return
        if self._snapshot is None or not isinstance(result, dict):
            return
        previous = self._snapshot.status
        self._snapshot.status = result
        self._update_connection_messages()
        if self._selection is not None and self._selection[1] is None:
            plugin = self._selection[0]
            before, after = previous.get(plugin), result.get(plugin)
            before_key = before.manifest.public_key if before and before.manifest else ""
            after_key = after.manifest.public_key if after and after.manifest else ""
            if (before.state if before else "", before_key) != (
                after.state if after else "",
                after_key,
            ):
                self._show_plugin(plugin)
        self._refresh_status()

    def _connection_read_failed(self, error: Exception) -> None:
        self.connection_problem.setText(
            f"Plugin status could not be refreshed: {error}. Showing the last known status. "
            "The recorder connection will be checked again automatically."
        )
        self.connection_problem.show()

    def _update_connection_messages(self) -> None:
        snapshot = self._snapshot
        if snapshot is None:
            return
        self.connection_problem.clear()
        self.connection_problem.hide()
        self._connection_notices = connection_notices(
            snapshot.config, snapshot.status, self._connection_notices
        )
        active = {notice.server for notice in self._connection_notices}
        for ident in list(self.connection_messages):
            if ident not in active:
                removed = self.connection_messages.pop(ident)
                self._connection_layout.removeWidget(removed)
                removed.hide()
                removed.deleteLater()
                removed.setParent(None)
        for notice in self._connection_notices:
            widget = self.connection_messages.get(notice.server)
            if widget is None:
                widget = QLabel()
                widget.setObjectName("pluginNotice")
                widget.setWordWrap(True)
                widget.setTextFormat(Qt.TextFormat.PlainText)
                self._connection_layout.addWidget(widget)
                self.connection_messages[notice.server] = widget
            if widget.text() != notice.text:
                widget.setText(notice.text)
                set_status_tone(widget, notice.tone)
                if notice.tone == "success" and not self._notices_started:
                    widget.hide()
                    continue
                widget.show()
                if notice.tone == "success":
                    # A connection that came back needs no lasting banner: the
                    # tree's dot keeps showing it. Failures stay until recovery.
                    QTimer.singleShot(
                        _NOTICE_MS,
                        self,
                        functools.partial(self._settle_notice, notice.server, notice.text),
                    )
        self._notices_started = True
        self._sync_notice_area()

    def _settle_notice(self, server: str, text: str) -> None:
        widget = self.connection_messages.get(server)
        if widget is not None and widget.text() == text:
            widget.hide()
        self._sync_notice_area()

    def _sync_notice_area(self) -> None:
        shown = any(not widget.isHidden() for widget in self.connection_messages.values())
        self._connection_area.setVisible(shown)
        if shown:
            self._fit_notice_area()

    def _fit_notice_area(self) -> None:
        """As tall as its banners (at most ``_NOTICE_MAX``); the rest scrolls."""
        messages = self._connection_area.widget()
        if messages is None:
            return
        width = max(1, self._connection_area.viewport().width())
        layout = messages.layout()
        wanted = (
            layout.totalHeightForWidth(width)
            if layout is not None and layout.hasHeightForWidth()
            else messages.sizeHint().height()
        )
        self._connection_area.setFixedHeight(min(_NOTICE_MAX, max(0, wanted)))

    def resizeEvent(self, event: Any) -> None:
        super().resizeEvent(event)
        if self._connection_area.isVisible():
            self._fit_notice_area()

    # ------------------------------------------------------------------
    # The tree
    # ------------------------------------------------------------------

    def _rebuild_tree(self) -> None:
        snapshot = self._snapshot
        if snapshot is None:
            return
        keep = self._selection
        self.tree.blockSignals(True)
        self.tree.clear()
        chosen: QTreeWidgetItem | None = None
        # By name: no plugin comes first for any other reason.
        for server in sorted(
            snapshot.config.servers, key=lambda s: (self._title(s).casefold(), s.id)
        ):
            item = QTreeWidgetItem([self._title(server)])
            item.setData(0, Qt.ItemDataRole.UserRole, (server.id, None))
            font = item.font(0)
            font.setWeight(QFont.Weight.DemiBold)
            item.setFont(0, font)
            self.tree.addTopLevelItem(item)
            if keep == (server.id, None):
                chosen = item
            for binding in snapshot.config.bindings_of(server.id):
                child = QTreeWidgetItem([self._camera_name(binding.camera_id)])
                child.setData(0, Qt.ItemDataRole.UserRole, (server.id, binding.camera_id))
                item.addChild(child)
                if keep == (server.id, binding.camera_id):
                    chosen = child
            item.setExpanded(True)
        self.tree.blockSignals(False)
        if chosen is None and self.tree.topLevelItemCount():
            chosen = self.tree.topLevelItem(0)
        has_plugins = bool(snapshot.config.servers)
        self.side.setVisible(has_plugins)
        self.empty_add.setVisible(not has_plugins)
        if chosen is not None:
            self.tree.setCurrentItem(chosen)
            self._select(chosen.data(0, Qt.ItemDataRole.UserRole))
        else:
            self._selection = None
            self.empty_label.setText(
                "No plugins yet. Start a plugin program on this computer (for motion "
                "detection: nw-motion), then add it here."
            )
            self.pages.setCurrentWidget(self.empty_page)
            self._show_live_camera()
        self._refresh_status()

    def _on_tree_selection(self, current: QTreeWidgetItem | None, _previous: object) -> None:
        if current is not None:
            self._select(current.data(0, Qt.ItemDataRole.UserRole))

    def select(self, plugin: str, camera_id: str | None = None) -> None:
        """Show a plugin, or one of its cameras."""
        for i in range(self.tree.topLevelItemCount()):
            item = self.tree.topLevelItem(i)
            if item is None:
                continue
            if camera_id is None and item.data(0, Qt.ItemDataRole.UserRole) == (plugin, None):
                self.tree.setCurrentItem(item)
                return
            for j in range(item.childCount()):
                child = item.child(j)
                if child is not None and child.data(0, Qt.ItemDataRole.UserRole) == (
                    plugin,
                    camera_id,
                ):
                    self.tree.setCurrentItem(child)
                    return

    def _select(self, selection: object) -> None:
        if not isinstance(selection, tuple):
            return
        changed = selection != self._selection
        self._selection = selection
        plugin, camera_id = selection
        if camera_id is None:
            self._show_plugin(plugin)
            self.pages.setCurrentWidget(self.plugin_page)
        elif changed or self.pages.currentWidget() is not self.camera_page:
            self._show_camera(plugin, camera_id)
            self.pages.setCurrentWidget(self.camera_page)
        self._show_live_camera()
        self.view_demand_changed.emit()

    # ------------------------------------------------------------------
    # A plugin
    # ------------------------------------------------------------------

    def _show_plugin(self, plugin: str) -> None:
        snapshot = self._snapshot
        server = self._server(plugin)
        if snapshot is None or server is None:
            return
        self._status_labels = []
        self._row_dots = []
        self._today_labels = {}
        page = QWidget()
        layout = QVBoxLayout(page)
        layout.setContentsMargins(0, 0, 8, 0)
        layout.setSpacing(14)
        manifest = self._manifest(plugin)

        # Name, maker, state; On and Remove.
        header = QHBoxLayout()
        header.setSpacing(12)
        titles = QVBoxLayout()
        titles.setSpacing(4)
        title = QLabel(self._title(server))
        title.setObjectName("pluginsTitle")
        titles.addWidget(title)
        if manifest is not None and (manifest.vendor or manifest.version):
            maker = " · ".join(
                part
                for part in (
                    manifest.vendor,
                    f"version {manifest.version}" if manifest.version else "",
                )
                if part
            )
            about = QLabel(maker)
            set_label_role(about, "hint")
            titles.addWidget(about)
        state_row = QHBoxLayout()
        state_row.setContentsMargins(0, 4, 0, 0)
        state = QLabel("")
        state.setObjectName("pluginState")
        self._status_labels.append((state, functools.partial(self._server_badge, server)))
        state_row.addWidget(state, 0, Qt.AlignmentFlag.AlignLeft)
        state_row.addStretch(1)
        titles.addLayout(state_row)
        self._state_detail = QLabel("")
        self._state_detail.setWordWrap(True)
        self._status_labels.append(
            (self._state_detail, functools.partial(self._server_detail, server))
        )
        titles.addWidget(self._state_detail)
        header.addLayout(titles, 1)
        actions = QHBoxLayout()
        actions.setSpacing(8)
        enabled = QCheckBox("On")
        enabled.setToolTip("Connect to this plugin and send it its cameras")
        enabled.setChecked(server.enabled)
        enabled.toggled.connect(lambda on, s=server: self._on_enabled(s, on))
        actions.addWidget(enabled)
        remove = QPushButton("Remove…")
        set_button_variant(remove, "ghost")
        remove.clicked.connect(lambda _c=False, s=server: self._on_remove(s))
        actions.addWidget(remove)
        header.addLayout(actions)
        header.setAlignment(actions, Qt.AlignmentFlag.AlignTop)
        layout.addLayout(header)

        if manifest is not None and manifest.public_key:
            current = snapshot.status.get(plugin)
            pending = current is not None and current.state == "pending_approval"
            approved = (
                server.approval is not None
                and server.approval.get("public_key") == manifest.public_key
            )
            approval, approval_body, _ = settings_card(
                "Approve this plugin" if pending else "Plugin identity",
                "Check the key with the plugin's maker, then approve it. Until then the "
                "plugin receives no cameras and no credentials."
                if pending
                else "The key and permissions approved for this plugin.",
            )
            if pending:
                approval.setObjectName("pluginCallout")
                approval.setProperty("tone", "warning")
            try:
                fingerprint = hashlib.sha256(bytes.fromhex(manifest.public_key)).hexdigest()
                shown = " ".join(fingerprint[i : i + 8] for i in range(0, len(fingerprint), 8))
            except ValueError:
                shown = "Invalid key"
            facts = QGridLayout()
            facts.setHorizontalSpacing(16)
            facts.setVerticalSpacing(8)
            facts.setColumnStretch(1, 1)
            key_name = QLabel("Key fingerprint")
            set_label_role(key_name, "hint")
            facts.addWidget(key_name, 0, 0, Qt.AlignmentFlag.AlignTop)
            key_label = QLabel(shown)
            key_label.setObjectName("pluginMono")
            key_label.setFont(mono_font(9))
            key_label.setWordWrap(True)
            key_label.setToolTip("SHA-256 of the plugin's public key")
            key_label.setTextInteractionFlags(Qt.TextInteractionFlag.TextSelectableByMouse)
            facts.addWidget(key_label, 0, 1)
            permissions_name = QLabel("Permissions")
            set_label_role(permissions_name, "hint")
            facts.addWidget(permissions_name, 1, 0, Qt.AlignmentFlag.AlignTop)
            permissions = QLabel(", ".join(manifest.permissions) or "None")
            permissions.setWordWrap(True)
            facts.addWidget(permissions, 1, 1)
            approval_body.addLayout(facts)
            buttons = QHBoxLayout()
            buttons.setSpacing(8)
            if pending:
                approve = QPushButton("Approve Key and Permissions")
                set_button_variant(approve, "primary")
                approve.clicked.connect(
                    lambda _checked=False, s=server, key=manifest.public_key: self._approve(s, key)
                )
                buttons.addWidget(approve)
            credential = QPushButton("Vendor Credential…")
            set_button_variant(credential, "secondary")
            credential.setEnabled(approved)
            credential.setToolTip(
                "Set or clear the licence key or account the plugin's maker gave you"
                if approved
                else "Approve the plugin first"
            )
            credential.clicked.connect(lambda _checked=False, s=server: self._credential(s))
            buttons.addWidget(credential)
            buttons.addStretch(1)
            approval_body.addLayout(buttons)
            layout.addWidget(approval)

        card, body, heading = settings_card(
            "Cameras", "The cameras this plugin analyses. Open one to see what it sees."
        )
        self.plugin_today = QLabel("")
        set_label_role(self.plugin_today, "hint")
        heading.addWidget(self.plugin_today, 0, Qt.AlignmentFlag.AlignTop)
        bind = QPushButton("Bind Camera…")
        set_button_variant(bind, "secondary")
        bound = {b.camera_id for b in snapshot.config.bindings_of(plugin)}
        bind.setEnabled(any(c.id not in bound for c in snapshot.cameras))
        if not bind.isEnabled():
            bind.setToolTip("Every camera is already bound to this plugin")
        bind.clicked.connect(lambda _c=False, s=server: self._on_bind(s))
        self.bind_button = bind
        heading.addWidget(bind, 0, Qt.AlignmentFlag.AlignTop)
        bindings = snapshot.config.bindings_of(plugin)
        if not bindings:
            none = QLabel("No cameras yet. Bind Camera… chooses which ones it analyses.")
            none.setWordWrap(True)
            set_label_role(none, "hint")
            body.addWidget(none)
        rows = QVBoxLayout()
        rows.setSpacing(0)
        for binding in bindings:
            row = QFrame()
            row.setObjectName("pluginRow")
            line = QHBoxLayout(row)
            line.setContentsMargins(0, 10, 0, 10)
            line.setSpacing(12)
            dot = QLabel()
            dot.setFixedSize(12, 12)
            line.addWidget(dot)
            texts = QVBoxLayout()
            texts.setSpacing(2)
            name = QLabel(self._camera_name(binding.camera_id))
            set_label_role(name, "emphasis")
            texts.addWidget(name)
            status = QLabel("")
            status.setWordWrap(True)
            self._status_labels.append(
                (status, functools.partial(self._binding_text, server, binding))
            )
            self._row_dots.append((dot, functools.partial(self._binding_text, server, binding)))
            texts.addWidget(status)
            line.addLayout(texts, 1)
            today = QLabel("")
            set_label_role(today, "hint")
            self._today_labels[binding.camera_id] = today
            line.addWidget(today)
            open_button = QPushButton("Open")
            set_button_variant(open_button, "secondary")
            open_button.clicked.connect(
                lambda _c=False, p=plugin, c=binding.camera_id: self.select(p, c)
            )
            line.addWidget(open_button)
            rows.addWidget(row)
        body.addLayout(rows)
        layout.addWidget(card)

        address, address_body, _ = settings_card("Connection")
        where = QLabel(server.address)
        where.setObjectName("pluginMono")
        where.setFont(mono_font(9))
        where.setWordWrap(True)
        where.setTextInteractionFlags(Qt.TextInteractionFlag.TextSelectableByMouse)
        address_body.addWidget(where)
        layout.addWidget(address)
        layout.addStretch(1)
        self.plugin_page.setWidget(page)
        self._refresh_status()
        if manifest is not None and manifest.reports_events:
            self._load_plugin_today(manifest.id, [b.camera_id for b in bindings])

    def _load_plugin_today(self, topic: str, cameras: list[str]) -> None:
        if not cameras or self._output_job is not None:
            return
        since = _midnight_ms(time.time())

        def job(rpc: PluginsRpc) -> list[Event]:
            raw = rpc(
                "events.list",
                {"source": "plugin", "topic": topic, "from_ms": since, "limit": 1000},
            )
            events = [Event.from_dict(e) for e in (raw or {}).get("events", [])]
            return [e for e in events if e is not None and e.start_ms >= since]

        worker = Job(self._rpc, job)
        worker.done.connect(self._on_plugin_today)
        worker.finished.connect(worker.deleteLater)
        self._output_job = worker
        worker.start()

    def _on_plugin_today(self, result: object) -> None:
        self._output_job = None
        if not isinstance(result, list) or not hasattr(self, "plugin_today"):
            return
        per_camera: dict[str, int] = {}
        for event in result:
            per_camera[event.camera_id or ""] = per_camera.get(event.camera_id or "", 0) + 1
        try:
            self.plugin_today.setText(_events_today(len(result)))
            for camera_id, today in self._today_labels.items():
                today.setText(_events_today(per_camera.get(camera_id, 0)))
        except RuntimeError:  # the page was replaced meanwhile
            pass

    # ------------------------------------------------------------------
    # A camera
    # ------------------------------------------------------------------

    def _show_camera(self, plugin: str, camera_id: str) -> None:
        server = self._server(plugin)
        if server is None:
            return
        self.camera_title.setText(self._camera_name(camera_id))
        self._layers = LayerChoice()
        self.figures_shown.setChecked(self.counts_choice(camera_id))
        self._fill_layer_boxes()
        self._fill_settings()
        # Output is the plugin's events: nothing to show for a plugin with none.
        manifest = self._manifest(server.id)
        has_output = manifest is None or manifest.reports_events
        if not has_output and self.camera_tabs.currentIndex() == self._output_tab:
            self.camera_tabs.setCurrentIndex(0)
        self.camera_tabs.setTabVisible(self._output_tab, has_output)
        if has_output:
            self._load_output()
        self._refresh_status()

    def _selected_binding(self) -> tuple[PluginServer, PluginBinding] | None:
        if self._snapshot is None or self._selection is None or self._selection[1] is None:
            return None
        plugin, camera_id = self._selection[0], self._selection[1]
        server = self._server(plugin)
        binding = self._snapshot.config.binding(plugin, camera_id)
        if server is None or binding is None:
            return None
        return server, binding

    def _show_live_camera(self) -> None:
        """The Live tab's picture: the selected camera, alone on page one."""
        camera_id = self._selection[1] if self._selection is not None else None
        camera = next((c for c in self._camera_configs if c.id == camera_id), None)
        shown = [dataclasses.replace(camera, wall_position=None)] if camera is not None else []
        ids = [c.id for c in shown]
        if ids != self._live_ids:
            self._live_ids = ids
            self.live_wall.set_cameras(shown, page=0)
        self._show_drawings()

    def _show_drawings(self) -> None:
        camera_id = self._selection[1] if self._selection is not None else None
        if camera_id is None:
            self.live_wall.set_drawings({})
            self._show_figures()
            return
        plugin = self._selection[0] if self._selection is not None else ""
        mine = [d for d in self._drawings.get(camera_id, []) if d.plugin == plugin]
        shown = self._layers.shown(mine)
        self.live_wall.set_drawings({camera_id: shown})
        self._marking_hold.update({f"{plugin}/{camera_id}": shown}, time.monotonic())
        self._show_figures()

    def refresh_counts_choice(self) -> None:
        """The camera shown here follows its tile's choice (``counts_choice``)."""
        camera_id = self.selected_camera()
        if camera_id is not None:
            self.figures_shown.setChecked(self.counts_choice(camera_id))

    def selected_camera(self) -> str | None:
        return self._selection[1] if self._selection is not None else None

    def _show_figures(self) -> None:
        """The selected plugin's figures for the selected camera."""
        selection = self._selection
        entry = None
        if selection is not None and selection[1] is not None:
            entry = next(
                (p for p in self._analytics.get(selection[1], []) if p.plugin == selection[0]),
                None,
            )
        figures = entry.figures if entry is not None else ()
        counts = self.figures_shown.isChecked()
        self.figures_shown.setVisible(bool(figures))
        self.live_figures.set_figures(figures if counts else ())
        view = f"{selection[0]}/{selection[1]}" if entry is not None and selection else ""
        drawn = self._marking_hold.shown([view], time.monotonic())
        reported = markings_reported(figures, counts)
        self.live_figures.set_markings(tuple(dict.fromkeys((*reported, *drawn))))

    def _fill_layer_boxes(self) -> None:
        while self._layers_box.count():
            item = self._layers_box.takeAt(0)
            widget = item.widget() if item is not None else None
            if widget is not None:
                widget.setParent(None)
                widget.deleteLater()
        selection = self._selection
        if selection is None or selection[1] is None:
            return
        plugin, camera_id = selection[0], selection[1]
        entry = next((p for p in self._analytics.get(camera_id, []) if p.plugin == plugin), None)
        layers = entry.layers if entry is not None else ()
        if not layers:
            none = QLabel("none sent yet")
            set_label_role(none, "hint")
            self._layers_box.addWidget(none)
            return
        for layer in layers:
            box = QCheckBox(layer.label)
            box.setChecked((plugin, layer.id) not in self._layers.hidden)
            box.toggled.connect(lambda on, key=(plugin, layer.id): self._toggle_layer(key, on))
            self._layers_box.addWidget(box)

    def _toggle_layer(self, key: tuple[str, str], on: bool) -> None:
        if on:
            self._layers.hidden.discard(key)
        else:
            self._layers.hidden.add(key)
        self._marking_hold.clear()  # the key follows the boxes at once
        self._show_drawings()

    def _on_camera_tab(self, _index: int) -> None:
        if self.camera_tabs.currentIndex() == 1:
            self._load_output()
        elif self.camera_tabs.currentIndex() == 2 and self._editor is not None:
            self._editor.load_snapshot()
        self.view_demand_changed.emit()

    def _fill_settings(self) -> None:
        """The selected camera's settings, as saved (Revert)."""
        selected = self._selected_binding()
        if self._editor is not None:
            self._editor.stop()
        self._editor = None
        self.settings_problem.setText("")
        if selected is None or self._snapshot is None:
            self.settings_scroll.setWidget(QWidget())
            return
        server, binding = selected
        cameras = [c for c in self._snapshot.cameras if c.id == binding.camera_id]
        self._editor = BindingEditor(
            self._manifest(server.id), cameras, binding, endpoint=self._endpoint
        )
        self.settings_scroll.setWidget(self._editor)
        if self.camera_tabs.currentIndex() == 2 and self.isVisible():
            self._editor.load_snapshot()

    def _save_settings(self) -> None:
        selected = self._selected_binding()
        if selected is None or self._snapshot is None or self._editor is None:
            return
        problem = self._editor.problem()
        if problem:
            self.settings_problem.setText(problem)
            return
        server, _binding = selected
        self._apply(self._snapshot.config.with_binding(self._editor.binding(server.id)))

    def _load_output(self) -> None:
        selected = self._selected_binding()
        if selected is None or self._output_job is not None:
            return
        server, binding = selected
        manifest = self._manifest(server.id)
        topic = manifest.id if manifest is not None else None
        since = int(time.time() * 1000) - _OUTPUT_DAYS * 86_400_000

        def job(rpc: PluginsRpc) -> list[Event]:
            params: dict[str, Any] = {
                "camera_ids": [binding.camera_id],
                "source": "plugin",
                "from_ms": since,
                "limit": 1000,
            }
            if topic:
                params["topic"] = topic
            raw = rpc("events.list", params)
            events = [Event.from_dict(e) for e in (raw or {}).get("events", [])]
            return [e for e in events if e is not None]

        worker = Job(self._rpc, job)
        worker.done.connect(self._on_output)
        worker.finished.connect(worker.deleteLater)
        self._output_job = worker
        worker.start()

    def _on_output(self, result: object) -> None:
        self._output_job = None
        if not isinstance(result, list):
            for value in self.count_labels.values():
                value.setText("—")
            return
        hour, today, week = output_counts(result, time.time())
        self.count_labels["hour"].setText(str(hour))
        self.count_labels["today"].setText(str(today))
        self.count_labels["week"].setText(str(week))
        self.recent.clear()
        for event in result[:_RECENT]:
            parts = [label(event.kind)]
            if event.end_ms is None:
                parts.append("going on")
            else:
                parts.append(duration_text(event))
            if event.score is not None:
                parts.append(f"score {event.score * 100:.0f}%")
            day = datetime.fromtimestamp(event.start_ms / 1000).strftime("%a %d %b")
            item = QListWidgetItem("  ·  ".join(parts) + f"\n{day}, {clock_text(event.start_ms)}")
            item.setToolTip("Double-click to play the recording")
            item.setData(Qt.ItemDataRole.UserRole, event)
            self.recent.addItem(item)
        if not result:
            empty = QListWidgetItem("No events from this plugin in the last 7 days.")
            empty.setFlags(Qt.ItemFlag.NoItemFlags)
            self.recent.addItem(empty)

    def _on_recent_activated(self, item: QListWidgetItem) -> None:
        event = item.data(Qt.ItemDataRole.UserRole)
        if isinstance(event, Event):
            self.play_requested.emit(event)

    # ------------------------------------------------------------------
    # Changes
    # ------------------------------------------------------------------

    def _apply(
        self,
        config: PluginsConfig,
        *,
        then: Callable[[PluginsRpc], None] | None = None,
    ) -> None:
        """Save ``config`` (and do ``then``), then read everything again."""

        def job(rpc: PluginsRpc) -> Snapshot:
            rpc("plugins.set", config.to_dict())
            if then is not None:
                then(rpc)
            return load(rpc)

        self._run(job, self._on_loaded)

    def _on_add(self) -> None:
        if self._snapshot is None:
            return
        taken = [s.id for s in self._snapshot.config.servers]
        dialog = AddPluginDialog(taken, self, rpc=self._rpc)
        if dialog.exec() != QDialog.DialogCode.Accepted:
            return
        server = dialog.server()
        self._selection = (server.id, None)
        self._run(load, self._on_loaded)

    def _approve(self, server: PluginServer, public_key: str) -> None:
        def job(rpc: PluginsRpc) -> Snapshot:
            rpc("plugins.approve", {"server": server.id, "public_key": public_key})
            return load(rpc)

        self._run(job, self._on_loaded)

    def _credential(self, server: PluginServer) -> None:
        dialog = CredentialDialog(self)
        if dialog.exec() != QDialog.DialogCode.Accepted:
            return
        credential = dialog.credential()

        def job(rpc: PluginsRpc) -> Snapshot:
            rpc("plugins.credentials", {"server": server.id, "credential": credential})
            return load(rpc)

        self._run(job, self._on_loaded)

    def _on_remove(self, server: PluginServer) -> None:
        if self._snapshot is None:
            return
        cameras = len(self._snapshot.config.bindings_of(server.id))
        detail = (
            f" Its {cameras} camera{'s' if cameras != 1 else ''} stop being analysed by it."
            if cameras
            else ""
        )
        if ask(
            self,
            "Remove plugin",
            f"Remove {self._title(server)}?{detail} The plugin program itself keeps running "
            "until you stop it.",
            yes_text="Remove",
            no_text="Cancel",
            destructive=True,
        ):
            self._selection = None
            self._apply(self._snapshot.config.without_server(server.id))

    def _on_enabled(self, server: PluginServer, enabled: bool) -> None:
        if self._snapshot is not None and server.enabled != enabled:
            self._apply(self._snapshot.config.with_server_enabled(server.id, enabled))

    def _on_bind(self, server: PluginServer) -> None:
        snapshot = self._snapshot
        if snapshot is None:
            return
        bound = {b.camera_id for b in snapshot.config.bindings_of(server.id)}
        choices = [c for c in snapshot.cameras if c.id not in bound]
        if not choices:
            return
        dialog = BindingDialog(
            self._title(server),
            self._manifest(server.id),
            choices,
            endpoint=self._endpoint,
            parent=self,
        )
        if dialog.exec() != QDialog.DialogCode.Accepted:
            return
        binding = dialog.binding(server.id)
        camera = next((c for c in snapshot.cameras if c.id == binding.camera_id), None)
        then = None
        if dialog.turn_off_builtin() and camera is not None:
            camera_id = camera.id
            source = without_builtin_motion(camera.motion)["source"]

            def then(rpc: PluginsRpc) -> None:
                rpc("motion.configure", {"camera_id": camera_id, "source": source})

        self._selection = (server.id, binding.camera_id)
        self._apply(snapshot.config.with_binding(binding), then=then)

    def _on_unbind_selected(self) -> None:
        selected = self._selected_binding()
        if selected is None or self._snapshot is None:
            return
        server, binding = selected
        name = self._camera_name(binding.camera_id)
        if ask(
            self,
            "Stop analysing camera",
            f"Stop sending {name} to {self._title(server)}? Its settings for this camera "
            "are removed.",
            yes_text="Stop",
            no_text="Cancel",
            destructive=True,
        ):
            self._selection = (server.id, None)
            self._apply(self._snapshot.config.without_binding(server.id, binding.camera_id))

    # ------------------------------------------------------------------
    # Status
    # ------------------------------------------------------------------

    def _refresh_status(self) -> None:
        snapshot = self._snapshot
        if snapshot is None:
            return
        # The tree's dots.
        for i in range(self.tree.topLevelItemCount()):
            item = self.tree.topLevelItem(i)
            if item is None:
                continue
            plugin, _ = item.data(0, Qt.ItemDataRole.UserRole)
            server = self._server(plugin)
            if server is None:
                continue
            text, tone = self._server_text(server)
            item.setIcon(0, _dot(tone))
            item.setToolTip(0, text)
            item.setText(0, self._title(server))
            for j in range(item.childCount()):
                child = item.child(j)
                if child is None:
                    continue
                _, camera_id = child.data(0, Qt.ItemDataRole.UserRole)
                binding = snapshot.config.binding(plugin, camera_id)
                if binding is None:
                    continue
                text, tone = self._binding_text(server, binding)
                child.setIcon(0, _dot(tone))
                child.setToolTip(0, text)
        for dot, read in self._row_dots:
            try:
                dot.setPixmap(_dot(read()[1]).pixmap(12, 12))
            except RuntimeError:  # its page was replaced
                continue
        for label_widget, read in self._status_labels:
            try:
                text, tone = read()
                label_widget.setText(text)
                label_widget.setVisible(bool(text))
                set_status_tone(label_widget, tone)
            except RuntimeError:  # its page was replaced
                continue
        selected = self._selected_binding()
        if selected is not None:
            server, binding = selected
            text, tone = self._binding_text(server, binding)
            status = snapshot.status.get(server.id)
            view = status.bindings.get(binding.camera_id) if status is not None else None
            details = []
            if view is not None and view.format:
                stream = {"sub": "sub stream", "main": "main stream"}.get(view.stream, "")
                details.append(f"{stream} {view.format}".strip())
            plugin_name = self._title(server)
            line = f"{plugin_name} · {text}"
            if details:
                line += " · " + " · ".join(details)
            self.camera_status.setText(line)
            set_status_tone(self.camera_status, tone)

    def _server_badge(self, server: PluginServer) -> tuple[str, str]:
        """A short word for the plugin page's state badge."""
        _text, tone = self._server_text(server)
        status = self._snapshot.status.get(server.id) if self._snapshot else None
        if not server.enabled:
            return ("Off", tone)
        state = status.state if status is not None else "connecting"
        word = {
            "connecting": "Connecting",
            "pending_approval": "Needs approval",
            "authorizing": "Checking authorization",
            "connected": "Connected",
        }.get(state, "Not reachable")
        return (word, tone)

    def _server_detail(self, server: PluginServer) -> tuple[str, str]:
        """The full state under the badge, when it says more than the badge."""
        text, tone = self._server_text(server)
        status = self._snapshot.status.get(server.id) if self._snapshot else None
        if (
            server.enabled
            and status is not None
            and status.state in ("connected", "pending_approval")
        ):
            return ("", tone)  # the badge, or the approval card below, says it all
        return (text, tone)

    def _server_text(self, server: PluginServer) -> tuple[str, str]:
        status = self._snapshot.status.get(server.id) if self._snapshot else None
        return server_text(server, status)

    def _binding_text(self, server: PluginServer, binding: PluginBinding) -> tuple[str, str]:
        status = self._snapshot.status.get(server.id) if self._snapshot else None
        view = status.bindings.get(binding.camera_id) if status is not None else None
        connected = status is not None and status.state == "connected"
        return binding_text(binding, view, connected)

    # ------------------------------------------------------------------
    # Helpers
    # ------------------------------------------------------------------

    def _server(self, plugin: str) -> PluginServer | None:
        if self._snapshot is None:
            return None
        return next((s for s in self._snapshot.config.servers if s.id == plugin), None)

    def _manifest(self, plugin: str) -> Any:
        status = self._snapshot.status.get(plugin) if self._snapshot else None
        return status.manifest if status is not None else None

    def _title(self, server: PluginServer) -> str:
        manifest = self._manifest(server.id)
        return manifest.name if manifest is not None and manifest.name else server.id

    def _camera_name(self, camera_id: str) -> str:
        cameras = self._snapshot.cameras if self._snapshot else []
        return next((c.name for c in cameras if c.id == camera_id), "A removed camera")
