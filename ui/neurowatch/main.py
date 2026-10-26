"""Neurowatch V1 Neuron — main window."""

from __future__ import annotations

import logging
import os
import shutil
import signal
import sys
import time
from collections.abc import Callable
from contextlib import contextmanager
from copy import deepcopy
from dataclasses import asdict, replace
from typing import Any

from PySide6.QtCore import (
    QEvent,
    QEventLoop,
    QFileSystemWatcher,
    Qt,
    QTimer,
    Signal,
)
from PySide6.QtGui import QCloseEvent, QCursor, QGuiApplication, QKeyEvent
from PySide6.QtWidgets import (
    QApplication,
    QDialog,
    QFileDialog,
    QHBoxLayout,
    QMainWindow,
    QStackedWidget,
    QVBoxLayout,
    QWidget,
)

from neurowatch import build_info, windows_icon
from neurowatch.app_launch import (
    LaunchOptions,
    SingleInstance,
    activate_running_instance,
    ensure_service,
    parse_launch_args,
    service_follows_ui,
    start_tray,
    stop_service,
)
from neurowatch.cloud import desktop as cloud_desktop
from neurowatch.dialogs.about_dialog import AboutDialog
from neurowatch.dialogs.camera_dialog import AddEditCameraDialog
from neurowatch.dialogs.confirm import confirm_action
from neurowatch.dialogs.custom_grid_dialog import CustomGridDialog
from neurowatch.dialogs.export_video_dialog import ExportVideoDialog
from neurowatch.dialogs.message import ask, show_message
from neurowatch.dialogs.new_window_dialog import NewWindowDialog, PageSummary, WindowSummary
from neurowatch.dialogs.settings_dialog import SettingsDialog
from neurowatch.ipc.alert_feed import AlertFeedWorker
from neurowatch.ipc.event_feed import EventFeedWorker
from neurowatch.ipc.quality_dispatcher import CameraQualityDispatcher
from neurowatch.ipc.sync_client import BackendOfflineError, IpcError, SyncIpcClient
from neurowatch.ipc.view_lease import ViewLeaseWorker
from neurowatch.ipc.viewer_client import NativeViewerClient
from neurowatch.models.camera import CameraConfig, WallPosition
from neurowatch.models.edition import CAMERA_LIMIT_REACHED, Edition, limit_message
from neurowatch.models.events import ACTIVITY_KINDS, SYSTEM_KINDS, ActivityTracker, Event
from neurowatch.models.preview_fps import PreviewFps
from neurowatch.models.video_decoder import restart_reply
from neurowatch.models.wall_layout import Placement
from neurowatch.models.window_session import saved_windows
from neurowatch.service.leftovers import clean_portable_copy, leftovers_message
from neurowatch.service.manager import ServiceController
from neurowatch.service.paths import service_endpoint
from neurowatch.service.startup import StartupPrefs, load_prefs
from neurowatch.theme import app_icon, apply_app_theme, icon_path
from neurowatch.utils.threads import Worker, retire, wait_for_retired
from neurowatch.widgets.alert_toasts import AlertToasts
from neurowatch.widgets.camera_panel import CameraPanel
from neurowatch.widgets.compact_bar import CompactTitleBar
from neurowatch.widgets.detached_wall import DEFAULT_CYCLE_INTERVAL, DetachedWallWindow
from neurowatch.widgets.edition_notice import EditionNotice, open_unlimited
from neurowatch.widgets.events_view import EventsView
from neurowatch.widgets.page_strip import PageStripWidget
from neurowatch.widgets.performance_page import PerformancePage
from neurowatch.widgets.playback_view import PlaybackView
from neurowatch.widgets.status_bar_widget import StatusBarWidget
from neurowatch.widgets.status_overlay import DEFAULT_OPACITY, StatusOverlay
from neurowatch.widgets.title_bar import TitleBar
from neurowatch.widgets.toolbar import ToolBar
from neurowatch.widgets.video_wall import VideoWallWidget
from neurowatch.widgets.welcome_screen import StartupStage, WelcomeScreen
from neurowatch.widgets.window_chrome import guard_window_size, guard_windows_on_focus

logger = logging.getLogger(__name__)

_DEFAULT_ROWS, _DEFAULT_COLS = 4, 8  # 8 cols × 4 rows (matches "8×4 Grid" label)
# The page an automatic page change shows next is decoded this long before
# it: enough to connect and reach a keyframe, so it opens on live pictures
# instead of each tile catching up on its own.
_PREWARM_LEAD_MS = 3000
#: After a page is picked by hand, the page left and the pages either side
#: keep decoding this long, so going back or on is instant, as auto-scroll's
#: warmed next page is. Then only the visible page decodes again.
_BROWSE_WARM_MS = 15_000
#: A page whose dot (or arrow) the pointer leaves keeps decoding this long:
#: the pointer may pass the dot just before the click.
_HOVER_WARM_MS = 2_000

StartupProgress = Callable[[StartupStage, str], None]


def _active_config_path() -> str:
    override = os.environ.get("NW_CONFIG")
    if override:
        return override
    if sys.platform == "win32":
        config_dir = os.environ.get("APPDATA") or os.path.expanduser("~/AppData/Roaming")
    else:
        config_dir = os.environ.get("XDG_CONFIG_HOME") or os.path.expanduser("~/.config")
    return os.path.join(config_dir, "neurowatch", "config.json")


class BackendUnavailableError(RuntimeError):
    """Raised during startup when nw-backend cannot be reached."""

    def __init__(self, endpoint: str) -> None:
        super().__init__(f"Cannot connect to nw-backend at {endpoint}")
        self.endpoint = endpoint


class _IpcCallWorker(Worker):
    """Run one synchronous IPC call in a background thread."""

    result = Signal(object)  # RPC result dict/object or Exception

    def __init__(
        self, method: str, params: dict | list | None = None, endpoint: str | None = None
    ) -> None:
        super().__init__()
        self._method = method
        self._params: dict | list = params if params is not None else {}
        self._endpoint = endpoint

    def run(self) -> None:
        try:
            client = SyncIpcClient(endpoint=self._endpoint)
            self.result.emit(client.call(self._method, self._params))
        except Exception as e:
            self.result.emit(e)


class _StartupApplyWorker(Worker):
    """Register what starts at sign-in and at boot (``systemctl``,
    ``loginctl``, login items) off the GUI thread."""

    done = Signal(list)  # what could not be done, in plain words

    def __init__(self, endpoint: str, prefs: StartupPrefs) -> None:
        super().__init__()
        self._endpoint = endpoint
        self._prefs = prefs

    def run(self) -> None:
        try:
            problems = ServiceController(endpoint=self._endpoint).apply_startup(self._prefs)
        except Exception as exc:  # reported, never raised in Qt
            logger.exception("Applying the startup choices failed")
            problems = [str(exc)]
        self.done.emit(problems)


def _startup_note() -> str | None:
    """Why Settings → Startup cannot change anything here, if it cannot."""
    if os.environ.get("NW_SERVICE") == "external":
        return "NeuroWatch's background service is managed outside this app (NW_SERVICE=external)."
    if service_follows_ui():
        return (
            "Running from source: NeuroWatch starts with this window and stops when it"
            " closes, so it does not start at sign-in (NW_DEV_KEEP_SERVICE=1 behaves as"
            " installed builds do)."
        )
    return None


class _SettingsSaveWorker(Worker):
    """Persist settings without freezing Qt during pipeline reconfiguration.

    A changed video decoder makes the backend restart after answering
    ``config.set`` (``restarting``); the window rides that out like any
    backend restart.

    A recording drive picked in Settings is prepared first (``storage.select``
    creates and checks its folder); if that fails, the other settings are
    still saved and the recording location stays as it was.
    """

    result = Signal(object)  # refreshed config dict or Exception
    drive_failed = Signal(str)  # why the chosen drive was not used

    def __init__(self, endpoint: str, config: dict, drive: str | None = None) -> None:
        super().__init__()
        self._endpoint = endpoint
        self._config = deepcopy(config)
        self._drive = drive

    def run(self) -> None:
        try:
            client = SyncIpcClient(endpoint=self._endpoint)
            if self._drive is not None:
                recording = self._config.setdefault("recording", {})
                try:
                    selected = client.call("storage.select", {"mount_point": self._drive})
                    if isinstance(selected, dict) and selected.get("storage_path"):
                        recording["storage_path"] = selected["storage_path"]
                except IpcError as exc:
                    recording.pop("storage_path", None)
                    self.drive_failed.emit(exc.message)
            reply = client.call("config.set", self._config)
            if restart_reply(reply):
                # The recorder restarts to apply a new video decoder choice:
                # it saved what was sent, and is not there to ask right now.
                logger.info("Backend restarting to apply the video decoder choice")
                self.result.emit(self._config)
                return
            self.result.emit(client.call("config.get") or self._config)
        except Exception as exc:
            self.result.emit(exc)


