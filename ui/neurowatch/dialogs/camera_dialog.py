"""Add / Edit camera dialog — Search network, ONVIF lookup and Advanced modes.

Network search and ONVIF run in the backend (``camera.discovery.*``), the
same service the web portal uses; this dialog only shows its answers.
"""

from __future__ import annotations

import base64
import logging
from urllib.parse import quote as url_quote

from PySide6.QtCore import QPointF, QRectF, QSize, Qt, QThread, Signal
from PySide6.QtGui import QColor, QPainter, QPaintEvent, QPen, QPixmap, QShowEvent
from PySide6.QtWidgets import (
    QButtonGroup,
    QCheckBox,
    QComboBox,
    QDialog,
    QDialogButtonBox,
    QFileDialog,
    QFormLayout,
    QFrame,
    QGroupBox,
    QHBoxLayout,
    QHeaderView,
    QLabel,
    QLineEdit,
    QListWidget,
    QListWidgetItem,
    QPlainTextEdit,
    QPushButton,
    QRadioButton,
    QSpinBox,
    QStackedWidget,
    QTableWidget,
    QTableWidgetItem,
    QTabWidget,
    QTextEdit,
    QVBoxLayout,
    QWidget,
)

from neurowatch.dialogs.message import show_message
from neurowatch.ipc.sync_client import BackendOfflineError, IpcError, SyncIpcClient
from neurowatch.models.camera import (
    CameraConfig,
    CameraRecordingConfig,
    CameraStream,
    DiscoveryIdentity,
    LocalRecordingDest,
)
from neurowatch.models.discovery import Candidate, StreamChoice, check_range, discovered_streams
from neurowatch.theme import (
    active_colors,
    apply_dialog_theme,
    set_button_variant,
    set_label_role,
    set_status_tone,
    style_button_box,
)
from neurowatch.widgets.window_chrome import install_dialog_chrome

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


def _password_b64(password: str) -> str:
    return base64.b64encode(password.encode()).decode() if password else ""


# ---------------------------------------------------------------------------
# Background workers — every backend call runs off the main thread.
# Network search and ONVIF run in the backend (camera.discovery.*).
# ---------------------------------------------------------------------------


class _RpcWorker(QThread):
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


class _StreamsWorker(QThread):
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


class _ScanWorker(QThread):
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


