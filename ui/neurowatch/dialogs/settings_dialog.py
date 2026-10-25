from __future__ import annotations

import sys
from copy import deepcopy

from PySide6.QtCore import QSize, Qt, QTime, Signal
from PySide6.QtGui import QGuiApplication
from PySide6.QtWidgets import (
    QCheckBox,
    QComboBox,
    QDialog,
    QDialogButtonBox,
    QFileDialog,
    QFormLayout,
    QFrame,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QListWidget,
    QPushButton,
    QRadioButton,
    QScrollArea,
    QSizePolicy,
    QSlider,
    QSpinBox,
    QStackedWidget,
    QTimeEdit,
    QVBoxLayout,
    QWidget,
)

from neurowatch.dialogs.drive_picker import (
    Drive,
    DrivePickerDialog,
    DriveScanWorker,
    StorageScan,
    describe_space,
    format_bytes,
)
from neurowatch.dialogs.message import ask, show_message
from neurowatch.ipc.sync_client import IpcError, SyncIpcClient
from neurowatch.models.archive import (
    MAX_AFTER_DAYS,
    ArchiveSettings,
    ArchiveStatus,
    MissingLocation,
)
from neurowatch.models.preview_fps import (
    DENSE_WALL_TILES,
    MAX_HIDDEN_FPS,
    MAX_VISIBLE_FPS,
    PreviewFps,
)
from neurowatch.models.video_decoder import (
    DecoderStatus,
    decoder_choices,
    decoder_from_config,
)
from neurowatch.service.startup import StartupPrefs
from neurowatch.theme import (
    apply_dialog_theme,
    set_button_variant,
    set_label_role,
    set_status_tone,
    style_button_box,
)
from neurowatch.utils.threads import Worker, retire
from neurowatch.widgets.form_parts import settings_card
from neurowatch.widgets.window_chrome import install_dialog_chrome, resize_window

#: Disk threshold shown when it is off or not set (the backend's default).
DEFAULT_DISK_THRESHOLD_PERCENT = 90


class _ArchiveStatusWorker(Worker):
    """Reads ``recording.archive_status`` off the UI thread."""

    done = Signal(object)  # ArchiveStatus, or Exception

    def __init__(self, endpoint: str) -> None:
        super().__init__()
        self._endpoint = endpoint

    def run(self) -> None:
        try:
            result = SyncIpcClient(endpoint=self._endpoint).call("recording.archive_status", {})
            self.done.emit(ArchiveStatus.from_json(result))
        except Exception as exc:
            self.done.emit(exc)


class _UseLocationWorker(Worker):
    """Asks the recorder to record to a missing location again
    (``recording.use_location``) off the UI thread."""

    done = Signal(object)  # None on success, or Exception

    def __init__(self, endpoint: str, path: str) -> None:
        super().__init__()
        self._endpoint = endpoint
        self._path = path

    def run(self) -> None:
        try:
            SyncIpcClient(endpoint=self._endpoint).call(
                "recording.use_location", {"path": self._path}
            )
            self.done.emit(None)
        except Exception as exc:
            self.done.emit(exc)


class _DecoderStatusWorker(Worker):
    """Reads ``system.status`` (the decoders found, the one in use) off the
    UI thread."""

    done = Signal(object)  # DecoderStatus, or Exception

    def __init__(self, endpoint: str) -> None:
        super().__init__()
        self._endpoint = endpoint

    def run(self) -> None:
        try:
            result = SyncIpcClient(endpoint=self._endpoint).call("system.status", {})
            self.done.emit(DecoderStatus.from_status(result))
        except Exception as exc:
            self.done.emit(exc)


#: The tabs, in order: key → title (``SettingsDialog(tab=key)`` opens one).
TABS = {
    "general": "General",
    "wall": "Video Wall",
    "recording": "Recording",
    "archive": "Archive",
    "startup": "Startup",
    "cloud": "Cloud",
    "debug": "Debug",
}


#: The window opens at this share of the screen (under GNOME's 75%, past which
#: it maximizes a new window), and resizes from its edges down to the minimum.
_WIDTH_SHARE = 0.72
_HEIGHT_SHARE = 0.62
_MIN_WIDTH = 900
#: The pages' least height (long pages scroll).
_MIN_PAGE_HEIGHT = 480
#: Each sidebar row's height.
_NAV_ROW_HEIGHT = 38