class _PollWorker(Worker):
    """Background thread that polls camera states every 2 s without blocking the UI."""

    poll_done = Signal(dict, dict, dict, bool)  # states, stream_infos, recording_states, backend_ok
    #: ``stats.get_performance`` (CPU, memory, network for the status bar),
    #: or None when the backend is unreachable.
    performance = Signal(object)

    def __init__(self, endpoint: str, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self._endpoint = endpoint

    def run(self) -> None:
        while not self.isInterruptionRequested():
            try:
                client = SyncIpcClient(endpoint=self._endpoint)
                states = client.call("camera.status_all") or {}
                try:
                    stream_infos = client.call("camera.stream_info_all") or {}
                except Exception:
                    # Older backend: codec strings only, no resolution.
                    stream_infos = client.call("camera.codec_all") or {}
                try:
                    rec_states = client.call("recording.status_all") or {}
                except Exception:
                    rec_states = {}
                self.poll_done.emit(
                    states if isinstance(states, dict) else {},
                    stream_infos if isinstance(stream_infos, dict) else {},
                    rec_states if isinstance(rec_states, dict) else {},
                    True,
                )
                try:
                    self.performance.emit(client.call("stats.get_performance"))
                except Exception:
                    self.performance.emit(None)
            except Exception:
                self.poll_done.emit({}, {}, {}, False)
                self.performance.emit(None)
            # Sleep in 200 ms increments so interruption is responsive
            for _ in range(10):
                if self.isInterruptionRequested():
                    return
                self.msleep(200)


def _delete_after_use(dialog: QDialog) -> None:
    """A dialog shown with ``exec()`` is the window's child: without this it
    stays alive, hidden, until the application quits, and its chrome gets
    events while Python shuts down. Deletion waits for the event loop, so the
    caller still reads the dialog's values after ``exec()`` returns."""
    dialog.finished.connect(dialog.deleteLater)


def tray_running() -> bool:
    """Whether the tray (which shows desktop alerts itself) runs for this user."""
    from neurowatch.service.lock import InstanceLock
    from neurowatch.service.paths import lock_path

    lock = InstanceLock(lock_path("tray"))
    if lock.acquire():
        lock.release()
        return False
    return True


class MainWindow(QMainWindow):
    """Neurowatch main window — frameless with custom title bar."""

    def __init__(self, progress: StartupProgress | None = None) -> None:
        super().__init__()
        self._progress: StartupProgress = progress or (lambda _stage, _detail: None)
        self.setWindowFlags(Qt.WindowType.FramelessWindowHint)
        self.setMinimumSize(1280, 720)
        # GNOME doubles a frameless window when a dialog opens over it.
        self._size_guard = guard_window_size(self)
        self.setWindowTitle("NeuroWatch")

        self._ipc = SyncIpcClient()
        self._cameras: list[CameraConfig] = []
        self._selected_cam_id: str | None = None
        self._config: dict = {}
        self._backend_online: bool = True
        self._add_worker: _IpcCallWorker | None = None
        self._edit_workers: dict[str, _IpcCallWorker] = {}
        self._ptz_dialogs: dict[str, Any] = {}
        self._talk_dialogs: dict[str, Any] = {}
        self._settings_worker: _SettingsSaveWorker | None = None
        self._startup_worker: _StartupApplyWorker | None = None
        # One camera.set_positions save at a time; a later edit re-saves.
        self._positions_worker: _IpcCallWorker | None = None
        self._positions_pending = False
        # Extra windows, each holding pages of cameras moved off this wall.
        self._detached: list[DetachedWallWindow] = []
        # Seconds between page changes, the same for every window.
        self._page_interval = DEFAULT_CYCLE_INTERVAL
        self._wall_config_worker: _IpcCallWorker | None = None
        self._wall_config_pending = False
        # Extra windows are saved in the config (desktop.windows) and reopen
        # at the next start; saves are batched and run on a worker.
        self._restoring_windows = False
        self._windows_worker: _IpcCallWorker | None = None
        self._windows_save_pending = False
        self._ignore_config_changes_until = 0.0
        self._pre_solo_grid: tuple[int, int, int] | None = None
        self._rec_states: dict[str, str] = {}
        self._shutting_down = False
        # Extensions' callbacks, run when the window shuts down.
        self._shutdown_callbacks: list[Callable[[], object]] = []

        self._progress(StartupStage.CONNECTING, "")
        self._require_backend()
        endpoint = str(getattr(self._ipc, "endpoint", getattr(self._ipc, "_endpoint", "")))
        self._quality_dispatcher = CameraQualityDispatcher(endpoint, self)
        self._quality_dispatcher.failed.connect(self._on_camera_quality_failed)
        self._progress(StartupStage.PREPARING_WORKSPACE, "")
        self._build_ui()
        app = QApplication.instance()
        if isinstance(app, QApplication):
            apply_app_theme(app, "dark")
        cloud_desktop.install(self, lambda: self._open_settings(tab="cloud"))
        self._progress(StartupStage.LOADING_CAMERAS, "")
        # Startup load errors are deferred until the welcome screen is gone.
        self._startup_load_error: tuple[bool, str, str] | None = None
        self._load_from_backend(during_startup=True)

    # ------------------------------------------------------------------
    # Backend startup guard
    # ------------------------------------------------------------------

    def _require_backend(self) -> None:
        for attempt in range(3):
            if self._ipc.is_online():
                logger.info("Backend connection established at %s", self._ipc._endpoint)
                return
            logger.warning("Backend not reachable (attempt %d/3)", attempt + 1)
            if attempt < 2:
                self._progress(
                    StartupStage.CONNECTING,
                    f"Backend not reachable — retrying (attempt {attempt + 2} of 3)",
                )
                _wait_responsively(1000)
        raise BackendUnavailableError(self._ipc._endpoint)

    # ------------------------------------------------------------------
    # UI construction
    # ------------------------------------------------------------------

    def _build_ui(self) -> None:
        root = QWidget()
        self.setCentralWidget(root)
        root.setStyleSheet("background-color: #12121c;")

        layout = QVBoxLayout(root)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(0)
        self._root_layout = layout

        # Compact view ("Hide panels"): only the wall, under a slim strip
        # laid out like an extra window's.
        self._compact = False
        self._compact_bar = CompactTitleBar()
        self._compact_bar.previous_page.connect(self._on_prev_page)
        self._compact_bar.next_page.connect(self._on_next_page)
        self._compact_bar.new_window.connect(self._on_new_window)
        self._compact_bar.show_panels.connect(lambda: self._set_compact(False))
        self._compact_bar.hide()
        layout.addWidget(self._compact_bar)

        # Title bar
        self._title_bar = TitleBar()
        self._title_bar.tab_changed.connect(self._on_tab_changed)
        self._title_bar.export_config_requested.connect(self._on_export_config)
        self._title_bar.export_video_requested.connect(self._on_export_video)
        self._title_bar.import_config_requested.connect(self._on_import_config)
        self._title_bar.settings_requested.connect(self._open_settings)
        self._title_bar.alerts_requested.connect(self._open_alerts)
        self._title_bar.users_requested.connect(self._open_users)
        self._title_bar.change_password_requested.connect(self._change_password)
        self._title_bar.sign_out_requested.connect(self._sign_out)
        self._title_bar.apply_role()
        self._title_bar.about_requested.connect(self._on_about)
        self._title_bar.hide_panels_requested.connect(lambda: self._set_compact(True))
        layout.addWidget(self._title_bar)

        # Toolbar — use safe defaults until config loaded from backend
        self._toolbar = ToolBar()
        self._toolbar.apply_role()
        self._toolbar.grid_changed.connect(self._on_grid_changed)
        self._toolbar.add_camera.connect(self._on_add_camera)
        self._toolbar.edit_camera.connect(self._on_edit_camera_selected)
        self._toolbar.ptz_requested.connect(lambda: self._on_ptz(self._selected_cam_id))
        self._toolbar.remove_camera.connect(self._on_remove_camera_selected)
        self._toolbar.prev_page.connect(self._on_prev_page)
        self._toolbar.next_page.connect(self._on_next_page)
        self._toolbar.fullscreen_toggled.connect(self._toggle_fullscreen)
        self._toolbar.auto_fit_toggled.connect(self._on_auto_fit_toggled)
        self._toolbar.custom_grid_requested.connect(self._on_custom_grid)
        self._toolbar.auto_scroll_changed.connect(self._on_auto_scroll_changed)
        self._toolbar.new_window_requested.connect(self._on_new_window)
        self._toolbar.set_grid(_DEFAULT_ROWS, _DEFAULT_COLS)
        self._toolbar.set_auto_fit(True)
        layout.addWidget(self._toolbar)

        # Content: live view (wall + camera panel) stacked with playback view
        live_content = QWidget()
        live_layout = QHBoxLayout(live_content)
        live_layout.setContentsMargins(0, 0, 0, 0)
        live_layout.setSpacing(0)

        self._viewer_client = NativeViewerClient(self._ipc)
        self._wall = VideoWallWidget(viewer_client=self._viewer_client)
        self._wall.cell_expand_requested.connect(self._on_expand_cell)
        self._wall.cell_single_click.connect(self._on_cell_single_click)
        self._wall.cell_add_camera_requested.connect(self._on_add_camera)
        self._wall.layout_edit_requested.connect(self._on_wall_layout_edit)
        self._wall.drag_page_changed.connect(lambda _page: self._update_page_display())
        self._wall.camera_quality_requested.connect(self._on_camera_quality_requested)
        self._wall.cell_edit_requested.connect(self._on_edit_camera)
        self._wall.cell_ptz_requested.connect(self._on_ptz)
        self._wall.cell_bookmark_requested.connect(self._on_bookmark)
        self._wall.cell_talk_requested.connect(self._on_talk)
        self._wall.cell_remove_requested.connect(self._on_remove_camera)
        self._wall.set_auto_fit(True)
        self._wall.set_grid(_DEFAULT_ROWS, _DEFAULT_COLS)

        live_layout.addWidget(self._wall, stretch=1)

        self._camera_panel = CameraPanel()
        self._camera_panel.camera_selected.connect(self._on_camera_selected)
        self._camera_panel.add_clicked.connect(self._on_add_camera)
        self._camera_panel.refresh_clicked.connect(self._load_from_backend)
        self._camera_panel.edit_requested.connect(self._on_edit_camera)
        self._camera_panel.remove_requested.connect(self._on_remove_camera)
        self._camera_panel.fullscreen_requested.connect(self._on_expand_cell)
        self._wall.camera_status_changed.connect(self._camera_panel.set_camera_status)
        live_layout.addWidget(self._camera_panel)

        self._playback_view = PlaybackView(self._ipc, self._cameras)
        self._playback_view.export_requested.connect(self._export_event)

        endpoint = str(getattr(self._ipc, "endpoint", getattr(self._ipc, "_endpoint", "")))
        self._performance_page = PerformancePage(
            client_factory=lambda: SyncIpcClient(endpoint=endpoint),
            refresh_interval_ms=2000,
        )

        self._events_view = EventsView(endpoint or None, self._cameras)
        self._events_view.play_requested.connect(self._play_event)
        self._events_view.export_requested.connect(self._export_event)

        self._content_stack = QStackedWidget()
        self._content_stack.addWidget(live_content)  # index 0 — live
        self._content_stack.addWidget(self._playback_view)  # index 1 — playback
        self._content_stack.addWidget(self._performance_page)  # index 2 — diagnostics
        self._content_stack.addWidget(self._events_view)  # index 3 — events

        layout.addWidget(self._content_stack, stretch=1)

        # Page dot strip (auto-hides when page_count == 1). The holder hides
        # it in compact view, whatever the strip decides for itself.
        self._page_strip_holder = QWidget()
        holder_layout = QVBoxLayout(self._page_strip_holder)
        holder_layout.setContentsMargins(0, 0, 0, 0)
        self._page_strip = PageStripWidget()
        self._page_strip.page_clicked.connect(self._on_page_strip_clicked)
        self._page_strip.page_hovered.connect(self._on_page_hovered)
        holder_layout.addWidget(self._page_strip)
        layout.addWidget(self._page_strip_holder)

        # Status bar
        self._status_bar = StatusBarWidget()
        self._edition = Edition()
        self._edition_notice: EditionNotice | None = None
        layout.addWidget(self._status_bar)
        # Holds the status readings over the wall while the fullscreen strip
        # of the compact view is hidden (ui.fullscreen_status).
        self._status_overlay = StatusOverlay(self._wall)

        # Poll backend for camera states in a background thread (never blocks the UI)
        # Only the cameras this wall shows are decoded, and only while it shows them.
        self._view_lease = ViewLeaseWorker(endpoint, self)
        self._view_lease.start()
        # The camera heard live (one for the whole app), or None.
        self._listening: str | None = None
        self._wall.listen_toggled.connect(self._toggle_listen)
        # Cameras of the pages the page clock shows next (main wall / windows).
        self._prewarm_main: frozenset[str] = frozenset()
        self._prewarm_windows: frozenset[str] = frozenset()
        self._prewarm_timer = QTimer(self)
        self._prewarm_timer.setSingleShot(True)
        self._prewarm_timer.timeout.connect(self._prewarm_next_pages)
        # Manual browsing: the page pointed at, and the pages around the one
        # just picked (see _BROWSE_WARM_MS / _HOVER_WARM_MS).
        self._hover_warm: frozenset[str] = frozenset()
        self._browse_warm: frozenset[str] = frozenset()
        self._hover_warm_timer = QTimer(self)
        self._hover_warm_timer.setSingleShot(True)
        self._hover_warm_timer.timeout.connect(lambda: self._set_hover_warm(frozenset()))
        self._browse_warm_timer = QTimer(self)
        self._browse_warm_timer.setSingleShot(True)
        self._browse_warm_timer.timeout.connect(lambda: self._set_browse_warm(frozenset()))
        self._wall.view_demand_changed.connect(self._sync_view_demand)
        self._content_stack.currentChanged.connect(self._sync_view_demand)
        self._sync_view_demand()

        self._poll_worker = _PollWorker(endpoint, self)
        self._poll_worker.poll_done.connect(self._on_poll_done)
        self._poll_worker.performance.connect(self._status_bar.set_performance)
        self._poll_worker.start()

        # Events as they happen: tiles show MOTION while a camera reports it,
        # the Events tab lists them and counts new ones while it is not shown.
        self._activity = ActivityTracker()
        self._motion_cameras: set[str] = set()
        self._pending_motion_cameras: set[str] = set()
        self._motion_ack_workers: dict[str, _IpcCallWorker] = {}
        self._wall.motion_acknowledge_requested.connect(self._acknowledge_motion)
        self._unseen_events = 0
        self._event_feed = EventFeedWorker(endpoint or None)
        self._event_feed.started_over.connect(self._on_events_started_over)
        self._event_feed.pending_over.connect(self._on_pending_events)
        self._event_feed.notices.connect(self._on_event_notices)
        self._event_feed.start()
        # Desktop alerts: the tray shows them as system notifications; when
        # no tray runs (a source checkout), toasts in this window do.
        self._alert_toasts = AlertToasts(self.centralWidget())
        self._alert_toasts.opened.connect(lambda _alert: self._title_bar.select_tab("events"))
        self._alert_feed = AlertFeedWorker(endpoint or None)
        self._alert_feed.alert.connect(self._on_desktop_alert)
        self._alert_feed.start()
        # Events without duration (a line crossed) show for a few seconds.
        self._activity_timer = QTimer(self)
        self._activity_timer.setInterval(1000)
        self._activity_timer.timeout.connect(self._apply_activity)
        self._activity_timer.start()

        # The one page clock: the main wall's auto-scroll and every extra
        # window change pages on it, at the same interval and moment.
        self._autoscroll_timer = QTimer(self)
        self._autoscroll_timer.timeout.connect(self._on_page_clock)

        self._windows_save_timer = QTimer(self)
        self._windows_save_timer.setSingleShot(True)
        self._windows_save_timer.setInterval(800)
        self._windows_save_timer.timeout.connect(self._save_windows)

        self._fullscreen_hide_timer = QTimer(self)
        self._fullscreen_hide_timer.setSingleShot(True)
        self._fullscreen_hide_timer.setInterval(3000)
        self._fullscreen_hide_timer.timeout.connect(self._hide_fullscreen_bars)

        self._proximity_timer = QTimer(self)
        self._proximity_timer.setInterval(200)
        self._proximity_timer.timeout.connect(self._check_mouse_proximity)

        _config_path = _active_config_path()
        self._config_watcher = QFileSystemWatcher([_config_path], self)
        self._config_watcher.fileChanged.connect(self._on_config_file_changed)
        self._config_reload_timer = QTimer(self)
        self._config_reload_timer.setSingleShot(True)
        self._config_reload_timer.setInterval(500)
        self._config_reload_timer.timeout.connect(self._reload_config_after_change)

        self._update_page_display()

        # Start (and return, on a click) keyboard focus on the wall, not the
        # first title-bar button: a focused button draws a focus highlight.
        # The wall handles no keys itself, so shortcuts still reach
        # keyPressEvent.
        self._wall.setFocusPolicy(Qt.FocusPolicy.ClickFocus)
        self._wall.setFocus(Qt.FocusReason.OtherFocusReason)

    # ------------------------------------------------------------------
    # Backend load / reload
    # ------------------------------------------------------------------

    def _load_from_backend(self, during_startup: bool = False) -> None:
        error: tuple[bool, str, str] | None
        try:
            backend_status = self._ipc.call("system.status") or {}
            self._config = self._ipc.call("config.get") or {}
            raw_cams = self._ipc.call("camera.list")
            self._cameras = [CameraConfig.from_dict(c) for c in (raw_cams or [])]
        except BackendOfflineError:
            logger.warning("Backend offline while loading camera list")
            error = (
                True,
                "Backend Offline",
                "Lost connection to nw-backend. Please restart the backend.",
            )
        except IpcError as e:
            logger.warning("IPC error loading camera list: %s", e.message)
            error = (False, "Backend Error", e.message)
        else:
            error = None
        if error is not None:
            if during_startup:
                self._startup_load_error = error
            else:
                self._show_load_error(error)
            return

        logger.info("Camera list loaded from backend: %d camera(s)", len(self._cameras))
        if isinstance(backend_status, dict):
            self._status_bar.set_video_acceleration(backend_status.get("video_acceleration", {}))
        self._set_edition(Edition.from_status(backend_status))
        self._apply_settings()
        self._refresh_all()

    def _show_load_error(self, error: tuple[bool, str, str]) -> None:
        critical, title, message = error
        if critical:
            show_message(self, title, message, kind="error")
        else:
            show_message(self, title, message, kind="warning")

    def report_startup_error(self) -> None:
        """Show a load error deferred from construction, once startup is done."""
        error, self._startup_load_error = self._startup_load_error, None
        if error is not None:
            self._show_load_error(error)

    # ------------------------------------------------------------------
    # Camera management
    # ------------------------------------------------------------------

    def _refresh_all(self, page: int | None = None) -> None:
        self._refresh_walls(page)
        for window in list(self._detached):
            window.update_cameras(self._cameras)
        self._camera_panel.set_cameras(self._cameras)
        self._playback_view.refresh_camera_list(self._cameras)
        self._events_view.set_cameras(self._cameras)
        self._performance_page.set_camera_names({c.id: c.name or c.id for c in self._cameras})

    def _refresh_walls(self, page: int | None = None) -> None:
        """Re-lay this window's wall only: the camera list itself is unchanged
        (cameras moved between windows), so the sidebar keeps its rows.
        Without ``page`` the wall stays on its page (a saved, added or removed
        camera, a reload): it used to jump back to the first."""
        if page is None:
            page = self._wall.page
        self._wall.set_cameras(self._main_wall_cameras(), page=page)
        self._update_page_display()

    def _on_camera_selected(self, cam_id: str) -> None:
        self._selected_cam_id = cam_id if cam_id else None
        self._toolbar.set_camera_selected(bool(cam_id))

    def _ipc_endpoint(self) -> str:
        return str(getattr(self._ipc, "endpoint", getattr(self._ipc, "_endpoint", "")))

    def _on_add_camera(self) -> None:
        dlg = AddEditCameraDialog(
            parent=self,
            ipc_endpoint=self._ipc_endpoint() or None,
            existing_names=[c.name for c in self._cameras],
        )
        _delete_after_use(dlg)
        accepted = dlg.exec() == AddEditCameraDialog.DialogCode.Accepted
        # Cameras added from the search (the backend already has them), also
        # when the dialog was closed after adding some.
        added = dlg.added_cameras()
        for camera in added:
            self._on_add_camera_result(camera, CameraConfig.from_dict(camera))
        if not accepted or added:
            return
        cam = dlg.get_config()
        if self._add_worker and self._add_worker.isRunning():
            show_message(self, "Add Camera", "A camera is already being added.")
            return

        logger.info("Requesting supervisor start for new camera '%s'", cam.name)
        endpoint = str(getattr(self._ipc, "endpoint", getattr(self._ipc, "_endpoint", "")))
        worker = _IpcCallWorker("camera.add", cam.to_dict(), endpoint)
        worker.result.connect(
            lambda result, submitted=cam: self._on_add_camera_result(result, submitted)
        )
        worker.finished.connect(worker.deleteLater)
        worker.finished.connect(self._clear_add_worker)
        self._add_worker = worker
        worker.start()

    def _on_add_camera_result(self, result: object, submitted: CameraConfig) -> None:
        if isinstance(result, BackendOfflineError):
            show_message(
                self,
                "Backend Offline",
                "Lost connection to nw-backend. Please restart the backend.",
                kind="error",
            )
            return
        if isinstance(result, IpcError) and result.code == CAMERA_LIMIT_REACHED:
            if ask(
                self,
                "Camera Limit Reached",
                limit_message(result.data),
                yes_text="Get NeuroWatch Unlimited",
                no_text="Close",
            ):
                open_unlimited(self._edition)
            return
        if isinstance(result, IpcError):
            show_message(self, "Backend Error", result.message, kind="warning")
            return
        if isinstance(result, Exception):
            show_message(self, "Backend Error", str(result), kind="warning")
            return

        added_cam = CameraConfig.from_dict(result) if isinstance(result, dict) else submitted
        if not any(c.id == added_cam.id for c in self._cameras):
            self._cameras.append(added_cam)

        self._refresh_all()

    def _clear_add_worker(self) -> None:
        self._add_worker = None

    def closeEvent(self, event: QCloseEvent) -> None:
        self.shutdown()
        super().closeEvent(event)

    def shutdown(self) -> None:
        """Join workers on every exit path, including QApplication.quit()."""
        if self._shutting_down:
            return
        self._shutting_down = True
        unsaved_windows = self._windows_save_timer.isActive() or self._windows_save_pending
        for timer in self.findChildren(QTimer):
            timer.stop()
        for callback in self._shutdown_callbacks:
            try:
                callback()
            except Exception:
                logger.exception("An extension's shutdown callback failed")
        self._poll_worker.requestInterruption()
        self._poll_worker.wait()
        # Its long poll ends within seconds; it must not hold up closing.
        self._event_feed.requestInterruption()
        retire(self._event_feed)
        self._alert_feed.requestInterruption()
        retire(self._alert_feed)
        self._events_view.shutdown()
        for dialog in list(self._ptz_dialogs.values()):
            dialog.close()
        self._ptz_dialogs.clear()
        for dialog in list(self._talk_dialogs.values()):
            dialog.close()
        self._talk_dialogs.clear()
        for edit_worker in self._edit_workers.values():
            edit_worker.result.disconnect()
            retire(edit_worker)
        self._edit_workers.clear()
        for ack_worker in self._motion_ack_workers.values():
            ack_worker.result.disconnect()
            retire(ack_worker)
        self._motion_ack_workers.clear()
        self._view_lease.stop()
        self._quality_dispatcher.shutdown()
        worker = self._add_worker
        if worker is not None and worker.isRunning():
            worker.wait()
        self._add_worker = None
        settings_worker = self._settings_worker
        if settings_worker is not None and settings_worker.isRunning():
            settings_worker.wait()
        self._settings_worker = None
        startup_worker = self._startup_worker
        if startup_worker is not None and startup_worker.isRunning():
            startup_worker.wait()
        self._startup_worker = None
        windows_worker = self._windows_worker
        if windows_worker is not None and windows_worker.isRunning():
            windows_worker.wait()
        self._windows_worker = None
        if unsaved_windows:
            # The last move or resize of a window, before the windows close.
            try:
                self._ipc.call("config.set", {"desktop": self._windows_session()})
            except (BackendOfflineError, IpcError) as e:
                logger.warning("Could not save the open windows: %s", e)
        for window in list(self._detached):
            window.animate_close = False
            window.close()
        self._detached.clear()
        positions_worker = self._positions_worker
        if positions_worker is not None and positions_worker.isRunning():
            positions_worker.wait()
        self._positions_worker = None
        wall_config_worker = self._wall_config_worker
        if wall_config_worker is not None and wall_config_worker.isRunning():
            wall_config_worker.wait()
        self._wall_config_worker = None
        self._wall.shutdown()
        self._performance_page.close()
        self._playback_view.close()
        # Workers dialogs let go of while they still ran (utils.threads.retire):
        # destroying one still running at exit would abort the application.
        wait_for_retired(8000)

    def _on_ptz(self, cam_id: str | None) -> None:
        from neurowatch.dialogs.ptz_dialog import PtzDialog

        camera = next((cam for cam in self._cameras if cam.id == cam_id), None)
        if camera is None:
            return
        if camera.id not in self._ptz_dialogs:
            dialog = PtzDialog(self._ipc_endpoint(), camera.id, camera.name, self)
            self._ptz_dialogs[camera.id] = dialog
            dialog.closed.connect(lambda: self._ptz_dialogs.pop(camera.id, None))
        dialog = self._ptz_dialogs[camera.id]
        dialog.show()
        dialog.raise_()
        dialog.activateWindow()

    def _open_alerts(self) -> None:
        from neurowatch.dialogs.alerts_dialog import AlertsDialog

        cameras = [(cam.id, cam.name) for cam in self._cameras]
        dialog = AlertsDialog(self._ipc_endpoint(), cameras, self)
        dialog.exec()
        dialog.deleteLater()

    def _open_users(self) -> None:
        from neurowatch.dialogs.users_dialog import UsersDialog

        cameras = [(cam.id, cam.name) for cam in self._cameras]
        dialog = UsersDialog(self._ipc_endpoint(), cameras, self)
        dialog.exec()
        dialog.deleteLater()
        if dialog.requires_sign_in_now:
            self._sign_in_again("Sign-in is now required. Sign in with an admin account.")

    def _change_password(self) -> None:
        from neurowatch.dialogs.users_dialog import ChangePasswordDialog

        dialog = ChangePasswordDialog(self._ipc_endpoint(), self)
        changed = dialog.exec()
        dialog.deleteLater()
        if changed:
            self._sign_in_again("Your password was changed. Sign in with the new one.")

    def _sign_out(self) -> None:
        from neurowatch.dialogs.sign_in import sign_out

        sign_out(self._ipc_endpoint())
        self._sign_in_again("Signed out.")

    def _sign_in_again(self, note: str) -> None:
        """Sign in (perhaps as someone else) or quit; then reload what this
        account may see."""
        from neurowatch.dialogs.sign_in import SignInPrompt

        if SignInPrompt(self._ipc_endpoint(), note=note).choose() != "sign_in":
            QApplication.quit()
            return
        self._title_bar.apply_role()
        self._toolbar.apply_role()
        self._load_from_backend()

    def _on_desktop_alert(self, alert: dict) -> None:
        from neurowatch.ipc.auth import current_user

        # The tray cannot read alerts of a recorder that requires sign-in.
        if not tray_running() or current_user() is not None:
            self._alert_toasts.show_alert(alert)

    def _on_talk(self, cam_id: str) -> None:
        """Talk through a camera's speaker (push-to-talk window)."""
        from neurowatch.dialogs.talk_dialog import TalkDialog

        camera = next((cam for cam in self._cameras if cam.id == cam_id), None)
        if camera is None:
            return
        if camera.id not in self._talk_dialogs:
            dialog = TalkDialog(self._ipc_endpoint(), camera.id, camera.name, self)
            self._talk_dialogs[camera.id] = dialog
            dialog.closed.connect(lambda: self._talk_dialogs.pop(camera.id, None))
        dialog = self._talk_dialogs[camera.id]
        dialog.show()
        dialog.raise_()
        dialog.activateWindow()

    def _on_bookmark(self, cam_id: str) -> None:
        """Bookmark this moment of a live camera."""
        from neurowatch.dialogs.bookmark_dialog import BookmarkDialog

        camera = next((cam for cam in self._cameras if cam.id == cam_id), None)
        if camera is None:
            return
        at_ms = int(time.time() * 1000)
        dialog = BookmarkDialog(self._ipc_endpoint(), [(camera.id, camera.name)], at_ms, self)
        dialog.exec()
        dialog.deleteLater()

    def _on_edit_camera_selected(self) -> None:
        if self._selected_cam_id:
            self._on_edit_camera(self._selected_cam_id)

    def _on_edit_camera(self, cam_id: str) -> None:
        if cam_id in self._edit_workers:
            show_message(self, "Save camera", "This camera is still being saved.")
            return
        cam = next((c for c in self._cameras if c.id == cam_id), None)
        if cam is None:
            return
        was_recording = self._rec_states.get(cam_id) == "recording"
        dlg = AddEditCameraDialog(
            camera=cam,
            is_recording=was_recording,
            parent=self,
            ipc_endpoint=self._ipc_endpoint() or None,
        )
        dlg.recording_policy_copied.connect(self._on_recording_policy_copied)
        _delete_after_use(dlg)
        if dlg.exec() != AddEditCameraDialog.DialogCode.Accepted:
            return
        updated = dlg.get_config()
        # camera.update reconciles automatic recording. Explicit Start/Stop
        # here would create a manual override and bypass an Off schedule.
        # The reply carries the saved camera: the config file it rewrites needs
        # no reload (a full reload of every setting and camera; see
        # `_on_config_file_changed`).
        self._ignore_config_changes_until = time.monotonic() + 2.0
        worker = _IpcCallWorker("camera.update", updated.to_dict(), self._ipc_endpoint())
        worker.result.connect(self._on_camera_updated)
        worker.finished.connect(lambda: self._edit_workers.pop(cam_id, None))
        worker.finished.connect(worker.deleteLater)
        self._edit_workers[cam_id] = worker
        worker.start()

    def _on_camera_updated(self, result: object) -> None:
        self._ignore_config_changes_until = time.monotonic() + 2.0
        if isinstance(result, Exception):
            show_message(self, "Save camera", str(result), kind="warning")
            return
        if isinstance(result, dict):
            updated = CameraConfig.from_dict(result)
            self._cameras = [updated if cam.id == updated.id else cam for cam in self._cameras]
            self._refresh_all()

    def _on_recording_policy_copied(self, target_ids: list[str], policy: dict) -> None:
        # The copy is already persisted, including when the source dialog is
        # canceled. Keep later edits from submitting stale target policies.
        for cam in self._cameras:
            if cam.id in target_ids:
                for key, value in policy.items():
                    setattr(cam.recording, key, deepcopy(value))

    def _on_remove_camera_selected(self) -> None:
        if self._selected_cam_id:
            self._on_remove_camera(self._selected_cam_id)

    def _on_remove_camera(self, cam_id: str) -> None:
        cam = next((c for c in self._cameras if c.id == cam_id), None)
        if cam is None:
            return
        if not confirm_action(
            self,
            "Remove Camera",
            f"Remove '{cam.name}'?",
            confirm_text="Remove",
            destructive=True,
        ):
            return
        logger.info("Requesting supervisor stop for cam_id=%s ('%s')", cam_id, cam.name)
        try:
            self._ipc.call("camera.remove", {"id": cam_id})
            raw = self._ipc.call("camera.list")
            self._cameras = [CameraConfig.from_dict(c) for c in raw]
            # A camera held over the free limit may run now.
            self._set_edition(Edition.from_status(self._ipc.call("system.status")))
        except BackendOfflineError:
            show_message(
                self,
                "Backend Offline",
                "Lost connection to nw-backend. Please restart the backend.",
                kind="error",
            )
            return
        except IpcError as e:
            show_message(self, "Backend Error", e.message, kind="warning")
            return
        if self._selected_cam_id == cam_id:
            self._selected_cam_id = None
            self._toolbar.set_camera_selected(False)
        self._refresh_all()

    # ------------------------------------------------------------------
    # Grid / page navigation
    # ------------------------------------------------------------------

    def _on_grid_changed(self, rows: int, cols: int) -> None:
        self._toolbar.set_grid(rows, cols)
        self._wall.set_grid(rows, cols)
        self._config.setdefault("wall", {})
        self._config["wall"]["rows"] = rows
        self._config["wall"]["cols"] = cols
        # On a worker, and marked as our own change: the config file watcher
        # must not ask to reload a save this window made.
        self._save_wall_config()
        self._update_page_display()
        self._restart_autoscroll_timer()

    def _on_auto_scroll_changed(self, interval_sec: int) -> None:
        self._config.setdefault("wall", {})
        self._config["wall"]["page_interval_sec"] = interval_sec
        self._apply_autoscroll()
        self._save_wall_config()

    def _save_wall_config(self) -> None:
        """Save the wall settings on a worker, one save at a time."""
        if self._wall_config_worker is not None and self._wall_config_worker.isRunning():
            self._wall_config_pending = True
            return
        self._wall_config_pending = False
        self._ignore_config_changes_until = time.monotonic() + 2.0
        worker = _IpcCallWorker(
            "config.set", {"wall": dict(self._config.get("wall", {}))}, self._ipc_endpoint()
        )
        worker.finished.connect(worker.deleteLater)
        worker.finished.connect(self._on_wall_config_saved)
        self._wall_config_worker = worker
        worker.start()

    def _on_wall_config_saved(self) -> None:
        self._wall_config_worker = None
        self._ignore_config_changes_until = time.monotonic() + 2.0
        if self._wall_config_pending and not self._shutting_down:
            self._save_wall_config()

    def _on_custom_grid(self) -> None:
        grid = self._config.get("wall", {})
        dlg = CustomGridDialog(
            grid.get("rows", _DEFAULT_ROWS), grid.get("cols", _DEFAULT_COLS), self
        )
        _delete_after_use(dlg)
        if dlg.exec() == CustomGridDialog.DialogCode.Accepted:
            self._on_grid_changed(dlg.rows, dlg.cols)

    def _on_prev_page(self) -> None:
        self._navigate_to_page(self._wall.page - 1)

    def _on_next_page(self) -> None:
        self._navigate_to_page(self._wall.page + 1)

    def _navigate_to_page(self, page: int) -> None:
        """Manual navigation: always restarts the auto-scroll countdown."""
        left = self._wall.page
        # Page first: restarting the countdown drops the warmed next page,
        # which may be the one this shows.
        self._change_page(page)
        self._restart_autoscroll_timer()
        self._warm_while_browsing(left)

    def _warm_while_browsing(self, left: int) -> None:
        """Keep the page just left and the pages either side of this one
        decoding for a while: browsing on or back then shows live video at
        once instead of reconnecting (``_BROWSE_WARM_MS``)."""
        wall = self._wall
        pages = {left, wall.page - 1, wall.page + 1} - {wall.page}
        cameras: set[str] = set()
        for page in pages:
            if 0 <= page < wall.page_count:
                cameras.update(wall.page_demand(page))
        self._set_browse_warm(frozenset(cameras))
        self._browse_warm_timer.start(_BROWSE_WARM_MS)

    def _on_page_hovered(self, page: int) -> None:
        """The pointer is on a page's dot or arrow: start that page's video
        before the click; it lingers briefly after the pointer leaves."""
        wall = self._wall
        if 0 <= page < wall.page_count and page != wall.page:
            self._hover_warm_timer.stop()
            self._set_hover_warm(frozenset(wall.page_demand(page)))
        elif self._hover_warm:
            self._hover_warm_timer.start(_HOVER_WARM_MS)

    def _set_hover_warm(self, cameras: frozenset[str]) -> None:
        if cameras != self._hover_warm:
            self._hover_warm = cameras
            self._sync_view_demand()

    def _set_browse_warm(self, cameras: frozenset[str]) -> None:
        if cameras != self._browse_warm:
            self._browse_warm = cameras
            self._sync_view_demand()

    def _update_page_display(self) -> None:
        page = self._wall.page
        total = max(1, self._wall.page_count)
        self._toolbar.set_page_info(page + 1, total)
        self._page_strip.set_state(self._wall.page_count, page)
        can_spare = self._can_open_window()
        self._toolbar.set_new_window_enabled(can_spare)
        for window in self._detached:
            window.set_can_add_page(can_spare)
        count = len(self._main_wall_cameras())
        site = str(self._config.get("site_name") or "My Site")
        self._compact_bar.set_state(
            title=f"NeuroWatch — {site} · {count} camera{'s' if count != 1 else ''}",
            page=page,
            count=total,
            solo=self._pre_solo_grid is not None,
            can_open_window=can_spare,
        )

    # ------------------------------------------------------------------
    # Extra windows: a page of cameras in its own window
    # ------------------------------------------------------------------

    def _walls(self) -> list[VideoWallWidget]:
        return [self._wall, *(window.wall for window in self._detached)]

    def _detached_camera_ids(self) -> set[str]:
        return {cam_id for window in self._detached for cam_id in window.camera_ids}

    def _main_wall_cameras(self) -> list[CameraConfig]:
        """Cameras this window's wall shows: all but those in other windows."""
        detached = self._detached_camera_ids()
        return [cam for cam in self._cameras if cam.id not in detached]

    def _can_open_window(self) -> bool:
        """A page can move out only while this wall keeps another one."""
        return self._pre_solo_grid is None and self._wall.page_count > 1

    def _on_new_window(self, target: object = None) -> None:
        """Ask which page to move, and where: a new window or an open one
        (``target``, when asked from that window's "＋ Page")."""
        if not self._can_open_window():
            return
        rows, cols = self._wall._rows, self._wall._cols
        names = {cam.id: cam.name or "Camera" for cam in self._cameras}
        placements = self._wall.placements()
        pages = [
            PageSummary(
                index=page,
                tiles=[
                    (p.slot, names.get(p.cam_id, "Camera")) for p in placements if p.page == page
                ],
            )
            for page in range(self._wall.page_count)
        ]
        windows = [WindowSummary(w.number, w.page_count, len(w.camera_ids)) for w in self._detached]
        chosen = target.number if isinstance(target, DetachedWallWindow) else None
        dlg = NewWindowDialog(pages, rows, cols, self, windows=windows, destination=chosen)
        _delete_after_use(dlg)
        if dlg.exec() != QDialog.DialogCode.Accepted or dlg.selected_page is None:
            return
        window = next((w for w in self._detached if w.number == dlg.selected_window), None)
        if window is None:
            self._open_detached_window(dlg.selected_page, grid=dlg.selected_grid)
        else:
            self._move_page_to_window(dlg.selected_page, window)

    def _page_shown_after_removing(self, page: int) -> int:
        """Keep showing the same cameras here when ``page`` leaves."""
        current = self._wall.page
        remaining = self._wall.page_count - 1
        return current - 1 if current > page else min(current, remaining - 1)

    def _move_page_to_window(self, page: int, window: DetachedWallWindow) -> bool:
        """Move wall page ``page`` (0-based) into the open ``window``."""
        if not self._can_open_window() or window not in self._detached:
            return False
        placements = [p for p in self._wall.placements() if p.page == page]
        if not placements:
            return False
        shown = self._page_shown_after_removing(page)
        handover = self._wall.hand_over(p.cam_id for p in placements)
        window.add_page(placements, self._cameras, handover)
        logger.info("Page %d moved to window %d", page + 1, window.number)
        self._refresh_walls(page=shown)
        window.present()  # brings it forward, showing the page it just got
        self._sync_view_demand()
        return True

    def _return_window_page(self, window: object, page: int) -> None:
        """A window's "Return page": that page's cameras come back here."""
        if not isinstance(window, DetachedWallWindow) or window not in self._detached:
            return
        if window.page_count <= 1:
            window.close()  # its last page: the same as closing it
            return
        cam_ids = window.page_camera_ids(page)
        handover = window.wall.hand_over(cam_ids)
        window.remove_page(page)
        self._wall.adopt(handover)
        self._refresh_walls(page=self._wall.page)
        self._sync_view_demand()

    def _open_detached_window(
        self, page: int, grid: tuple[int, int] | None = None
    ) -> DetachedWallWindow | None:
        """Move wall page ``page`` (0-based) to a new window laid out in
        ``grid`` (rows, cols; default: this wall's grid)."""
        if not self._can_open_window():
            return None
        placements = [p for p in self._wall.placements() if p.page == page]
        if not placements:
            return None
        used = {window.number for window in self._detached}
        number = next(n for n in range(2, len(used) + 3) if n not in used)
        shown = self._page_shown_after_removing(page)
        # Their running readers and last pictures go along: tiles open at once.
        handover = self._wall.hand_over(p.cam_id for p in placements)
        rows, cols = grid or (self._wall._rows, self._wall._cols)
        window = DetachedWallWindow(
            number,
            self._cameras,
            placements,
            rows,
            cols,
            auto_fit=bool(self._config.get("wall", {}).get("auto_fit", True)),
            viewer_client=self._viewer_client,
            handover=handover,
            cycle_interval=self._page_interval,  # the same for every window
        )
        self._connect_window(window)
        logger.info("Page %d moved to window %d (%d cameras)", page + 1, number, len(placements))
        self._refresh_walls(page=shown)
        window.place_near(self)
        window.present()
        self._sync_view_demand()
        self._schedule_windows_save()
        return window

    def _preview_fps(self) -> PreviewFps:
        return PreviewFps.from_config(self._config.get("ui", {}))

    def _connect_window(self, window: DetachedWallWindow) -> None:
        window.wall.set_preview_fps(self._preview_fps())
        window.wall.set_motion_cameras(self._motion_cameras)
        window.wall.set_pending_motion_cameras(self._pending_motion_cameras)
        window.wall.motion_acknowledge_requested.connect(self._acknowledge_motion)
        window.closed.connect(self._on_detached_closed)
        window.add_page_requested.connect(self._on_new_window)
        window.return_page_requested.connect(self._return_window_page)
        window.cycling_changed.connect(self._sync_page_clock)
        # A page picked by hand restarts the clock for everyone, so all
        # windows keep changing pages together.
        window.navigated_by_hand.connect(self._restart_autoscroll_timer)
        window.interval_chosen.connect(self._set_page_interval)
        window.view_demand_changed.connect(self._sync_view_demand)
        window.wall.listen_toggled.connect(self._toggle_listen)
        window.wall.set_listening(self._listening)
        window.layout_changed.connect(self._schedule_windows_save)
        window.wall.camera_quality_requested.connect(self._on_camera_quality_requested)
        window.wall.cell_edit_requested.connect(self._on_edit_camera)
        window.wall.cell_ptz_requested.connect(self._on_ptz)
        window.wall.cell_bookmark_requested.connect(self._on_bookmark)
        window.wall.cell_talk_requested.connect(self._on_talk)
        window.wall.cell_remove_requested.connect(self._on_remove_camera)
        self._detached.append(window)

    # ------------------------------------------------------------------
    # Open windows are saved in the config and reopen at the next start
    # ------------------------------------------------------------------

    def restore_windows(self) -> None:
        """Reopen the extra windows saved in the config, each with its
        cameras, layout and place. Call once, after this window is shown.
        Cameras removed since are left out; a window left with none stays
        closed."""
        saved = saved_windows(self._config)
        if not saved or self._detached:
            return
        known = {cam.id for cam in self._cameras}
        taken: set[str] = set()
        auto_fit = bool(self._config.get("wall", {}).get("auto_fit", True))
        self._restoring_windows = True
        try:
            for entry in saved:
                tiles = [t for t in entry.tiles if t.camera_id in known - taken]
                if not tiles:
                    continue
                ids = [t.camera_id for t in tiles]
                taken.update(ids)
                # Their running readers go along, as when a page moves out.
                handover = self._wall.hand_over(ids)
                placements = [
                    Placement(t.camera_id, 0, t.row, t.col, t.row_span, t.col_span) for t in tiles
                ]
                window = DetachedWallWindow(
                    entry.number,
                    self._cameras,
                    placements,
                    entry.rows,
                    entry.cols,
                    auto_fit=auto_fit,
                    viewer_client=self._viewer_client,
                    handover=handover,
                    cycle_interval=self._page_interval,
                )
                self._connect_window(window)
                window.restore_session(replace(entry, tiles=tiles))
                state = window.place_saved(entry.geometry, self)
                window.present()
                if state == "maximized":
                    window.showMaximized()
                elif state == "fullscreen":
                    window.showFullScreen()
                logger.info("Window %d reopened with %d cameras", entry.number, len(ids))
            self._refresh_walls(page=0)
            self._sync_page_clock()
            self._sync_view_demand()
        finally:
            self._restoring_windows = False
        self._schedule_windows_save()  # without the cameras that are gone

    def _windows_session(self) -> dict:
        windows = sorted(self._detached, key=lambda w: w.number)
        return {"windows": [window.session().to_dict() for window in windows]}

    def _schedule_windows_save(self) -> None:
        """Save the open windows shortly: a drag or resize saves once."""
        if self._restoring_windows or self._shutting_down:
            return
        self._windows_save_timer.start()

    def _save_windows(self) -> None:
        if self._shutting_down:
            return
        if self._windows_worker is not None and self._windows_worker.isRunning():
            self._windows_save_pending = True
            return
        self._windows_save_pending = False
        desktop = self._windows_session()
        self._config["desktop"] = desktop
        self._ignore_config_changes_until = time.monotonic() + 2.0
        worker = _IpcCallWorker("config.set", {"desktop": desktop}, self._ipc_endpoint())
        worker.finished.connect(worker.deleteLater)
        worker.finished.connect(self._on_windows_saved)
        self._windows_worker = worker
        worker.start()

    def _on_windows_saved(self) -> None:
        self._windows_worker = None
        self._ignore_config_changes_until = time.monotonic() + 2.0
        if self._windows_save_pending and not self._shutting_down:
            self._save_windows()

    def _on_detached_closed(self, window: object) -> None:
        """A window closed: its cameras come back to this wall — where they
        were, unless this wall's layout changed meanwhile (then at the end)."""
        if not isinstance(window, DetachedWallWindow):
            return
        if window in self._detached:
            self._detached.remove(window)
        if self._shutting_down:
            return
        self._sync_page_clock()
        self._wall.adopt(window.wall.hand_over(window.camera_ids))
        self._refresh_walls(page=self._wall.page)
        self._sync_view_demand()
        self._schedule_windows_save()  # closed by hand: it does not reopen

    # ------------------------------------------------------------------
    # Fullscreen / expand
    # ------------------------------------------------------------------

    def _exit_solo(self) -> None:
        """Exit solo (1×1) expand mode, restoring the previous grid. Fullscreen state unchanged."""
        if self._pre_solo_grid is None:
            return
        rows, cols, page = self._pre_solo_grid
        self._pre_solo_grid = None
        logger.debug("Exiting solo mode, restoring grid %dx%d page=%d", rows, cols, page)
        # Restore the multi-cell geometry before exposing all cameras again;
        # otherwise the temporary 1x1 layout would promote every hidden camera
        # to its main stream and immediately switch it back to sub quality.
        self._wall.set_grid(rows, cols)
        self._wall.set_solo_camera(None)
        self._wall.set_page(page)
        self._update_page_display()

    def _on_expand_cell(self, cam_id: str) -> None:
        # If already in solo mode, toggling the same or another cell exits solo (stays fullscreen).
        if self._pre_solo_grid is not None:
            self._exit_solo()
            return
        cam = next((c for c in self._cameras if c.id == cam_id), None)
        if cam is None:
            return
        self._pre_solo_grid = (self._wall._rows, self._wall._cols, self._wall.page)
        logger.debug("Entering solo mode for cam_id=%s", cam_id)
        self._wall.set_solo_camera(cam_id)
        self._wall.set_grid(1, 1)
        self._update_page_display()
        if not self.isFullScreen():
            self._enter_fullscreen_mode()

    def _on_cell_single_click(self, cam_id: str) -> None:
        # Single-click in solo mode exits solo without touching fullscreen state.
        if self._pre_solo_grid is not None:
            self._exit_solo()

    def _toggle_fullscreen(self) -> None:
        if self.isFullScreen():
            # Exit solo first (restores grid) then exit fullscreen.
            self._exit_solo()
            self.showNormal()
            self._exit_fullscreen_mode()
        else:
            self._enter_fullscreen_mode()

    def _on_auto_fit_toggled(self, enabled: bool) -> None:
        for wall in self._walls():
            wall.set_auto_fit(enabled)
        self._config.setdefault("wall", {})
        self._config["wall"]["auto_fit"] = enabled
        self._save_wall_config()

    def _on_page_strip_clicked(self, page: int) -> None:
        self._navigate_to_page(page)

    def _on_wall_layout_edit(self, placements: object, spans: object, page: int) -> None:
        """Apply a wall drag/drop/resize: store every tile's position and size
        exactly as the wall previewed it."""
        if not isinstance(placements, list) or not isinstance(spans, dict):
            return
        by_id = {cam.id: cam for cam in self._cameras}
        reordered: list[CameraConfig] = []
        for placement in placements:
            cam = by_id.pop(placement.cam_id, None) if isinstance(placement, Placement) else None
            if cam is None:
                continue
            row_span, col_span = spans.get(cam.id, placement.span)
            cam.wall_position = WallPosition(
                page=placement.page,
                row=placement.row,
                col=placement.col,
                row_span=int(row_span),
                col_span=int(col_span),
            )
            reordered.append(cam)
        next_page = max((p.page for p in placements if isinstance(p, Placement)), default=-1) + 1
        reordered += self._park_detached_cameras(by_id, next_page)
        reordered += by_id.values()  # cameras added meanwhile take free spots
        self._cameras = reordered
        self._refresh_all(page=page)
        self._save_wall_positions()

    def _park_detached_cameras(
        self, by_id: dict[str, CameraConfig], first_page: int
    ) -> list[CameraConfig]:
        """Saved positions for cameras shown in other windows once this wall's
        layout changed: each of their pages after this wall's last page (window
        by window), so they come back at the end. Takes them out of ``by_id``."""
        parked: list[CameraConfig] = []
        page = first_page
        for window in self._detached:
            for tiles in window.pages():
                for cam_id, position in tiles.items():
                    cam = by_id.pop(cam_id, None)
                    if cam is None:
                        continue
                    row_span, col_span = cam.tile_span
                    cam.wall_position = replace(
                        position, page=page, row_span=row_span, col_span=col_span
                    )
                    parked.append(cam)
                page += 1
        return parked

    def _save_wall_positions(self) -> None:
        if self._positions_worker is not None and self._positions_worker.isRunning():
            self._positions_pending = True
            return
        self._positions_pending = False
        positions = [
            {"id": c.id, "wall_position": asdict(c.wall_position)}
            for c in self._cameras
            if c.wall_position is not None
        ]
        # The backend rewrites the config file: that is not an outside edit.
        self._ignore_config_changes_until = time.monotonic() + 2.0
        worker = _IpcCallWorker("camera.set_positions", positions, self._ipc_endpoint())
        worker.result.connect(self._on_positions_saved)
        worker.finished.connect(worker.deleteLater)
        worker.finished.connect(self._on_positions_worker_finished)
        self._positions_worker = worker
        worker.start()

    def _on_positions_saved(self, result: object) -> None:
        self._ignore_config_changes_until = time.monotonic() + 2.0
        if isinstance(result, Exception):
            logger.warning("Saving wall positions failed: %s", result)

    def _on_positions_worker_finished(self) -> None:
        self._positions_worker = None
        if self._positions_pending and not self._shutting_down:
            self._save_wall_positions()

    def _on_camera_quality_requested(self, cam_id: str, stream: str) -> None:
        self._quality_dispatcher.request(cam_id, stream)

    def _on_camera_quality_failed(self, cam_id: str, stream: str, error: object) -> None:
        logger.warning(
            "Failed to switch camera stream for cam_id=%s stream=%s: %s",
            cam_id,
            stream,
            error,
        )
        # Allow retry on the next assignment/update if this request failed.
        for wall in self._walls():
            wall.mark_quality_unsynced(cam_id)

    def keyPressEvent(self, event: QKeyEvent) -> None:
        mods = event.modifiers()
        ctrl_shift = Qt.KeyboardModifier.ControlModifier | Qt.KeyboardModifier.ShiftModifier
        if event.key() == Qt.Key.Key_N and (mods & ctrl_shift) == ctrl_shift:
            self._on_new_window()
        elif event.key() == Qt.Key.Key_H and (mods & ctrl_shift) == ctrl_shift:
            self._set_compact(not self._compact)
        elif (
            event.key() == Qt.Key.Key_S and event.modifiers() & Qt.KeyboardModifier.ControlModifier
        ):
            try:
                self._ipc.call("config.save")
            except (BackendOfflineError, IpcError):
                pass
        elif event.key() == Qt.Key.Key_Escape:
            if self._pre_solo_grid is not None:
                # In solo mode: exit solo but stay fullscreen
                self._exit_solo()
            elif self.isFullScreen():
                self.showNormal()
                self._exit_fullscreen_mode()
        elif event.key() == Qt.Key.Key_F11:
            self._toggle_fullscreen()
        elif event.key() == Qt.Key.Key_PageUp:
            self._on_prev_page()
        elif event.key() == Qt.Key.Key_PageDown:
            self._on_next_page()
        elif event.key() == Qt.Key.Key_Home:
            self._navigate_to_page(0)
        elif event.key() == Qt.Key.Key_End:
            self._navigate_to_page(self._wall.page_count - 1)
        else:
            super().keyPressEvent(event)

    # ------------------------------------------------------------------
    # Tabs
    # ------------------------------------------------------------------

    def _on_tab_changed(self, tab: str) -> None:
        if tab == "settings":
            self._open_settings()
            return
        # Any switch away from diagnostics stops the periodic refresh so
        # we never poll the backend while the user cannot see the page.
        if tab != "diagnostics":
            self._performance_page.stop_refresh()
        if tab != "playback":
            # Nobody watches it now: stop the video (the position stays).
            self._playback_view.pause()
        if tab == "live":
            self._content_stack.setCurrentIndex(0)
            self._toolbar.show()
            self._page_strip.show()
        elif tab == "playback":
            self._content_stack.setCurrentIndex(1)
            self._toolbar.hide()
            self._page_strip.hide()
            self._playback_view.refresh_camera_list(self._cameras)
        elif tab == "diagnostics":
            self._content_stack.setCurrentIndex(2)
            self._toolbar.hide()
            self._page_strip.hide()
            self._performance_page.start_refresh()
        elif tab == "events":
            self._content_stack.setCurrentIndex(3)
            self._toolbar.hide()
            self._page_strip.hide()
            self._unseen_events = 0
            self._title_bar.set_tab_badge("events", 0)
            self._events_view.reload()
        else:
            logger.info("Tab: %s (not yet implemented)", tab)

    # ------------------------------------------------------------------
    # Backend sync / polling
    # ------------------------------------------------------------------

    # ------------------------------------------------------------------
    # Events
    # ------------------------------------------------------------------

    def _on_events_started_over(self, open_events: list) -> None:
        """Connected (again) to the backend: the events going on now."""
        events = [Event.from_dict(e) for e in open_events if isinstance(e, dict)]
        self._activity.reset(e for e in events if e is not None)
        self._apply_activity()

    def _on_event_notices(self, notices: list) -> None:
        events_shown = self._content_stack.currentIndex() == 3
        new = 0
        for notice in notices:
            raw = notice.get("event") if isinstance(notice, dict) else None
            event = Event.from_dict(raw) if isinstance(raw, dict) else None
            if event is None:
                continue
            phase = str(notice.get("phase"))
            self._activity.apply(phase, event)
            worth_counting = event.kind in ACTIVITY_KINDS or (
                event.kind in SYSTEM_KINDS and event.kind != "backend_started"
            )
            if phase == "start" and worth_counting and not events_shown:
                new += 1
        self._events_view.add_notices(notices)
        self._playback_view.add_notices(notices)
        if new:
            self._unseen_events += new
            self._title_bar.set_tab_badge("events", self._unseen_events)
        self._apply_activity()

    def _on_pending_events(self, raw_events: list) -> None:
        events = [Event.from_dict(raw) for raw in raw_events if isinstance(raw, dict)]
        self._activity.restore_pending(event for event in events if event is not None)
        self._apply_activity()

    def _acknowledge_motion(self, camera_id: str) -> None:
        if camera_id in self._motion_ack_workers:
            return
        ids = self._activity.pending_ids(camera_id)
        if not ids:
            return
        worker = _IpcCallWorker("events.acknowledge", {"ids": ids}, self._ipc_endpoint())
        self._motion_ack_workers[camera_id] = worker

        def done(result: object) -> None:
            if isinstance(result, Exception):
                logger.warning("Could not acknowledge motion for %s: %s", camera_id, result)
                show_message(
                    self,
                    "Motion Alert",
                    "Could not clear the motion alert. Click it to retry.",
                    kind="warning",
                )
                return
            self._activity.acknowledge(camera_id, ids)
            self._apply_activity()

        def finished() -> None:
            self._motion_ack_workers.pop(camera_id, None)
            worker.deleteLater()

        worker.result.connect(done)
        worker.finished.connect(finished)
        worker.start()

    def _apply_activity(self) -> None:
        active = self._activity.active()
        if active != self._motion_cameras:
            self._motion_cameras = active
            for wall in self._walls():
                wall.set_motion_cameras(active)
            self._camera_panel.set_motion_cameras(active)
        pending = self._activity.pending()
        if pending != self._pending_motion_cameras:
            self._pending_motion_cameras = pending
            for wall in self._walls():
                wall.set_pending_motion_cameras(pending)

    def _play_event(self, camera_id: str, ts_ms: int) -> None:
        """An event's Play Recording: that moment of that camera in Playback."""
        self._title_bar.show_tab("playback")
        self._playback_view.refresh_camera_list(self._cameras)
        self._playback_view.show_moment(camera_id, ts_ms)

    def _sync_view_demand(self, *_args: object) -> None:
        """Ask the backend to decode the wall's visible cameras, or none while
        the wall is not on screen (another page, or the window minimized)."""
        shown = self._content_stack.currentIndex() == 0 and not self.isMinimized()
        demand = set(self._wall.desktop_demand() if shown else [])
        if shown:
            demand.update(self._prewarm_main)
            demand.update(self._hover_warm)
            demand.update(self._browse_warm)
        for window in self._detached:
            demand.update(window.desktop_demand())
        demand.update(self._prewarm_windows)
        self._view_lease.set_cameras(demand)
        # Live audio follows the camera on screen: a page change, another
        # tab, a minimized window or a closed one ends it.
        if self._listening is not None and self._listening not in self._on_screen(shown):
            self._set_listening(None)

    def _on_screen(self, main_shown: bool) -> set[str]:
        """Cameras with a tile on screen in the main wall or an extra window."""
        cameras = self._wall.visible_camera_ids() if main_shown else set()
        for window in self._detached:
            if window.isVisible() and not window.isMinimized():
                cameras |= window.wall.visible_camera_ids()
        return cameras

    def _toggle_listen(self, cam_id: str) -> None:
        """A tile's speaker: hear this camera live, or stop if it is the one heard."""
        self._set_listening(None if cam_id == self._listening else cam_id)

    def _set_listening(self, cam_id: str | None) -> None:
        if cam_id == self._listening:
            return
        logger.info("Live audio: %s", cam_id or "off")
        self._listening = cam_id
        for wall in self._walls():
            wall.set_listening(cam_id)
        self._view_lease.set_audio_camera(cam_id)

    def changeEvent(self, event: QEvent) -> None:
        if event.type() == QEvent.Type.WindowStateChange:
            self._sync_view_demand()
            self._title_bar.sync_window_state()
            self._compact_bar.sync_window_state()
            if self._compact and not self.isFullScreen():
                self._compact_bar.show()
            self._sync_status_overlay()
        super().changeEvent(event)

    def _on_poll_done(
        self, states: dict, stream_infos: dict, rec_states: dict, backend_ok: bool
    ) -> None:
        """Called on the main thread with poll results from the background worker."""
        for cam_id, state in states.items():
            if isinstance(state, str):
                # Only log when status actually changes
                for cam in self._cameras:
                    if cam.id == cam_id and getattr(cam, "_last_logged_state", None) != state:
                        logger.debug("Camera %s status -> %s", cam_id, state)
                        cam._last_logged_state = state  # type: ignore[attr-defined]
                        break
                # A wall still receiving this camera's pictures keeps it live
                # while the backend's status catches up with a reconnect.
                applied = [wall.set_cell_status(cam_id, state) for wall in self._walls()]
                shown = "streaming" if "streaming" in applied else state
                self._camera_panel.set_camera_status(cam_id, shown)
        for cam_id, info in stream_infos.items():
            if isinstance(info, dict):
                codec = info.get("codec")
                if isinstance(codec, str):
                    for wall in self._walls():
                        wall.set_camera_codec(cam_id, codec, info.get("width"), info.get("height"))
                audio = info.get("audio")
                for wall in self._walls():
                    wall.set_camera_audio(cam_id, audio if isinstance(audio, str) else None)
            elif isinstance(info, str):
                for wall in self._walls():
                    wall.set_camera_codec(cam_id, info)
        self._rec_states = {k: v for k, v in rec_states.items() if isinstance(v, str)}
        for cam_id, rec_state in self._rec_states.items():
            for wall in self._walls():
                wall.set_cell_recording(cam_id, rec_state)
            self._camera_panel.set_camera_recording(cam_id, rec_state)
        if backend_ok != self._backend_online:
            self._backend_online = backend_ok
            self._status_bar.set_backend_status(backend_ok)
            if backend_ok:
                logger.info("Backend connection restored")
            else:
                logger.warning("Backend connection lost")

    def _apply_autoscroll(self) -> None:
        interval = int(self._config.get("wall", {}).get("page_interval_sec", 0))
        if interval > 0:
            self._page_interval = interval
        for window in self._detached:
            window.set_cycle_interval(self._page_interval)
        self._sync_page_clock(restart=True)

    def _main_autoscroll_on(self) -> bool:
        return int(self._config.get("wall", {}).get("page_interval_sec", 0)) > 0

    def _sync_page_clock(self, *, restart: bool = False) -> None:
        """Run the page clock while anything changes pages on it: this wall's
        auto-scroll or a cycling extra window."""
        needed = self._main_autoscroll_on() or any(w.cycling for w in self._detached)
        if not needed:
            self._autoscroll_timer.stop()
            self._schedule_prewarm()
            return
        interval_ms = self._page_interval * 1000
        if (
            restart
            or not self._autoscroll_timer.isActive()
            or self._autoscroll_timer.interval() != interval_ms
        ):
            self._autoscroll_timer.start(interval_ms)
            self._schedule_prewarm()

    def _on_page_clock(self) -> None:
        if self._main_autoscroll_on():
            self._auto_advance_page()
        for window in list(self._detached):
            window.advance()
        # The warmed pages are on screen now (their own demand keeps them).
        self._schedule_prewarm()

    def _schedule_prewarm(self) -> None:
        """Drop the current warm-up and decode the next pages shortly before
        the page clock's next tick."""
        self._prewarm_timer.stop()
        self._set_prewarm(frozenset(), frozenset())
        if self._autoscroll_timer.isActive():
            remaining = self._autoscroll_timer.remainingTime()
            if remaining < 0:
                remaining = self._autoscroll_timer.interval()
            self._prewarm_timer.start(max(0, remaining - _PREWARM_LEAD_MS))

    def _prewarm_next_pages(self) -> None:
        main: set[str] = set()
        wall = self._wall
        if self._main_autoscroll_on() and wall.page_count > 1 and not wall.layout_drag_active:
            main.update(wall.page_demand((wall.page + 1) % wall.page_count))
        windows: set[str] = set()
        for window in self._detached:
            windows.update(window.upcoming_demand())
        self._set_prewarm(frozenset(main), frozenset(windows))

    def _set_prewarm(self, main: frozenset[str], windows: frozenset[str]) -> None:
        if (main, windows) == (self._prewarm_main, self._prewarm_windows):
            return
        self._prewarm_main, self._prewarm_windows = main, windows
        self._sync_view_demand()

    def _set_page_interval(self, seconds: int) -> None:
        """An interval picked in an extra window: every window takes it, and
        so does this wall's auto-scroll when it is on."""
        self._page_interval = max(1, int(seconds))
        if self._main_autoscroll_on():
            self._toolbar.set_auto_scroll(self._page_interval)
            self._on_auto_scroll_changed(self._page_interval)
            return
        self._toolbar.set_auto_scroll_interval(self._page_interval)
        self._apply_autoscroll()

    def _apply_settings(self) -> None:
        self._status_bar.set_storage_path(
            str(self._config.get("recording", {}).get("storage_path") or "~/Videos/neurowatch")
        )
        theme = str(self._config.get("ui", {}).get("theme", "dark"))
        app = QApplication.instance()
        if isinstance(app, QApplication):
            apply_app_theme(app, theme)
        self._events_view.apply_theme(theme)
        root = self.centralWidget()
        if root is not None:
            root.setStyleSheet(
                "background-color: #f3f6fb;" if theme == "light" else "background-color: #12121c;"
            )
        self._title_bar.set_site_name(self._config.get("site_name", "My Site"))
        grid = self._config.get("wall", {})
        rows = int(grid.get("rows", _DEFAULT_ROWS))
        cols = int(grid.get("cols", _DEFAULT_COLS))
        auto_fit = bool(grid.get("auto_fit", True))
        self._toolbar.set_grid(rows, cols)
        self._toolbar.set_auto_fit(auto_fit)
        self._wall.set_grid(rows, cols)
        self._wall.set_auto_fit(auto_fit)
        for wall in self._walls():
            wall.set_preview_fps(self._preview_fps())
        self._playback_view.set_preview_fps(self._preview_fps())
        self._apply_autoscroll()
        self._sync_status_overlay()
        interval = int(grid.get("page_interval_sec", 0))
        self._toolbar.set_auto_scroll(interval)
        if interval <= 0:
            # Off: the box still shows the shared interval turning it on uses.
            self._toolbar.set_auto_scroll_interval(self._page_interval)
        self._update_page_display()

    def _restart_autoscroll_timer(self) -> None:
        if self._autoscroll_timer.isActive():
            self._autoscroll_timer.start(self._autoscroll_timer.interval())
            self._schedule_prewarm()

    def _auto_advance_page(self) -> None:
        if self._wall.layout_drag_active:
            return  # never flip the page under a tile being dragged
        next_page = (self._wall.page + 1) % max(1, self._wall.page_count)
        self._change_page(next_page)

    def _change_page(self, new_page: int) -> None:
        """Show ``new_page`` at once. No fade: fading the wall dims it towards
        black and composites every tile (GL readbacks included) per animation
        frame, so the page visibly drew tile by tile. The wall paints every
        tile's cached picture in one pass instead."""
        new_page = max(0, min(new_page, self._wall.page_count - 1))
        if new_page == self._wall.page:
            return
        logger.debug(
            "Page navigation: %d -> %d (of %d)",
            self._wall.page + 1,
            new_page + 1,
            self._wall.page_count,
        )
        self._wall.set_page(new_page)
        self._update_page_display()

    def _open_settings(self, tab: str | None = None) -> None:
        if self._settings_worker is not None and self._settings_worker.isRunning():
            show_message(self, "Settings", "Settings are still being saved.")
            return
        startup = load_prefs()
        dlg = SettingsDialog(
            self._config,
            self,
            ipc_endpoint=self._ipc_endpoint() or None,
            tab=tab,
            startup=startup,
            startup_note=_startup_note(),
        )
        _delete_after_use(dlg)
        if dlg.exec() == SettingsDialog.DialogCode.Accepted:
            wanted = dlg.startup_prefs()
            if wanted is not None and wanted != startup:
                self._apply_startup(wanted)
            updated = dlg.get_config()
            drive = dlg.selected_drive()
            # Apply visual settings immediately; backend persistence may restart
            # many pipelines and therefore belongs off the GUI thread.
            self._config = updated
            self._apply_settings()
            endpoint = str(getattr(self._ipc, "endpoint", getattr(self._ipc, "_endpoint", "")))
            self._ignore_config_changes_until = time.monotonic() + 2.0
            worker = _SettingsSaveWorker(endpoint, updated, drive)
            worker.drive_failed.connect(self._on_settings_drive_failed)
            worker.result.connect(self._on_settings_saved)
            worker.finished.connect(worker.deleteLater)
            worker.finished.connect(self._clear_settings_worker)
            self._settings_worker = worker
            worker.start()

    def _apply_startup(self, prefs: StartupPrefs) -> None:
        worker = _StartupApplyWorker(self._ipc_endpoint() or service_endpoint(), prefs)
        worker.done.connect(self._on_startup_applied)
        worker.finished.connect(worker.deleteLater)
        self._startup_worker = worker
        worker.start()

    def _on_startup_applied(self, problems: list) -> None:
        self._startup_worker = None
        if problems:
            show_message(self, "Startup", "\n\n".join(str(p) for p in problems), kind="warning")

    def _on_settings_saved(self, result: object) -> None:
        # A save that restarted cameras can outlast the window set when it began.
        self._ignore_config_changes_until = time.monotonic() + 2.0
        if isinstance(result, BackendOfflineError):
            show_message(
                self,
                "Backend Offline",
                "Settings could not be saved because the backend is offline.",
                kind="error",
            )
            return
        if isinstance(result, IpcError):
            show_message(self, "Backend Error", result.message, kind="warning")
            return
        if isinstance(result, Exception):
            show_message(self, "Settings Error", str(result), kind="warning")
            return
        if isinstance(result, dict):
            self._config = result
            self._apply_settings()

    def _on_settings_drive_failed(self, reason: str) -> None:
        show_message(
            self,
            "Recording Drive",
            f"Recordings stay where they were: the chosen drive could not be used.\n\n{reason}",
            kind="warning",
        )

    def _clear_settings_worker(self) -> None:
        self._settings_worker = None

    def _on_export_config(self) -> None:
        path, _ = QFileDialog.getSaveFileName(
            self, "Export Config", "neurowatch_config.json", "JSON Files (*.json)"
        )
        if not path:
            return
        config_path = _active_config_path()
        if os.path.exists(config_path):
            shutil.copy(config_path, path)
        else:
            show_message(
                self,
                "Export Failed",
                f"Config file not found at {config_path}.\nIs the backend running?",
                kind="warning",
            )

    def _on_export_video(self) -> None:
        """Cut a clip from a camera's recordings and save it as an MP4."""
        endpoint = str(getattr(self._ipc, "endpoint", ""))
        dialog = ExportVideoDialog(self._cameras, endpoint, self)
        try:
            dialog.exec()
        finally:
            dialog.shutdown()
            dialog.deleteLater()

    def _export_event(self, event: object) -> None:
        """Use the existing export dialog; preparing a preset starts no export job."""
        if not isinstance(event, Event) or event.camera_id is None:
            return
        endpoint = str(getattr(self._ipc, "endpoint", ""))
        dialog = ExportVideoDialog(self._cameras, endpoint, self, camera_id=event.camera_id)
        try:
            dialog.load_event(event)
            dialog.exec()
        finally:
            dialog.shutdown()
            dialog.deleteLater()

    def _on_import_config(self) -> None:
        path, _ = QFileDialog.getOpenFileName(self, "Import Config", "", "JSON Files (*.json)")
        if not path:
            return
        config_path = _active_config_path()
        try:
            os.makedirs(os.path.dirname(config_path), exist_ok=True)
            shutil.copy(path, config_path)
        except OSError as e:
            show_message(self, "Import Failed", str(e), kind="warning")
            return
        self._load_from_backend()

    def _on_about(self) -> None:
        dlg = AboutDialog(self)
        _delete_after_use(dlg)
        dlg.exec()

    # ------------------------------------------------------------------
    # Status bar, menu and shutdown hooks (NeuroWatch Cloud uses them)
    # ------------------------------------------------------------------

    @property
    def ipc_endpoint(self) -> str:
        """The backend endpoint, for background workers of their own."""
        return self._ipc_endpoint()

    def add_menu_action(self, text: str, callback: Callable[[], object]) -> None:
        """Add an entry to the ≡ menu."""
        self._title_bar.add_menu_action(text, callback)

    def _set_edition(self, edition: Edition) -> None:
        """Remember the recorder's edition; show the camera-limit notice
        while it holds cameras back."""
        self._edition = edition
        if self._edition_notice is None:
            if not edition.notice:
                return
            self._edition_notice = EditionNotice()
            self.add_status_indicator(self._edition_notice)
        self._edition_notice.set_edition(edition)

    def add_status_indicator(self, widget: QWidget) -> None:
        """Show a widget in the bottom status bar, before the version."""
        self._status_bar.add_indicator(widget)

    def add_shutdown_callback(self, callback: Callable[[], object]) -> None:
        """Run ``callback`` when the window shuts down (stop and join workers there)."""
        self._shutdown_callbacks.append(callback)

    def bring_to_front(self) -> None:
        """NeuroWatch was opened again (the tray, the app menu): show this window."""
        if self.isMinimized():
            self.showNormal()
        self.show()
        self.raise_()
        self.activateWindow()

    def _enter_fullscreen_mode(self) -> None:
        logger.debug("Entering fullscreen mode")
        self.showFullScreen()
        self._show_top_bars()
        self._fullscreen_hide_timer.start()
        self._proximity_timer.start()

    def _exit_fullscreen_mode(self) -> None:
        logger.debug("Exiting fullscreen mode")
        self._fullscreen_hide_timer.stop()
        self._proximity_timer.stop()
        self._show_top_bars()

    def _show_top_bars(self) -> None:
        if self._compact:
            self._compact_bar.show()
            self._sync_status_overlay()
        else:
            self._title_bar.show()
            self._toolbar.show()

    def _hide_fullscreen_bars(self) -> None:
        if not self.isFullScreen():
            return
        if self._compact:
            # Like an extra window in fullscreen: nothing over the wall. The
            # strip (with the window buttons) comes back at the top edge.
            self._compact_bar.hide()
            self._sync_status_overlay()
        else:
            # Frameless fullscreen windows have no OS controls. Keep our title
            # bar available so minimise/restore/close are never trapped.
            self._title_bar.show()
            self._toolbar.hide()

    def _check_mouse_proximity(self) -> None:
        if not self.isFullScreen():
            return
        pos = self.mapFromGlobal(QCursor.pos())
        if pos.y() <= 5:
            self._show_top_bars()
            self._fullscreen_hide_timer.start()
        elif not self._fullscreen_hide_timer.isActive():
            self._hide_fullscreen_bars()

    # ------------------------------------------------------------------
    # Compact view ("Hide panels")
    # ------------------------------------------------------------------

    def _set_compact(self, compact: bool) -> None:
        """Hide the title bar, toolbar, camera list and page dots (compact
        view: the wall under a slim strip that also carries the status bar's
        readings), or show them again."""
        if compact == self._compact:
            return
        self._status_overlay.release()  # the status bar is placed below
        if compact:
            self._title_bar.show_tab("live")  # the strip has no tabs
        self._compact = compact
        logger.debug("Compact view %s", "on" if compact else "off")
        self._title_bar.setVisible(not compact)
        self._camera_panel.setVisible(not compact)
        self._page_strip_holder.setVisible(not compact)
        # The status readings move into the strip, and back to the bottom.
        self._status_bar.set_inline(compact)
        if compact:
            self._compact_bar.set_status(self._status_bar)
        else:
            self._root_layout.addWidget(self._status_bar)
            self._compact_bar.set_status(None)
        self._status_bar.show()
        self._compact_bar.setVisible(compact)
        if compact:
            self._toolbar.hide()
        else:
            self._toolbar.setVisible(self._content_stack.currentIndex() == 0)
        self._compact_bar.sync_window_state()
        if self.isFullScreen():
            self._fullscreen_hide_timer.start()  # the bars hide again shortly
        self._update_page_display()
        self._wall.setFocus(Qt.FocusReason.OtherFocusReason)

    def _fullscreen_status_on(self) -> bool:
        return bool(self._config.get("ui", {}).get("fullscreen_status", True))

    def _fullscreen_status_opacity(self) -> int:
        value = self._config.get("ui", {}).get("fullscreen_status_opacity", DEFAULT_OPACITY)
        try:
            return max(0, min(100, int(value)))
        except (TypeError, ValueError):
            return DEFAULT_OPACITY

    def _sync_status_overlay(self) -> None:
        """Show the status readings over the wall while the compact view's
        strip, which carries them, is hidden in fullscreen (when the setting
        is on); give them back to the strip otherwise."""
        if not hasattr(self, "_status_overlay"):
            return  # a window-state change while the window is being built
        wanted = (
            self._compact
            and self.isFullScreen()
            and self._compact_bar.isHidden()
            and self._fullscreen_status_on()
        )
        self._status_overlay.set_opacity(self._fullscreen_status_opacity())
        if wanted:
            self._compact_bar.set_status(None)
            self._status_overlay.hold(self._status_bar)
        elif self._status_overlay.holding:
            self._status_overlay.release()
            if self._compact:
                self._status_bar.set_inline(True)  # the strip's form again
                self._compact_bar.set_status(self._status_bar)

    def _on_config_file_changed(self, path: str) -> None:
        # Re-add path: editors often write a temp file then rename, dropping the watcher entry
        if os.path.exists(path):
            self._config_watcher.addPath(path)
        if time.monotonic() < self._ignore_config_changes_until:
            return
        if self._settings_worker is not None and self._settings_worker.isRunning():
            return
        self._config_reload_timer.start()  # restarts if already running (debounce)

    def _reload_config_after_change(self) -> None:
        # Applied without asking: the file changes when cameras are added from
        # a search, by the web portal or by hand, and the new settings are wanted.
        logger.info("Config file changed on disk — reloading cameras and settings")
        self._load_from_backend()


# ---------------------------------------------------------------------------


def _configure_logging() -> None:
    """Set up root logger: INFO to stderr, DEBUG to rotating file."""
    import logging.handlers
    import os

    if sys.platform == "win32":
        data_dir = os.environ.get("LOCALAPPDATA") or os.path.expanduser("~/AppData/Local")
    else:
        data_dir = os.environ.get("XDG_DATA_HOME") or os.path.expanduser("~/.local/share")
    log_dir = os.path.join(data_dir, "neurowatch")
    os.makedirs(log_dir, exist_ok=True)
    log_file = os.path.join(log_dir, "neurowatch.log")

    root = logging.getLogger()
    root.setLevel(logging.DEBUG)

    # stderr — INFO and above
    stream_handler = logging.StreamHandler()
    stream_handler.setLevel(logging.INFO)
    stream_handler.setFormatter(
        logging.Formatter("%(asctime)s %(levelname)-8s %(name)s: %(message)s")
    )
    root.addHandler(stream_handler)

    # rotating file — DEBUG and above (max 5 MB × 3 files)
    file_handler = logging.handlers.RotatingFileHandler(
        log_file, maxBytes=5 * 1024 * 1024, backupCount=3, encoding="utf-8"
    )
    file_handler.setLevel(logging.DEBUG)
    file_handler.setFormatter(
        logging.Formatter("%(asctime)s %(levelname)-8s %(name)s: %(message)s")
    )
    root.addHandler(file_handler)
    _capture_crashes(log_dir)


#: Windows taskbar identity, shared with the installer's shortcuts (windows.iss),
#: so the running app groups under the shortcut and shows the NeuroWatch logo.
WINDOWS_APP_ID = "NeuroWatch.NeuroWatch"


def _set_windows_app_id() -> None:
    if sys.platform != "win32":
        return
    import ctypes

    try:
        shell32 = ctypes.CDLL("shell32")  # 64-bit Windows: one calling convention
        shell32.SetCurrentProcessExplicitAppUserModelID(ctypes.c_wchar_p(WINDOWS_APP_ID))
    except (AttributeError, OSError) as e:
        logger.warning("Could not set the Windows taskbar identity: %s", e)


#: Kept open for the life of the process: faulthandler writes to it on a crash.
_crash_log = None


def _capture_crashes(log_dir: str) -> None:
    """Leave a trace of crashes that end the process without a Python error:
    native faults go to ``crash.log`` (every thread's stack), and Qt's own
    warnings and fatal errors (such as a thread destroyed while running) to
    the log. A windowed Windows build has no console to show either."""
    global _crash_log
    import faulthandler

    from PySide6.QtCore import QtMsgType, qInstallMessageHandler

    try:
        _crash_log = open(os.path.join(log_dir, "crash.log"), "a", encoding="utf-8")
        faulthandler.enable(file=_crash_log, all_threads=True)
    except OSError as e:
        logger.warning("Crash log unavailable: %s", e)
    qt_logger = logging.getLogger("qt")
    levels = {
        QtMsgType.QtDebugMsg: logging.DEBUG,
        QtMsgType.QtInfoMsg: logging.INFO,
        QtMsgType.QtWarningMsg: logging.WARNING,
        QtMsgType.QtCriticalMsg: logging.ERROR,
        QtMsgType.QtFatalMsg: logging.CRITICAL,
    }

    def to_log(kind: QtMsgType, _context: object, message: str) -> None:
        qt_logger.log(levels.get(kind, logging.WARNING), "%s", message)
        if kind == QtMsgType.QtFatalMsg:
            for handler in logging.getLogger().handlers:
                handler.flush()

    qInstallMessageHandler(to_log)


class _ShutdownRequest:
    """Signal-requested shutdown state shared between startup and the main loop."""

    def __init__(self) -> None:
        self.requested = False
        self.starting = False

    @contextmanager
    def during_startup(self):
        """Mark code that runs before ``app.exec()``.

        Event loops running here (e.g. a startup error popup) are nested, not
        the main loop: a signal only dismisses the popup and stays requested.
        """
        self.starting = True
        try:
            yield
        finally:
            self.starting = False


@contextmanager
def _shutdown_signals(app: QApplication):
    """Defer POSIX termination to Qt; never raise KeyboardInterrupt in a slot."""
    state = _ShutdownRequest()

    def request_shutdown(_signum, _frame):
        state.requested = True

    def check_shutdown():
        if not state.requested:
            return
        if state.starting:
            # Quitting before app.exec() would be lost; unblock any modal popup
            # and let the caller skip app.exec() once startup returns.
            modal = QApplication.activeModalWidget()
            if isinstance(modal, QDialog):
                modal.reject()
            return
        # Startup pumps events before app.exec(); quitting then would be a
        # no-op and lose the request, so wait for a running event loop.
        if app.thread().loopLevel() > 0:
            timer.stop()
            app.closeAllWindows()
            app.quit()

    previous = {
        sig: signal.signal(sig, request_shutdown) for sig in (signal.SIGINT, signal.SIGTERM)
    }
    # A Python callback also lets CPython dispatch signals while Qt is idle.
    timer = QTimer(app)
    timer.setInterval(100)
    timer.timeout.connect(check_shutdown)
    timer.start()
    try:
        yield state
    finally:
        timer.stop()
        for sig, handler in previous.items():
            signal.signal(sig, handler)


def _wait_responsively(ms: int) -> None:
    """Wait ``ms`` while still dispatching events (no nested event loop)."""
    done = False

    def _finish() -> None:
        nonlocal done
        done = True

    QTimer.singleShot(ms, _finish)
    while not done:
        QApplication.processEvents(QEventLoop.ProcessEventsFlag.WaitForMoreEvents)


def _start_main_window(
    app: QApplication, version: str, launch: LaunchOptions | None = None
) -> MainWindow:
    """Build and show the main window behind the welcome screen.

    With ``launch`` (the real app; tests leave it out) the background service
    is started first unless it runs, and the tray icon is started.

    The welcome screen closes before any startup error popup, and otherwise
    only once the fully built main window is shown.
    """
    welcome = WelcomeScreen(version=version)
    welcome.present()
    welcome.set_stage(StartupStage.STARTING)
    if launch is not None:
        # Development: the service lives with the app (stopped on close), so
        # nothing starts it at login and no tray watches it.
        follows_ui = service_follows_ui()
        running, error = ensure_service(
            lambda detail: welcome.set_stage(StartupStage.CONNECTING, detail),
            autostart=launch.autostart and not follows_ui,
        )
        if not running:
            welcome.dismiss()
            show_message(
                None,
                "NeuroWatch Service Not Running",
                "The NeuroWatch background service could not be started.\n\n"
                f"{error}\n\nStart it with `neurowatch service start` and open NeuroWatch again.",
                kind="error",
            )
            sys.exit(1)
        if not follows_ui:
            start_tray()
        # A recorder with local accounts asks who is using it first.
        from neurowatch.dialogs.sign_in import SignInPrompt, sign_in_required

        if sign_in_required() and SignInPrompt().choose() != "sign_in":
            welcome.dismiss()
            sys.exit(0)
    try:
        window = MainWindow(progress=welcome.set_stage)
    except BackendUnavailableError as exc:
        welcome.dismiss()
        show_message(
            None,
            "Backend Not Running",
            f"Cannot connect to nw-backend.\nEndpoint: {exc.endpoint}\n\n"
            "Please start the backend and relaunch.",
            kind="error",
        )
        sys.exit(1)
    except BaseException:
        welcome.dismiss()
        raise
    app.aboutToQuit.connect(window.shutdown)
    welcome.set_stage(StartupStage.OPENING)
    welcome.finish(window)
    if bool(getattr(window, "_config", {}).get("ui", {}).get("fullscreen", True)):
        enter_fullscreen = getattr(window, "_enter_fullscreen_mode", None)
        if callable(enter_fullscreen):
            enter_fullscreen()
        else:
            window.showFullScreen()
    else:
        window.resize(1440, 900)
        screen = QGuiApplication.screenAt(QCursor.pos()) or QGuiApplication.primaryScreen()
        if screen is not None:
            frame = window.frameGeometry()
            frame.moveCenter(screen.availableGeometry().center())
            window.move(frame.topLeft())
        window.show()
    welcome.dismiss()
    window.restore_windows()  # the extra windows open last time
    window.report_startup_error()
    return window


def main() -> None:
    _configure_logging()
    import os

    _build = build_info.build_identity()
    _version = _build.label
    _config_path = _active_config_path()
    _configured_socket = os.getenv("NW_IPC_ENDPOINT")
    if os.name == "nt":
        _socket_path = _configured_socket or "tcp:127.0.0.1:7676"
    else:
        _runtime_dir = os.getenv("XDG_RUNTIME_DIR", "/tmp")
        if _configured_socket == "unix:/control.sock" or not _configured_socket:
            _socket_path = f"unix:{_runtime_dir}/neurowatch/control.sock"
        else:
            _socket_path = _configured_socket
    launch = parse_launch_args(sys.argv)
    # Terminal commands print only their own output; the line stays in debug logs.
    terminal = launch.mode in ("service", "command", "headless")
    logger.log(
        logging.DEBUG if terminal else logging.INFO,
        "Neurowatch starting — build %s (%s), package %s, config %s, socket %s",
        _version,
        _build.source.value,
        _build.package_version,
        _config_path,
        _socket_path,
    )
    # A portable copy unpacked over an older one removes what that one left
    # before anything (the service among them) can load it.
    leftovers = clean_portable_copy()
    if leftovers and terminal:
        print(leftovers_message(leftovers), file=sys.stderr)
    if launch.mode == "service":
        from neurowatch.service.cli import main as service_main

        sys.exit(service_main(list(launch.rest)))
    if launch.mode == "command":
        # `neurowatch events ...` or `neurowatch cloud ...`.
        from neurowatch.headless import main as headless_main

        sys.exit(headless_main(list(launch.rest)))
    if launch.mode == "headless":
        # A release bundle's `neurowatch --headless`: the service in headless
        # mode and its status in this terminal (no Qt window).
        from neurowatch.headless import main as headless_main

        sys.exit(headless_main(["monitor", "--service", *launch.rest]))
    if launch.mode == "tray":
        from neurowatch.tray import run as run_tray

        sys.exit(run_tray([sys.argv[0], *launch.rest]))
    _set_windows_app_id()
    app = QApplication([sys.argv[0], *launch.rest])
    app.setApplicationName("NeuroWatch")
    app.setOrganizationName("Neurowatch")
    app.setWindowIcon(app_icon())
    # Windows: the logo on taskbar buttons straight through Win32.
    windows_icon.install(app, icon_path("neurowatch.ico"))
    # Linux docks match windows to the installed neurowatch.desktop (its logo).
    app.setDesktopFileName("neurowatch")
    if activate_running_instance():
        logger.info("NeuroWatch is already open; brought it to the front")
        sys.exit(0)
    single_instance = SingleInstance(parent=app)
    single_instance.listen()
    apply_app_theme(app)
    # GNOME (Wayland, mixed display scales) can double a window's size when
    # another window opens; every window returns to its own size.
    guard_windows_on_focus(app)

    with _shutdown_signals(app) as shutdown:
        with shutdown.during_startup():
            window = _start_main_window(app, _version, launch if launch.manage_service else None)
        single_instance.activate_requested.connect(window.bring_to_front)
        if leftovers:
            QTimer.singleShot(
                0,
                lambda: show_message(window, "NeuroWatch Updated", leftovers_message(leftovers)),
            )
        if shutdown.requested:
            # Signalled before the main loop existed: exit instead of app.exec().
            logger.info("Shutdown requested during startup; exiting")
            app.closeAllWindows()
            window.shutdown()
            result = 0
        else:
            result = app.exec()
    if launch.manage_service and service_follows_ui():
        stop_service()
    sys.exit(result)


if __name__ == "__main__":
    main()
