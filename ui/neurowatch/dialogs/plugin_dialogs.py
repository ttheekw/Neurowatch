"""Plugins: what the Plugins page and its dialogs share.

``Job`` runs recorder calls off the UI thread; ``AddPluginDialog`` adds a
plugin by name and address; ``BindingEditor`` is a camera's settings for a
plugin (which camera, which video, what shows on the live wall, and the
plugin's own settings drawn from its schema, with zones on the camera's
picture); ``BindingDialog`` shows the editor to bind a new camera.
"""

from __future__ import annotations

import logging
from collections.abc import Callable
from dataclasses import dataclass
from typing import Any

from PySide6.QtCore import QSize, Qt, Signal
from PySide6.QtGui import QGuiApplication, QShowEvent
from PySide6.QtWidgets import (
    QCheckBox,
    QComboBox,
    QDialog,
    QDialogButtonBox,
    QDoubleSpinBox,
    QFormLayout,
    QFrame,
    QGridLayout,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QPushButton,
    QScrollArea,
    QSpinBox,
    QVBoxLayout,
    QWidget,
)

from neurowatch.ipc.sync_client import SyncIpcClient
from neurowatch.models.plugins import (
    MOTION_PLUGIN_ID,
    FormField,
    Manifest,
    PluginBinding,
    PluginsConfig,
    PluginServer,
    ServerStatus,
    address_problem,
    form_fields,
    motion_config_from_camera,
    parse_status,
    server_id_problem,
    uses_builtin_motion,
)
from neurowatch.models.shapes import DRAWN_KINDS
from neurowatch.theme import (
    apply_dialog_theme,
    set_label_role,
    set_status_tone,
    style_button_box,
)
from neurowatch.utils.threads import Worker
from neurowatch.widgets.form_parts import Segmented, settings_card
from neurowatch.widgets.shape_editor import ShapeEditor
from neurowatch.widgets.window_chrome import install_dialog_chrome, resize_window
from neurowatch.widgets.zone_editor import ZoneEditor

logger = logging.getLogger(__name__)

#: ``rpc(method, params)`` → result; raises on failure.
PluginsRpc = Callable[[str, dict[str, Any]], Any]

#: The Video choices: (stored value, shown text).
STREAM_TEXT = (
    (None, "The plugin's choice"),
    ("sub", "Sub stream (lighter, less detail)"),
    ("main", "Main stream (more detail, more work for the plugin)"),
)


@dataclass
class Camera:
    id: str
    name: str
    motion: dict[str, Any]


@dataclass
class Snapshot:
    """What the Plugins page shows: the saved settings, the cameras, the status."""

    config: PluginsConfig
    cameras: list[Camera]
    status: dict[str, ServerStatus]


def client_rpc(endpoint: str | None) -> PluginsRpc:
    def call(method: str, params: dict[str, Any]) -> Any:
        return SyncIpcClient(endpoint=endpoint).call(method, params)

    return call


def load(rpc: PluginsRpc) -> Snapshot:
    cameras = [
        Camera(
            str(c["id"]),
            str(c.get("name") or c["id"]),
            c["motion"] if isinstance(c.get("motion"), dict) else {},
        )
        for c in rpc("camera.list", {}) or []
        if isinstance(c, dict) and "id" in c
    ]
    config = rpc("plugins.get", {})
    return Snapshot(
        PluginsConfig.from_dict(config), cameras, parse_status(rpc("plugins.status", {}))
    )


class Job(Worker):
    """Runs ``job(rpc)`` off the UI thread."""

    done = Signal(object)  # the job's result, or Exception

    def __init__(self, rpc: PluginsRpc, job: Callable[[PluginsRpc], Any]) -> None:
        super().__init__()
        self._rpc = rpc
        self._job = job

    def run(self) -> None:
        try:
            self.done.emit(self._job(self._rpc))
        except Exception as exc:  # shown on the page, never raised in Qt
            self.done.emit(exc)