class SettingsDialog(QDialog):
    """Settings pages (``TABS``), picked in a sidebar: General, Video Wall
    (with the live preview frame rates), Recording (with the storage
    calculator), Archive, Startup (what starts at sign-in and at boot,
    fullscreen), Cloud (link to NeuroWatch Cloud) and Debug. Each page is a
    column of titled cards (``widgets.form_parts.settings_card``).

    With ``ipc_endpoint`` the recording drive can be picked from the
    backend's drive scan (``storage.list``); the choice is applied on save
    (``selected_drive``). ``startup`` is the user's saved startup choice
    (``startup_prefs`` returns the edited one); ``startup_note`` says why it
    cannot be changed here (a source checkout, where the service follows
    the window), and disables it.
    """

    def __init__(
        self,
        config: dict,
        parent: QWidget | None = None,
        *,
        ipc_endpoint: str | None = None,
        tab: str | None = None,
        startup: StartupPrefs | None = None,
        startup_note: str | None = None,
    ) -> None:
        super().__init__(parent)
        self.setWindowTitle("Settings")
        screen = (parent.screen() if parent is not None else None) or (
            QGuiApplication.primaryScreen()
        )
        available = screen.availableGeometry() if screen is not None else None
        minimum = _MIN_WIDTH
        self._opening_size = QSize(_MIN_WIDTH, _MIN_PAGE_HEIGHT + 140)
        if available is not None:
            minimum = min(_MIN_WIDTH, int(available.width() * 0.95))  # a small screen
            self._opening_size = QSize(
                max(minimum, int(available.width() * _WIDTH_SHARE)),
                max(_MIN_PAGE_HEIGHT + 140, int(available.height() * _HEIGHT_SHARE)),
            )
        self.setMinimumWidth(minimum)
        self._config = deepcopy(config)
        self._endpoint = ipc_endpoint or None
        self._scan: StorageScan | None = None
        self._scan_worker: DriveScanWorker | None = None
        self._archive_worker: _ArchiveStatusWorker | None = None
        self._use_worker: _UseLocationWorker | None = None
        self._decoder_worker: _DecoderStatusWorker | None = None
        self._chosen_drive: Drive | None = None
        apply_dialog_theme(self)

        layout = install_dialog_chrome(self, "Settings", resizable=True)
        body = QHBoxLayout()
        body.setSpacing(20)
        self._nav = QListWidget()
        self._nav.setObjectName("settingsNav")
        self._nav.setAccessibleName("Settings sections")
        self._nav.setFixedWidth(190)
        self._stack = QStackedWidget()
        self._stack.setMinimumHeight(_MIN_PAGE_HEIGHT)
        self._pages: dict[str, QVBoxLayout] = {}
        for key, title in TABS.items():
            self._nav.addItem(title)
            item = self._nav.item(self._nav.count() - 1)
            item.setSizeHint(QSize(0, _NAV_ROW_HEIGHT))
            page = QWidget()
            page.setObjectName("settingsPage")
            page_layout = QVBoxLayout(page)
            page_layout.setContentsMargins(0, 0, 12, 0)
            page_layout.setSpacing(14)
            heading = QLabel(title)
            heading.setObjectName("settingsPageTitle")
            page_layout.addWidget(heading)
            scroll = QScrollArea()
            scroll.setWidgetResizable(True)
            scroll.setFrameShape(QFrame.Shape.NoFrame)
            scroll.setWidget(page)
            self._stack.addWidget(scroll)
            self._pages[key] = page_layout
        body.addWidget(self._nav)
        body.addWidget(self._stack, stretch=1)
        layout.addLayout(body, stretch=1)

        general, general_form = self._card(
            "Site and appearance", "The name shown in the title bar, and how NeuroWatch looks."
        )
        self._site_name = QLineEdit(self._config.get("site_name", "My Site"))
        general_form.addRow("Site Name:", self._site_name)

        self._theme = QComboBox()
        self._theme.addItems(["Dark", "Light"])
        theme = self._config.get("ui", {}).get("theme", "dark").capitalize()
        idx = self._theme.findText(theme)
        if idx >= 0:
            self._theme.setCurrentIndex(idx)
        self._theme.currentTextChanged.connect(
            lambda value: apply_dialog_theme(self, value.lower())
        )
        general_form.addRow("Theme:", self._theme)

        self._fullscreen_status = QCheckBox("Show status over video in fullscreen")
        self._fullscreen_status.setToolTip(
            "With Hide panels in fullscreen, keep the status readings (disk, CPU, memory,"
            " network, decoder, backend) over the top of the video, half"
            " transparent, while the title strip is hidden"
        )
        self._fullscreen_status.setChecked(
            bool(self._config.get("ui", {}).get("fullscreen_status", True))
        )
        general_form.addRow("", self._fullscreen_status)

        # Opacity of that overlay's background; the readings stay opaque.
        opacity_row = QWidget()
        opacity_layout = QHBoxLayout(opacity_row)
        opacity_layout.setContentsMargins(0, 0, 0, 0)
        self._status_opacity = QSlider(Qt.Orientation.Horizontal)
        self._status_opacity.setRange(0, 100)
        self._status_opacity.setSingleStep(5)
        self._status_opacity.setPageStep(10)
        self._status_opacity.setAccessibleName("Status overlay background opacity")
        self._status_opacity.setToolTip(
            "How opaque the status bar's background is over the video (0% shows only the readings)"
        )
        opacity = self._config.get("ui", {}).get("fullscreen_status_opacity", 50)
        try:
            self._status_opacity.setValue(max(0, min(100, int(opacity))))
        except (TypeError, ValueError):
            self._status_opacity.setValue(50)
        self._status_opacity_value = QLabel()
        self._status_opacity_value.setMinimumWidth(
            self._status_opacity_value.fontMetrics().horizontalAdvance("100%") + 4
        )
        self._status_opacity.valueChanged.connect(
            lambda value: self._status_opacity_value.setText(f"{value}%")
        )
        self._status_opacity_value.setText(f"{self._status_opacity.value()}%")
        opacity_layout.addWidget(self._status_opacity, stretch=1)
        opacity_layout.addWidget(self._status_opacity_value)
        self._fullscreen_status.toggled.connect(opacity_row.setEnabled)
        opacity_row.setEnabled(self._fullscreen_status.isChecked())
        general_form.addRow("Overlay opacity:", opacity_row)
        self._pages["general"].addWidget(general)

        wall_cfg = self._config.get("wall", {})
        wall, wall_form = self._card("Layout", "How the wall arranges and pages your cameras.")
        self._interval = QSpinBox()
        self._interval.setRange(0, 300)
        self._interval.setValue(int(wall_cfg.get("page_interval_sec", 0)))
        wall_form.addRow("Auto-scroll (sec):", self._interval)
        hint = QLabel("0 = disabled")
        set_label_role(hint, "hint")
        wall_form.addRow("", hint)

        rc = QHBoxLayout()
        rc.setSpacing(8)
        self._rows = QSpinBox()
        self._rows.setRange(1, 10)
        self._rows.setValue(int(wall_cfg.get("rows", 4)))
        self._cols = QSpinBox()
        self._cols.setRange(1, 10)
        self._cols.setValue(int(wall_cfg.get("cols", 8)))
        self._rows.setAccessibleName("Default grid rows")
        self._cols.setAccessibleName("Default grid columns")
        rc.addWidget(self._rows)
        rc.addWidget(QLabel("×"))
        rc.addWidget(self._cols)
        rc.addStretch(1)
        wall_form.addRow("Default Grid:", rc)
        self._pages["wall"].addWidget(wall)

        # Live preview frame rates: what the desktop wall paints, per case.
        preview = PreviewFps.from_config(self._config.get("ui", {}))
        preview_box, preview_form = self._card(
            "Live preview frame rate",
            "The most frames per second the desktop shows, live and in Playback, by how"
            " many cameras are on screen.",
        )

        def fps_box(value: int, top: int, name: str, tip: str) -> QSpinBox:
            box = QSpinBox()
            box.setRange(1, top)
            box.setValue(value)
            box.setSuffix(" fps")
            box.setAccessibleName(name)
            box.setToolTip(tip)
            return box

        self._fps_single = fps_box(
            preview.single,
            MAX_VISIBLE_FPS,
            "Preview frame rate, one camera",
            "Single view: one camera fills the wall",
        )
        self._fps_wall = fps_box(
            preview.wall,
            MAX_VISIBLE_FPS,
            f"Preview frame rate, up to {DENSE_WALL_TILES} tiles",
            f"Each tile of a page showing up to {DENSE_WALL_TILES} cameras",
        )
        self._fps_dense = fps_box(
            preview.dense,
            MAX_VISIBLE_FPS,
            f"Preview frame rate, more than {DENSE_WALL_TILES} tiles",
            f"Each tile of a page showing more than {DENSE_WALL_TILES} cameras",
        )
        self._fps_hidden = fps_box(
            preview.hidden,
            MAX_HIDDEN_FPS,
            "Preview frame rate, hidden pages",
            "Cameras on pages not shown keep a slow preview, so their page"
            " opens on a recent picture",
        )
        preview_form.addRow("One camera:", self._fps_single)
        preview_form.addRow(f"Up to {DENSE_WALL_TILES} tiles:", self._fps_wall)
        preview_form.addRow(f"More than {DENSE_WALL_TILES}:", self._fps_dense)
        preview_form.addRow("Hidden pages:", self._fps_hidden)
        preview_hint = QLabel(
            "Upper limits for the desktop preview; a camera never shows more than it"
            " sends. Recording and web viewing always get every frame. Higher values"
            " use more CPU."
        )
        preview_hint.setWordWrap(True)
        set_label_role(preview_hint, "hint")
        preview_form.addRow(preview_hint)
        self._pages["wall"].addWidget(preview_box)
        self._pages["wall"].addWidget(self._build_decoder())

        # ------------------------------------------------------------------
        # Recording
        # ------------------------------------------------------------------
        rec_cfg = self._config.get("recording", {})
        recording, rec_form = self._card(
            "Storage", "Where recordings are saved, and when the oldest are deleted."
        )

        storage_row = QWidget()
        storage_hl = QHBoxLayout(storage_row)
        storage_hl.setContentsMargins(0, 0, 0, 0)
        storage_hl.setSpacing(6)
        self._rec_storage = QLineEdit(rec_cfg.get("storage_path", "~/Videos/neurowatch"))
        storage_hl.addWidget(self._rec_storage)
        self._browse_btn = QPushButton("Browse…")
        self._browse_btn.setAccessibleName("Browse recording storage path")
        set_button_variant(self._browse_btn, "secondary")
        self._browse_btn.clicked.connect(self._browse_storage)
        storage_hl.addWidget(self._browse_btn)
        rec_form.addRow("Storage Path:", storage_row)

        drive_row = QWidget()
        drive_hl = QHBoxLayout(drive_row)
        drive_hl.setContentsMargins(0, 0, 0, 0)
        drive_hl.setSpacing(6)
        self._drive_label = QLabel("Looking for drives…")
        self._drive_label.setWordWrap(True)
        set_label_role(self._drive_label, "hint")
        drive_hl.addWidget(self._drive_label, stretch=1)
        self._choose_drive_btn = QPushButton("Choose Drive…")
        self._choose_drive_btn.setAccessibleName("Choose the recording drive")
        self._choose_drive_btn.setEnabled(False)
        set_button_variant(self._choose_drive_btn, "secondary")
        self._choose_drive_btn.clicked.connect(self._choose_drive)
        drive_hl.addWidget(self._choose_drive_btn)
        rec_form.addRow("Drive:", drive_row)
        rec_form.setRowVisible(drive_row, self._endpoint is not None)
        self._rec_storage.textChanged.connect(self._update_drive_label)

        self._rec_segment = QSpinBox()
        self._rec_segment.setRange(1, 60)
        self._rec_segment.setSuffix(" min")
        self._rec_segment.setValue(int(rec_cfg.get("segment_minutes", 1)))
        rec_form.addRow("Segment Length:", self._rec_segment)

        self._rec_prebuf = QSpinBox()
        self._rec_prebuf.setRange(5, 60)
        self._rec_prebuf.setSuffix(" s")
        self._rec_prebuf.setValue(int(rec_cfg.get("pre_buffer_sec", 10)))
        rec_form.addRow("Pre-buffer:", self._rec_prebuf)

        self._rec_local_all = QCheckBox("Enable local recording for all cameras")
        self._rec_local_all.setChecked(bool(rec_cfg.get("local_all_cameras", False)))
        rec_form.addRow("", self._rec_local_all)

        cleanup_row = QWidget()
        cleanup_hl = QHBoxLayout(cleanup_row)
        cleanup_hl.setContentsMargins(0, 0, 0, 0)
        cleanup_hl.setSpacing(6)
        self._rec_cleanup = QCheckBox("Delete oldest recordings above")
        self._rec_cleanup.setToolTip(
            "When the recording drive gets fuller than this, the oldest NeuroWatch "
            "recordings are deleted to make room. Other files are never deleted."
        )
        cleanup_hl.addWidget(self._rec_cleanup)
        self._rec_threshold = QSpinBox()
        self._rec_threshold.setRange(10, 99)
        self._rec_threshold.setSuffix(" % full")
        self._rec_threshold.setAccessibleName("Disk threshold for deleting old recordings")
        cleanup_hl.addWidget(self._rec_threshold)
        cleanup_hl.addStretch(1)
        try:
            threshold = int(rec_cfg.get("disk_threshold_percent", DEFAULT_DISK_THRESHOLD_PERCENT))
        except (TypeError, ValueError):
            threshold = DEFAULT_DISK_THRESHOLD_PERCENT
        self._rec_cleanup.setChecked(threshold > 0)
        self._rec_threshold.setValue(threshold if threshold > 0 else DEFAULT_DISK_THRESHOLD_PERCENT)
        self._rec_threshold.setEnabled(self._rec_cleanup.isChecked())
        self._rec_cleanup.toggled.connect(self._rec_threshold.setEnabled)
        rec_form.addRow("Disk Cleanup:", cleanup_row)
        self._threshold_hint = QLabel()
        self._threshold_hint.setWordWrap(True)
        set_label_role(self._threshold_hint, "hint")
        rec_form.addRow("", self._threshold_hint)
        self._rec_cleanup.toggled.connect(self._update_threshold_hint)
        self._rec_threshold.valueChanged.connect(self._update_threshold_hint)
        self._rec_storage.textChanged.connect(self._update_threshold_hint)
        self._update_threshold_hint()

        self._pages["recording"].addWidget(recording)
        self._pages["archive"].addWidget(self._build_archive(rec_cfg.get("archive")))

        # ------------------------------------------------------------------
        # Storage Calculator
        # ------------------------------------------------------------------
        calc, calc_form = self._card(
            "Storage calculator", "Estimate how much space your recordings need."
        )

        self._calc_cams = QSpinBox()
        self._calc_cams.setRange(1, 256)
        self._calc_cams.setValue(4)
        self._calc_cams.valueChanged.connect(self._update_calc)
        calc_form.addRow("Cameras:", self._calc_cams)

        self._calc_bitrate = QSpinBox()
        self._calc_bitrate.setRange(1, 100)
        self._calc_bitrate.setValue(2)
        self._calc_bitrate.setSuffix(" Mbps")
        self._calc_bitrate.valueChanged.connect(self._update_calc)
        calc_form.addRow("Avg Bitrate:", self._calc_bitrate)

        self._calc_days = QSpinBox()
        self._calc_days.setRange(1, 365)
        self._calc_days.setValue(7)
        self._calc_days.setSuffix(" days")
        self._calc_days.valueChanged.connect(self._update_calc)
        calc_form.addRow("Retention:", self._calc_days)

        self._calc_result = QLabel()
        self._calc_result.setAlignment(Qt.AlignmentFlag.AlignRight)
        set_label_role(self._calc_result, "emphasis")
        calc_form.addRow("Estimated:", self._calc_result)
        self._update_calc()
        self._pages["recording"].addWidget(calc)

        # ------------------------------------------------------------------
        # Debug
        # ------------------------------------------------------------------
        debug, debug_form = self._card("Logging", "How much the recorder's media engine logs.")
        self._gst_log = QComboBox()
        self._gst_log.addItems(["error", "warning", "info", "debug"])
        gst_level = self._config.get("debug", {}).get("gst_log", "warning")
        if gst_level == "warn":
            gst_level = "warning"
        idx = self._gst_log.findText(gst_level)
        if idx >= 0:
            self._gst_log.setCurrentIndex(idx)
        debug_form.addRow("GStreamer Log Level:", self._gst_log)
        self._pages["debug"].addWidget(debug)

        self._build_startup(startup or StartupPrefs(), startup_note)
        self._build_cloud()
        for page_layout in self._pages.values():
            page_layout.addStretch(1)
        # Fields keep their own width; only text and folder fields stretch.
        for form in self.findChildren(QFormLayout):
            form.setFieldGrowthPolicy(QFormLayout.FieldGrowthPolicy.ExpandingFieldsGrow)
        for label in self.findChildren(QLabel):
            if label.wordWrap():  # notes use the card's width, not their own
                policy = label.sizePolicy()
                policy.setHorizontalPolicy(QSizePolicy.Policy.Expanding)
                label.setSizePolicy(policy)
        self._nav.currentRowChanged.connect(self._on_tab_changed)
        self._nav.setCurrentRow(list(TABS).index(tab) if tab in TABS else 0)

        btns = QDialogButtonBox(
            QDialogButtonBox.StandardButton.Ok | QDialogButtonBox.StandardButton.Cancel
        )
        btns.button(QDialogButtonBox.StandardButton.Ok).setText("Save")
        btns.accepted.connect(self.accept)
        btns.rejected.connect(self.reject)
        style_button_box(btns)
        layout.addWidget(btns)
        resize_window(self, self._opening_size.expandedTo(self.minimumSizeHint()))
        self._scan_drives()
        self._read_archive_status()
        self._read_decoder_status()

    @staticmethod
    def _card(title: str, subtitle: str = "") -> tuple[QFrame, QFormLayout]:
        """A titled card with a form for its rows."""
        card, body, _ = settings_card(title, subtitle)
        form = QFormLayout()
        form.setContentsMargins(0, 4, 0, 0)
        form.setHorizontalSpacing(16)
        form.setVerticalSpacing(10)
        body.addLayout(form)
        return card, form

    # ------------------------------------------------------------------
    # Video decoding
    # ------------------------------------------------------------------

    def _build_decoder(self) -> QFrame:
        """Which decoder shows live video: automatic, a graphics card's
        decoder, or the CPU. The recorder applies it when it starts, so saving
        a change restarts it."""
        box, form = self._card(
            "Video decoding",
            "What decodes the cameras' video for live view and playback.",
        )
        self._decoder_initial = decoder_from_config(self._config)
        self._decoder = QComboBox()
        self._decoder.setAccessibleName("Video decoder")
        self._decoder.setToolTip(
            "Automatic uses the graphics card when it can. Pick a decoder to use only"
            " that one, or CPU only to never use the graphics card."
        )
        self._fill_decoders(DecoderStatus())
        form.addRow("Decoder:", self._decoder)
        self._decoder_now = QLabel("Checking which decoder is in use…")
        self._decoder_now.setWordWrap(True)
        set_label_role(self._decoder_now, "hint")
        form.addRow("", self._decoder_now)
        self._decoder_now.setVisible(self._endpoint is not None)
        self._decoder_warning = QLabel("")
        self._decoder_warning.setWordWrap(True)
        set_status_tone(self._decoder_warning, "warning")
        form.addRow("", self._decoder_warning)
        self._decoder_warning.setVisible(False)
        note = QLabel(
            "NeuroWatch restarts its background service to apply this (recording pauses"
            " for a few seconds)."
        )
        note.setWordWrap(True)
        set_label_role(note, "hint")
        form.addRow("", note)
        return box

    def _fill_decoders(self, status: DecoderStatus) -> None:
        """List Automatic, the decoders the recorder found and CPU only,
        keeping the selection."""
        current = self._decoder.currentData() if self._decoder.count() else None
        if not isinstance(current, str):
            current = self._decoder_initial
        self._decoder.blockSignals(True)
        self._decoder.clear()
        for value, text in decoder_choices(self._decoder_initial, status.available):
            self._decoder.addItem(text, value)
        index = self._decoder.findData(current)
        self._decoder.setCurrentIndex(max(0, index))
        self._decoder.blockSignals(False)

    def _read_decoder_status(self) -> None:
        if self._endpoint is None:
            return
        worker = _DecoderStatusWorker(self._endpoint)
        worker.done.connect(self._on_decoder_status)
        worker.finished.connect(worker.deleteLater)
        self._decoder_worker = worker
        worker.start()

    def _on_decoder_status(self, result: object) -> None:
        self._decoder_worker = None
        if not isinstance(result, DecoderStatus):
            self._decoder_now.setText("The decoder in use could not be read.")
            return
        self._fill_decoders(result)
        self._decoder_now.setText(f"Now decoding with: {result.active}" if result.active else "")
        self._decoder_now.setVisible(bool(result.active))
        self._decoder_warning.setText(result.warning)
        self._decoder_warning.setVisible(bool(result.warning))

    def video_decoder(self) -> str:
        """The decoder choice as edited (``video.decoder``)."""
        value = self._decoder.currentData()
        return value if isinstance(value, str) else self._decoder_initial

    def decoder_changed(self) -> bool:
        """Whether saving changes the decoder (and so restarts the recorder)."""
        return self.video_decoder() != self._decoder_initial

    # ------------------------------------------------------------------
    # Startup
    # ------------------------------------------------------------------

    def _build_startup(self, prefs: StartupPrefs, note: str | None) -> None:
        page = self._pages["startup"]
        login, login_layout, _ = settings_card("When you sign in")
        self._at_login = QCheckBox("Start NeuroWatch when I sign in")
        self._at_login.setToolTip(
            "Start the recorder in the background when you sign in to this computer"
        )
        self._at_login.setChecked(prefs.at_login)
        login_layout.addWidget(self._at_login)
        self._open_tray = QRadioButton("Stay in the background (tray icon only)")
        self._open_window = QRadioButton("Open the NeuroWatch window")
        self._open_window.setChecked(prefs.open_window)
        self._open_tray.setChecked(not prefs.open_window)
        choices = QWidget()
        choices_layout = QVBoxLayout(choices)
        choices_layout.setContentsMargins(24, 0, 0, 0)
        choices_layout.addWidget(self._open_tray)
        choices_layout.addWidget(self._open_window)
        login_layout.addWidget(choices)
        self._at_login.toggled.connect(choices.setEnabled)
        choices.setEnabled(prefs.at_login)
        page.addWidget(login)

        boot, boot_layout, _ = settings_card("Before anyone signs in")
        self._at_boot = QCheckBox("Record from boot")
        self._at_boot.setChecked(prefs.at_boot and prefs.at_login)
        self._at_boot.setEnabled(prefs.at_login)
        self._at_login.toggled.connect(self._at_boot.setEnabled)
        boot_layout.addWidget(self._at_boot)
        boot_hint = QLabel(
            "Cameras record as soon as the computer starts, even before you sign in."
            " Linux only; it may ask for an administrator's permission once."
        )
        boot_hint.setWordWrap(True)
        set_label_role(boot_hint, "hint")
        boot_layout.addWidget(boot_hint)
        boot.setVisible(sys.platform.startswith("linux"))
        page.addWidget(boot)

        window, window_layout, _ = settings_card("Window")
        self._start_fullscreen = QCheckBox("Open the window in fullscreen")
        self._start_fullscreen.setChecked(bool(self._config.get("ui", {}).get("fullscreen", True)))
        window_layout.addWidget(self._start_fullscreen)
        page.addWidget(window)

        self._startup_managed = note is None
        if note is not None:
            for box in (login, boot):
                box.setEnabled(False)
            why = QLabel(note)
            why.setWordWrap(True)
            set_label_role(why, "hint")
            page.insertWidget(1, why)  # under the page's title

    def startup_prefs(self) -> StartupPrefs | None:
        """The startup choice as edited (save it with the service manager);
        None when it cannot be changed here (``startup_note``)."""
        if not self._startup_managed:
            return None
        at_login = self._at_login.isChecked()
        return StartupPrefs(
            at_login=at_login,
            open_window=self._open_window.isChecked(),
            at_boot=at_login and self._at_boot.isChecked(),
        )

    # ------------------------------------------------------------------
    # Cloud
    # ------------------------------------------------------------------

    def _build_cloud(self) -> None:
        """The link panel is made when the tab is first shown (it asks the
        recorder for ``cloud.status``); without a recorder, a hint."""
        self._cloud_panel: QWidget | None = None
        if self._endpoint is None:
            hint = QLabel("Connect to the recorder to link it to NeuroWatch Cloud.")
            hint.setWordWrap(True)
            set_label_role(hint, "hint")
            self._pages["cloud"].addWidget(hint)

    def current_page(self) -> str:
        """The key (``TABS``) of the page shown."""
        return list(TABS)[self._stack.currentIndex()]

    def _on_tab_changed(self, index: int) -> None:
        if index < 0:
            return
        self._stack.setCurrentIndex(index)
        if list(TABS)[index] != "cloud" or self._cloud_panel is not None:
            return
        if self._endpoint is None:
            return
        from neurowatch.cloud.link_panel import CloudLinkPanel

        self._cloud_panel = CloudLinkPanel(endpoint=self._endpoint)
        self._pages["cloud"].insertWidget(1, self._cloud_panel)  # under the page's title

    # ------------------------------------------------------------------
    # Archive
    # ------------------------------------------------------------------

    def _build_archive(self, raw: object) -> QFrame:
        """Older recordings move to another folder (usually a network share),
        where playback and export still find them."""
        settings = ArchiveSettings.from_json(raw)
        box, form = self._card(
            "Archive older recordings",
            "Move recordings to another folder, usually a network share, where Playback"
            " and Export still find them.",
        )
        self._archive_on = QCheckBox("Move older recordings to an archive folder")
        self._archive_on.setToolTip(
            "Each recording is copied, checked against the original and only then "
            "removed from the recording drive. Playback and export find it in either place."
        )
        self._archive_on.setChecked(settings.enabled)
        form.addRow(self._archive_on)

        fields = QWidget()
        fields_form = QFormLayout(fields)
        fields_form.setContentsMargins(0, 0, 0, 0)
        folder_row = QWidget()
        folder_hl = QHBoxLayout(folder_row)
        folder_hl.setContentsMargins(0, 0, 0, 0)
        folder_hl.setSpacing(6)
        self._archive_path = QLineEdit(settings.path)
        self._archive_path.setPlaceholderText("/mnt/nas/neurowatch-archive")
        self._archive_path.setAccessibleName("Archive folder")
        folder_hl.addWidget(self._archive_path)
        browse = QPushButton("Browse…")
        browse.setAccessibleName("Browse for the archive folder")
        set_button_variant(browse, "secondary")
        browse.clicked.connect(self._browse_archive)
        folder_hl.addWidget(browse)
        fields_form.addRow("Folder:", folder_row)

        self._archive_after = QSpinBox()
        self._archive_after.setRange(1, MAX_AFTER_DAYS)
        self._archive_after.setSuffix(" days")
        self._archive_after.setAccessibleName("Move recordings older than")
        self._archive_after.setValue(settings.after_days)
        fields_form.addRow("Move after:", self._archive_after)

        def optional_row(
            label: str, spin: QSpinBox | None, *, value: int | None
        ) -> tuple[QWidget, QCheckBox, QHBoxLayout]:
            row = QWidget()
            hl = QHBoxLayout(row)
            hl.setContentsMargins(0, 0, 0, 0)
            hl.setSpacing(6)
            check = QCheckBox(label)
            hl.addWidget(check)
            if spin is not None:
                check.setChecked(value is not None)
                if value is not None:
                    spin.setValue(value)
                spin.setEnabled(check.isChecked())
                check.toggled.connect(spin.setEnabled)
                hl.addWidget(spin)
            hl.addStretch(1)
            return row, check, hl

        self._archive_quota = QSpinBox()
        self._archive_quota.setRange(1, 1_000_000)
        self._archive_quota.setValue(1000)
        self._archive_quota.setSuffix(" GB")
        self._archive_quota.setAccessibleName("Archive size limit")
        quota_row, self._archive_quota_on, _ = optional_row(
            "Delete oldest above", self._archive_quota, value=settings.quota_gb
        )
        self._archive_quota_on.setToolTip(
            "Archived recordings without events go first; footage a kept bookmark"
            " covers is never deleted"
        )
        fields_form.addRow("Size limit:", quota_row)

        hours_row, self._archive_hours_on, hours_hl = optional_row("Only from", None, value=None)
        start, end = settings.hours or ("22:00", "06:00")
        self._archive_start = QTimeEdit(QTime.fromString(start, "HH:mm"))
        self._archive_end = QTimeEdit(QTime.fromString(end, "HH:mm"))
        for edit, name in ((self._archive_start, "start"), (self._archive_end, "end")):
            edit.setDisplayFormat("HH:mm")
            edit.setAccessibleName(f"Archive hours {name}")
            edit.setEnabled(settings.hours is not None)
            self._archive_hours_on.toggled.connect(edit.setEnabled)
        self._archive_hours_on.setChecked(settings.hours is not None)
        hours_hl.insertWidget(hours_hl.count() - 1, self._archive_start)
        hours_hl.insertWidget(hours_hl.count() - 1, QLabel("to"))
        hours_hl.insertWidget(hours_hl.count() - 1, self._archive_end)
        fields_form.addRow("Hours:", hours_row)

        self._archive_speed = QSpinBox()
        self._archive_speed.setRange(1, 100_000)
        self._archive_speed.setValue(100)
        self._archive_speed.setSuffix(" Mbit/s")
        self._archive_speed.setAccessibleName("Archive copy speed limit")
        speed_row, self._archive_speed_on, _ = optional_row(
            "Limit to", self._archive_speed, value=settings.max_mbps
        )
        fields_form.addRow("Speed:", speed_row)
        fields.setEnabled(self._archive_on.isChecked())
        self._archive_on.toggled.connect(fields.setEnabled)
        form.addRow(fields)

        self._archive_status = QLabel("")
        self._archive_status.setWordWrap(True)
        set_label_role(self._archive_status, "hint")
        form.addRow(self._archive_status)
        self._archive_status.setVisible(False)
        self._missing_locations = QLabel("")
        self._missing_locations.setWordWrap(True)
        set_status_tone(self._missing_locations, "error")
        form.addRow(self._missing_locations)
        self._missing_locations.setVisible(False)
        # One "Record Here Anyway…" per missing location, for a plain folder
        # whose marker was deleted (it looks like an unmounted drive).
        self._missing_actions = QWidget()
        self._missing_actions_layout = QVBoxLayout(self._missing_actions)
        self._missing_actions_layout.setContentsMargins(0, 0, 0, 0)
        form.addRow(self._missing_actions)
        self._missing_actions.setVisible(False)
        return box

    def _browse_archive(self) -> None:
        path = QFileDialog.getExistingDirectory(self, "Select Archive Folder")
        if path:
            self._archive_path.setText(path)

    def archive_settings(self) -> ArchiveSettings:
        hours = None
        if self._archive_hours_on.isChecked():
            hours = (
                self._archive_start.time().toString("HH:mm"),
                self._archive_end.time().toString("HH:mm"),
            )
        return ArchiveSettings(
            enabled=self._archive_on.isChecked(),
            path=self._archive_path.text().strip(),
            after_days=int(self._archive_after.value()),
            quota_gb=int(self._archive_quota.value())
            if self._archive_quota_on.isChecked()
            else None,
            hours=hours,
            max_mbps=int(self._archive_speed.value())
            if self._archive_speed_on.isChecked()
            else None,
        )

    def _read_archive_status(self) -> None:
        if self._endpoint is None:
            return
        worker = _ArchiveStatusWorker(self._endpoint)
        worker.done.connect(self._on_archive_status)
        worker.finished.connect(worker.deleteLater)
        self._archive_worker = worker
        worker.start()

    def _on_archive_status(self, result: object) -> None:
        self._archive_worker = None
        if not isinstance(result, ArchiveStatus):
            # An older recorder without the archive: say nothing.
            return
        self._archive_status.setText(result.summary())
        self._archive_status.setVisible(True)
        missing = result.missing_text()
        self._missing_locations.setText(f"Not available now:\n{missing}" if missing else "")
        self._missing_locations.setVisible(bool(missing))
        self._show_missing_actions(result.missing_locations)

    def _show_missing_actions(self, locations: tuple[MissingLocation, ...]) -> None:
        layout = self._missing_actions_layout
        while layout.count():
            item = layout.takeAt(0)
            widget = item.widget() if item is not None else None
            if widget is not None:
                widget.setParent(None)
                widget.deleteLater()
        for location in locations:
            button = QPushButton(f"Record Here Anyway: {location.path}…")
            button.setToolTip(
                "Use this folder although its NeuroWatch marker file is missing,"
                " for example a folder on this computer"
            )
            set_button_variant(button, "secondary")
            button.clicked.connect(lambda _=False, path=location.path: self._use_location(path))
            layout.addWidget(button)
        self._missing_actions.setVisible(bool(locations) and self._endpoint is not None)

    def _use_location(self, path: str) -> None:
        if self._endpoint is None or self._use_worker is not None:
            return
        if not ask(
            self,
            "Record Here Anyway?",
            f"NeuroWatch stopped recording to {path} because its marker file"
            " (.neurowatch-storage) is missing. That is what an unmounted drive or"
            " network share looks like: its empty folder is left on this computer's"
            " disk.\n\nContinue only if this is the right folder, for example a folder"
            " on this computer whose marker was deleted. If it is a drive or network"
            " share, connect it instead.",
            yes_text="Record Here",
            no_text="Cancel",
        ):
            return
        worker = _UseLocationWorker(self._endpoint, path)
        worker.done.connect(self._on_location_used)
        worker.finished.connect(worker.deleteLater)
        self._use_worker = worker
        worker.start()

    def _on_location_used(self, result: object) -> None:
        self._use_worker = None
        if isinstance(result, Exception):
            show_message(self, "Cannot Record There", str(result), kind="error")
        self._read_archive_status()

    # ------------------------------------------------------------------
    # Recording drive
    # ------------------------------------------------------------------

    def _scan_drives(self) -> None:
        if self._endpoint is None:
            return
        worker = DriveScanWorker(self._endpoint)
        worker.done.connect(self._on_drives)
        worker.finished.connect(worker.deleteLater)
        self._scan_worker = worker
        worker.start()

    def _on_drives(self, result: object) -> None:
        self._scan_worker = None
        if isinstance(result, StorageScan):
            self._scan = result
            self._choose_drive_btn.setEnabled(bool(result.drives))
            self._update_drive_label()
            return
        reason = result.message if isinstance(result, IpcError) else str(result)
        self._drive_label.setText(f"Drives could not be listed: {reason}")

    def _update_drive_label(self) -> None:
        if self._scan is None:
            return
        drive = self._scan.drive_of(self._rec_storage.text().strip())
        if drive is None:
            self._drive_label.setText("Not on a listed drive")
        else:
            self._drive_label.setText(f"{drive.mount_point} · {describe_space(drive)}")
        self._update_threshold_hint()

    def _update_threshold_hint(self) -> None:
        if not self._rec_cleanup.isChecked():
            self._threshold_hint.setText("Old recordings are kept until the drive is full.")
            return
        percent = int(self._rec_threshold.value())
        drive = None
        if self._scan is not None:
            drive = self._scan.drive_of(self._rec_storage.text().strip())
        if drive is None or drive.total_bytes <= 0:
            self._threshold_hint.setText(
                f"The oldest recordings are deleted once the drive is {percent}% full."
            )
            return
        limit = drive.total_bytes * percent // 100
        self._threshold_hint.setText(
            f"The oldest recordings are deleted once {format_bytes(limit)} of "
            f"{format_bytes(drive.total_bytes)} is used on {drive.mount_point}."
        )

    def _choose_drive(self) -> None:
        if self._scan is None:
            return
        picker = DrivePickerDialog(self._scan, self._rec_storage.text().strip(), self)
        if picker.exec() != QDialog.DialogCode.Accepted:
            return
        drive = picker.chosen_drive()
        if drive is not None:
            self._chosen_drive = drive
            self._rec_storage.setText(self._scan.folder_on(drive))

    def selected_drive(self) -> str | None:
        """Mount point of the drive picked for recordings, while the storage
        path is still that drive's folder; the backend prepares it on save."""
        drive = self._chosen_drive
        if drive is None or self._scan is None:
            return None
        if self._rec_storage.text().strip() != self._scan.folder_on(drive):
            return None
        return drive.mount_point

    def _stop_scan(self) -> None:
        workers: tuple[
            tuple[
                DriveScanWorker
                | _ArchiveStatusWorker
                | _UseLocationWorker
                | _DecoderStatusWorker
                | None,
                object,
            ],
            ...,
        ] = (
            (self._scan_worker, self._on_drives),
            (self._archive_worker, self._on_archive_status),
            (self._use_worker, self._on_location_used),
            (self._decoder_worker, self._on_decoder_status),
        )
        for worker, slot in workers:
            if worker is None:
                continue
            try:
                worker.done.disconnect(slot)
            except (RuntimeError, TypeError):
                pass
            if worker.isRunning():
                worker.wait(2000)
            retire(worker)  # still running: kept until it finishes, never destroyed running
        self._scan_worker = None
        self._archive_worker = None
        self._use_worker = None
        self._decoder_worker = None

    def done(self, result: int) -> None:
        self._stop_scan()
        super().done(result)

    def _browse_storage(self) -> None:
        path = QFileDialog.getExistingDirectory(self, "Select Recording Storage Directory")
        if path:
            self._rec_storage.setText(path)

    def _update_calc(self) -> None:
        cams = self._calc_cams.value()
        bitrate = self._calc_bitrate.value()
        days = self._calc_days.value()
        # GiB = cameras x bitrate_mbps x 0.125 MB/s x seconds/day x days / 1024
        total_gb = cams * bitrate * 0.125 * 86400 * days / 1024
        if total_gb >= 1024:
            self._calc_result.setText(f"≈ {total_gb / 1024:.1f} TB")
        else:
            self._calc_result.setText(f"≈ {total_gb:.1f} GB")

    def get_config(self) -> dict:
        cfg = deepcopy(self._config)
        cfg["site_name"] = self._site_name.text().strip() or "My Site"
        cfg.setdefault("ui", {})
        cfg["ui"]["theme"] = self._theme.currentText().lower()
        cfg["ui"]["fullscreen"] = self._start_fullscreen.isChecked()
        cfg["ui"]["fullscreen_status"] = self._fullscreen_status.isChecked()
        cfg["ui"]["fullscreen_status_opacity"] = int(self._status_opacity.value())
        cfg["ui"]["preview_fps"] = PreviewFps(
            single=int(self._fps_single.value()),
            wall=int(self._fps_wall.value()),
            dense=int(self._fps_dense.value()),
            hidden=int(self._fps_hidden.value()),
        ).to_json()
        cfg.setdefault("wall", {})
        cfg["wall"]["page_interval_sec"] = int(self._interval.value())
        cfg["wall"]["rows"] = int(self._rows.value())
        cfg["wall"]["cols"] = int(self._cols.value())
        cfg.setdefault("recording", {})
        cfg["recording"]["storage_path"] = self._rec_storage.text().strip() or "~/Videos/neurowatch"
        cfg["recording"]["segment_minutes"] = int(self._rec_segment.value())
        cfg["recording"]["pre_buffer_sec"] = int(self._rec_prebuf.value())
        cfg["recording"]["local_all_cameras"] = self._rec_local_all.isChecked()
        cfg["recording"]["disk_threshold_percent"] = (
            int(self._rec_threshold.value()) if self._rec_cleanup.isChecked() else 0
        )
        cfg["recording"]["archive"] = self.archive_settings().to_json()
        cfg.setdefault("debug", {})
        cfg["debug"]["gst_log"] = self._gst_log.currentText()
        cfg.setdefault("video", {})
        cfg["video"]["decoder"] = self.video_decoder()
        return cfg
