from __future__ import annotations

from copy import deepcopy

from PySide6.QtCore import Qt
from PySide6.QtWidgets import (
    QCheckBox,
    QComboBox,
    QDialog,
    QDialogButtonBox,
    QFileDialog,
    QFormLayout,
    QGroupBox,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QPushButton,
    QSlider,
    QSpinBox,
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
from neurowatch.ipc.sync_client import IpcError
from neurowatch.theme import (
    apply_dialog_theme,
    set_button_variant,
    set_label_role,
    style_button_box,
)
from neurowatch.widgets.window_chrome import install_dialog_chrome

#: Disk threshold shown when it is off or not set (the backend's default).
DEFAULT_DISK_THRESHOLD_PERCENT = 90


class SettingsDialog(QDialog):
    """Site, wall, recording and debug settings.

    With ``ipc_endpoint`` the recording drive can be picked from the
    backend's drive scan (``storage.list``); the choice is applied on save
    (``selected_drive``).
    """

    def __init__(
        self,
        config: dict,
        parent: QWidget | None = None,
        *,
        ipc_endpoint: str | None = None,
    ) -> None:
        super().__init__(parent)
        self.setWindowTitle("Settings")
        self.setMinimumWidth(460)
        self._config = deepcopy(config)
        self._endpoint = ipc_endpoint or None
        self._scan: StorageScan | None = None
        self._scan_worker: DriveScanWorker | None = None
        self._chosen_drive: Drive | None = None
        apply_dialog_theme(self)

        layout = install_dialog_chrome(self, "Settings")

        general = QGroupBox("General")
        general_form = QFormLayout(general)
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

        self._start_fullscreen = QCheckBox("Start in fullscreen")
        self._start_fullscreen.setChecked(bool(self._config.get("ui", {}).get("fullscreen", True)))
        general_form.addRow("", self._start_fullscreen)
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
        layout.addWidget(general)

        wall_cfg = self._config.get("wall", {})
        wall = QGroupBox("Video Wall")
        wall_form = QFormLayout(wall)
        self._interval = QSpinBox()
        self._interval.setRange(0, 300)
        self._interval.setValue(int(wall_cfg.get("page_interval_sec", 0)))
        wall_form.addRow("Auto-scroll (sec):", self._interval)
        hint = QLabel("0 = disabled")
        set_label_role(hint, "hint")
        wall_form.addRow("", hint)

        rc = QHBoxLayout()
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
        wall_form.addRow("Default Grid:", rc)
        layout.addWidget(wall)

        # ------------------------------------------------------------------
        # Recording
        # ------------------------------------------------------------------
        rec_cfg = self._config.get("recording", {})
        recording = QGroupBox("Recording")
        rec_form = QFormLayout(recording)

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
        drive_row.setVisible(self._endpoint is not None)
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

        layout.addWidget(recording)

        # ------------------------------------------------------------------
        # Storage Calculator
        # ------------------------------------------------------------------
        calc = QGroupBox("Storage Calculator")
        calc_form = QFormLayout(calc)

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
        layout.addWidget(calc)

        # ------------------------------------------------------------------
        # Debug
        # ------------------------------------------------------------------
        debug = QGroupBox("Debug")
        debug_form = QFormLayout(debug)
        self._gst_log = QComboBox()
        self._gst_log.addItems(["error", "warning", "info", "debug"])
        gst_level = self._config.get("debug", {}).get("gst_log", "warning")
        if gst_level == "warn":
            gst_level = "warning"
        idx = self._gst_log.findText(gst_level)
        if idx >= 0:
            self._gst_log.setCurrentIndex(idx)
        debug_form.addRow("GStreamer Log Level:", self._gst_log)
        layout.addWidget(debug)

        btns = QDialogButtonBox(
            QDialogButtonBox.StandardButton.Ok | QDialogButtonBox.StandardButton.Cancel
        )
        btns.button(QDialogButtonBox.StandardButton.Ok).setText("Save")
        btns.accepted.connect(self.accept)
        btns.rejected.connect(self.reject)
        style_button_box(btns)
        layout.addWidget(btns)
        self._scan_drives()

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
        worker = self._scan_worker
        if worker is None:
            return
        try:
            worker.done.disconnect(self._on_drives)
        except (RuntimeError, TypeError):
            pass
        if worker.isRunning():
            worker.wait(2000)
        self._scan_worker = None

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
        cfg.setdefault("debug", {})
        cfg["debug"]["gst_log"] = self._gst_log.currentText()
        return cfg