class CredentialDialog(QDialog):
    """The application's own frameless credential prompt."""

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setWindowTitle("Vendor credential")
        self.setMinimumWidth(440)
        apply_dialog_theme(self)
        layout = install_dialog_chrome(self, "Vendor credential")
        form = QFormLayout()
        self.credential_edit = QLineEdit()
        self.credential_edit.setEchoMode(QLineEdit.EchoMode.Password)
        self.credential_edit.setAccessibleName("Vendor credential")
        self.credential_edit.setMaxLength(16384)
        form.addRow("Credential", self.credential_edit)
        layout.addLayout(form)
        hint = QLabel("Enter a new credential, or leave it empty to clear the saved one.")
        hint.setWordWrap(True)
        set_label_role(hint, "hint")
        layout.addWidget(hint)
        buttons = QDialogButtonBox(
            QDialogButtonBox.StandardButton.Save | QDialogButtonBox.StandardButton.Cancel
        )
        buttons.accepted.connect(self.accept)
        buttons.rejected.connect(self.reject)
        style_button_box(buttons)
        layout.addWidget(buttons)

    def credential(self) -> str:
        return self.credential_edit.text()


class AddPluginDialog(QDialog):
    """Choose a discovered local daemon or supply an encrypted plugin URL."""

    def __init__(
        self, taken: list[str], parent: QWidget | None = None, *, rpc: PluginsRpc | None = None
    ) -> None:
        super().__init__(parent)
        self._taken = taken
        self._rpc = rpc or client_rpc(None)
        self._worker: Job | None = None
        self._adding = False
        self._closed = False
        self._started = False
        self._added: PluginServer | None = None
        self.setWindowTitle("Add Plugin")
        self.setMinimumWidth(520)
        apply_dialog_theme(self)
        layout = install_dialog_chrome(self, "Add plugin")
        layout.setSpacing(12)
        intro = QLabel("Choose a running plugin on this recorder, or connect by URL.")
        intro.setWordWrap(True)
        set_label_role(intro, "hint")
        layout.addWidget(intro)
        self.source = Segmented([("Local", "local"), ("URL", "url")])
        self.source.setAccessibleName("Plugin connection type")
        layout.addWidget(self.source)
        self.local_panel = QWidget()
        local = QVBoxLayout(self.local_panel)
        local.setContentsMargins(0, 0, 0, 0)
        label = QLabel("Local plugin")
        self.local_plugins = QComboBox()
        self.local_plugins.setAccessibleName("Local plugin")
        label.setBuddy(self.local_plugins)
        local.addWidget(label)
        row = QHBoxLayout()
        row.addWidget(self.local_plugins, 1)
        self.refresh_button = QPushButton("Refresh")
        self.refresh_button.clicked.connect(self.refresh)
        row.addWidget(self.refresh_button)
        local.addLayout(row)
        self.local_note = QLabel("Looking for plugin sockets…")
        self.local_note.setWordWrap(True)
        set_label_role(self.local_note, "hint")
        local.addWidget(self.local_note)
        layout.addWidget(self.local_panel)
        self.url_panel = QWidget()
        url_form = QFormLayout(self.url_panel)
        url_form.setContentsMargins(0, 0, 0, 0)
        self.address_edit = QLineEdit()
        self.address_edit.setMaxLength(2048)
        self.address_edit.setPlaceholderText("quic://host:port or tls://host:port")
        self.address_edit.setAccessibleName("Plugin URL")
        url_form.addRow("Plugin URL", self.address_edit)
        hint = QLabel("Start the plugin first. Use TLS when its network blocks QUIC.")
        hint.setWordWrap(True)
        set_label_role(hint, "hint")
        url_form.addRow("", hint)
        layout.addWidget(self.url_panel)
        form = QFormLayout()
        self.name_edit = QLineEdit()
        self.name_edit.setMaxLength(64)
        self.name_edit.setAccessibleName("Plugin name")
        form.addRow("Name", self.name_edit)
        layout.addLayout(form)
        self.problem = QLabel("")
        self.problem.setWordWrap(True)
        set_status_tone(self.problem, "error")
        layout.addWidget(self.problem)
        self.buttons = QDialogButtonBox(
            QDialogButtonBox.StandardButton.Ok | QDialogButtonBox.StandardButton.Cancel
        )
        self.add_button = self.buttons.button(QDialogButtonBox.StandardButton.Ok)
        self.add_button.setText("Add")
        self.buttons.accepted.connect(self.accept)
        self.buttons.rejected.connect(self.reject)
        style_button_box(self.buttons)
        layout.addWidget(self.buttons)
        self.source.changed.connect(self._source_changed)
        self.local_plugins.currentIndexChanged.connect(self._local_changed)
        self._source_changed("local")

    def showEvent(self, event: QShowEvent) -> None:
        super().showEvent(event)
        if not self._started:
            self._started = True
            self.refresh()

    def _source_changed(self, value: object) -> None:
        self.local_panel.setVisible(value == "local")
        self.url_panel.setVisible(value == "url")
        self.problem.clear()
        self._local_changed()

    def _local_changed(self) -> None:
        candidate = self.local_plugins.currentData()
        local = self.source.value() == "local"
        valid = isinstance(candidate, dict) and not candidate.get("added_as")
        self.add_button.setEnabled(
            self._worker is None and not self._adding and (not local or valid)
        )
        if local and isinstance(candidate, dict):
            manifest = candidate.get("manifest", {})
            self.name_edit.setText(str(manifest.get("id", "")))
            self.local_note.setText(
                f"Already added as {candidate['added_as']}."
                if candidate.get("added_as")
                else str(candidate.get("address", ""))
            )

    def refresh(self) -> None:
        if self._worker is not None:
            return
        self.refresh_button.setEnabled(False)
        self.local_note.setText("Looking for plugin sockets…")
        self._job(lambda rpc: rpc("plugins.discover", {}), self._discovered)

    def _job(self, job: Callable[[PluginsRpc], Any], done: Callable[[object], None]) -> None:
        worker = Job(self._rpc, job)
        worker.done.connect(done)
        worker.finished.connect(worker.deleteLater)
        self._worker = worker
        worker.start()

    def _discovered(self, result: object) -> None:
        self._worker = None
        if self._closed:
            return
        self.refresh_button.setEnabled(True)
        self.local_plugins.clear()
        if isinstance(result, Exception):
            self.local_note.setText(f"Local plugins could not be read: {result}")
            return
        entries = result.get("plugins", []) if isinstance(result, dict) else []
        errors = []
        for candidate in entries:
            if not isinstance(candidate, dict):
                continue
            manifest = candidate.get("manifest")
            if not isinstance(manifest, dict):
                if candidate.get("error"):
                    errors.append(str(candidate["error"]))
                continue
            label = str(manifest.get("name") or manifest.get("id"))
            if candidate.get("added_as"):
                label += " · Already added"
            self.local_plugins.addItem(label, candidate)
        available = next(
            (
                i
                for i in range(self.local_plugins.count())
                if not self.local_plugins.itemData(i).get("added_as")
            ),
            -1,
        )
        self.local_plugins.setCurrentIndex(available if available >= 0 else 0)
        if not self.local_plugins.count():
            self.local_note.setText(
                errors[0]
                if errors
                else "No running local plugins found. Start a plugin, then refresh."
            )
        self._local_changed()

    def server(self) -> PluginServer:
        if self._added is not None:
            return self._added
        candidate = self.local_plugins.currentData()
        address = (
            str(candidate["address"])
            if self.source.value() == "local" and isinstance(candidate, dict)
            else self.address_edit.text().strip()
        )
        return PluginServer(self.name_edit.text().strip(), address)

    def accept(self) -> None:
        if self._worker is not None:
            return
        server = self.server()
        problem = server_id_problem(server.id, self._taken) or address_problem(server.address)
        if self.source.value() == "url" and not server.address.startswith(("quic:", "tls:")):
            problem = "Use a quic://host:port or tls://host:port URL."
        if self.source.value() == "local":
            candidate = self.local_plugins.currentData()
            if not isinstance(candidate, dict):
                problem = "Choose a running local plugin."
            elif candidate.get("added_as"):
                problem = "This plugin has already been added."
        if problem:
            self.problem.setText(problem)
            return
        self._adding = True
        for control in (
            self.source,
            self.name_edit,
            self.address_edit,
            self.local_plugins,
            self.refresh_button,
        ):
            control.setEnabled(False)
        self.problem.clear()
        self.add_button.setText("Adding…")
        self.add_button.setEnabled(False)
        self.buttons.button(QDialogButtonBox.StandardButton.Cancel).setEnabled(False)
        self._job(
            lambda rpc: rpc("plugins.add", {"id": server.id, "address": server.address}),
            self._on_added,
        )

    def _on_added(self, result: object) -> None:
        self._worker = None
        self._adding = False
        for control in (
            self.source,
            self.name_edit,
            self.address_edit,
            self.local_plugins,
            self.refresh_button,
        ):
            control.setEnabled(True)
        self.add_button.setText("Add")
        self.buttons.button(QDialogButtonBox.StandardButton.Cancel).setEnabled(True)
        self._local_changed()
        if isinstance(result, Exception):
            self.problem.setText(str(result))
            return
        servers = PluginsConfig.from_dict({"servers": [result]}).servers
        if not servers:
            self.problem.setText("The recorder returned an invalid plugin.")
            return
        self._added = servers[0]
        self._closed = True
        QDialog.accept(self)

    def reject(self) -> None:
        if self._adding:
            return
        self._closed = True
        super().reject()


