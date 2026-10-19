"""Add / Edit camera dialog — Search network, ONVIF lookup and Advanced modes.

Network search and ONVIF run in the backend (``camera.discovery.*``), the
same service the web portal uses; this dialog only shows its answers.
"""

from __future__ import annotations

import base64
import logging
from collections.abc import Iterable
from urllib.parse import quote as url_quote
from urllib.parse import unquote as url_unquote

from PySide6.QtCore import QPointF, QRectF, QSize, Qt, QTimer, Signal
from PySide6.QtGui import QColor, QIcon, QPainter, QPaintEvent, QPen, QPixmap, QShowEvent
from PySide6.QtWidgets import (
    QBoxLayout,
    QButtonGroup,
    QCheckBox,
    QComboBox,
    QDialog,
    QDialogButtonBox,
    QDoubleSpinBox,
    QFileDialog,
    QFormLayout,
    QFrame,
    QGridLayout,
    QHBoxLayout,
    QHeaderView,
    QLabel,
    QLineEdit,
    QListWidget,
    QListWidgetItem,
    QPlainTextEdit,
    QProgressBar,
    QPushButton,
    QRadioButton,
    QScrollArea,
    QSlider,
    QSpinBox,
    QStackedWidget,
    QTableWidget,
    QTableWidgetItem,
    QTabWidget,
    QVBoxLayout,
    QWidget,
)

from neurowatch.ipc.sync_client import BackendOfflineError, IpcError, SyncIpcClient
from neurowatch.models import bulk_add
from neurowatch.models.camera import (
    CameraConfig,
    CameraRecordingConfig,
    CameraStream,
    DiscoveryIdentity,
    LocalRecordingDest,
)
from neurowatch.models.discovery import Candidate, StreamChoice, check_range, discovered_streams
from neurowatch.models.motion import (
    camera_event_topics,
    camera_events_status,
    source_of,
    uses_camera_events,
    valid_motion_zones,
    with_camera_events,
)
from neurowatch.models.recording_policy import policy_problem
from neurowatch.theme import (
    active_colors,
    apply_dialog_theme,
    set_button_variant,
    set_label_role,
    set_status_tone,
    style_button_box,
)
from neurowatch.utils.threads import Worker, retire
from neurowatch.widgets.recording_policy import RecordingPolicyEditor
from neurowatch.widgets.window_chrome import (
    card_overlay,
    chrome_size_hint,
    install_dialog_chrome,
    positions_windows,
)

logger = logging.getLogger(__name__)


def _is_local_device(url: str) -> bool:
    """True for V4L2 capture devices, which take no credentials."""
    return url.strip().startswith("/dev/video")


def _inject_creds(url: str, user: str, password: str) -> str:
    """Embed user:password@ into a URL if credentials are given and not already present."""
    if not user or "://" not in url:
        return url
    scheme, rest = url.split("://", 1)
    if "@" in rest:
        return url  # already embedded
    enc_u = url_quote(user, safe="")
    enc_p = url_quote(password, safe="")
    return f"{scheme}://{enc_u}:{enc_p}@{rest}" if enc_p else f"{scheme}://{enc_u}@{rest}"


def _manual_streams(
    url: str, sub_url: str, extra: list[str], known: list[CameraStream]
) -> list[CameraStream]:
    """Stream list for URLs entered by hand: main, sub, then the rest.

    Empty when there is nothing beyond main and sub and no earlier list to
    keep (their built-in options cover it). Name and resolution carry over
    from ``known`` by URL.
    """
    if not extra and not known:
        return []
    by_url = {s.url: s for s in known}
    urls = list(dict.fromkeys(u for u in [url, sub_url, *extra] if u))
    streams: list[CameraStream] = []
    for index, stream_url in enumerate(urls):
        previous = by_url.get(stream_url)
        streams.append(
            CameraStream(
                id=str(index + 1),
                url=stream_url,
                name=previous.name if previous else "",
                width=previous.width if previous else None,
                height=previous.height if previous else None,
            )
        )
    return streams


def _split_credentials(url: str) -> tuple[str, str, str]:
    """(url without ``user:password@``, user, password)."""
    if "://" not in url:
        return url, "", ""
    scheme, rest = url.split("://", 1)
    authority, slash, path = rest.partition("/")
    if "@" not in authority:
        return url, "", ""
    userinfo, host = authority.rsplit("@", 1)
    user, _, password = userinfo.partition(":")
    return f"{scheme}://{host}{slash}{path}", url_unquote(user), url_unquote(password)


def _url_host_port(url: str) -> tuple[str, int | None]:
    """The host of a URL (no credentials) and its port, if it names one."""
    if "://" not in url:
        return "", None
    authority = url.split("://", 1)[1].split("/", 1)[0].rsplit("@", 1)[-1]
    if authority.startswith("["):
        host, _, rest = authority[1:].partition("]")
        port = rest[1:] if rest.startswith(":") else ""
    else:
        host, _, port = authority.partition(":")
    return host, int(port) if port.isdigit() else None


def _saved_stream_choices(cam: CameraConfig) -> list[StreamChoice]:
    """A camera's saved streams, to choose from again without asking the camera."""
    streams = cam.streams or [s for s in cam.stream_options() if s.url]
    return [
        StreamChoice(
            stream_id=s.id,
            name=s.name,
            width=s.width,
            height=s.height,
            url=_split_credentials(s.url)[0],
        )
        for s in streams
        if s.url
    ]


def _password_b64(password: str) -> str:
    return base64.b64encode(password.encode()).decode() if password else ""


# ---------------------------------------------------------------------------
# Background workers — every backend call runs off the main thread.
# Network search and ONVIF run in the backend (camera.discovery.*).
# ---------------------------------------------------------------------------


class _RpcWorker(Worker):
    """Runs one backend call."""

    done = Signal(object)  # result, or Exception

    def __init__(self, endpoint: str | None, method: str, params: dict | None = None) -> None:
        super().__init__()
        self._endpoint = endpoint
        self._method = method
        self._params = params or {}

    def run(self) -> None:
        try:
            self.done.emit(SyncIpcClient(endpoint=self._endpoint).call(self._method, self._params))
        except Exception as e:
            self.done.emit(e)


class _StreamsWorker(Worker):
    """Finds a device by address (unless it came from a search), then reads
    its streams with the credentials given for it."""

    done = Signal(object)  # camera.discovery.profiles result, or Exception

    def __init__(
        self,
        endpoint: str | None,
        candidate_id: str | None,
        host: str,
        port: int,
        username: str,
        password: str,
    ) -> None:
        super().__init__()
        self._endpoint = endpoint
        self._candidate_id = candidate_id
        self._host = host
        self._port = port
        self._username = username
        self._password = password

    def run(self) -> None:
        try:
            client = SyncIpcClient(endpoint=self._endpoint)
            candidate_id = self._candidate_id
            if not candidate_id:
                found = client.call(
                    "camera.discovery.lookup", {"address": self._host, "port": self._port}
                )
                candidate_id = str(found["candidate"]["candidate_id"])
            params: dict = {"candidate_id": candidate_id}
            if self._username:
                params["username"] = self._username
                params["password_b64"] = _password_b64(self._password)
            self.done.emit(client.call("camera.discovery.profiles", params))
        except Exception as e:
            self.done.emit(e)


class _BatchWorker(Worker):
    """Signs in to the ticked cameras with one login and, when ``add``, adds
    each with the streams the backend picks; a few at a time."""

    row = Signal(str, str, str, object)  # candidate_id, phase, detail, added camera dict
    #: How many cameras are contacted at once.
    PARALLEL = 4

    def __init__(
        self,
        endpoint: str | None,
        jobs: list[tuple[str, str]],  # (candidate_id, name)
        username: str,
        password: str,
        *,
        add: bool,
        record: bool,
    ) -> None:
        super().__init__()
        self._endpoint = endpoint
        self._jobs = jobs
        self._username = username
        self._password = password
        self._add = add
        self._record = record

    def run(self) -> None:
        from concurrent.futures import ThreadPoolExecutor

        with ThreadPoolExecutor(max_workers=self.PARALLEL) as pool:
            for job in self._jobs:
                pool.submit(self._one, *job)

    def _one(self, candidate_id: str, name: str) -> None:
        if self.isInterruptionRequested():
            return
        client = SyncIpcClient(endpoint=self._endpoint)
        self.row.emit(candidate_id, bulk_add.CHECKING, "", None)
        params: dict = {"candidate_id": candidate_id}
        if self._username:
            params["username"] = self._username
            params["password_b64"] = _password_b64(self._password)
        try:
            found = client.call("camera.discovery.profiles", params)
        except Exception as e:  # IpcError or the backend going away
            message = getattr(e, "message", str(e))
            self.row.emit(candidate_id, bulk_add.phase_for_error(message), message, None)
            return
        if not isinstance(found, dict) or not found.get("main_stream_id"):
            self.row.emit(candidate_id, bulk_add.NO_STREAMS, "", None)
            return
        detail = bulk_add.stream_detail(found)
        if not self._add or self.isInterruptionRequested():
            self.row.emit(candidate_id, bulk_add.SIGNED_IN, detail, None)
            return
        self.row.emit(candidate_id, bulk_add.ADDING, detail, None)
        request = {
            "candidate_id": candidate_id,
            "main_stream_id": found["main_stream_id"],
            "name": name,
            "record": self._record,
        }
        if found.get("sub_stream_id"):
            request["sub_stream_id"] = found["sub_stream_id"]
        try:
            camera = client.call("camera.discovery.add", request)
        except Exception as e:
            self.row.emit(candidate_id, bulk_add.FAILED, getattr(e, "message", str(e)), None)
            return
        self.row.emit(candidate_id, bulk_add.ADDED, detail, camera)


class _ScanWorker(Worker):
    """Starts a network search and polls it until it ends or is stopped."""

    progress = Signal(object)  # camera.discovery.results dict
    done = Signal(object)  # final results dict, or Exception

    POLL_MS = 1200

    def __init__(self, endpoint: str | None, scope: dict) -> None:
        super().__init__()
        self._endpoint = endpoint
        self._scope = scope
        self._stopping = False

    def stop(self) -> None:
        self._stopping = True

    def run(self) -> None:
        try:
            client = SyncIpcClient(endpoint=self._endpoint)
            results = client.call("camera.discovery.start", self._scope)
            self.progress.emit(results)
            scan_id = results["scan_id"]
            while results.get("status") == "running":
                for _ in range(self.POLL_MS // 100):
                    if self._stopping:
                        break
                    self.msleep(100)
                if self._stopping:
                    client.call("camera.discovery.cancel", {"scan_id": scan_id})
                results = client.call("camera.discovery.results", {"scan_id": scan_id})
                self.progress.emit(results)
                if self._stopping:
                    break
            self.done.emit(results)
        except Exception as e:
            self.done.emit(e)


class _TestWorker(Worker):
    """Runs camera.test IPC call off the main thread."""

    result = Signal(object)

    def __init__(
        self, url: str, username: str = "", password_b64: str = "", endpoint: str | None = None
    ) -> None:
        super().__init__()
        self._url = url
        self._username = username
        self._password_b64 = password_b64
        self._endpoint = endpoint

    def run(self) -> None:
        try:
            client = SyncIpcClient(endpoint=self._endpoint)
            # This dialog shows the test picture: an explicit desktop preview.
            params: dict = {"url": self._url, "preview": True}
            if self._username:
                params["username"] = self._username
            if self._password_b64:
                params["password_b64"] = self._password_b64
            self.result.emit(client.call("camera.test", params))
        except Exception as e:
            self.result.emit(e)


# ---------------------------------------------------------------------------
# Dialog
# ---------------------------------------------------------------------------


class _SearchGlyph(QWidget):
    """A magnifier drawn in the theme's muted colour (no icon files needed)."""

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setFixedSize(QSize(40, 40))
        self.searching = False

    def paintEvent(self, _event: QPaintEvent) -> None:
        colors = active_colors()
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing, True)
        color = QColor(colors.accent if self.searching else colors.text_muted)
        pen = QPen(color, 2.5)
        pen.setCapStyle(Qt.PenCapStyle.RoundCap)
        painter.setPen(pen)
        painter.setBrush(Qt.BrushStyle.NoBrush)
        painter.drawEllipse(QRectF(7, 7, 19, 19))
        painter.drawLine(QPointF(23.5, 23.5), QPointF(32, 32))
        painter.end()