class _TestWorker(QThread):
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

    Search tab   — search the recorder's network for ONVIF cameras (adding only).
    ONVIF tab    — a camera's address + credentials; streams read over ONVIF.
    Advanced tab — manually enter full RTSP URL(s) with optional credentials.
    """

    def __init__(
        self,
        camera: CameraConfig | None = None,
        is_recording: bool = False,
        parent: QWidget | None = None,
        ipc_endpoint: str | None = None,
    ) -> None:
        super().__init__(parent)
        self._endpoint = ipc_endpoint or None
        self._is_edit = camera is not None
        self._is_recording = is_recording
        self.setWindowTitle("Edit Camera" if self._is_edit else "Add Camera")
        self.setMinimumWidth(540)
        self.setModal(True)
        apply_dialog_theme(self)

        self._test_worker: _TestWorker | None = None
        self._streams_worker: _StreamsWorker | None = None
        self._scan_worker: _ScanWorker | None = None
        self._interfaces_worker: _RpcWorker | None = None
        # The device the ONVIF tab reads, when it came from a search or lookup.
        self._candidate: Candidate | None = None
        self._discovered: list[StreamChoice] = []
        # Streams of the camera being edited, so names and resolutions survive
        # an edit in the Advanced tab.
        self._original_streams: list[CameraStream] = list(camera.streams) if camera else []
        self._camera_id: str | None = camera.id if camera else None

        layout = install_dialog_chrome(self, "Edit camera" if self._is_edit else "Add camera")
        subtitle = QLabel(
            "Search the network for cameras, look one up over ONVIF, or enter stream URLs."
        )
        subtitle.setObjectName("dialogSubtitle")
        layout.addWidget(subtitle)

        # ── Tab widget ──────────────────────────────────────────────────────
        self._tabs = QTabWidget()
        self._search_tab: QWidget | None = None
        if not self._is_edit:
            self._search_tab = self._build_search_tab()
            self._tabs.addTab(self._search_tab, "Search Network")
        self._simple_tab = self._build_simple_tab()
        self._tabs.addTab(self._simple_tab, "ONVIF")
        self._advanced_tab = self._build_advanced_tab()
        self._tabs.addTab(self._advanced_tab, "Advanced")
        self._tabs.addTab(self._build_recording_tab(), "Recording")
        layout.addWidget(self._tabs)

        # ── Shared bottom section ───────────────────────────────────────────
        self._preview = QLabel("No preview")
        self._preview.setFixedSize(320, 180)
        self._preview.setAlignment(Qt.AlignmentFlag.AlignCenter)
        self._preview.setObjectName("previewSurface")
        self._preview.setAccessibleName("Connection test preview")
        layout.addWidget(self._preview, alignment=Qt.AlignmentFlag.AlignHCenter)

        self._status_label = QLabel()
        self._status_label.setWordWrap(True)
        layout.addWidget(self._status_label)

        self._test_btn = QPushButton("Test Connection")
        set_button_variant(self._test_btn, "secondary")
        self._test_btn.clicked.connect(self._on_test)
        layout.addWidget(self._test_btn)

        buttons = QDialogButtonBox(
            QDialogButtonBox.StandardButton.Ok | QDialogButtonBox.StandardButton.Cancel
        )
        buttons.button(QDialogButtonBox.StandardButton.Ok).setText(
            "Save Changes" if self._is_edit else "Add Camera"
        )
        buttons.accepted.connect(self._on_accept)
        buttons.rejected.connect(self.reject)
        style_button_box(buttons)
        layout.addWidget(buttons)

        # Pre-fill if editing; open Advanced tab for edits (URL is already known)
        if camera:
            self._fill(camera)
            self._tabs.setCurrentWidget(self._advanced_tab)
        self._interfaces_loaded = False

    def showEvent(self, event: QShowEvent) -> None:
        super().showEvent(event)
        # The recorder's networks are listed once the dialog is on screen.
        if self._search_tab is not None and not self._interfaces_loaded:
            self._interfaces_loaded = True
            self._load_interfaces()

    # ------------------------------------------------------------------
    # Search tab
    # ------------------------------------------------------------------

    def _build_search_tab(self) -> QWidget:
        w = QWidget()
        vl = QVBoxLayout(w)
        vl.setSpacing(8)

        hint = QLabel(
            "Find ONVIF cameras on the recorder's network, like an NVR's device list. "
            "Nothing is sent until you click Search."
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
        self._results_table.doubleClicked.connect(lambda _index: self._use_selected())

        # No rows: a panel says why (not searched yet, searching, none found,
        # failed) instead of an empty table.
        self._results_empty = _EmptyResults()
        self._results_empty.search_again.clicked.connect(self._on_search)
        self._results_empty.lookup.clicked.connect(
            lambda: self._tabs.setCurrentWidget(self._simple_tab)
        )
        self._results_empty.manual.clicked.connect(
            lambda: self._tabs.setCurrentWidget(self._advanced_tab)
        )
        self._results_stack = QStackedWidget()
        self._results_stack.setMinimumHeight(170)
        self._results_stack.addWidget(self._results_empty)
        self._results_stack.addWidget(self._results_table)
        vl.addWidget(self._results_stack, stretch=1)
        self._results_state = "idle"

        self._use_row = QWidget()
        use_row = QHBoxLayout(self._use_row)
        use_row.setContentsMargins(0, 0, 0, 0)
        note = QLabel(
            "Not every camera answers a search. Look one up by address on the ONVIF tab, "
            "or enter its stream URL on Advanced."
        )
        note.setWordWrap(True)
        set_label_role(note, "hint")
        use_row.addWidget(note, stretch=1)
        self._use_btn = QPushButton("Use Selected Camera")
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
        self._results_table.setRowCount(len(candidates))
        for row, candidate in enumerate(candidates):
            title = QTableWidgetItem(candidate.title)
            title.setData(Qt.ItemDataRole.UserRole, candidate)
            title.setToolTip(" ".join(p for p in (candidate.manufacturer, candidate.model) if p))
            self._results_table.setItem(row, 0, title)
            self._results_table.setItem(row, 1, QTableWidgetItem(candidate.address))
            self._results_table.setItem(row, 2, QTableWidgetItem(candidate.status_text))
            if selected and selected.candidate_id == candidate.candidate_id:
                self._results_table.selectRow(row)
        self._set_results_state(self._results_state)
        return candidates

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
        self._discovered = []
        self._profile_list.clear()
        self._s_main_combo.clear()
        self._s_sub_combo.clear()
        self._s_host.setText(candidate.address)
        if candidate.title != "ONVIF device":
            self._s_name.setText(candidate.title)
        self._tabs.setCurrentWidget(self._simple_tab)
        tone = "warning" if candidate.state == "possible_match" else "info"
        set_status_tone(self._discover_status, tone)
        prefix = (
            f"{candidate.status_text}: add it only if it is another camera or channel. "
            if candidate.state == "possible_match"
            else ""
        )
        self._discover_status.setText(
            f"{prefix}Enter the username and password of {candidate.title}, then Discover Streams."
        )
        self._s_pass.setFocus()

    # ------------------------------------------------------------------
    # Simple tab
    # ------------------------------------------------------------------

    def _build_simple_tab(self) -> QWidget:
        w = QWidget()
        vl = QVBoxLayout(w)
        vl.setSpacing(8)

        form = QFormLayout()
        form.setLabelAlignment(Qt.AlignmentFlag.AlignRight)

        self._s_host = QLineEdit()
        self._s_host.setPlaceholderText("192.168.1.100")
        form.addRow("IP / Host:", self._s_host)

        self._s_port = QSpinBox()
        self._s_port.setRange(1, 65535)
        self._s_port.setValue(80)
        self._s_port.setToolTip("ONVIF HTTP port — usually 80 or 8080")
        form.addRow("ONVIF Port:", self._s_port)

        self._s_user = QLineEdit()
        self._s_user.setPlaceholderText("admin")
        form.addRow("Username:", self._s_user)

        self._s_pass = QLineEdit()
        self._s_pass.setEchoMode(QLineEdit.EchoMode.Password)
        self._s_pass.setPlaceholderText("••••••••")
        form.addRow("Password:", self._s_pass)

        vl.addLayout(form)

        # Discover row
        disc_row = QHBoxLayout()
        self._discover_btn = QPushButton("Discover Streams")
        set_button_variant(self._discover_btn, "secondary")
        self._discover_btn.setToolTip("Connect via ONVIF and list available stream profiles")
        self._discover_btn.clicked.connect(self._on_discover)
        disc_row.addWidget(self._discover_btn)
        self._discover_status = QLabel()
        self._discover_status.setWordWrap(True)
        disc_row.addWidget(self._discover_status, stretch=1)
        vl.addLayout(disc_row)

        # Profile list
        profiles_lbl = QLabel("Available Streams:")
        set_label_role(profiles_lbl, "sectionLabel")
        vl.addWidget(profiles_lbl)

        self._profile_list = QListWidget()
        self._profile_list.setMaximumHeight(110)
        self._profile_list.setSelectionMode(QListWidget.SelectionMode.NoSelection)
        vl.addWidget(self._profile_list)

        # Stream selection
        sel_form = QFormLayout()
        sel_form.setLabelAlignment(Qt.AlignmentFlag.AlignRight)

        self._s_name = QLineEdit()
        self._s_name.setPlaceholderText("Camera name (auto-filled from host)")
        sel_form.addRow("Camera Name:", self._s_name)

        self._s_main_combo = QComboBox()
        self._s_main_combo.setToolTip(
            "Recorded, and shown in full screen. Defaults to the highest resolution."
        )
        sel_form.addRow("Main Stream:", self._s_main_combo)

        self._s_sub_combo = QComboBox()
        self._s_sub_combo.setToolTip(
            "Shown in multi-camera grids. Defaults to the lowest resolution. "
            "Any stream can also be picked per view from a tile's right-click menu."
        )
        sel_form.addRow("Grid Stream:", self._s_sub_combo)

        vl.addLayout(sel_form)
        return w

    # ------------------------------------------------------------------
    # Advanced tab
    # ------------------------------------------------------------------

    def _build_advanced_tab(self) -> QWidget:
        w = QWidget()
        vl = QVBoxLayout(w)
        vl.setSpacing(8)

        form = QFormLayout()
        form.setLabelAlignment(Qt.AlignmentFlag.AlignRight)

        self._a_name = QLineEdit()
        self._a_name.setPlaceholderText("Front Door")
        form.addRow("Name:", self._a_name)

        self._a_url = QLineEdit()
        self._a_url.setPlaceholderText("rtsp://192.168.1.100:554/stream1")
        form.addRow("Main URL:", self._a_url)

        self._a_sub_url = QLineEdit()
        self._a_sub_url.setPlaceholderText("rtsp://… (optional low-res sub stream)")
        form.addRow("Sub URL:", self._a_sub_url)

        self._a_more_streams = QPlainTextEdit()
        self._a_more_streams.setFixedHeight(52)
        self._a_more_streams.setPlaceholderText("rtsp://…/stream3  (optional, one URL per line)")
        self._a_more_streams.setToolTip(
            "Further streams (e.g. a third stream), selectable per view from a "
            "tile's right-click menu."
        )
        form.addRow("More Streams:", self._a_more_streams)

        self._a_user = QLineEdit()
        self._a_user.setPlaceholderText("admin  (optional if embedded in URL)")
        form.addRow("Username:", self._a_user)

        self._a_pass = QLineEdit()
        self._a_pass.setEchoMode(QLineEdit.EchoMode.Password)
        self._a_pass.setPlaceholderText("••••••••  (optional if embedded in URL)")
        form.addRow("Password:", self._a_pass)
        self._a_url.textChanged.connect(self._update_credential_fields)

        self._a_protocol = QComboBox()
        for p in ("Auto", "RTSP", "RTMP", "MJPEG", "V4L2", "File"):
            self._a_protocol.addItem(p)
        form.addRow("Protocol:", self._a_protocol)

        vl.addLayout(form)

        # Transport
        tg = QGroupBox("Transport")
        tl = QHBoxLayout(tg)
        self._a_tcp = QRadioButton("TCP (recommended)")
        self._a_udp = QRadioButton("UDP")
        self._a_tcp.setChecked(True)
        bg = QButtonGroup(self)
        bg.addButton(self._a_tcp)
        bg.addButton(self._a_udp)
        tl.addWidget(self._a_tcp)
        tl.addWidget(self._a_udp)
        tl.addStretch()
        vl.addWidget(tg)

        # Notes
        self._a_notes = QTextEdit()
        self._a_notes.setFixedHeight(52)
        self._a_notes.setPlaceholderText("Optional notes…")
        vl.addWidget(QLabel("Notes:"))
        vl.addWidget(self._a_notes)

        return w

    def _update_credential_fields(self) -> None:
        enabled = not _is_local_device(self._a_url.text())
        self._a_user.setEnabled(enabled)
        self._a_pass.setEnabled(enabled)

    def _advanced_creds(self) -> tuple[str, str]:
        """Return (username, raw_password), empty for credential-less devices."""
        if not self._a_user.isEnabled():
            return "", ""
        return self._a_user.text().strip(), self._a_pass.text()

    # ------------------------------------------------------------------
    # Recording tab
    # ------------------------------------------------------------------

    def _build_recording_tab(self) -> QWidget:
        w = QWidget()
        vl = QVBoxLayout(w)
        vl.setSpacing(10)

        # Shared schedule row
        sched_form = QFormLayout()
        sched_form.setLabelAlignment(Qt.AlignmentFlag.AlignRight)
        self._rec_schedule = QLineEdit()
        self._rec_schedule.setPlaceholderText("Always  (or cron: 0 8 * * 1-5)")
        self._rec_schedule.setToolTip("Leave blank for always-on; applies to both destinations")
        sched_form.addRow("Schedule:", self._rec_schedule)
        vl.addLayout(sched_form)

        # ── Local Storage group ─────────────────────────────────────────
        local_grp = QGroupBox("Local Storage")
        local_form = QFormLayout(local_grp)
        local_form.setLabelAlignment(Qt.AlignmentFlag.AlignRight)

        self._local_enabled = QCheckBox("Enable local recording")
        local_form.addRow("", self._local_enabled)

        local_storage_row = QWidget()
        local_storage_hl = QHBoxLayout(local_storage_row)
        local_storage_hl.setContentsMargins(0, 0, 0, 0)
        local_storage_hl.setSpacing(6)
        self._local_storage = QLineEdit()
        self._local_storage.setPlaceholderText("Default (global setting)")
        local_storage_hl.addWidget(self._local_storage)
        local_browse_btn = QPushButton("Browse…")
        local_browse_btn.setAccessibleName("Browse local storage path")
        set_button_variant(local_browse_btn, "secondary")
        local_browse_btn.clicked.connect(self._browse_local_storage)
        local_storage_hl.addWidget(local_browse_btn)
        local_form.addRow("Storage Path:", local_storage_row)

        self._local_quota = QSpinBox()
        self._local_quota.setRange(0, 100000)
        self._local_quota.setSuffix(" GB")
        self._local_quota.setSpecialValueText("No limit")
        self._local_quota.setValue(0)
        self._local_quota.setToolTip(
            "Auto-delete oldest segments when this limit is reached (0 = no limit)"
        )
        local_form.addRow("Storage Quota:", self._local_quota)

        vl.addWidget(local_grp)

        # Start / Stop Now buttons (only useful in edit mode with backend connected)
        ctrl_row = QHBoxLayout()
        self._rec_start_btn = QPushButton("▶ Start Recording Now")
        set_button_variant(self._rec_start_btn, "secondary")
        self._rec_start_btn.setEnabled(self._is_edit)
        self._rec_start_btn.clicked.connect(self._on_rec_start)
        ctrl_row.addWidget(self._rec_start_btn)

        self._rec_stop_btn = QPushButton("■ Stop Recording Now")
        set_button_variant(self._rec_stop_btn, "danger")
        self._rec_stop_btn.setEnabled(self._is_edit)
        self._rec_stop_btn.clicked.connect(self._on_rec_stop)
        ctrl_row.addWidget(self._rec_stop_btn)
        vl.addLayout(ctrl_row)

        self._rec_status_lbl = QLabel()
        set_label_role(self._rec_status_lbl, "hint")
        self._rec_status_lbl.setWordWrap(True)
        vl.addWidget(self._rec_status_lbl)
        vl.addStretch()
        return w

    def _browse_local_storage(self) -> None:
        path = QFileDialog.getExistingDirectory(self, "Select Local Storage Directory")
        if path:
            self._local_storage.setText(path)

    def _on_rec_start(self) -> None:
        cam_id = self._camera_id
        if not cam_id:
            return
        try:
            client = SyncIpcClient(endpoint=self._endpoint)
            client.call("recording.start", {"id": cam_id})
            set_status_tone(self._rec_status_lbl, "success")
            self._rec_status_lbl.setText("Recording started.")
        except Exception as e:
            set_status_tone(self._rec_status_lbl, "error")
            self._rec_status_lbl.setText(f"Error: {e}")

    def _on_rec_stop(self) -> None:
        cam_id = self._camera_id
        if not cam_id:
            return
        try:
            client = SyncIpcClient(endpoint=self._endpoint)
            client.call("recording.stop", {"id": cam_id})
            set_status_tone(self._rec_status_lbl, "warning")
            self._rec_status_lbl.setText("Recording stopped.")
        except Exception as e:
            set_status_tone(self._rec_status_lbl, "error")
            self._rec_status_lbl.setText(f"Error: {e}")

    # ------------------------------------------------------------------
    # Pre-fill for edit mode
    # ------------------------------------------------------------------

    def _fill(self, cam: CameraConfig) -> None:
        self._a_name.setText(cam.name)
        self._a_url.setText(cam.url)
        self._a_sub_url.setText(cam.sub_url or "")
        extra = [s.url for s in cam.streams if s.url not in {cam.url, cam.sub_url}]
        self._a_more_streams.setPlainText("\n".join(extra))
        self._a_user.setText(cam.username or "")
        self._a_tcp.setChecked(cam.transport != "udp")
        self._a_udp.setChecked(cam.transport == "udp")
        self._a_notes.setPlainText(cam.notes)
        proto = (cam.protocol or "auto").capitalize()
        if proto.lower() == "auto":
            proto = "Auto"
        idx = self._a_protocol.findText(proto, Qt.MatchFlag.MatchFixedString)
        if idx >= 0:
            self._a_protocol.setCurrentIndex(idx)
        # Pre-fill Simple tab host from the stored URL
        url = cam.url
        if "://" in url:
            rest = url.split("://", 1)[1]
            if "@" in rest:
                rest = rest.split("@", 1)[1]
            host = rest.split("/")[0].split(":")[0]
            self._s_host.setText(host)
        self._s_user.setText(cam.username or "")
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
        self._discover_status.setText("Connecting via ONVIF…")
        self._profile_list.clear()
        self._s_main_combo.clear()
        self._s_sub_combo.clear()
        self._discovered = []

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
            self._s_user.text().strip(),
            self._s_pass.text(),
        )
        worker.done.connect(self._on_discover_done)
        worker.finished.connect(worker.deleteLater)
        worker.start()
        self._streams_worker = worker

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
                f"ONVIF failed: {message}\nTry Advanced mode and enter the RTSP URL manually."
            )
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
            self._discover_status.setText("No streams found. Try Advanced mode.")
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

        self._profile_list.clear()
        self._s_main_combo.clear()
        self._s_sub_combo.clear()
        self._s_sub_combo.addItem("None (no sub stream)", None)
        for i, choice in enumerate(self._discovered):
            self._profile_list.addItem(QListWidgetItem(choice.label))
            self._s_main_combo.addItem(choice.label, i)
            self._s_sub_combo.addItem(choice.label, i)

        if not self._s_name.text():
            title = candidate.title if candidate else ""
            self._s_name.setText(
                title if title and title != "ONVIF device" else self._s_host.text()
            )
        # The backend suggests main (largest) and grid (smallest) streams.
        ids = [c.stream_id for c in self._discovered]
        main_id, sub_id = result.get("main_stream_id"), result.get("sub_stream_id")
        if main_id in ids:
            self._s_main_combo.setCurrentIndex(ids.index(main_id))
        if sub_id in ids:
            self._s_sub_combo.setCurrentIndex(ids.index(sub_id) + 1)  # 0 = None
        elif len(ids) >= 2:
            self._s_sub_combo.setCurrentIndex(len(ids))

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
        self._streams_worker = None

    def _stop_background_work(self) -> None:
        self._stop_streams_worker()
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
            self._scan_worker = None
        interfaces = self._interfaces_worker
        if interfaces is not None:
            try:
                interfaces.done.disconnect(self._on_interfaces)
            except Exception:
                pass
            if interfaces.isRunning():
                interfaces.wait(2000)
            self._interfaces_worker = None

    # ------------------------------------------------------------------
    # Test connection
    # ------------------------------------------------------------------

    def _on_test(self) -> None:
        url, username, password_b64 = self._effective_url_and_creds()
        if not url:
            set_status_tone(self._status_label, "warning")
            self._status_label.setText(
                "No URL to test. Discover streams (Simple) or enter a URL (Advanced)."
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
        self._test_worker = None

    # ------------------------------------------------------------------
    # Accept / build config
    # ------------------------------------------------------------------

    def _on_accept(self) -> None:
        if self._search_tab is not None and self._tabs.currentWidget() is self._search_tab:
            show_message(
                self,
                "Choose a Camera",
                "Select a camera in the search results and click Use Selected Camera, "
                "then read its streams on the ONVIF tab.",
            )
            return
        is_simple = self._tabs.currentWidget() is self._simple_tab
        if is_simple:
            if not self._discovered:
                show_message(
                    self,
                    "No Streams Discovered",
                    "Run ONVIF discovery first, or switch to the Advanced tab "
                    "and enter the RTSP URL manually.",
                    kind="warning",
                )
                return
            if self._s_main_combo.currentData() is None:
                show_message(self, "Select Stream", "Select a main stream.", kind="warning")
                return
        else:
            if not self._a_url.text().strip():
                show_message(self, "Validation", "Stream URL is required.", kind="warning")
                return
        self.accept()

    def _effective_url_and_creds(self) -> tuple[str, str, str]:
        """Return (url_with_creds, username, password_b64) for the active tab."""
        is_simple = self._tabs.currentWidget() is self._simple_tab
        if is_simple:
            idx = self._s_main_combo.currentData()
            if idx is None or not self._discovered:
                return "", "", ""
            # Discovered URLs carry no credentials; camera.test adds them.
            url = self._discovered[idx].url or ""
            user = self._s_user.text().strip()
            return url, user, _password_b64(self._s_pass.text())
        else:
            url = self._a_url.text().strip()
            user, raw_pass = self._advanced_creds()
            pb64 = base64.b64encode(raw_pass.encode()).decode() if raw_pass else ""
            # Embed credentials into URL for testing if user supplied them separately
            effective = _inject_creds(url, user, raw_pass) if user else url
            return effective, user, pb64

    def get_config(self) -> CameraConfig:
        """Return a CameraConfig built from the active tab's fields."""
        cam = CameraConfig()
        if self._camera_id:
            cam.id = self._camera_id

        is_simple = self._tabs.currentWidget() is self._simple_tab

        if is_simple:
            main_idx = self._s_main_combo.currentData()
            sub_idx = self._s_sub_combo.currentData()
            main = self._discovered[main_idx] if main_idx is not None else None
            cam.name = self._s_name.text().strip() or self._s_host.text().strip() or "Camera"
            cam.url = (main.url or "") if main else ""
            cam.sub_url = (self._discovered[sub_idx].url or "") if sub_idx is not None else ""
            cam.streams = discovered_streams(self._discovered)
            # Remembered so later searches show this camera as already added.
            epr = self._candidate.endpoint_reference if self._candidate else None
            if epr and main:
                cam.discovery = DiscoveryIdentity(endpoint_reference=epr, source=main.source)
            cam.username = self._s_user.text().strip()
            raw_pass = self._s_pass.text()
            if raw_pass:
                cam.password_b64 = base64.b64encode(raw_pass.encode()).decode()
            cam.transport = "tcp"
            cam.protocol = "auto"
        else:
            user, raw_pass = self._advanced_creds()
            url = self._a_url.text().strip()
            sub_url = self._a_sub_url.text().strip()

            extra = [
                line.strip()
                for line in self._a_more_streams.toPlainText().splitlines()
                if line.strip()
            ]

            # Embed credentials into every URL if not already present
            if user:
                url = _inject_creds(url, user, raw_pass)
                if sub_url:
                    sub_url = _inject_creds(sub_url, user, raw_pass)
                extra = [_inject_creds(u, user, raw_pass) for u in extra]
            cam.streams = _manual_streams(url, sub_url, extra, self._original_streams)

            cam.name = self._a_name.text().strip() or url.split("/")[-1] or "Camera"
            cam.url = url
            cam.sub_url = sub_url
            cam.username = user
            if raw_pass:
                cam.password_b64 = base64.b64encode(raw_pass.encode()).decode()
            cam.transport = "udp" if self._a_udp.isChecked() else "tcp"
            cam.protocol = self._a_protocol.currentText().lower()
            cam.notes = self._a_notes.toPlainText().strip()

        # Recording settings (from dedicated tab — applies regardless of which stream tab is active)
        local_quota = self._local_quota.value()
        cam.recording = CameraRecordingConfig(
            schedule=self._rec_schedule.text().strip() or None,
            local=LocalRecordingDest(
                enabled=self._local_enabled.isChecked(),
                storage_path=self._local_storage.text().strip() or None,
                quota_gb=local_quota if local_quota > 0 else None,
            ),
        )

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