class _ManyChoices(QWidget):
    """Any number of a plugin's choices, as checkboxes (``many`` settings)."""

    def __init__(self, item: FormField, value: object, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        grid = QGridLayout(self)
        grid.setContentsMargins(0, 0, 0, 0)
        grid.setHorizontalSpacing(16)
        grid.setVerticalSpacing(6)
        self._boxes: dict[str, QCheckBox] = {}
        for index, choice in enumerate(item.choices):
            box = QCheckBox(item.choice_labels.get(choice, choice))
            self._boxes[choice] = box
            grid.addWidget(box, index // 2, index % 2)
        if item.description:
            note = QLabel(item.description)
            note.setWordWrap(True)
            set_label_role(note, "hint")
            grid.addWidget(note, grid.rowCount(), 0, 1, 2)
        self.set_values(value)

    def set_values(self, value: object) -> None:
        chosen = {str(v) for v in value} if isinstance(value, list) else set()
        for choice, box in self._boxes.items():
            box.setChecked(choice in chosen)

    def values(self) -> list[str]:
        return [choice for choice, box in self._boxes.items() if box.isChecked()]


class BindingEditor(QWidget):
    """A camera's settings for a plugin. Settings the form cannot show are
    kept as they were; a camera moving from the built-in motion detector
    starts with its settings and zones."""

    def __init__(
        self,
        manifest: Manifest | None,
        cameras: list[Camera],
        binding: PluginBinding | None,
        *,
        endpoint: str | None = None,
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        self._manifest = manifest
        self._cameras = cameras
        self._existing = binding
        self._fields = form_fields(manifest.config_schema if manifest else None)
        self._inputs: dict[str, QWidget] = {}
        self._layer_boxes: dict[str, QCheckBox] = {}
        self._zones: ZoneEditor | None = None
        self._zones_key = ""
        self._shapes: ShapeEditor | None = None

        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(12)
        general, general_body, _ = settings_card(
            "Analysis", "What the plugin receives from this camera and what the wall shows."
        )
        form = QFormLayout()
        form.setHorizontalSpacing(16)
        form.setVerticalSpacing(10)
        form.setLabelAlignment(Qt.AlignmentFlag.AlignLeft | Qt.AlignmentFlag.AlignVCenter)
        form.setFieldGrowthPolicy(QFormLayout.FieldGrowthPolicy.FieldsStayAtSizeHint)
        self.camera = QComboBox()
        self.camera.setAccessibleName("Camera")
        self.camera.setMinimumWidth(280)
        for camera in cameras:
            self.camera.addItem(camera.name, camera.id)
        form.addRow("Camera", self.camera)
        if binding is not None:
            # The page already names the camera; it cannot change here.
            self.camera.setEnabled(False)
            form.setRowVisible(self.camera, False)
        self.enabled = QCheckBox("Analyse this camera")
        self.enabled.setChecked(binding.enabled if binding is not None else True)
        form.addRow("", self.enabled)
        self.stream = QComboBox()
        self.stream.setAccessibleName("Video the plugin analyses")
        self.stream.setMinimumWidth(280)
        for value, text in STREAM_TEXT:
            self.stream.addItem(text, value)
        chosen = binding.stream if binding is not None else None
        self.stream.setCurrentIndex(next(i for i, (v, _) in enumerate(STREAM_TEXT) if v == chosen))
        self.stream.setToolTip(
            "The camera's sub stream is smaller and cheaper to analyse; the main stream "
            "has more detail (useful for licence plates or faces far away)"
        )
        form.addRow("Video", self.stream)
        self.own_decoder = QCheckBox("Always decode in the plugin")
        self.own_decoder.setChecked(binding.own_decoder if binding is not None else False)
        self.own_decoder.setToolTip(
            "Off (automatic): the plugin uses the pictures NeuroWatch already decodes "
            "(for a video wall, or a stream set to decode all the time in the camera's "
            "settings) and decodes the camera itself only when there are none. On: it "
            "always decodes the camera itself."
        )
        if manifest is None or manifest.uses_recorder_pictures:
            form.addRow("Decoding", self.own_decoder)
        else:
            self.own_decoder.setVisible(False)
            only_own = QLabel("This plugin always decodes the camera itself.")
            set_label_role(only_own, "hint")
            form.addRow("Decoding", only_own)
        if manifest is not None and manifest.layers:
            shown = set(manifest.wall_layers(binding or PluginBinding("", "")))
            # Two to a row: one long row was the page's minimum width.
            boxes = QGridLayout()
            boxes.setHorizontalSpacing(16)
            boxes.setVerticalSpacing(6)
            for index, (ident, label, _default) in enumerate(manifest.layers):
                box = QCheckBox(label)
                box.setChecked(ident in shown)
                self._layer_boxes[ident] = box
                boxes.addWidget(box, index // 2, index % 2)
            boxes.setColumnStretch(2, 1)
            form.addRow("On the live wall", boxes)
        general_body.addLayout(form)
        layout.addWidget(general)

        self.builtin_card, builtin_body, _ = settings_card("Built-in motion detection")
        self.builtin = QCheckBox("Turn off NeuroWatch's built-in motion detection for this camera")
        self.builtin.setChecked(True)
        self.builtin.setToolTip(
            "With both on, every movement is reported twice: once by each detector"
        )
        self.builtin_note = QLabel(
            "This camera still uses the built-in motion detection, which this plugin "
            "replaces. Its sensitivity and zones were copied here."
        )
        self.builtin_note.setWordWrap(True)
        set_label_role(self.builtin_note, "hint")
        builtin_body.addWidget(self.builtin_note)
        builtin_body.addWidget(self.builtin)
        layout.addWidget(self.builtin_card)

        config = binding.config if binding is not None else self._starting_config()
        settings_fields = [f for f in self._fields if f.kind not in DRAWN_KINDS]
        note: QLabel | None = None
        if manifest is None:
            note = QLabel(
                "The plugin is not connected, so its settings cannot be shown. The camera "
                "can still be bound; its settings keep what they were."
            )
        elif not self._fields:
            note = QLabel("This plugin has no settings for its cameras.")
        if note is not None:
            note.setWordWrap(True)
            set_label_role(note, "hint")
            layout.addWidget(note)
        # Lanes, lines or areas: all of them together on one picture. A plugin with
        # zones alone (motion) keeps the simpler editor beside its settings.
        drawn = [f for f in self._fields if f.kind in DRAWN_KINDS]
        if any(f.kind != "zones" for f in drawn):
            card, body, _ = settings_card(
                "Drawing on the picture",
                "Choose what to change, then click the camera's picture.",
            )
            self._shapes = ShapeEditor(
                drawn, dict(config), endpoint=endpoint, camera_id=self.camera_id()
            )
            body.addWidget(self._shapes)
            layout.addWidget(card)
        # Settings beside the zones, so the camera picture gets the height.
        columns = QHBoxLayout()
        columns.setSpacing(12)
        zones_field = next((f for f in drawn if f.kind == "zones"), None)
        if self._shapes is not None:
            zones_field = None
        if settings_fields:
            card, body, _ = settings_card("Settings", "This plugin's own settings.")
            card.setMinimumWidth(300)
            if zones_field is None:
                card.setMaximumWidth(720)
            settings_form = QFormLayout()
            settings_form.setHorizontalSpacing(16)
            settings_form.setVerticalSpacing(10)
            settings_form.setRowWrapPolicy(QFormLayout.RowWrapPolicy.WrapLongRows)
            for item in settings_fields:
                widget = self._input(item, config.get(item.key, item.default))
                if item.description:
                    widget.setToolTip(item.description)
                self._inputs[item.key] = widget
                if item.kind == "bool":
                    settings_form.addRow("", widget)
                else:
                    settings_form.addRow(item.title, widget)
            body.addLayout(settings_form)
            body.addStretch(1)
            if zones_field is not None:
                columns.addWidget(card, 2, Qt.AlignmentFlag.AlignTop)
            else:
                columns.addWidget(card, 1)
                columns.addStretch(0)
        if zones_field is not None:
            card, body, _ = settings_card(
                zones_field.title or "Zones",
                zones_field.description or "Draw where movement counts.",
            )
            zones = config.get(zones_field.key)
            self._zones = ZoneEditor(
                zones if isinstance(zones, list) else [],
                endpoint=endpoint,
                camera_id=self.camera_id(),
            )
            self._zones_key = zones_field.key
            body.addWidget(self._zones)
            columns.addWidget(card, 5)
        layout.addLayout(columns, 1)
        self._config: dict[str, Any] = dict(config)

        self.camera.currentIndexChanged.connect(self._on_camera)
        self._on_camera()

    @property
    def zones(self) -> ZoneEditor | None:
        return self._zones

    @property
    def shapes(self) -> ShapeEditor | None:
        return self._shapes

    def _pictures(self) -> list[ZoneEditor | ShapeEditor]:
        return [editor for editor in (self._shapes, self._zones) if editor is not None]

    def camera_id(self) -> str:
        return str(self.camera.currentData() or "")

    def _camera(self) -> Camera | None:
        return next((c for c in self._cameras if c.id == self.camera_id()), None)

    def _is_motion_plugin(self) -> bool:
        return self._manifest is not None and self._manifest.id == MOTION_PLUGIN_ID

    def _starting_config(self) -> dict[str, Any]:
        camera = self._cameras[0] if self._cameras else None
        if self._is_motion_plugin() and camera is not None and uses_builtin_motion(camera.motion):
            return motion_config_from_camera(camera.motion)
        return {}

    def _on_camera(self) -> None:
        camera = self._camera()
        moving = self._builtin_applies()
        self.builtin_card.setVisible(moving)
        self.builtin.setVisible(moving)
        if moving and camera is not None and self._existing is None:
            self._fill(motion_config_from_camera(camera.motion))
        for editor in self._pictures():
            editor.set_camera(self.camera_id() or None)
            if self.isVisible():
                editor.load_snapshot()

    def _fill(self, config: dict[str, Any]) -> None:
        """Show ``config``'s values in the form (a new binding's camera choice)."""
        self._config.update(config)
        for key, widget in self._inputs.items():
            if key not in config:
                continue
            value = config[key]
            if isinstance(widget, _ManyChoices):
                widget.set_values(value)
            elif isinstance(widget, QCheckBox):
                widget.setChecked(bool(value))
            elif isinstance(widget, QComboBox):
                index = widget.findData(str(value))
                if index >= 0:
                    widget.setCurrentIndex(index)
            elif isinstance(widget, QSpinBox) and isinstance(value, (int, float)):
                widget.setValue(int(value))
            elif isinstance(widget, QDoubleSpinBox) and isinstance(value, (int, float)):
                widget.setValue(float(value))
            elif isinstance(widget, QLineEdit):
                widget.setText(str(value))
        if self._zones is not None and isinstance(config.get(self._zones_key), list):
            self._zones.canvas.set_zones(config[self._zones_key])
        if self._shapes is not None:
            self._shapes.set_values(self._config)

    def load_snapshot(self) -> None:
        """Take the camera's picture for the zones and lanes, once."""
        for editor in self._pictures():
            if not editor.canvas.has_snapshot():
                editor.load_snapshot()

    @staticmethod
    def _input(item: FormField, value: Any) -> QWidget:
        if item.kind == "bool":
            box = QCheckBox(item.title)
            box.setChecked(bool(value))
            return box
        if item.kind == "many":
            return _ManyChoices(item, value)
        if item.kind == "choice":
            combo = QComboBox()
            for choice in item.choices:
                combo.addItem(item.choice_labels.get(choice, choice), choice)
            if str(value) in item.choices:
                combo.setCurrentIndex(item.choices.index(str(value)))
            return combo
        if item.kind == "int":
            spin = QSpinBox()
            spin.setRange(
                int(item.minimum) if item.minimum is not None else -1_000_000,
                int(item.maximum) if item.maximum is not None else 1_000_000,
            )
            spin.setValue(int(value) if isinstance(value, (int, float)) else spin.minimum())
            return spin
        if item.kind == "float":
            double = QDoubleSpinBox()
            double.setDecimals(1)
            double.setRange(
                float(item.minimum) if item.minimum is not None else -1e6,
                float(item.maximum) if item.maximum is not None else 1e6,
            )
            double.setValue(float(value) if isinstance(value, (int, float)) else double.minimum())
            return double
        return QLineEdit("" if value is None else str(value))

    def config(self) -> dict[str, Any]:
        """The plugin's settings to save: the form's values over what was there."""
        config = dict(self._config)
        for key, widget in self._inputs.items():
            if isinstance(widget, _ManyChoices):
                config[key] = widget.values()
            elif isinstance(widget, QCheckBox):
                config[key] = widget.isChecked()
            elif isinstance(widget, QComboBox):
                config[key] = widget.currentData()
            elif isinstance(widget, QSpinBox):
                config[key] = widget.value()
            elif isinstance(widget, QDoubleSpinBox):
                config[key] = round(widget.value(), 3)
            elif isinstance(widget, QLineEdit):
                config[key] = widget.text()
        if self._zones is not None:
            config[self._zones_key] = self._zones.zones()
        if self._shapes is not None:
            config.update(self._shapes.values())
        return config

    def _wall_layers(self) -> list[str] | None:
        """The chosen wall layers; None while they are the plugin's defaults
        (so a plugin that changes its defaults is followed)."""
        if not self._layer_boxes or self._manifest is None:
            return self._existing.wall_layers if self._existing is not None else None
        chosen = [ident for ident, box in self._layer_boxes.items() if box.isChecked()]
        defaults = [ident for ident, _label, on in self._manifest.layers if on]
        return None if chosen == defaults else chosen

    def binding(self, server: str) -> PluginBinding:
        return PluginBinding(
            server,
            self.camera_id(),
            self.enabled.isChecked(),
            self.config(),
            self.stream.currentData(),
            self._wall_layers(),
            self.own_decoder.isChecked(),
        )

    def turn_off_builtin(self) -> bool:
        """Switch the camera's built-in motion detection off once bound."""
        return self._builtin_applies() and self.builtin.isChecked()

    def _builtin_applies(self) -> bool:
        camera = self._camera()
        return (
            self._is_motion_plugin() and camera is not None and uses_builtin_motion(camera.motion)
        )

    def problem(self) -> str | None:
        """Why these settings cannot be saved (shown where the problem is)."""
        if self._shapes is not None:
            shapes_problem = self._shapes.problem()
            if shapes_problem is not None:
                self._shapes.show_problem(shapes_problem)
                return shapes_problem
        if self._zones is not None and not self._zones.valid():
            text = "Complete each zone with 3–64 points, or remove it."
            self._zones.show_problem(text)
            return text
        return None

    def stop(self) -> None:
        for editor in self._pictures():
            editor.stop()


class BindingDialog(QDialog):
    """Bind a camera to a plugin: the camera and its settings."""

    def __init__(
        self,
        plugin_name: str,
        manifest: Manifest | None,
        cameras: list[Camera],
        binding: PluginBinding | None = None,
        *,
        endpoint: str | None = None,
        parent: QWidget | None = None,
    ) -> None:
        super().__init__(parent)
        editing = binding is not None
        title = f"{plugin_name}: camera settings" if editing else f"Bind a camera to {plugin_name}"
        self.setWindowTitle(title)
        apply_dialog_theme(self)
        layout = install_dialog_chrome(self, title, resizable=True)
        layout.setSpacing(12)
        self.editor = BindingEditor(manifest, cameras, binding, endpoint=endpoint)
        # Scrolls: the settings can be taller than the screen, and a window held
        # smaller than its contents lays them over each other.
        self.settings_area = QScrollArea()
        self.settings_area.setWidgetResizable(True)
        self.settings_area.setFrameShape(QFrame.Shape.NoFrame)
        self.settings_area.setWidget(self.editor)
        layout.addWidget(self.settings_area, 1)
        buttons = QDialogButtonBox(
            QDialogButtonBox.StandardButton.Ok | QDialogButtonBox.StandardButton.Cancel
        )
        buttons.button(QDialogButtonBox.StandardButton.Ok).setText("Save" if editing else "Bind")
        buttons.accepted.connect(self.accept)
        buttons.rejected.connect(self.reject)
        style_button_box(buttons)
        layout.addWidget(buttons)
        self._open_size()

    def showEvent(self, event: Any) -> None:
        super().showEvent(event)
        self.editor.load_snapshot()

    def _open_size(self) -> None:
        screen = self.screen() or QGuiApplication.primaryScreen()
        if screen is None:
            return
        available = screen.availableGeometry()
        tall = self.editor.zones is not None or self.editor.shapes is not None
        size = QSize(
            min(int(available.width() * 0.7), 1180 if tall else 600),
            min(int(available.height() * 0.75), 760 if tall else self.sizeHint().height() + 40),
        )
        resize_window(self, size.expandedTo(self.minimumSizeHint()))

    def binding(self, server: str) -> PluginBinding:
        return self.editor.binding(server)

    def turn_off_builtin(self) -> bool:
        return self.editor.turn_off_builtin()

    def accept(self) -> None:
        if self.editor.problem() is None:
            super().accept()

    def done(self, result: int) -> None:
        self.editor.stop()
        super().done(result)