class _EmptyResults(QFrame):
    """What the search list shows instead of an empty table."""

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setObjectName("emptyState")
        layout = QVBoxLayout(self)
        layout.setContentsMargins(24, 16, 24, 16)
        layout.setSpacing(6)
        layout.addStretch(1)
        self._glyph = _SearchGlyph()
        layout.addWidget(self._glyph, alignment=Qt.AlignmentFlag.AlignHCenter)
        self.title = QLabel()
        self.title.setObjectName("emptyTitle")
        self.title.setAlignment(Qt.AlignmentFlag.AlignCenter)
        layout.addWidget(self.title)
        self.detail = QLabel()
        self.detail.setObjectName("emptyDetail")
        self.detail.setAlignment(Qt.AlignmentFlag.AlignCenter)
        self.detail.setWordWrap(True)
        layout.addWidget(self.detail)

        self._actions = QWidget()
        actions = QHBoxLayout(self._actions)
        actions.setContentsMargins(0, 6, 0, 0)
        actions.setSpacing(8)
        actions.addStretch(1)
        self.search_again = QPushButton("Search Again")
        set_button_variant(self.search_again, "primary")
        self.lookup = QPushButton("Look Up by Address")
        set_button_variant(self.lookup, "secondary")
        self.manual = QPushButton("Enter Stream URL")
        set_button_variant(self.manual, "secondary")
        for button in (self.search_again, self.lookup, self.manual):
            actions.addWidget(button)
        actions.addStretch(1)
        layout.addWidget(self._actions)
        layout.addStretch(1)

    def show_state(
        self, title: str, detail: str, *, actions: bool = False, searching: bool = False
    ) -> None:
        self.title.setText(title)
        self.detail.setText(detail)
        self.detail.setVisible(bool(detail))
        self._actions.setVisible(actions)
        self._glyph.searching = searching
        self._glyph.update()


class AddEditCameraDialog(QDialog):
    """Modal dialog for creating or editing a CameraConfig.

    Search tab  — search the recorder's network for ONVIF cameras (adding only).
    Camera tab  — name and credentials, then the streams: found over ONVIF
                  (address → Load Streams → main and grid stream) or entered
                  as URLs. The chosen method is what Save uses, whichever tab
                  is showing; an edited camera keeps its saved streams and
                  password unless they are changed.
    Recording, Motion tabs — the camera's other settings.
    """

    recording_policy_copied = Signal(object, object)

    def __init__(
        self,
        camera: CameraConfig | None = None,
        is_recording: bool = False,
        parent: QWidget | None = None,
        ipc_endpoint: str | None = None,
        existing_names: Iterable[str] = (),
    ) -> None:
        super().__init__(parent)
        self._endpoint = ipc_endpoint or None
        #: Names in use, so cameras added from a search get distinct ones.
        self._camera_names: set[str] = set(existing_names)
        self._is_edit = camera is not None
        self._is_recording = is_recording
        self.setWindowTitle("Edit Camera" if self._is_edit else "Add Camera")
        # One width for every tab: the motion and schedule editors need room.
        self.setMinimumWidth(840)
        self.setModal(True)
        apply_dialog_theme(self)

        self._test_worker: _TestWorker | None = None
        self._streams_worker: _StreamsWorker | None = None
        self._scan_worker: _ScanWorker | None = None
        self._interfaces_worker: _RpcWorker | None = None
        # Search tab: the ticked cameras, their progress, and those added.
        self._batch_worker: _BatchWorker | None = None
        self._ticked: dict[str, bool] = {}
        self._progress: dict[str, bulk_add.RowProgress] = {}
        self._added: list[dict] = []
        self._batch_ids: list[str] = []
        self._batch_adds = False
        # The device the ONVIF tab reads, when it came from a search or lookup.
        self._candidate: Candidate | None = None
        self._discovered: list[StreamChoice] = []
        # Streams of the camera being edited, so names and resolutions survive
        # an edit in the Advanced tab.
        self._original_streams: list[CameraStream] = list(camera.streams) if camera else []
        self._camera_id: str | None = camera.id if camera else None
        # Kept unless a new password is typed (never shown).
        self._saved_password_b64: str = camera.password_b64 if camera else ""
        self._original_discovery: DiscoveryIdentity | None = camera.discovery if camera else None
        # Main and grid stream URLs to keep selected when streams load again.
        self._keep_choice: tuple[str | None, str | None] = (None, None)
        # Settings this dialog does not show, kept as they were.
        self._motion: dict = dict(camera.motion) if camera else {}
        self._onvif: dict | None = camera.onvif if camera else None
        self._motion_worker: _RpcWorker | None = None
        self._policy_worker: _RpcWorker | None = None
        self._record_action_worker: _RpcWorker | None = None
        self._record_action = ""
        self._snapshot_worker: _RpcWorker | None = None
        self._recording_policy = {
            key: getattr(camera.recording, key) if camera else None
            for key in (
                "mode",
                "weekly_rules",
                "event_pre_sec",
                "event_post_sec",
                "non_event_keep_hours",
            )
        }

        layout = install_dialog_chrome(self, "Edit camera" if self._is_edit else "Add camera")
        subtitle = QLabel(
            "Change how NeuroWatch connects to this camera, how it records, and its motion events."
            if self._is_edit
            else "Search the network for cameras, look one up over ONVIF, or enter stream URLs."
        )
        subtitle.setObjectName("dialogSubtitle")
        layout.addWidget(subtitle)

        # ── Tab widget ──────────────────────────────────────────────────────
        self._tabs = QTabWidget()
        self._search_tab: QWidget | None = None
        if not self._is_edit:
            self._search_tab = self._build_search_tab()
            self._tabs.addTab(self._search_tab, "Search Network")
        self._camera_tab = self._build_camera_tab()
        self._tabs.addTab(self._camera_tab, "Camera")
        self._tabs.addTab(self._build_recording_tab(), "Recording")
        self._tabs.addTab(self._build_motion_tab(), "Motion")

        # ── Connection test: below the tabs, or beside them on a short screen ──
        self._test_panel = QWidget()
        test_layout = QVBoxLayout(self._test_panel)
        test_layout.setContentsMargins(0, 0, 0, 0)
        test_layout.setSpacing(10)
        self._preview = QLabel("No preview")
        self._preview.setFixedSize(320, 180)
        self._preview.setAlignment(Qt.AlignmentFlag.AlignCenter)
        self._preview.setObjectName("previewSurface")
        self._preview.setAccessibleName("Connection test preview")
        test_layout.addWidget(self._preview, alignment=Qt.AlignmentFlag.AlignHCenter)

        self._status_label = QLabel()
        self._status_label.setWordWrap(True)
        test_layout.addWidget(self._status_label)

        self._test_btn = QPushButton("Test Connection")
        set_button_variant(self._test_btn, "secondary")
        self._test_btn.clicked.connect(self._on_test)
        test_layout.addWidget(self._test_btn)
        test_layout.addStretch(1)

        self._body = QBoxLayout(QBoxLayout.Direction.TopToBottom)
        self._body.setSpacing(16)
        self._body.addWidget(self._tabs, stretch=1)
        self._body.addWidget(self._test_panel)
        layout.addLayout(self._body)

        buttons = QDialogButtonBox(
            QDialogButtonBox.StandardButton.Ok | QDialogButtonBox.StandardButton.Cancel
        )
        self._ok_btn = buttons.button(QDialogButtonBox.StandardButton.Ok)
        self._ok_btn.setText("Save Changes" if self._is_edit else "Add Camera")
        buttons.accepted.connect(self._on_accept)
        buttons.rejected.connect(self.reject)
        style_button_box(buttons)
        layout.addWidget(buttons)

        self._tabs.currentChanged.connect(lambda _index: self._on_tab_changed())
        self._motion_timer = QTimer(self)
        self._motion_timer.setInterval(2000)
        self._motion_timer.timeout.connect(self._poll_motion_if_visible)
        self._motion_timer.start()
        self._on_tab_changed()

        # Pre-fill if editing; edits open on the Camera tab.
        if camera:
            self._fill(camera)
            self._tabs.setCurrentWidget(self._camera_tab)
        self._interfaces_loaded = False

    #: Share of the screen's height the stacked layout may use before the
    #: connection test moves beside the tabs.
    STACKED_SCREEN_SHARE = 0.85

    def setVisible(self, visible: bool) -> None:  # noqa: N802
        # Decided before showing, so the dialog opens at its final size and
        # is centred with it.
        if visible and not self.isVisible():
            self.fit_to_screen()
        super().setVisible(visible)

    def fit_to_screen(self) -> None:
        """Stack the connection test under the tabs when that fits the
        screen's height, else put it beside them."""
        self._set_direction(QBoxLayout.Direction.TopToBottom)
        stacked = chrome_size_hint(self).height()
        # Only a shown connection test can move beside the tabs.
        wide = (
            not self._test_panel.isHidden()
            and stacked > self._available_height() * self.STACKED_SCREEN_SHARE
        )
        if wide:
            self._set_direction(QBoxLayout.Direction.LeftToRight)
        self.setProperty("columns", 2 if wide else 1)

    def _set_direction(self, direction: QBoxLayout.Direction) -> None:
        self._body.setDirection(direction)
        # Every enclosing layout caches its size: measure the new arrangement.
        widget: QWidget | None = self._test_panel
        while widget is not None:
            layout = widget.layout()
            if layout is not None:
                layout.invalidate()
            widget.updateGeometry()  # its parent layout caches its size too
            if widget is self:
                break
            widget = widget.parentWidget()
        self._body.invalidate()

    def _available_height(self) -> int:
        parent = self.parentWidget()
        screen = (parent.window().screen() if parent is not None else None) or self.screen()
        return screen.availableGeometry().height() if screen is not None else 10_000

    def showEvent(self, event: QShowEvent) -> None:
        super().showEvent(event)
        # The recorder's networks are listed once the dialog is on screen.
        if self._search_tab is not None and not self._interfaces_loaded:
            self._interfaces_loaded = True
            self._load_interfaces()
        self._load_motion_status()

    # ------------------------------------------------------------------
    # Search tab
    # ------------------------------------------------------------------

    def _build_search_tab(self) -> QWidget:
        w = QWidget()
        vl = QVBoxLayout(w)
        vl.setSpacing(8)

        hint = QLabel(
            "Find the ONVIF cameras on the recorder's network, tick the ones to add, "
            "and add them all with one login. Nothing is sent until you click Search."
        )
        hint.setWordWrap(True)
        set_label_role(hint, "hint")
        vl.addWidget(hint)

        row = QHBoxLayout()
        self._net_combo = QComboBox()
        self._net_combo.setAccessibleName("Network to search")
        self._net_combo.addItem("This recorder's network", {"scope": "current"})
        self._net_combo.addItem("Another range…", {"scope": "cidr"})
        self._net_combo.currentIndexChanged.connect(self._on_network_changed)
        row.addWidget(self._net_combo, stretch=1)
        self._range_edit = QLineEdit()
        self._range_edit.setPlaceholderText("10.0.10.0/24")
        self._range_edit.setAccessibleName("Range to search")
        self._range_edit.setVisible(False)
        self._range_edit.returnPressed.connect(self._on_search)
        row.addWidget(self._range_edit, stretch=1)
        self._search_btn = QPushButton("Search")
        set_button_variant(self._search_btn, "primary")
        self._search_btn.clicked.connect(self._on_search)
        row.addWidget(self._search_btn)
        vl.addLayout(row)

        self._search_status = QLabel()
        self._search_status.setWordWrap(True)
        vl.addWidget(self._search_status)

        self._results_table = QTableWidget(0, 3)
        self._results_table.setHorizontalHeaderLabels(["Device", "Address", "Status"])
        self._results_table.setSelectionBehavior(QTableWidget.SelectionBehavior.SelectRows)
        self._results_table.setSelectionMode(QTableWidget.SelectionMode.SingleSelection)
        self._results_table.setEditTriggers(QTableWidget.EditTrigger.NoEditTriggers)
        self._results_table.setShowGrid(False)
        self._results_table.setAlternatingRowColors(True)
        self._results_table.verticalHeader().setVisible(False)
        header = self._results_table.horizontalHeader()
        header.setHighlightSections(False)
        header.setDefaultAlignment(Qt.AlignmentFlag.AlignLeft | Qt.AlignmentFlag.AlignVCenter)
        # The device name takes the spare width; address and status fit.
        header.setSectionResizeMode(0, QHeaderView.ResizeMode.Stretch)
        header.setSectionResizeMode(1, QHeaderView.ResizeMode.ResizeToContents)
        header.setSectionResizeMode(2, QHeaderView.ResizeMode.ResizeToContents)
        self._results_table.itemSelectionChanged.connect(self._on_result_selected)
        self._results_table.itemChanged.connect(self._on_result_ticked)

        # No rows: a panel says why (not searched yet, searching, none found,
        # failed) instead of an empty table.
        self._results_empty = _EmptyResults()
        self._results_empty.search_again.clicked.connect(self._on_search)
        self._results_empty.lookup.clicked.connect(lambda: self._show_camera_tab("onvif"))
        self._results_empty.manual.clicked.connect(lambda: self._show_camera_tab("urls"))
        self._results_stack = QStackedWidget()
        self._results_stack.setMinimumHeight(170)
        self._results_stack.addWidget(self._results_empty)
        self._results_stack.addWidget(self._results_table)
        vl.addWidget(self._results_stack, stretch=1)
        self._results_state = "idle"

        # One login for every ticked camera.
        self._login_row = QWidget()
        login = QHBoxLayout(self._login_row)
        login.setContentsMargins(0, 0, 0, 0)
        login.setSpacing(6)
        login_label = QLabel("Camera login:")
        login.addWidget(login_label)
        self._bulk_user = QLineEdit()
        self._bulk_user.setPlaceholderText("Username (e.g. admin)")
        self._bulk_user.setAccessibleName("Username for the ticked cameras")
        login.addWidget(self._bulk_user, stretch=1)
        self._bulk_pass = QLineEdit()
        self._bulk_pass.setEchoMode(QLineEdit.EchoMode.Password)
        self._bulk_pass.setPlaceholderText("Password")
        self._bulk_pass.setAccessibleName("Password for the ticked cameras")
        self._bulk_pass.returnPressed.connect(lambda: self._run_batch(add=True))
        login.addWidget(self._bulk_pass, stretch=1)
        self._check_btn = QPushButton("Check Login")
        self._check_btn.setToolTip("Sign in to every ticked camera without adding it")
        set_button_variant(self._check_btn, "secondary")
        self._check_btn.clicked.connect(lambda: self._run_batch(add=False))
        login.addWidget(self._check_btn)
        vl.addWidget(self._login_row)

        self._use_row = QWidget()
        use_row = QHBoxLayout(self._use_row)
        use_row.setContentsMargins(0, 0, 0, 0)
        self._bulk_record = QCheckBox("Record the added cameras")
        self._bulk_record.setChecked(True)
        use_row.addWidget(self._bulk_record)
        note = QLabel(
            "Not listed? Look a camera up by address on ONVIF, or enter its URL on Advanced."
        )
        note.setWordWrap(True)
        set_label_role(note, "hint")
        use_row.addWidget(note, stretch=1)
        self._use_btn = QPushButton("Set Up Manually…")
        self._use_btn.setToolTip("Choose the streams of the selected camera yourself")
        set_button_variant(self._use_btn, "secondary")
        self._use_btn.setEnabled(False)
        self._use_btn.clicked.connect(self._use_selected)
        use_row.addWidget(self._use_btn)
        vl.addWidget(self._use_row)
        self._set_results_state("idle")
        return w

    def _set_results_state(self, state: str, detail: str = "") -> None:
        """Show the result table when it has rows, else the panel for
        ``state``: "idle", "searching", "none" or "failed"."""
        self._results_state = state
        if self._results_table.rowCount():
            self._results_stack.setCurrentWidget(self._results_table)
        else:
            network = self._net_combo.currentText()
            if network.startswith("This "):
                network = "t" + network[1:]  # mid-sentence
            if state == "searching":
                self._results_empty.show_state(
                    "Searching…", "Cameras appear here as they answer.", searching=True
                )
            elif state == "none":
                self._results_empty.show_state(
                    "No cameras found",
                    detail or "No ONVIF camera answered the search.",
                    actions=True,
                )
            elif state == "failed":
                self._results_empty.show_state("Search failed", detail, actions=True)
            else:
                self._results_empty.show_state(
                    "Search for cameras",
                    f"Click Search to list the ONVIF cameras on {network}."
                    if self._range_edit.isHidden()
                    else "Enter a range, then click Search.",
                )
            self._results_stack.setCurrentWidget(self._results_empty)
        self._sync_search_status()

    def _sync_search_status(self) -> None:
        """The status line is hidden while the panel already says the same."""
        said_by_panel = not self._results_table.rowCount() and self._results_state in (
            "none",
            "failed",
        )
        self._search_status.setVisible(bool(self._search_status.text()) and not said_by_panel)
        # Nothing to select; the panel offers the alternatives the hint names.
        self._use_row.setVisible(not said_by_panel)

    def _load_interfaces(self) -> None:
        """List the recorder's networks (local only; nothing is probed)."""
        worker = _RpcWorker(self._endpoint, "camera.discovery.interfaces")
        worker.done.connect(self._on_interfaces)
        worker.finished.connect(worker.deleteLater)
        self._interfaces_worker = worker
        worker.start()

    def _on_interfaces(self, result: object) -> None:
        self._interfaces_worker = None
        if not isinstance(result, dict):
            return
        interfaces = [i for i in result.get("interfaces", []) if isinstance(i, dict)]
        for iface in interfaces:
            label = f"{iface.get('name', '')} ({iface.get('cidr', '')})"
            if iface.get("primary"):
                self._net_combo.setItemText(0, f"This recorder's network ({iface.get('cidr', '')})")
            else:
                self._net_combo.insertItem(
                    self._net_combo.count() - 1,
                    label,
                    {"scope": "interface", "interface": iface.get("id", "")},
                )
        if self._results_state == "idle":
            self._set_results_state("idle")  # names the network found

    def _on_network_changed(self) -> None:
        scope = self._net_combo.currentData() or {}
        self._range_edit.setVisible(scope.get("scope") == "cidr")
        if self._results_state == "idle":
            self._set_results_state("idle")

    def _on_search(self) -> None:
        if self._scan_worker is not None:
            self._scan_worker.stop()
            self._search_btn.setEnabled(False)
            set_status_tone(self._search_status, "info")
            self._search_status.setText("Stopping…")
            return
        scope = dict(self._net_combo.currentData() or {"scope": "current"})
        if scope.get("scope") == "cidr":
            text = self._range_edit.text().strip()
            problem = check_range(text) if text else "Enter a range such as 192.168.1.0/24."
            if problem:
                set_status_tone(self._search_status, "warning")
                self._search_status.setText(problem)
                self._search_status.setVisible(True)
                return
            scope["cidr"] = text
        self._results_table.setRowCount(0)
        self._use_btn.setEnabled(False)
        set_status_tone(self._search_status, "info")
        self._search_status.setText("Searching…")
        self._set_results_state("searching")
        self._search_btn.setText("Stop")
        worker = _ScanWorker(self._endpoint, scope)
        worker.progress.connect(self._on_scan_progress)
        worker.done.connect(self._on_scan_done)
        worker.finished.connect(worker.deleteLater)
        self._scan_worker = worker
        worker.start()

    def _show_results(self, results: dict) -> list[Candidate]:
        candidates = [
            Candidate.from_dict(c) for c in results.get("candidates", []) if isinstance(c, dict)
        ]
        candidates.sort(key=lambda c: (not c.can_add, c.state == "possible_match", c.address))
        selected = self._selected_candidate()
        self._results_table.blockSignals(True)
        self._results_table.setRowCount(len(candidates))
        for row, candidate in enumerate(candidates):
            cid = candidate.candidate_id
            addable = (
                candidate.can_add
                and self._progress.get(cid, bulk_add.RowProgress()).phase != bulk_add.ADDED
            )
            # Ticked by default, except a device that may be a camera already added.
            ticked = self._ticked.setdefault(cid, addable and candidate.state != "possible_match")
            title = QTableWidgetItem(candidate.title)
            title.setData(Qt.ItemDataRole.UserRole, candidate)
            title.setToolTip(" ".join(p for p in (candidate.manufacturer, candidate.model) if p))
            flags = title.flags() & ~Qt.ItemFlag.ItemIsEditable
            if addable:
                title.setFlags(flags | Qt.ItemFlag.ItemIsUserCheckable)
                title.setCheckState(
                    Qt.CheckState.Checked if ticked and addable else Qt.CheckState.Unchecked
                )
            else:
                title.setFlags(flags & ~Qt.ItemFlag.ItemIsUserCheckable)
            self._results_table.setItem(row, 0, title)
            self._results_table.setItem(row, 1, QTableWidgetItem(candidate.address))
            status = QTableWidgetItem()
            self._paint_status(status, candidate)
            self._results_table.setItem(row, 2, status)
            if selected and selected.candidate_id == cid:
                self._results_table.selectRow(row)
        self._results_table.blockSignals(False)
        self._set_results_state(self._results_state)
        self._sync_add_button()
        return candidates

    def _paint_status(self, item: QTableWidgetItem, candidate: Candidate) -> None:
        """The status text, green when it worked and red when it did not."""
        item.setText(self._status_text(candidate))
        progress = self._progress.get(candidate.candidate_id)
        tone = progress.tone if progress and progress.phase != bulk_add.IDLE else ""
        colors = active_colors()
        color = {"success": colors.success, "error": colors.error}.get(tone)
        if color:
            item.setForeground(QColor(color))
        else:
            item.setData(Qt.ItemDataRole.ForegroundRole, None)

    def _status_text(self, candidate: Candidate) -> str:
        progress = self._progress.get(candidate.candidate_id)
        return progress.text if progress and progress.text else candidate.status_text

    # -- one login, several cameras ------------------------------------

    def _rows(self) -> list[tuple[int, Candidate]]:
        rows = []
        for row in range(self._results_table.rowCount()):
            item = self._results_table.item(row, 0)
            data = item.data(Qt.ItemDataRole.UserRole) if item else None
            if isinstance(data, Candidate):
                rows.append((row, data))
        return rows

    def _ticked_candidates(self) -> list[Candidate]:
        return [
            candidate
            for row, candidate in self._rows()
            if (item := self._results_table.item(row, 0)) is not None
            and item.flags() & Qt.ItemFlag.ItemIsUserCheckable
            and item.checkState() == Qt.CheckState.Checked
        ]

    def _on_result_ticked(self, item: QTableWidgetItem) -> None:
        candidate = item.data(Qt.ItemDataRole.UserRole)
        if item.column() == 0 and isinstance(candidate, Candidate):
            self._ticked[candidate.candidate_id] = item.checkState() == Qt.CheckState.Checked
            self._sync_add_button()

    def _on_tab_changed(self) -> None:
        # The connection test belongs to one camera's streams (Camera tab):
        # the Search tab tests each camera as it adds it.
        on_search = self._search_tab is not None and self._tabs.currentWidget() is self._search_tab
        self._test_panel.setVisible(
            not on_search
            and self._tabs.tabText(self._tabs.currentIndex()) not in ("Motion", "Recording")
        )
        tall = min(640, max(200, self._available_height() - 220))
        for page in (self._recording_scroll, self._motion_scroll):
            page.setMinimumHeight(tall if self._tabs.currentWidget() is page else 0)
        self._sync_add_button()
        on_motion = self._tabs.tabText(self._tabs.currentIndex()) == "Motion"
        if on_motion and self.isVisible():
            if self._camera_id and not self._zone_canvas.has_snapshot():
                self._load_zone_snapshot()
            self._load_motion_status()
        if self.isVisible():
            # Its size changes with the panel: fit the screen again, centred.
            self.fit_to_screen()
            QTimer.singleShot(0, self._recentre)

    def _recentre(self) -> None:
        if not positions_windows() or card_overlay(self) is not None:
            return  # Wayland's card overlay centres the card itself
        parent = self.parentWidget()
        area = parent.window().frameGeometry() if parent is not None else None
        if area is None and self.screen() is not None:
            area = self.screen().availableGeometry()
        if area is not None:
            frame = self.frameGeometry()
            frame.moveCenter(area.center())
            self.move(frame.topLeft())

    def _sync_add_button(self) -> None:
        """On the Search tab the dialog's button adds the ticked cameras."""
        if self._is_edit:
            return
        busy = self._batch_worker is not None
        on_search = self._search_tab is not None and self._tabs.currentWidget() is self._search_tab
        if not on_search:
            self._ok_btn.setText("Add Camera")
            self._ok_btn.setEnabled(True)
            return
        count = len(self._ticked_candidates())
        if busy:
            self._ok_btn.setText("Adding…" if self._batch_adds else "Checking…")
        elif count:
            self._ok_btn.setText(f"Add {count} Camera{'s' if count != 1 else ''}")
        else:
            self._ok_btn.setText("Add Cameras")
        self._ok_btn.setEnabled(bool(count) and not busy)
        self._check_btn.setEnabled(bool(count) and not busy)

    def _run_batch(self, *, add: bool) -> None:
        """Sign in to every ticked camera with the login entered; ``add``
        also adds each one that signs in."""
        ticked = self._ticked_candidates()
        if not ticked or self._batch_worker is not None:
            return
        taken = set(self._camera_names)
        names = bulk_add.camera_names([(c.candidate_id, c.title, c.address) for c in ticked], taken)
        self._batch_ids = [c.candidate_id for c in ticked]
        self._batch_adds = add
        for c in ticked:
            self._progress[c.candidate_id] = bulk_add.RowProgress(bulk_add.CHECKING)
        worker = _BatchWorker(
            self._endpoint,
            [(c.candidate_id, names[c.candidate_id]) for c in ticked],
            self._bulk_user.text().strip(),
            self._bulk_pass.text(),
            add=add,
            record=self._bulk_record.isChecked(),
        )
        worker.row.connect(self._on_batch_row)
        worker.finished.connect(self._on_batch_done)
        worker.finished.connect(worker.deleteLater)
        self._batch_worker = worker
        set_status_tone(self._search_status, "info")
        self._search_status.setText(
            f"{'Adding' if add else 'Checking'} {len(ticked)} camera"
            f"{'s' if len(ticked) != 1 else ''}…"
        )
        self._search_status.setVisible(True)
        self._refresh_statuses()
        self._sync_add_button()
        worker.start()

    def _on_batch_row(self, candidate_id: str, phase: str, detail: str, camera: object) -> None:
        self._progress[candidate_id] = bulk_add.RowProgress(phase, detail)
        if phase == bulk_add.ADDED and isinstance(camera, dict):
            self._added.append(camera)
            self._ticked[candidate_id] = False
            name = camera.get("name")
            if isinstance(name, str):
                self._camera_names.add(name)
        self._refresh_statuses()

    def _refresh_statuses(self) -> None:
        self._results_table.blockSignals(True)
        for row, candidate in self._rows():
            status = self._results_table.item(row, 2)
            if status is not None:
                self._paint_status(status, candidate)
            progress = self._progress.get(candidate.candidate_id)
            title = self._results_table.item(row, 0)
            if title is not None and progress and progress.phase == bulk_add.ADDED:
                title.setFlags(title.flags() & ~Qt.ItemFlag.ItemIsUserCheckable)
                title.setData(Qt.ItemDataRole.CheckStateRole, None)
        self._results_table.blockSignals(False)

    def _on_batch_done(self) -> None:
        self._batch_worker = None
        ids = self._batch_ids
        summary = bulk_add.BatchSummary.of(self._progress, ids)
        text, tone = summary.message(len(ids), self._batch_adds)
        set_status_tone(self._search_status, tone)
        self._search_status.setText(text)
        self._search_status.setVisible(True)
        self._sync_add_button()
        if self._batch_adds and summary.added == len(ids):
            self.accept()  # every ticked camera was added

    def added_cameras(self) -> list[dict]:
        """Cameras this dialog added itself (the Search tab), as the backend
        returned them; the caller adds nothing more for them."""
        return list(self._added)

    def _on_scan_progress(self, results: object) -> None:
        if not isinstance(results, dict):
            return
        found = self._show_results(results)
        if results.get("status") == "running":
            total = int(results.get("total") or 0)
            probed = int(results.get("probed") or 0)
            share = f" {round(probed * 100 / total)}%" if total else ""
            self._search_status.setText(
                f"Searching {results.get('scope', '')}…{share} · {len(found)} found"
            )

    def _on_scan_done(self, result: object) -> None:
        self._scan_worker = None
        self._search_btn.setText("Search")
        self._search_btn.setEnabled(True)
        if isinstance(result, BackendOfflineError):
            self._search_failed("The recorder's backend is offline, so nothing was searched.")
            return
        if isinstance(result, IpcError):
            self._search_failed(result.message)
            return
        if isinstance(result, Exception) or not isinstance(result, dict):
            self._search_failed(f"Search failed: {result}")
            return
        found = self._show_results(result)
        scope = result.get("scope", "")
        if result.get("status") == "failed":
            self._search_failed(str(result.get("error") or "The search failed."))
        elif result.get("status") == "cancelled":
            set_status_tone(self._search_status, "warning")
            self._search_status.setText(f"Search stopped · {len(found)} found on {scope}.")
            self._set_results_state("idle")
        elif found:
            set_status_tone(self._search_status, "success")
            self._search_status.setText(
                f"Found {len(found)} device(s) on {scope}. "
                "Select one and click Use Selected Camera."
            )
            self._set_results_state("found")
        else:
            message = (
                f"No ONVIF cameras answered on {scope}. Discovery may be off on the cameras "
                "or filtered by the network."
            )
            set_status_tone(self._search_status, "warning")
            self._search_status.setText(message)
            self._set_results_state(
                "none",
                f"Nothing on {scope} answered. ONVIF discovery may be turned off on the "
                "cameras, or the network may block it. Look a camera up by its address, "
                "or enter its stream URL.",
            )

    def _search_failed(self, message: str) -> None:
        set_status_tone(self._search_status, "error")
        self._search_status.setText(message)
        self._set_results_state("failed", message)

    def _selected_candidate(self) -> Candidate | None:
        rows = self._results_table.selectionModel().selectedRows() if self._results_table else []
        if not rows:
            return None
        item = self._results_table.item(rows[0].row(), 0)
        data = item.data(Qt.ItemDataRole.UserRole) if item else None
        return data if isinstance(data, Candidate) else None

    def _on_result_selected(self) -> None:
        candidate = self._selected_candidate()
        self._use_btn.setEnabled(bool(candidate and candidate.can_add))

    def _use_selected(self) -> None:
        candidate = self._selected_candidate()
        if candidate is None or not candidate.can_add:
            return
        self._candidate = candidate
        self._show_stream_choices([], None, None)
        self._s_host.setText(candidate.address)
        if candidate.title != "ONVIF device":
            self._name.setText(candidate.title)
        self._show_camera_tab("onvif")
        tone = "warning" if candidate.state == "possible_match" else "info"
        set_status_tone(self._discover_status, tone)
        prefix = (
            f"{candidate.status_text}: add it only if it is another camera or channel. "
            if candidate.state == "possible_match"
            else ""
        )
        self._discover_status.setText(
            f"{prefix}Enter the username and password of {candidate.title}, then Load Streams."
        )
        self._pass.setFocus()

    def _show_camera_tab(self, mode: str) -> None:
        self._set_mode(mode)
        self._tabs.setCurrentWidget(self._camera_tab)

    # ------------------------------------------------------------------
    # Camera tab: name, credentials, and where the streams come from
    # ------------------------------------------------------------------

    def _build_camera_tab(self) -> QWidget:
        w = QWidget()
        vl = QVBoxLayout(w)
        vl.setSpacing(8)

        form = QFormLayout()
        form.setLabelAlignment(Qt.AlignmentFlag.AlignRight)
        self._name = QLineEdit()
        self._name.setPlaceholderText("Front Door")
        form.addRow("Name:", self._name)
        self._user = QLineEdit()
        self._user.setPlaceholderText("admin")
        form.addRow("Username:", self._user)
        self._pass = QLineEdit()
        self._pass.setEchoMode(QLineEdit.EchoMode.Password)
        self._pass.setPlaceholderText("••••••••")
        form.addRow("Password:", self._pass)

        mode_row = QWidget()
        mode_layout = QHBoxLayout(mode_row)
        mode_layout.setContentsMargins(0, 0, 0, 0)
        self._mode_onvif = QRadioButton("Automatic (ONVIF)")
        self._mode_onvif.setToolTip(
            "Ask the camera for its streams with its address, username and password"
        )
        self._mode_urls = QRadioButton("Stream URLs")
        self._mode_urls.setToolTip("Type the camera's RTSP (or other) addresses yourself")
        self._mode_group = QButtonGroup(self)
        self._mode_group.addButton(self._mode_onvif)
        self._mode_group.addButton(self._mode_urls)
        mode_layout.addWidget(self._mode_onvif)
        mode_layout.addWidget(self._mode_urls)
        mode_layout.addStretch()
        form.addRow("Streams:", mode_row)
        vl.addLayout(form)

        self._simple_tab = self._build_simple_tab()
        self._advanced_tab = self._build_advanced_tab()
        self._source_stack = QStackedWidget()
        self._source_stack.addWidget(self._simple_tab)
        self._source_stack.addWidget(self._advanced_tab)
        vl.addWidget(self._source_stack)

        notes_form = QFormLayout()
        notes_form.setLabelAlignment(Qt.AlignmentFlag.AlignRight)
        self._notes = QLineEdit()
        self._notes.setPlaceholderText("Optional")
        notes_form.addRow("Notes:", self._notes)
        vl.addLayout(notes_form)
        vl.addStretch(1)

        self._mode_onvif.setChecked(True)
        self._mode_onvif.toggled.connect(self._on_mode_toggled)
        return w

    def _mode(self) -> str:
        """``"onvif"`` or ``"urls"``: how this camera's streams are chosen."""
        return "onvif" if self._mode_onvif.isChecked() else "urls"

    def _set_mode(self, mode: str) -> None:
        (self._mode_onvif if mode == "onvif" else self._mode_urls).setChecked(True)
        self._on_mode_toggled()

    def _on_mode_toggled(self, _checked: bool = False) -> None:
        onvif = self._mode() == "onvif"
        self._source_stack.setCurrentWidget(self._simple_tab if onvif else self._advanced_tab)
        if onvif:
            # Start from the address typed as a URL.
            if not self._s_host.text().strip():
                host, _port = _url_host_port(self._a_url.text())
                if host:
                    self._s_host.setText(host)
        elif not self._a_url.text().strip():
            # Start from the streams the camera offered, to adjust them.
            main, sub = self._chosen_streams()
            if main is not None:
                self._a_url.setText(main.url or "")
                self._a_sub_url.setText(sub.url or "" if sub else "")
                rest = [c.url for c in self._discovered if c.url and c not in (main, sub)]
                self._a_more_streams.setPlainText("\n".join(u for u in rest if u))
        self._update_credential_fields()

    def _chosen_streams(self) -> tuple[StreamChoice | None, StreamChoice | None]:
        """The ONVIF main and grid streams chosen, if any."""
        main_idx = self._s_main_combo.currentData()
        sub_idx = self._s_sub_combo.currentData()
        main = self._discovered[main_idx] if isinstance(main_idx, int) else None
        sub = self._discovered[sub_idx] if isinstance(sub_idx, int) else None
        return main, sub

    def _password(self) -> str:
        """The password typed, else the one saved with the camera being edited."""
        typed = self._pass.text()
        if typed:
            return typed
        if self._saved_password_b64 and self._user.text().strip():
            try:
                return base64.b64decode(self._saved_password_b64).decode()
            except (ValueError, UnicodeDecodeError):
                return ""
        return ""

    def _credentials(self) -> tuple[str, str]:
        """(username, password); none for devices that take no credentials."""
        if self._mode() == "urls" and _is_local_device(self._a_url.text()):
            return "", ""
        user = self._user.text().strip()
        return (user, self._password()) if user else ("", "")

    def _build_simple_tab(self) -> QWidget:
        w = QWidget()
        vl = QVBoxLayout(w)
        vl.setContentsMargins(0, 0, 0, 0)
        vl.setSpacing(8)

        form = QFormLayout()
        form.setLabelAlignment(Qt.AlignmentFlag.AlignRight)

        address_row = QWidget()
        address_layout = QHBoxLayout(address_row)
        address_layout.setContentsMargins(0, 0, 0, 0)
        address_layout.setSpacing(6)
        self._s_host = QLineEdit()
        self._s_host.setPlaceholderText("192.168.1.100")
        self._s_host.setMinimumWidth(140)
        address_layout.addWidget(self._s_host, stretch=1)
        address_layout.addWidget(QLabel("Port:"))
        self._s_port = QSpinBox()
        self._s_port.setRange(1, 65535)
        self._s_port.setValue(80)
        self._s_port.setToolTip("ONVIF HTTP port — usually 80 or 8080")
        self._s_port.setMaximumWidth(90)
        address_layout.addWidget(self._s_port)
        self._discover_btn = QPushButton("Load Streams")
        set_button_variant(self._discover_btn, "secondary")
        self._discover_btn.setToolTip("Ask the camera over ONVIF for the streams it offers")
        self._discover_btn.clicked.connect(self._on_discover)
        address_layout.addWidget(self._discover_btn)
        form.addRow("Address:", address_row)

        self._discover_status = QLabel()
        self._discover_status.setWordWrap(True)
        form.addRow("", self._discover_status)

        self._s_main_combo = QComboBox()
        self._s_main_combo.setToolTip(
            "Recorded, and shown in single view. Usually the highest resolution."
        )
        form.addRow("Main stream:", self._s_main_combo)

        self._s_sub_combo = QComboBox()
        self._s_sub_combo.setToolTip(
            "Shown in multi-camera grids. Usually the lowest resolution. "
            "Any stream can also be picked per view from a tile's right-click menu."
        )
        form.addRow("Grid stream:", self._s_sub_combo)

        vl.addLayout(form)
        self._show_stream_choices([], None, None)
        return w

    def _show_stream_choices(
        self, choices: list[StreamChoice], main_url: str | None, sub_url: str | None
    ) -> None:
        """Fill the stream lists; preselect the streams with these URLs."""
        self._discovered = list(choices)
        self._s_main_combo.clear()
        self._s_sub_combo.clear()
        if not choices:
            self._s_main_combo.addItem("Load the camera's streams first", None)
            self._s_sub_combo.addItem("Load the camera's streams first", None)
            self._s_main_combo.setEnabled(False)
            self._s_sub_combo.setEnabled(False)
            return
        self._s_main_combo.setEnabled(True)
        self._s_sub_combo.setEnabled(True)
        self._s_sub_combo.addItem("Same as main stream", None)
        for i, choice in enumerate(choices):
            self._s_main_combo.addItem(choice.label or f"Stream {i + 1}", i)
            self._s_sub_combo.addItem(choice.label or f"Stream {i + 1}", i)
        urls = [c.url for c in choices]
        if main_url in urls:
            self._s_main_combo.setCurrentIndex(urls.index(main_url))
        if sub_url and sub_url in urls:
            self._s_sub_combo.setCurrentIndex(urls.index(sub_url) + 1)  # 0: same as main

    def _build_advanced_tab(self) -> QWidget:
        w = QWidget()
        vl = QVBoxLayout(w)
        vl.setContentsMargins(0, 0, 0, 0)
        vl.setSpacing(8)

        form = QFormLayout()
        form.setLabelAlignment(Qt.AlignmentFlag.AlignRight)

        self._a_url = QLineEdit()
        self._a_url.setPlaceholderText("rtsp://192.168.1.100:554/stream1")
        self._a_url.setToolTip("Recorded, and shown in single view. Leave the password out.")
        form.addRow("Main URL:", self._a_url)

        self._a_sub_url = QLineEdit()
        self._a_sub_url.setPlaceholderText("rtsp://…  (optional: a smaller stream for grids)")
        form.addRow("Grid URL:", self._a_sub_url)

        self._a_more_streams = QPlainTextEdit()
        self._a_more_streams.setFixedHeight(52)
        self._a_more_streams.setPlaceholderText("rtsp://…/stream3  (optional, one URL per line)")
        self._a_more_streams.setToolTip(
            "Further streams (e.g. a third stream), selectable per view from a "
            "tile's right-click menu."
        )
        form.addRow("More streams:", self._a_more_streams)
        self._a_url.textChanged.connect(self._update_credential_fields)

        self._a_protocol = QComboBox()
        for p in ("Auto", "RTSP", "RTMP", "MJPEG", "V4L2", "File"):
            self._a_protocol.addItem(p)
        form.addRow("Protocol:", self._a_protocol)

        options = QWidget()
        options_layout = QHBoxLayout(options)
        options_layout.setContentsMargins(0, 0, 0, 0)
        self._a_tcp = QRadioButton("TCP (recommended)")
        self._a_udp = QRadioButton("UDP")
        self._a_tcp.setChecked(True)
        transport = QButtonGroup(self)
        transport.addButton(self._a_tcp)
        transport.addButton(self._a_udp)
        options_layout.addWidget(self._a_tcp)
        options_layout.addWidget(self._a_udp)
        options_layout.addStretch()
        form.addRow("Transport:", options)

        vl.addLayout(form)
        return w

    def _update_credential_fields(self) -> None:
        enabled = not (self._mode() == "urls" and _is_local_device(self._a_url.text()))
        self._user.setEnabled(enabled)
        self._pass.setEnabled(enabled)

    # ------------------------------------------------------------------
    # Recording tab
    # ------------------------------------------------------------------

    def _build_recording_tab(self) -> QWidget:
        from neurowatch.widgets.form_parts import settings_card

        w = QWidget()
        w.setObjectName("cameraRecordingPage")
        vl = QVBoxLayout(w)
        vl.setContentsMargins(4, 4, 8, 4)
        vl.setSpacing(14)

        # ── Recording on or off, what it does now, and the default mode ──
        card, body, heading = settings_card("Recording")
        self._rec_start_btn = QPushButton("Start Now")
        self._rec_start_btn.setToolTip("Record now, whatever the schedule says")
        set_button_variant(self._rec_start_btn, "secondary")
        self._rec_start_btn.setEnabled(self._is_edit)
        self._rec_start_btn.clicked.connect(self._on_rec_start)
        heading.addWidget(self._rec_start_btn)
        self._rec_stop_btn = QPushButton("Stop Now")
        self._rec_stop_btn.setToolTip("Stop recording now, whatever the schedule says")
        set_button_variant(self._rec_stop_btn, "danger")
        self._rec_stop_btn.setEnabled(self._is_edit)
        self._rec_stop_btn.clicked.connect(self._on_rec_stop)
        heading.addWidget(self._rec_stop_btn)
        self._local_enabled = QCheckBox("Record this camera")
        self._local_enabled.setToolTip("Save this camera's video on this computer")
        body.addWidget(self._local_enabled)
        self._policy_status_lbl = QLabel()
        self._policy_status_lbl.setWordWrap(True)
        set_label_role(self._policy_status_lbl, "hint")
        body.addWidget(self._policy_status_lbl)
        self._rec_status_lbl = QLabel()
        set_label_role(self._rec_status_lbl, "hint")
        self._rec_status_lbl.setWordWrap(True)
        body.addWidget(self._rec_status_lbl)
        self._rec_status_lbl.hide()
        vl.addWidget(card)

        # Preserve the unused legacy string without advertising cron as an
        # enforced schedule. All new schedules use explicit weekly windows.
        self._rec_schedule = QLineEdit()
        self._rec_schedule.hide()
        self._policy_editor = RecordingPolicyEditor(self._recording_policy, mode_host=body)
        self._policy_editor.copy_button.setEnabled(self._is_edit)
        self._policy_editor.copy_requested.connect(self._copy_recording_policy)
        vl.addWidget(self._policy_editor)

        # ── Where recordings go ────────────────────────────────────────
        card, body, _ = settings_card(
            "Storage", "Where this camera's recordings go, and how much space they may use."
        )
        storage = QGridLayout()
        storage.setHorizontalSpacing(14)
        storage.setVerticalSpacing(8)
        storage.setColumnStretch(1, 1)
        local_storage_row = QHBoxLayout()
        local_storage_row.setSpacing(6)
        self._local_storage = QLineEdit()
        self._local_storage.setPlaceholderText("Default folder (Settings)")
        local_storage_row.addWidget(self._local_storage, 1)
        local_browse_btn = QPushButton("Browse…")
        local_browse_btn.setAccessibleName("Browse local storage path")
        set_button_variant(local_browse_btn, "secondary")
        local_browse_btn.clicked.connect(self._browse_local_storage)
        local_storage_row.addWidget(local_browse_btn)
        storage.addWidget(QLabel("Folder"), 0, 0)
        storage.addLayout(local_storage_row, 0, 1)
        self._local_quota = QSpinBox()
        self._local_quota.setRange(0, 100000)
        self._local_quota.setSuffix(" GB")
        self._local_quota.setSpecialValueText("No limit")
        self._local_quota.setValue(0)
        self._local_quota.setMinimumWidth(140)
        self._local_quota.setToolTip(
            "When this camera's recordings reach this size, the oldest are deleted"
        )
        storage.addWidget(QLabel("Space limit"), 1, 0)
        storage.addWidget(self._local_quota, 1, 1, Qt.AlignmentFlag.AlignLeft)
        body.addLayout(storage)
        vl.addWidget(card)
        vl.addStretch()
        scroll = QScrollArea()
        scroll.setWidgetResizable(True)
        scroll.setFrameShape(QFrame.Shape.NoFrame)
        scroll.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
        scroll.setWidget(w)
        self._recording_scroll = scroll
        return scroll

    def _show_rec_status(self, text: str) -> None:
        self._rec_status_lbl.setText(text)
        self._rec_status_lbl.setVisible(bool(text))

    def _copy_recording_policy(self) -> None:
        from neurowatch.dialogs.copy_recording_policy_dialog import CopyRecordingPolicyDialog

        if not self._camera_id:
            return
        policy = self._policy_editor.policy()
        problem = policy_problem(policy)
        if problem:
            self._show_rec_status(problem)
            return
        self._copy_policy_dialog = CopyRecordingPolicyDialog(
            self._endpoint, self._camera_id, policy, self
        )
        self._copy_policy_dialog.copied.connect(self.recording_policy_copied)
        self._copy_policy_dialog.open()

    def _build_motion_tab(self) -> QWidget:
        from neurowatch.widgets.form_parts import OptionCard, settings_card
        from neurowatch.widgets.motion_zones import ZoneCanvas

        w = QWidget()
        w.setObjectName("cameraMotionPage")
        vl = QVBoxLayout(w)
        vl.setContentsMargins(4, 4, 8, 4)
        vl.setSpacing(14)

        # ── Who detects motion ──────────────────────────────────────────
        card, body, _ = settings_card(
            "Motion detection", "Choose who detects motion. Both can run together."
        )
        sources = QHBoxLayout()
        sources.setSpacing(10)
        camera_card = OptionCard(
            "Camera's own detection",
            "The camera reports motion over ONVIF, at no cost to this computer. Cameras "
            "with analytics also report intrusion, line crossing, tampering and alarm "
            "inputs. Set its areas and sensitivity in the camera's web settings.",
        )
        self._camera_events = camera_card.checkbox
        self._camera_events.setChecked(uses_camera_events(self._motion))
        self._motion_status_lbl = camera_card.status
        sources.addWidget(camera_card, 1)
        builtin_card = OptionCard(
            "NeuroWatch detection",
            "This computer analyses a small video stream (the grid stream when there is "
            "one) at 5 pictures a second, using the zones below. Recording is never "
            "affected.",
        )
        self._builtin_events = builtin_card.checkbox
        self._builtin_events.setChecked(source_of(self._motion) in ("builtin", "both"))
        self._builtin_status_lbl = builtin_card.status
        sources.addWidget(builtin_card, 1)
        body.addLayout(sources)
        vl.addWidget(card)

        # ── Detection settings ──────────────────────────────────────────
        self._builtin_controls = QWidget()
        controls = QVBoxLayout(self._builtin_controls)
        controls.setContentsMargins(0, 0, 0, 0)
        controls.setSpacing(14)
        card, body, _ = settings_card(
            "Detection settings", "How NeuroWatch detection decides that something moved."
        )
        form = QGridLayout()
        form.setHorizontalSpacing(14)
        form.setVerticalSpacing(10)
        form.setColumnStretch(1, 1)
        self._motion_sensitivity = QSlider(Qt.Orientation.Horizontal)
        self._motion_sensitivity.setRange(1, 10)
        self._motion_sensitivity.setPageStep(1)
        self._motion_sensitivity.setAccessibleName("Motion sensitivity")
        self._motion_sensitivity.setValue(int(self._motion.get("sensitivity", 5)))
        sensitivity_value = QLabel()
        sensitivity_value.setMinimumWidth(96)

        def show_sensitivity(value: int) -> None:
            word = "Low" if value <= 3 else "Medium" if value <= 7 else "High"
            sensitivity_value.setText(f"{value} · {word}")

        self._motion_sensitivity.valueChanged.connect(show_sensitivity)
        show_sensitivity(self._motion_sensitivity.value())
        form.addWidget(QLabel("Sensitivity"), 0, 0)
        form.addWidget(self._motion_sensitivity, 0, 1)
        form.addWidget(sensitivity_value, 0, 2)
        self._motion_min_duration = QDoubleSpinBox()
        self._motion_min_duration.setRange(0, 60)
        self._motion_min_duration.setDecimals(1)
        self._motion_min_duration.setSingleStep(0.5)
        self._motion_min_duration.setSuffix(" s")
        self._motion_min_duration.setValue(float(self._motion.get("min_duration_sec", 0.5)))
        self._motion_min_duration.setToolTip(
            "Movement shorter than this (a flicker, a passing shadow) starts no event"
        )
        form.addWidget(QLabel("Ignore movement shorter than"), 1, 0)
        form.addWidget(self._motion_min_duration, 1, 1, 1, 2, Qt.AlignmentFlag.AlignLeft)
        self._motion_cooldown = QDoubleSpinBox()
        self._motion_cooldown.setRange(0, 600)
        self._motion_cooldown.setDecimals(1)
        self._motion_cooldown.setSingleStep(1)
        self._motion_cooldown.setSuffix(" s")
        self._motion_cooldown.setValue(float(self._motion.get("cooldown_sec", 10)))
        self._motion_cooldown.setToolTip(
            "An event ends once the picture has been still this long; movement before "
            "then continues the same event"
        )
        form.addWidget(QLabel("End the event after stillness of"), 2, 0)
        form.addWidget(self._motion_cooldown, 2, 1, 1, 2, Qt.AlignmentFlag.AlignLeft)
        for spin in (self._motion_min_duration, self._motion_cooldown):
            spin.setMinimumWidth(110)
        body.addLayout(form)
        self._motion_low_cpu = QCheckBox("Low CPU mode (keyframes only)")
        self._motion_low_cpu.setChecked(bool(self._motion.get("low_cpu", False)))
        self._motion_low_cpu.setToolTip(
            "Decodes keyframes only. Pictures may be seconds apart, so short movements "
            "can be missed. Uses software decoding; normal detection remains the default."
        )
        body.addWidget(self._motion_low_cpu)
        low_cpu_hint = QLabel(
            "Analyses only the stream's keyframes, often seconds apart: much less work, "
            "but short movements can be missed."
        )
        low_cpu_hint.setWordWrap(True)
        set_label_role(low_cpu_hint, "hint")
        body.addWidget(low_cpu_hint)
        controls.addWidget(card)

        # ── Zones and live view ─────────────────────────────────────────
        card, body, heading = settings_card(
            "Zones and live view",
            "Draw where movement counts. Without zones the whole picture is watched; "
            "ignored areas always win. Blue blocks show what moves right now.",
        )
        self._zone_snapshot_btn = QPushButton("Refresh Picture")
        self._zone_snapshot_btn.setToolTip("Take a new picture from the camera to draw on")
        set_button_variant(self._zone_snapshot_btn, "secondary")
        self._zone_snapshot_btn.clicked.connect(self._load_zone_snapshot)
        self._zone_snapshot_btn.setEnabled(bool(self._camera_id))
        heading.addWidget(self._zone_snapshot_btn, 0, Qt.AlignmentFlag.AlignTop)
        editor = QHBoxLayout()
        editor.setSpacing(12)
        picture_column = QVBoxLayout()
        picture_column.setSpacing(8)
        self._zone_canvas = ZoneCanvas()
        self._zone_canvas.set_zones(self._motion.get("zones", []))
        self._zone_canvas.changed.connect(self._refresh_zone_picker)
        self._analysis_preview = self._zone_canvas
        picture_column.addWidget(self._zone_canvas)
        meter = QHBoxLayout()
        meter.setSpacing(8)
        meter_label = QLabel("Movement now")
        set_label_role(meter_label, "hint")
        meter.addWidget(meter_label)
        self._motion_score = QProgressBar()
        self._motion_score.setObjectName("motionMeter")
        self._motion_score.setRange(0, 100)
        self._motion_score.setValue(0)
        self._motion_score.setTextVisible(False)
        self._motion_score.setFixedHeight(6)
        self._motion_score.setAccessibleName("Live motion score")
        meter.addWidget(self._motion_score, 1)
        self._motion_score_text = QLabel("—")
        self._motion_score_text.setMinimumWidth(36)
        self._motion_score_text.setAlignment(Qt.AlignmentFlag.AlignRight)
        set_label_role(self._motion_score_text, "hint")
        meter.addWidget(self._motion_score_text)
        picture_column.addLayout(meter)
        self._zone_status = QLabel(
            "" if self._camera_id else "Save the camera first to see its picture here."
        )
        self._zone_status.setWordWrap(True)
        set_label_role(self._zone_status, "hint")
        picture_column.addWidget(self._zone_status)
        editor.addLayout(picture_column, 3)

        zone_column = QVBoxLayout()
        zone_column.setSpacing(8)
        zones_label = QLabel("Zones")
        set_label_role(zones_label, "sectionLabel")
        zone_column.addWidget(zones_label)
        self._zone_picker = QListWidget()
        self._zone_picker.setObjectName("zoneList")
        self._zone_picker.setAccessibleName("Motion zones")
        self._zone_picker.setMinimumWidth(180)
        self._zone_picker.currentRowChanged.connect(self._select_zone)
        zone_column.addWidget(self._zone_picker, 1)
        self._zone_name = QLineEdit()
        self._zone_name.setMaxLength(64)
        self._zone_name.setPlaceholderText("Zone name")
        self._zone_name.setAccessibleName("Zone name")
        self._zone_name.textEdited.connect(self._rename_zone)
        zone_column.addWidget(self._zone_name)
        add_row = QHBoxLayout()
        add_row.setSpacing(6)
        for label, kind, tip in (
            ("+ Watch", "include", "Add an area where movement counts"),
            ("+ Ignore", "exclude", "Add an area where movement never counts"),
        ):
            button = QPushButton(label)
            button.setToolTip(tip)
            set_button_variant(button, "secondary")
            button.clicked.connect(lambda _checked=False, kind=kind: self._add_zone(kind))
            add_row.addWidget(button)
        zone_column.addLayout(add_row)
        self._zone_remove = QPushButton("Remove Zone")
        set_button_variant(self._zone_remove, "ghost")
        self._zone_remove.clicked.connect(self._remove_zone)
        zone_column.addWidget(self._zone_remove)
        editor.addLayout(zone_column, 1)
        body.addLayout(editor)
        zone_hint = QLabel(
            "Click the picture to add points (three or more make a zone), drag a point to "
            "move it, right-click a point to remove it. Check the zones again after the "
            "camera is moved or re-aimed."
        )
        zone_hint.setWordWrap(True)
        set_label_role(zone_hint, "hint")
        body.addWidget(zone_hint)
        controls.addWidget(card)
        self._refresh_zone_picker()
        self._builtin_controls.setEnabled(self._builtin_events.isChecked())
        self._builtin_events.toggled.connect(self._builtin_controls.setEnabled)
        vl.addWidget(self._builtin_controls)
        vl.addStretch()
        scroll = QScrollArea()
        scroll.setWidgetResizable(True)
        scroll.setFrameShape(QFrame.Shape.NoFrame)
        scroll.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
        scroll.setWidget(w)
        self._motion_scroll = scroll
        return scroll

    def _refresh_zone_picker(self) -> None:
        from neurowatch.widgets.motion_zones import zone_color

        zones = self._zone_canvas.zones()
        selected = self._zone_canvas.selected
        self._zone_picker.blockSignals(True)
        self._zone_picker.clear()
        for zone in zones:
            points = len(zone.get("points", []))
            kind = "Ignore" if zone.get("kind") == "exclude" else "Watch"
            text = str(zone.get("name") or zone["id"])
            item = QListWidgetItem(f"{text}\n{kind}" + ("" if points >= 3 else " · add points"))
            swatch = QPixmap(12, 12)
            swatch.fill(zone_color(zone))
            item.setIcon(QIcon(swatch))
            self._zone_picker.addItem(item)
        if not zones:
            item = QListWidgetItem("Whole picture\nNo zones yet")
            item.setFlags(Qt.ItemFlag.NoItemFlags)
            self._zone_picker.addItem(item)
        else:
            self._zone_picker.setCurrentRow(selected)
        self._zone_picker.blockSignals(False)
        self._select_zone(selected)

    def _select_zone(self, index: int) -> None:
        zones = self._zone_canvas.zones()
        if not 0 <= index < len(zones):
            index = 0
        self._zone_canvas.selected = index
        name = str(zones[index].get("name", "")) if zones else ""
        if self._zone_name.text() != name:
            self._zone_name.setText(name)
        self._zone_name.setEnabled(bool(zones))
        self._zone_remove.setEnabled(bool(zones))
        self._zone_canvas.update()

    def _rename_zone(self, name: str) -> None:
        zones = self._zone_canvas.zones()
        if zones:
            zones[self._zone_canvas.selected]["name"] = name
            self._zone_canvas.set_zones(zones)
            self._refresh_zone_picker()

    def _add_zone(self, kind: str) -> None:
        if self._zone_canvas.add_zone(kind):
            self._zone_name.setFocus()
            self._zone_name.selectAll()

    def _remove_zone(self) -> None:
        zones = self._zone_canvas.zones()
        if zones:
            zones.pop(self._zone_canvas.selected)
            self._zone_canvas.set_zones(zones)
            self._refresh_zone_picker()

    def _load_zone_snapshot(self) -> None:
        if not self._camera_id or self._snapshot_worker is not None:
            return
        self._zone_snapshot_btn.setEnabled(False)
        self._zone_status.setText("Taking a picture from the camera…")
        worker = _RpcWorker(self._endpoint, "snapshot.capture", {"camera_id": self._camera_id})
        worker.done.connect(self._on_zone_snapshot)
        worker.finished.connect(worker.deleteLater)
        self._snapshot_worker = worker
        worker.start()

    def _on_zone_snapshot(self, result: object) -> None:
        self._snapshot_worker = None
        self._zone_snapshot_btn.setEnabled(True)
        set_status_tone(self._zone_status, "muted")
        if not isinstance(result, dict):
            self._zone_status.setText(
                "The camera's picture is unavailable; zones can still be drawn. "
                "Check the camera connection, then Refresh Picture."
            )
            return
        picture = QPixmap()
        try:
            data = base64.b64decode(result.get("jpeg_b64", ""), validate=True)
        except (ValueError, TypeError):
            data = b""
        if picture.loadFromData(data):
            self._zone_canvas.set_snapshot(picture)
            self._zone_status.setText("")
        else:
            self._zone_status.setText("The camera did not return a usable picture.")

    def _load_motion_status(self) -> None:
        """Show the camera's event subscription (edit mode, off the UI thread)."""
        if not self._camera_id or self._motion_worker is not None:
            return
        preview = self.isVisible() and self._tabs.tabText(self._tabs.currentIndex()) == "Motion"
        worker = _RpcWorker(
            self._endpoint,
            "motion.status",
            {
                "camera_id": self._camera_id,
                "include_preview": preview,
            },
        )
        worker.done.connect(self._on_motion_status)
        worker.finished.connect(worker.deleteLater)
        self._motion_worker = worker
        worker.start()

    def _poll_motion_if_visible(self) -> None:
        if self.isVisible() and self._tabs.tabText(self._tabs.currentIndex()) == "Motion":
            self._load_motion_status()
        elif self.isVisible() and self._tabs.tabText(self._tabs.currentIndex()) == "Recording":
            self._load_policy_status()

    def _load_policy_status(self) -> None:
        if (
            not self._camera_id
            or self._policy_worker is not None
            or self._record_action_worker is not None
        ):
            return
        worker = _RpcWorker(self._endpoint, "recording.policy_status")
        worker.done.connect(self._on_policy_status)
        worker.finished.connect(worker.deleteLater)
        self._policy_worker = worker
        worker.start()

    def _on_policy_status(self, result: object) -> None:
        self._policy_worker = None
        if not isinstance(result, dict) or not self._camera_id:
            return
        status = result.get(self._camera_id)
        if not isinstance(status, dict):
            return
        if not self._local_enabled.isChecked():
            self._policy_status_lbl.setText("Not recording: turn on Record this camera.")
            return
        names = {"continuous": "continuously", "events": "events only", "off": "nothing"}
        mode = str(status.get("active_mode"))
        text = f"Recording {names[mode]} now" if mode in names else "Recording policy"
        if status.get("manual_override"):
            text += " (started or stopped by hand)"
        detail = str(status.get("detail") or "").strip()
        if detail:
            text += f" · {detail}"
        change = status.get("next_change_wall_minutes")
        if type(change) is int and change > 0:
            hours, minutes = divmod(change, 60)
            when = f"{hours} h {minutes} min" if hours else f"{minutes} min"
            text += f" Next scheduled change in {when}."
        self._policy_status_lbl.setText(text)

    def _on_motion_status(self, result: object) -> None:
        self._motion_worker = None
        if not isinstance(result, dict) or not self._camera_id:
            return
        camera_status = result.get("cameras", {}).get(self._camera_id, {})
        if not isinstance(camera_status, dict):
            camera_status = {}
        if self._camera_events.isChecked() and uses_camera_events(self._motion):
            text, tone = camera_events_status(result, self._camera_id)
            self._camera_card_status(text, tone)
        elif self._camera_events.isChecked():
            self._camera_card_status("Starts when you save.", "info")
        else:
            self._camera_card_status("", "muted")
        topics = camera_event_topics(result, self._camera_id)
        self._motion_status_lbl.setToolTip(
            "The camera's names for them:\n" + "\n".join(topics) if topics else ""
        )
        builtin = camera_status.get("builtin")
        running = isinstance(builtin, dict) and builtin.get("state") == "running"
        self._analysis_preview.set_analysis(
            camera_status.get("preview") if running else None,
            builtin.get("blocks") if isinstance(builtin, dict) else None,
        )
        score = (
            round(float(builtin.get("score", 0)) * 100)
            if running and isinstance(builtin, dict)
            else 0
        )
        self._motion_score.setValue(score)
        self._motion_score_text.setText(f"{score}%" if running else "—")
        if not self._builtin_events.isChecked():
            self._builtin_card_status("", "muted")
        elif not isinstance(builtin, dict):
            self._builtin_card_status("Starts when you save.", "info")
        elif running:
            text = f"Running · {float(builtin.get('analysis_fps', 0)):.1f} pictures a second"
            if builtin.get("low_cpu"):
                text += " · keyframes only"
            if not builtin.get("using_substream"):
                text += " · main stream (a grid stream would use less CPU)"
            self._builtin_card_status(text, "success")
        else:
            state = str(builtin.get("state", "connecting"))
            detail = str(builtin.get("detail") or "").strip()
            tone = "error" if state in ("unsupported", "error", "failed") else "info"
            self._builtin_card_status(
                detail or {"backoff": "Reconnecting…"}.get(state, state.capitalize() + "…"),
                tone,
            )

    def _camera_card_status(self, text: str, tone: str) -> None:
        from neurowatch.widgets.form_parts import OptionCard

        card = self._camera_events.parentWidget()
        if isinstance(card, OptionCard):
            card.set_status(text, tone)

    def _builtin_card_status(self, text: str, tone: str) -> None:
        from neurowatch.widgets.form_parts import OptionCard

        card = self._builtin_events.parentWidget()
        if isinstance(card, OptionCard):
            card.set_status(text, tone)

    def _browse_local_storage(self) -> None:
        path = QFileDialog.getExistingDirectory(self, "Select Local Storage Directory")
        if path:
            self._local_storage.setText(path)

    def _on_rec_start(self) -> None:
        self._request_recording_action("start")

    def _on_rec_stop(self) -> None:
        self._request_recording_action("stop")

    def _request_recording_action(self, action: str) -> None:
        if not self._camera_id or self._record_action_worker is not None:
            return
        self._record_action = action
        self._rec_start_btn.setEnabled(False)
        self._rec_stop_btn.setEnabled(False)
        self._show_rec_status("Requesting manual recording change…")
        worker = _RpcWorker(self._endpoint, f"recording.{action}", {"id": self._camera_id})
        worker.done.connect(self._on_recording_action)
        worker.finished.connect(worker.deleteLater)
        self._record_action_worker = worker
        worker.start()

    def _on_recording_action(self, result: object) -> None:
        self._record_action_worker = None
        self._rec_start_btn.setEnabled(self._is_edit)
        self._rec_stop_btn.setEnabled(self._is_edit)
        if not isinstance(result, dict) or result.get("ok") is False:
            set_status_tone(self._rec_status_lbl, "error")
            self._show_rec_status("Recording change failed. Check the recorder and try again.")
        else:
            set_status_tone(self._rec_status_lbl, "success")
            self._show_rec_status(
                "Manual recording requested."
                if self._record_action == "start"
                else "Manual recording stopped."
            )
            self._load_policy_status()

    # ------------------------------------------------------------------
    # Pre-fill for edit mode
    # ------------------------------------------------------------------

    def _fill(self, cam: CameraConfig) -> None:
        self._name.setText(cam.name)
        url, url_user, url_password = _split_credentials(cam.url)
        self._user.setText(cam.username or url_user)
        if cam.password_b64:
            self._pass.setPlaceholderText("Saved — type a new one to change it")
        elif url_password and not cam.username:
            # An older camera with the password in its URL: shown as a field.
            self._pass.setText(url_password)
        self._notes.setText(cam.notes)
        self._a_url.setText(url)
        self._a_sub_url.setText(_split_credentials(cam.sub_url or "")[0])
        extra = [
            _split_credentials(s.url)[0] for s in cam.streams if s.url not in {cam.url, cam.sub_url}
        ]
        self._a_more_streams.setPlainText("\n".join(extra))
        self._a_tcp.setChecked(cam.transport != "udp")
        self._a_udp.setChecked(cam.transport == "udp")
        proto = (cam.protocol or "auto").capitalize()
        if proto.lower() == "auto":
            proto = "Auto"
        idx = self._a_protocol.findText(proto, Qt.MatchFlag.MatchFixedString)
        if idx >= 0:
            self._a_protocol.setCurrentIndex(idx)
        host, _port = _url_host_port(url)
        self._s_host.setText(host)
        onvif_url = (cam.onvif or {}).get("device_url")
        onvif_port = _url_host_port(onvif_url)[1] if isinstance(onvif_url, str) else None
        if onvif_port:
            self._s_port.setValue(onvif_port)
        # A camera whose streams came from ONVIF opens with them to choose from.
        from_onvif = cam.discovery is not None or cam.onvif is not None
        choices = _saved_stream_choices(cam) if from_onvif else []
        if choices:
            self._show_stream_choices(choices, url, _split_credentials(cam.sub_url or "")[0])
            set_status_tone(self._discover_status, "info")
            self._discover_status.setText(
                "These are the streams saved with this camera. Load Streams asks the camera again."
            )
            self._set_mode("onvif")
        else:
            self._set_mode("urls")
        # Recording tab
        rec = cam.recording
        self._rec_schedule.setText(rec.schedule or "")
        # Local destination — checkbox reflects live state OR persisted config.
        # A camera may be recording via the global "all cameras" toggle even when
        # rec.local.enabled is False in the stored config.
        self._local_enabled.setChecked(rec.local.enabled or self._is_recording)
        self._local_storage.setText(rec.local.storage_path or "")
        self._local_quota.setValue(rec.local.quota_gb or 0)

    # ------------------------------------------------------------------
    # ONVIF discovery
    # ------------------------------------------------------------------

    def _on_discover(self) -> None:
        host = self._s_host.text().strip()
        if not host:
            set_status_tone(self._discover_status, "warning")
            self._discover_status.setText("Enter a camera IP address first.")
            return
        self._stop_streams_worker()
        self._discover_btn.setEnabled(False)
        set_status_tone(self._discover_status, "info")
        self._discover_status.setText("Asking the camera for its streams…")
        main_before, sub_before = self._chosen_streams()
        self._keep_choice = (
            main_before.url if main_before else None,
            sub_before.url if sub_before else None,
        )
        self._show_stream_choices([], None, None)

        port = self._s_port.value()
        # A device from a search is asked by its result id; a typed address
        # is looked up first.
        candidate_id = (
            self._candidate.candidate_id
            if self._candidate and self._candidate.address == host
            else None
        )
        logger.info("ONVIF discovery started: host=%s, port=%d", host, port)
        worker = _StreamsWorker(
            self._endpoint,
            candidate_id,
            host,
            port,
            *self._onvif_credentials(),
        )
        worker.done.connect(self._on_discover_done)
        worker.finished.connect(worker.deleteLater)
        worker.start()
        self._streams_worker = worker

    def _onvif_credentials(self) -> tuple[str, str]:
        user = self._user.text().strip()
        return (user, self._password()) if user else ("", "")

    def _on_discover_done(self, result: object) -> None:
        self._discover_btn.setEnabled(True)
        self._streams_worker = None

        if isinstance(result, BackendOfflineError):
            set_status_tone(self._discover_status, "error")
            self._discover_status.setText("Backend offline — cannot reach cameras.")
            return
        if isinstance(result, Exception) or not isinstance(result, dict):
            message = result.message if isinstance(result, IpcError) else str(result)
            logger.warning("ONVIF discovery failed: %s", message)
            set_status_tone(self._discover_status, "error")
            self._discover_status.setText(
                f"The camera's streams could not be read: {message}. "
                "Check the address and password, or choose Enter stream URLs."
            )
            self._restore_saved_choices()
            return

        candidate_raw = result.get("candidate")
        if isinstance(candidate_raw, dict):
            self._candidate = Candidate.from_dict(candidate_raw)
        choices = [
            StreamChoice.from_dict(s) for s in result.get("streams", []) if isinstance(s, dict)
        ]
        choices = [c for c in choices if c.url]
        if not choices:
            logger.warning("ONVIF discovery returned no streams")
            set_status_tone(self._discover_status, "warning")
            self._discover_status.setText(
                "The camera offered no streams. Choose Enter stream URLs instead."
            )
            self._restore_saved_choices()
            return

        logger.info("ONVIF discovery found %d stream(s)", len(choices))
        # Highest resolution first, so Stream 1 is the main stream as on most NVRs.
        self._discovered = sorted(choices, key=lambda c: -c.pixels)
        candidate = self._candidate
        if candidate and candidate.state == "already_added":
            set_status_tone(self._discover_status, "warning")
            self._discover_status.setText(
                f"Found {len(choices)} stream(s). {candidate.status_text}."
            )
        else:
            set_status_tone(self._discover_status, "success")
            self._discover_status.setText(f"Found {len(choices)} stream(s).")

        if not self._name.text():
            title = candidate.title if candidate else ""
            self._name.setText(title if title and title != "ONVIF device" else self._s_host.text())
        # Keep the streams chosen before, else take the backend's suggestion:
        # main the largest, grid the smallest.
        by_id = {c.stream_id: c.url for c in self._discovered}
        kept_main, kept_sub = self._keep_choice
        urls = [c.url for c in self._discovered]
        main_url = kept_main if kept_main in urls else by_id.get(str(result.get("main_stream_id")))
        if kept_main in urls:
            sub_url = kept_sub if kept_sub in urls else None
        else:
            sub_url = by_id.get(str(result.get("sub_stream_id")))
            if sub_url is None and len(urls) >= 2:
                sub_url = urls[-1]
        self._show_stream_choices(self._discovered, main_url, sub_url)

    def _restore_saved_choices(self) -> None:
        """A failed reload of an edited camera leaves its saved streams to choose from."""
        if self._is_edit and self._original_streams:
            kept_main, kept_sub = self._keep_choice
            choices = [
                StreamChoice(
                    stream_id=s.id,
                    name=s.name,
                    width=s.width,
                    height=s.height,
                    url=_split_credentials(s.url)[0],
                )
                for s in self._original_streams
                if s.url
            ]
            self._show_stream_choices(choices, kept_main, kept_sub)

    def _stop_streams_worker(self) -> None:
        w = self._streams_worker
        if w is None:
            return
        try:
            w.done.disconnect(self._on_discover_done)
        except Exception:
            pass
        if w.isRunning():
            w.wait(2000)
        retire(w)  # still running: kept until it finishes, never destroyed running
        self._streams_worker = None

    def _stop_background_work(self) -> None:
        self._motion_timer.stop()
        for attribute, callback in (
            ("_policy_worker", self._on_policy_status),
            ("_record_action_worker", self._on_recording_action),
        ):
            worker = getattr(self, attribute)
            if worker is not None:
                try:
                    worker.done.disconnect(callback)
                except Exception:
                    pass
                retire(worker)
                setattr(self, attribute, None)
        snapshot = self._snapshot_worker
        if snapshot is not None:
            try:
                snapshot.done.disconnect(self._on_zone_snapshot)
            except Exception:
                pass
            retire(snapshot)
            self._snapshot_worker = None
        self._stop_streams_worker()
        batch = self._batch_worker
        if batch is not None:
            try:
                batch.row.disconnect(self._on_batch_row)
                batch.finished.disconnect(self._on_batch_done)
            except Exception:
                pass
            batch.requestInterruption()
            if batch.isRunning():
                batch.wait(3000)
            retire(batch)
            self._batch_worker = None
        scan = self._scan_worker
        if scan is not None:
            try:
                scan.progress.disconnect(self._on_scan_progress)
                scan.done.disconnect(self._on_scan_done)
            except Exception:
                pass
            scan.stop()
            if scan.isRunning():
                scan.wait(3000)
            retire(scan)
            self._scan_worker = None
        interfaces = self._interfaces_worker
        if interfaces is not None:
            try:
                interfaces.done.disconnect(self._on_interfaces)
            except Exception:
                pass
            if interfaces.isRunning():
                interfaces.wait(2000)
            retire(interfaces)
            self._interfaces_worker = None
        motion = self._motion_worker
        if motion is not None:
            try:
                motion.done.disconnect(self._on_motion_status)
            except Exception:
                pass
            retire(motion)
            self._motion_worker = None

    # ------------------------------------------------------------------
    # Test connection
    # ------------------------------------------------------------------

    def _on_test(self) -> None:
        url, username, password_b64 = self._effective_url_and_creds()
        if not url:
            set_status_tone(self._status_label, "warning")
            self._status_label.setText(
                "Nothing to test yet: load the camera's streams, or enter its stream URL."
            )
            return
        self._stop_test_worker()
        set_status_tone(self._status_label, "info")
        self._status_label.setText("Testing…")
        self._preview.clear()
        self._preview.setText("No preview")
        self._test_btn.setEnabled(False)

        # Log the test start without including credentials or embedded URL passwords
        _safe_url = url.split("@")[-1] if "@" in url else url
        logger.info(
            "Camera test started: host/path=%s, username=%s",
            _safe_url,
            username or "(none)",
        )
        worker = _TestWorker(url, username, password_b64, self._endpoint)
        worker.result.connect(self._on_test_result)
        worker.finished.connect(worker.deleteLater)
        worker.start()
        self._test_worker = worker

    def _on_test_result(self, result: object) -> None:
        self._test_btn.setEnabled(True)
        self._test_worker = None

        if isinstance(result, BackendOfflineError):
            logger.warning("Camera test skipped: backend offline")
            set_status_tone(self._status_label, "warning")
            self._status_label.setText(
                "Backend offline — cannot test. Camera will be added anyway."
            )
        elif isinstance(result, IpcError):
            logger.warning("Camera test IPC error: %s", result.message)
            set_status_tone(self._status_label, "error")
            self._status_label.setText(f"Error: {result.message}")
        elif isinstance(result, Exception):
            logger.warning("Camera test failed: %s", result)
            set_status_tone(self._status_label, "error")
            self._status_label.setText(f"Error: {result}")
        elif isinstance(result, dict) and result.get("ok"):
            logger.info(
                "Camera test succeeded: %dx%d @ %s fps",
                result.get("width", 0),
                result.get("height", 0),
                result.get("fps", "?"),
            )
            set_status_tone(self._status_label, "success")
            self._status_label.setText(
                f"Connected — {result.get('width')}×{result.get('height')}"
                f" @ {result.get('fps', '?')} fps"
            )
            thumb = result.get("thumbnail_b64")
            if thumb:
                pixmap = QPixmap()
                if pixmap.loadFromData(base64.b64decode(thumb)):
                    self._preview.setPixmap(
                        pixmap.scaled(
                            320,
                            180,
                            Qt.AspectRatioMode.KeepAspectRatio,
                            Qt.TransformationMode.SmoothTransformation,
                        )
                    )
        else:
            error = (
                result.get("error", "Unknown error")
                if isinstance(result, dict)
                else "Unknown error"
            )
            logger.warning("Camera test returned failure: %s", error)
            set_status_tone(self._status_label, "error")
            self._status_label.setText(f"Failed: {error}")

    def _stop_test_worker(self, wait: bool = True) -> None:
        w = self._test_worker
        if w is None:
            return
        try:
            w.result.disconnect(self._on_test_result)
        except Exception:
            pass
        if wait and w.isRunning():
            w.wait(5500)
        retire(w)
        self._test_worker = None

    # ------------------------------------------------------------------
    # Accept / build config
    # ------------------------------------------------------------------

    def _on_accept(self) -> None:
        problem = policy_problem(
            {key: value for key, value in self._policy_editor.policy().items() if value is not None}
        )
        if problem:
            set_status_tone(self._rec_status_lbl, "warning")
            self._show_rec_status(problem)
            for index in range(self._tabs.count()):
                if self._tabs.tabText(index) == "Recording":
                    self._tabs.setCurrentIndex(index)
                    break
            return
        if not valid_motion_zones(self._zone_canvas.zones()):
            set_status_tone(self._zone_status, "warning")
            self._zone_status.setText(
                "Complete each zone with 3–64 points, or remove it before saving."
            )
            for index in range(self._tabs.count()):
                if self._tabs.tabText(index) == "Motion":
                    self._tabs.setCurrentIndex(index)
                    break
            return
        if self._search_tab is not None and self._tabs.currentWidget() is self._search_tab:
            self._run_batch(add=True)
            return
        # What is checked is how the camera connects, whichever tab is showing.
        problem = self._connection_problem()
        if problem is not None:
            self._show_camera_tab(self._mode())
            label = self._discover_status if self._mode() == "onvif" else self._status_label
            set_status_tone(label, "warning")
            label.setText(problem)
            (self._discover_btn if self._mode() == "onvif" else self._a_url).setFocus()
            return
        self.accept()

    def _connection_problem(self) -> str | None:
        """Why the camera cannot be saved as it is, or None."""
        if self._mode() == "onvif":
            if self._chosen_streams()[0] is None:
                return (
                    "Load the camera's streams first (enter its address, username and "
                    "password, then Load Streams), or choose Enter stream URLs."
                )
            return None
        if not self._a_url.text().strip():
            return "Enter the camera's main stream URL."
        return None

    def _main_url(self) -> str:
        """The main stream's URL without credentials, for the chosen method."""
        if self._mode() == "onvif":
            main, _ = self._chosen_streams()
            return main.url or "" if main else ""
        return _split_credentials(self._a_url.text().strip())[0]

    def _onvif_endpoint(self, main_url: str) -> dict | None:
        """Where the camera answers ONVIF, for its events: the address and port
        typed for Load Streams, else what was saved (unless the camera moved)."""
        host, port = self._s_host.text().strip(), self._s_port.value()
        saved = self._onvif
        saved_url = saved.get("device_url") if isinstance(saved, dict) else None
        saved_host, saved_port = (
            _url_host_port(saved_url) if isinstance(saved_url, str) else ("", None)
        )
        if self._mode() == "onvif" and host:
            if saved is not None and saved_host == host and (saved_port or 80) == port:
                return saved
            authority = host if port == 80 else f"{host}:{port}"
            return {"device_url": f"http://{authority}/onvif/device_service"}
        if saved is not None and saved_host == _url_host_port(main_url)[0]:
            return saved
        return None

    def _effective_url_and_creds(self) -> tuple[str, str, str]:
        """(url, username, password_b64) to test; the backend adds the credentials."""
        url = self._main_url()
        if not url:
            return "", "", ""
        user, password = self._credentials()
        typed_user, typed_password = _split_credentials(self._a_url.text().strip())[1:]
        if self._mode() == "urls" and not user and typed_user:
            user, password = typed_user, typed_password
        return url, user, _password_b64(password)

    def get_config(self) -> CameraConfig:
        """The camera as configured: streams from the chosen method (ONVIF or
        URLs), whichever tab is showing. URLs are kept without credentials:
        the backend adds the username and password when it connects."""
        cam = CameraConfig()
        if self._camera_id:
            cam.id = self._camera_id
        user, password = self._credentials()

        if self._mode() == "onvif":
            main, sub = self._chosen_streams()
            cam.url = main.url or "" if main else ""
            cam.sub_url = sub.url or "" if sub and sub is not main else ""
            cam.streams = discovered_streams(self._discovered)
            # Remembered so later searches show this camera as already added.
            epr = self._candidate.endpoint_reference if self._candidate else None
            if epr and main:
                cam.discovery = DiscoveryIdentity(endpoint_reference=epr, source=main.source)
            else:
                cam.discovery = self._original_discovery
            cam.name = self._name.text().strip() or self._s_host.text().strip() or "Camera"
            cam.protocol = "auto"
        else:
            url, url_user, url_password = _split_credentials(self._a_url.text().strip())
            if not user and url_user:
                # Credentials typed into the URL: kept as fields instead.
                user, password = url_user, url_password
            sub_url = _split_credentials(self._a_sub_url.text().strip())[0]
            extra = [
                _split_credentials(line.strip())[0]
                for line in self._a_more_streams.toPlainText().splitlines()
                if line.strip()
            ]
            cam.streams = _manual_streams(url, sub_url, extra, self._original_streams)
            cam.url = url
            cam.sub_url = sub_url
            cam.discovery = self._original_discovery if self._is_edit else None
            cam.name = self._name.text().strip() or url.split("/")[-1] or "Camera"
            cam.protocol = self._a_protocol.currentText().lower()
        cam.transport = "udp" if self._a_udp.isChecked() else "tcp"
        cam.username = user
        cam.password_b64 = _password_b64(password) if user else ""
        cam.notes = self._notes.text().strip()
        cam.onvif = self._onvif_endpoint(cam.url)

        # Recording settings (from dedicated tab — applies regardless of which stream tab is active)
        local_quota = self._local_quota.value()
        cam.recording = CameraRecordingConfig(
            **self._policy_editor.policy(),
            schedule=self._rec_schedule.text().strip() or None,
            local=LocalRecordingDest(
                enabled=self._local_enabled.isChecked(),
                storage_path=self._local_storage.text().strip() or None,
                quota_gb=local_quota if local_quota > 0 else None,
            ),
        )

        # Motion: only send it when it changes, so the backend keeps the rest.
        if self._camera_events.isChecked() != uses_camera_events(self._motion):
            cam.motion = with_camera_events(self._motion, self._camera_events.isChecked())
        else:
            cam.motion = dict(self._motion)

        builtin = self._builtin_events.isChecked()
        if builtin != (source_of(cam.motion) in ("builtin", "both")):
            onvif = uses_camera_events(cam.motion)
            cam.motion["source"] = (
                "both"
                if builtin and onvif
                else "builtin"
                if builtin
                else "onvif"
                if onvif
                else "off"
            )
            cam.motion["enabled"] = False
        for key, value, default in (
            ("sensitivity", self._motion_sensitivity.value(), 5),
            ("cooldown_sec", self._motion_cooldown.value(), 10.0),
            ("min_duration_sec", self._motion_min_duration.value(), 0.5),
            ("low_cpu", self._motion_low_cpu.isChecked(), False),
        ):
            if value != self._motion.get(key, default):
                cam.motion[key] = value
        zones = self._zone_canvas.zones()
        if zones != self._motion.get("zones", []):
            cam.motion["zones"] = zones

        logger.info(
            "Camera config built: name='%s', protocol=%s, has_sub=%s, streams=%d, local_rec=%s",
            cam.name,
            cam.protocol,
            bool(cam.sub_url),
            len(cam.stream_options()),
            cam.recording.local.enabled,
        )
        return cam

    # ------------------------------------------------------------------

    def reject(self) -> None:
        self._stop_test_worker(wait=True)
        self._stop_background_work()
        super().reject()

    def done(self, result: int) -> None:
        self._stop_test_worker(wait=True)
        self._stop_background_work()
        super().done(result)
