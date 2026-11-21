"""The Plugins tab: the model, the page against a fake recorder, a camera's
settings (with a move from the built-in motion detector), the zone editor,
and what plugins show on the tiles: a marker and their drawings."""

from __future__ import annotations

import copy
import dataclasses
import time
from typing import Any

import pytest
from PySide6.QtCore import QObject, QPointF, Signal
from PySide6.QtGui import QPixmap
from PySide6.QtWidgets import QCheckBox, QLabel, QPushButton, QWidget

from neurowatch.models.plugins import (
    PluginBinding,
    PluginsConfig,
    PluginServer,
    address_problem,
    binding_text,
    connection_notices,
    form_fields,
    parse_status,
    server_id_problem,
    server_text,
)


class _NoFrames(QObject):
    """No frame reader threads: the walls here never read real frames."""

    frame_ready = Signal(str, bytes, int, int, str)

    def __init__(self, cam_id: str, max_emit_fps: float | None = None, parent=None) -> None:
        super().__init__(parent)

    def start(self) -> None:
        pass

    def stop(self) -> None:
        pass

    def set_max_emit_fps(self, max_emit_fps: float | None) -> None:
        pass


@pytest.fixture(autouse=True)
def _no_frame_readers(monkeypatch) -> None:
    monkeypatch.setattr("neurowatch.widgets.video_wall.FrameReaderThread", _NoFrames)


SCHEMA = {
    "type": "object",
    "properties": {
        "sensitivity": {
            "type": "integer",
            "minimum": 1,
            "maximum": 10,
            "default": 5,
            "title": "Sensitivity",
        },
        "cooldown_sec": {"type": "number", "minimum": 0, "maximum": 600, "default": 10},
        "low_cpu": {"type": "boolean", "default": False, "title": "Keyframes only"},
        "mode": {"enum": ["fast", "accurate"], "default": "fast"},
        "label": {"type": "string"},
        "zones": {"type": "array", "x-nw-widget": "nw:zones", "title": "Zones"},
        "lines": {"type": "array"},
    },
}
MANIFEST = {
    "id": "nw.motion",
    "name": "Motion detection",
    "vendor": "NeuroWatch",
    "version": "0.1.0",
    "config_schema": SCHEMA,
    "overlay_layers": [
        {"id": "detections", "label": "Motion boxes", "default_on": True},
        {"id": "debug", "label": "Analysis", "default_on": False},
    ],
}
ZONES = [{"id": "door", "name": "Door", "kind": "include", "points": [[0, 0], [1, 0], [1, 1]]}]


def test_config_round_trips_and_edits_without_touching_the_original() -> None:
    raw = {
        "servers": [{"id": "motion", "address": "unix:/tmp/m.sock"}],
        "bindings": [
            {"server": "motion", "camera_id": "c1", "config": {"sensitivity": 7}},
            {"server": "gone"},
        ],
    }
    config = PluginsConfig.from_dict(raw)
    assert config.to_dict() == {
        "servers": [{"id": "motion", "address": "unix:/tmp/m.sock", "enabled": True}],
        "bindings": [
            {"server": "motion", "camera_id": "c1", "enabled": True, "config": {"sensitivity": 7}}
        ],
    }
    replaced = config.with_binding(PluginBinding("motion", "c1", False, {"sensitivity": 2}))
    assert len(replaced.bindings) == 1 and not replaced.bindings[0].enabled
    assert config.bindings[0].config == {"sensitivity": 7}
    assert config.without_binding("motion", "c1").bindings == []
    assert config.without_server("motion").to_dict() == {"servers": [], "bindings": []}
    assert not config.with_server_enabled("motion", False).servers[0].enabled
    assert PluginsConfig.from_dict(None).to_dict() == {"servers": [], "bindings": []}
    assert PluginsConfig.from_dict({"servers": 3, "bindings": "x"}).to_dict() == {
        "servers": [],
        "bindings": [],
    }
    assert parse_status({"plugins": [{"id": "m", "bindings": [1]}]})["m"].bindings == {}


def test_names_and_addresses_are_checked_like_the_recorder() -> None:
    assert server_id_problem("motion", []) is None
    assert server_id_problem("motion", ["motion"])
    assert server_id_problem("my plugin", [])
    assert server_id_problem("", [])
    assert address_problem("unix:/run/user/1000/neurowatch/plugins/nw-motion.sock") is None
    assert address_problem("tcp:127.0.0.1:7690") is None
    assert address_problem("quic:analytics.example:443") is None
    assert address_problem("tls:[::1]:7690") is None
    assert address_problem("tls:host:0")
    assert address_problem("unix:")
    assert address_problem("tcp:192.168.1.10:7690")
    assert address_problem("http://x")


def test_status_reads_as_plain_text() -> None:
    status = parse_status(
        {
            "plugins": [
                {
                    "id": "motion",
                    "state": "connected",
                    "manifest": MANIFEST,
                    "bindings": {
                        "c1": {
                            "state": "active",
                            "sent": 9,
                            "dropped": 2,
                            "plugin": {"state": "running", "fps": 4.9, "decoder": "avdec_h264"},
                        },
                        "c2": {"state": "refused", "detail": "No capacity"},
                        "c3": {"state": "active", "plugin": {"state": "settling"}},
                    },
                },
                {"id": "lpr", "state": "error", "detail": "Could not connect: No such file"},
                "junk",
            ]
        }
    )
    server = PluginServer("motion", "unix:/x")
    assert server_text(server, status["motion"]) == (
        "Connected · Motion detection 0.1.0 by NeuroWatch",
        "success",
    )
    text, tone = server_text(PluginServer("lpr", "unix:/y"), status["lpr"])
    assert "Could not connect" in text and tone == "error"
    assert server_text(PluginServer("x", "unix:/z", enabled=False), None)[1] == "muted"
    assert server_text(server, None) == ("Connecting…", "info")

    views = status["motion"].bindings
    binding = PluginBinding("motion", "c1")
    assert binding_text(binding, views["c1"], True) == (
        "Running · 5 pictures a second · avdec_h264 · 2 frames dropped",
        "success",
    )
    assert binding_text(binding, views["c2"], True) == ("Refused: No capacity", "error")
    assert binding_text(binding, views["c3"], True)[0] == "Paused while the camera moves"
    assert binding_text(binding, None, False)[0] == "Waiting for the plugin"
    assert binding_text(PluginBinding("motion", "c1", enabled=False), None, True)[0] == "Off"


def test_the_form_shows_what_it_can_and_keeps_the_rest() -> None:
    fields = {f.key: f for f in form_fields(SCHEMA)}
    assert [f.kind for f in fields.values()] == ["int", "float", "bool", "choice", "text", "zones"]
    assert "lines" not in fields
    assert (fields["sensitivity"].minimum, fields["sensitivity"].maximum) == (1, 10)
    assert fields["cooldown_sec"].title == "Cooldown sec"
    assert fields["mode"].choices == ["fast", "accurate"]
    assert form_fields(None) == [] and form_fields({"properties": 3}) == []


class FakeRecorder:
    """``plugins.*``, ``camera.list`` and ``motion.configure``, in memory."""

    def __init__(self) -> None:
        self.config: dict[str, Any] = {"servers": [], "bindings": []}
        self.cameras = [
            {
                "id": "c1",
                "name": "Door",
                "motion": {"source": "builtin", "sensitivity": 8, "zones": ZONES},
            },
            {"id": "c2", "name": "Yard", "motion": {"source": "onvif"}},
        ]
        self.calls: list[tuple[str, dict[str, Any]]] = []

    def __call__(self, method: str, params: dict[str, Any]) -> Any:
        self.calls.append((method, copy.deepcopy(params)))
        if method == "plugins.get":
            return copy.deepcopy(self.config)
        if method == "plugins.add":
            server = {**params, "enabled": True}
            self.config["servers"].append(server)
            return copy.deepcopy(server)
        if method == "plugins.set":
            self.config = copy.deepcopy(params)
            return self.config
        if method == "plugins.status":
            return {
                "plugins": [
                    {
                        "id": s["id"],
                        "state": "connected",
                        "manifest": MANIFEST,
                        "bindings": {
                            b["camera_id"]: {
                                "state": "active",
                                "plugin": {"state": "running", "fps": 5.0},
                            }
                            for b in self.config["bindings"]
                            if b["server"] == s["id"]
                        },
                    }
                    for s in self.config["servers"]
                ]
            }
        if method == "camera.list":
            return copy.deepcopy(self.cameras)
        if method == "motion.configure":
            camera = next(c for c in self.cameras if c["id"] == params["camera_id"])
            camera["motion"]["source"] = params["source"]
            return camera["motion"]
        if method == "events.list":
            import time

            now = int(time.time() * 1000)
            return {
                "events": [
                    {
                        "id": 9,
                        "kind": "motion",
                        "camera_id": "c1",
                        "source": "plugin",
                        "topic": "nw.motion",
                        "start_ms": now - 60_000,
                        "end_ms": now - 50_000,
                        "score": 0.4,
                    },
                    {
                        "id": 8,
                        "kind": "motion",
                        "camera_id": "c1",
                        "source": "plugin",
                        "topic": "nw.motion",
                        "start_ms": now - 3 * 86_400_000,
                        "end_ms": now - 3 * 86_400_000 + 1000,
                    },
                ]
            }
        raise AssertionError(method)


def _view(qtbot, recorder: FakeRecorder):
    from neurowatch.widgets.plugins_view import PluginsView

    view = PluginsView(None, rpc=recorder)
    qtbot.addWidget(view)
    view.resize(1100, 700)
    view.show()
    view.activate()
    qtbot.waitUntil(lambda: view._snapshot is not None and view._job is None, timeout=3000)
    return view


def _wait_saved(qtbot, view, recorder: FakeRecorder, count: int) -> None:
    qtbot.waitUntil(
        lambda: (
            sum(m in ("plugins.set", "plugins.add") for m, _ in recorder.calls) == count
            and view._job is None
        ),
        timeout=3000,
    )


def test_adding_a_plugin_binding_a_camera_and_removing_it(qtbot, monkeypatch) -> None:
    from neurowatch.dialogs.plugin_dialogs import AddPluginDialog, BindingDialog
    from neurowatch.widgets import plugins_view

    recorder = FakeRecorder()
    view = _view(qtbot, recorder)
    assert view.pages.currentWidget() is view.empty_page
    assert "No plugins yet" in view.empty_label.text()

    def add_dialog(dialog):
        raw = dialog._rpc("plugins.add", {"id": "motion", "address": "unix:/tmp/m.sock"})
        dialog._added = PluginServer(raw["id"], raw["address"])
        return AddPluginDialog.DialogCode.Accepted

    monkeypatch.setattr(AddPluginDialog, "exec", add_dialog)
    view._on_add()
    _wait_saved(qtbot, view, recorder, 1)
    assert recorder.config["servers"] == [
        {"id": "motion", "address": "unix:/tmp/m.sock", "enabled": True}
    ]
    assert view.pages.currentWidget() is view.plugin_page, "the new plugin is shown"
    assert view.tree.topLevelItem(0).text(0) == "Motion detection"
    assert view.tree.topLevelItem(0).toolTip(0).startswith("Connected")

    # Binding Door: the plugin's own settings (nothing carried over from the
    # old built-in detector, which the recorder no longer has), and its page opens.
    monkeypatch.setattr(BindingDialog, "exec", lambda self: BindingDialog.DialogCode.Accepted)
    view._on_bind(view._snapshot.config.servers[0])
    _wait_saved(qtbot, view, recorder, 2)
    binding = recorder.config["bindings"][0]
    assert binding["camera_id"] == "c1"
    assert binding["config"].get("zones") != ZONES, "the old detector's zones are not copied"
    assert "stream" not in binding and "wall_layers" not in binding, "defaults stay unset"
    assert not any(method == "motion.configure" for method, _ in recorder.calls)
    assert view.pages.currentWidget() is view.camera_page
    assert view.camera_title.text() == "Door"
    assert view.tree.topLevelItem(0).child(0).toolTip(0).startswith("Running")

    # Settings: the main stream, and the analysis drawn on the wall too.
    editor = view._editor
    assert editor is not None
    editor.stream.setCurrentIndex(editor.stream.findData("main"))
    editor._layer_boxes["debug"].setChecked(True)
    view._save_settings()
    _wait_saved(qtbot, view, recorder, 3)
    saved = recorder.config["bindings"][0]
    assert saved["stream"] == "main"
    assert saved["wall_layers"] == ["detections", "debug"]

    # Output: counts and recent events; double-click plays one.
    view.camera_tabs.setCurrentIndex(1)
    qtbot.waitUntil(lambda: view.recent.count() == 2, timeout=3000)
    assert view.count_labels["hour"].text() == "1"
    assert view.count_labels["week"].text() == "2"
    played = []
    view.play_requested.connect(played.append)
    view._on_recent_activated(view.recent.item(0))
    assert played[0].id == 9

    # Unbinding and removing ask first.
    monkeypatch.setattr(plugins_view, "ask", lambda *a, **k: True)
    view._on_unbind_selected()
    _wait_saved(qtbot, view, recorder, 4)
    assert recorder.config["bindings"] == []
    assert view.pages.currentWidget() is view.plugin_page
    view._on_remove(view._snapshot.config.servers[0])
    _wait_saved(qtbot, view, recorder, 5)
    assert recorder.config == {"servers": [], "bindings": []}
    assert view.pages.currentWidget() is view.empty_page
    view.stop()


def test_the_live_tab_decodes_only_while_it_shows(qtbot) -> None:
    from neurowatch.models.analytics import CameraPlugin, Drawing, Layer
    from neurowatch.models.camera import CameraConfig

    recorder = FakeRecorder()
    recorder.config = {
        "servers": [{"id": "motion", "address": "unix:/tmp/m.sock"}],
        "bindings": [{"server": "motion", "camera_id": "c2", "config": {}}],
    }
    view = _view(qtbot, recorder)
    camera = CameraConfig(id="c2", name="Yard", url="rtsp://x/1")
    view.set_cameras([camera])
    view.select("motion", "c2")
    assert view.pages.currentWidget() is view.camera_page
    assert view.desktop_demand() == ["c2"], "the Live tab shows the camera"
    view.camera_tabs.setCurrentIndex(2)
    assert view.desktop_demand() == [], "nothing decodes behind the Settings tab"
    view.camera_tabs.setCurrentIndex(0)

    # Every layer shows here, unless switched off on this page.
    layers = (Layer("detections", "Motion boxes", True), Layer("debug", "Analysis"))
    view.set_analytics({"c2": [CameraPlugin("motion", "Motion detection", "running", layers)]})
    box = {"type": "box", "rect": [0.1, 0.1, 0.2, 0.2]}
    drawings = {
        "c2": [
            Drawing("c2", "motion", "detections", (box,), 1e12),
            Drawing("c2", "motion", "debug", (box,), 1e12),
            Drawing("c2", "other", "x", (box,), 1e12),
        ]
    }
    view.set_drawings(drawings)
    cell = view.live_wall._cells[0]
    assert [d.layer for d in cell.drawings] == ["detections", "debug"]
    view._toggle_layer(("motion", "debug"), False)
    assert [d.layer for d in cell.drawings] == ["detections"]
    view.hide()
    assert view.desktop_demand() == []
    view.stop()


def test_a_recorder_that_refuses_shows_why(qtbot) -> None:
    from neurowatch.widgets.plugins_view import PluginsView

    def refuse(method: str, params: dict[str, Any]) -> Any:
        raise RuntimeError("Not allowed")

    view = PluginsView(None, rpc=refuse)
    qtbot.addWidget(view)
    view.activate()
    qtbot.waitUntil(lambda: view._job is None, timeout=3000)
    assert "Not allowed" in view.empty_label.text()
    view.stop()


def test_a_cameras_settings_draw_the_plugins_schema(qtbot) -> None:
    from neurowatch.dialogs.plugin_dialogs import BindingEditor, Camera
    from neurowatch.models.plugins import Manifest

    manifest = Manifest.from_dict(MANIFEST)
    assert manifest is not None
    cameras = [Camera("c2", "Yard", {"source": "onvif"})]
    existing = PluginBinding(
        "motion", "c2", True, {"sensitivity": 3, "lines": [[0, 0]], "mode": "accurate"}
    )
    editor = BindingEditor(manifest, cameras, existing)
    qtbot.addWidget(editor)
    assert not editor.camera.isEnabled(), "an existing binding keeps its camera"
    assert editor._inputs["sensitivity"].value() == 3
    assert editor._inputs["mode"].currentText() == "accurate"
    assert editor.stream.currentData() is None
    assert editor._layer_boxes["detections"].isChecked()
    assert not editor._layer_boxes["debug"].isChecked()
    editor._inputs["sensitivity"].setValue(9)
    editor._inputs["low_cpu"].setChecked(True)
    editor.enabled.setChecked(False)
    saved = editor.binding("motion")
    assert not saved.enabled
    assert saved.config["sensitivity"] == 9 and saved.config["low_cpu"] is True
    assert saved.config["lines"] == [[0, 0]], "settings the form cannot show are kept"
    assert saved.config["zones"] == []
    assert saved.stream is None and saved.wall_layers is None
    editor._layer_boxes["detections"].setChecked(False)
    assert editor.binding("motion").wall_layers == []

    # A zone with too few points is not saved.
    editor.zones.add_zone("include")
    assert editor.problem() is not None
    assert "3–64 points" in editor.zones.status.text()


def test_the_binding_dialog_without_a_connected_plugin_keeps_settings(qtbot) -> None:
    from neurowatch.dialogs.plugin_dialogs import BindingDialog, Camera

    cameras = [Camera("c1", "Door", {"source": "builtin", "sensitivity": 8})]
    existing = PluginBinding("motion", "c1", True, {"sensitivity": 4}, "sub", ["detections"])
    dialog = BindingDialog("motion", None, cameras, existing)
    qtbot.addWidget(dialog)
    assert dialog.editor._inputs == {} and dialog.editor.zones is None
    saved = dialog.binding("motion")
    assert saved.config == {"sensitivity": 4}
    assert saved.stream == "sub" and saved.wall_layers == ["detections"]


def test_drawings_and_markers_reach_the_tiles(qtbot) -> None:
    from neurowatch.models.analytics import (
        CameraPlugin,
        Layer,
        marker_text,
        on_wall,
        parse_cameras,
        parse_overlays,
    )
    from neurowatch.models.camera import CameraConfig
    from neurowatch.widgets.video_wall import VideoWallWidget

    cameras = parse_cameras(
        {
            "cameras": {
                "c1": [
                    {
                        "plugin": "motion",
                        "name": "Motion detection",
                        "state": "running",
                        "layers": [{"id": "detections", "label": "Boxes", "default_on": True}],
                        "wall_layers": ["detections"],
                    }
                ],
                "c2": [{"plugin": "motion", "name": "Motion detection", "state": "offline"}],
            }
        }
    )
    assert cameras["c1"][0] == CameraPlugin(
        "motion",
        "Motion detection",
        "running",
        (Layer("detections", "Boxes", True),),
        frozenset({"detections"}),
    )
    assert marker_text(cameras["c1"]) == "Analysed by Motion detection (running)"
    assert marker_text(cameras["c2"]) == "Analysed by Motion detection (not connected)"
    assert marker_text([]) is None

    drawings = parse_overlays(
        {
            "seq": 3,
            "overlays": [
                {
                    "camera_id": "c1",
                    "plugin": "motion",
                    "layer": "detections",
                    "ttl_ms": 1000,
                    "items": [
                        {"type": "box", "rect": [0.1, 0.1, 0.2, 0.2], "label": "Motion 20%"},
                        {"type": "circle"},
                        {"type": "polygon", "points": [[0, 0]]},
                    ],
                },
                {
                    "camera_id": "c1",
                    "plugin": "motion",
                    "layer": "debug",
                    "items": [{"type": "text", "lines": ["Score 2%"]}],
                },
            ],
        },
        now=time.monotonic(),
    )
    assert [d.layer for d in drawings["c1"]] == ["detections", "debug"]
    assert len(drawings["c1"][0].items) == 1, "items a tile cannot draw are left out"
    assert drawings["c1"][0].until - time.monotonic() > 0.9
    wall_drawings = on_wall(drawings, cameras)
    assert [d.layer for d in wall_drawings["c1"]] == ["detections"]

    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall.resize(640, 360)
    wall.set_grid(1, 1)
    wall.set_cameras([CameraConfig(id="c1", name="Door", url="rtsp://x/1")])
    wall.show()
    wall.set_analytics({"c1": (marker_text(cameras["c1"]), True)})
    wall.set_drawings(wall_drawings)
    cell = wall._cells[0]
    assert cell.analytics == "Analysed by Motion detection (running)"
    assert "Analysed by Motion detection" in cell.toolTip()
    assert [d.layer for d in cell.drawings] == ["detections"]
    # A picture to draw on; painting must not fail and shows the marker.
    import numpy as np

    frame = np.full((180, 320, 3), 40, dtype=np.uint8).tobytes()
    cell.update_frame(frame, 320, 180, "rgb")
    image = cell.grab().toImage()
    assert not cell.analytics_marker_rect().isNull()
    marker = cell.analytics_marker_rect().center()
    assert image.pixelColor(marker.x(), marker.y() + 6).blue() > 150, "a violet marker"
    # The drawing's box edge is painted in the warning/accent palette.
    picture = cell._frame_rect(320, 180)
    edge = (picture.x() + round(0.1 * picture.width()), picture.y() + round(0.2 * picture.height()))
    inside = (edge[0] + 20, edge[1])
    assert image.pixelColor(*edge) != image.pixelColor(*inside), "the box's edge is drawn"
    # A drawing whose time ran out is not drawn.
    expired = [dataclasses.replace(d, until=time.monotonic() - 1) for d in wall_drawings["c1"]]
    cell.set_drawings(expired)
    image = cell.grab().toImage()
    assert image.pixelColor(*edge) == image.pixelColor(*inside)
    wall.set_analytics({})
    wall.set_drawings({})
    assert cell.analytics is None and cell.drawings == []


def test_zone_editor_edits_and_round_trips(qtbot) -> None:
    from neurowatch.widgets.zone_editor import ZoneEditor

    editor = ZoneEditor([], camera_id=None)
    qtbot.addWidget(editor)
    assert not editor.snapshot_button.isEnabled()
    picture = QPixmap(640, 360)
    picture.fill()
    editor.canvas.set_snapshot(picture)
    editor.add_zone("exclude")
    assert not editor.valid()
    for point in (QPointF(10, 10), QPointF(200, 10), QPointF(200, 150)):
        editor.canvas.append_point(point)
    assert editor.valid()
    editor._rename("Road")
    zones = editor.zones()
    assert len(zones) == 1 and zones[0]["kind"] == "exclude" and zones[0]["name"] == "Road"
    editor._remove()
    assert editor.zones() == []
    editor.set_camera("c1")
    assert editor.snapshot_button.isEnabled()
    editor._snapshots._done(RuntimeError("offline"))
    assert "unavailable" in editor.status.text()


def test_the_wall_shows_no_drawings_each_cameras_choice_or_everything() -> None:
    from neurowatch.models.analytics import CameraPlugin, Drawing, Layer, on_wall

    layers = (Layer("detections", "Boxes", True), Layer("debug", "Analysis"))
    cameras = {
        "c1": [CameraPlugin("motion", "Motion", "running", layers, frozenset({"detections"}))]
    }
    box = ({"type": "box", "rect": [0.1, 0.1, 0.2, 0.2]},)
    drawings = {
        "c1": [
            Drawing("c1", "motion", "detections", box, 1e12),
            Drawing("c1", "motion", "debug", box, 1e12),
            Drawing("c1", "unbound", "x", box, 1e12),
        ],
        "c2": [Drawing("c2", "motion", "detections", box, 1e12)],
    }
    assert on_wall(drawings, cameras, "off") == {}
    assert [d.layer for d in on_wall(drawings, cameras)["c1"]] == ["detections"]
    assert [d.layer for d in on_wall(drawings, cameras, "all")["c1"]] == ["detections", "debug"]
    assert "c2" not in on_wall(drawings, cameras, "all"), "only plugins bound to the camera"


def test_settings_choose_the_wall_drawings(qtbot, monkeypatch) -> None:
    from neurowatch.dialogs.settings_dialog import SettingsDialog

    monkeypatch.setattr(SettingsDialog, "_scan_drives", lambda self: None)
    monkeypatch.setattr(SettingsDialog, "_read_archive_status", lambda self: None)
    dialog = SettingsDialog({"ui": {"wall_drawings": "all"}})
    qtbot.addWidget(dialog)
    assert dialog._wall_drawings.value() == "all"
    dialog._wall_drawings.set_value("off")
    assert dialog.get_config()["ui"]["wall_drawings"] == "off"
    fresh = SettingsDialog({"ui": {"wall_drawings": "bogus"}})
    qtbot.addWidget(fresh)
    assert fresh.get_config()["ui"]["wall_drawings"] == "chosen"


def test_the_marker_is_drawn_at_high_resolution(qtbot) -> None:
    from neurowatch.widgets.analytics_paint import marker_pixmap

    for dpr in (1.0, 2.0):
        marker = marker_pixmap(24, dpr, True)
        assert marker.width() >= 24 * dpr * 4, "supersampled"
        assert marker.deviceIndependentSize().width() == 24
    running = marker_pixmap(24, 1.0, True).toImage()
    idle = marker_pixmap(24, 1.0, False).toImage()
    corner = (running.width() * 3 // 24, running.height() // 2)  # the tile, beside the hexagon
    assert running.pixelColor(*corner).blue() > running.pixelColor(*corner).green()
    assert idle.pixelColor(*corner) != running.pixelColor(*corner)


def test_vendor_credential_uses_frameless_chrome_and_masks_its_value(qtbot) -> None:
    from PySide6.QtCore import Qt
    from PySide6.QtWidgets import QLineEdit

    from neurowatch.dialogs.plugin_dialogs import CredentialDialog

    dialog = CredentialDialog()
    qtbot.addWidget(dialog)
    assert dialog.windowFlags() & Qt.WindowType.FramelessWindowHint
    assert dialog.credential_edit.echoMode() == QLineEdit.EchoMode.Password
    dialog.credential_edit.setText("test-only credential")
    assert dialog.credential() == "test-only credential"


def test_plugin_picker_discovers_local_daemons_and_rejects_added_ones(qtbot) -> None:
    from neurowatch.dialogs.plugin_dialogs import AddPluginDialog

    calls = []

    def rpc(method, params):
        calls.append((method, params))
        if method == "plugins.discover":
            return {
                "plugins": [
                    {
                        "address": "unix:/plugins/motion.sock",
                        "manifest": MANIFEST,
                        "added_as": "included-motion",
                    },
                    {
                        "address": "unix:/plugins/count.sock",
                        "manifest": {"id": "test.count", "name": "Counter"},
                    },
                ]
            }
        if method == "plugins.add":
            return {**params, "enabled": True, "plugin_id": "test.count"}
        raise AssertionError(method)

    dialog = AddPluginDialog(["included-motion"], rpc=rpc)
    qtbot.addWidget(dialog)
    dialog.show()
    from neurowatch.dialogs.plugin_dialogs import ADD_PLUGIN_WIDTH
    from PySide6.QtGui import QGuiApplication

    screen = QGuiApplication.primaryScreen().availableGeometry().width()
    assert dialog.width() >= min(ADD_PLUGIN_WIDTH, int(screen * 0.9)), "room for a socket path"
    qtbot.waitUntil(lambda: dialog.local_plugins.count() == 2)
    assert dialog.local_plugins.currentText() == "Counter"
    dialog.local_plugins.setCurrentIndex(0)
    assert not dialog.add_button.isEnabled()
    dialog.accept()
    assert "already been added" in dialog.problem.text()
    assert not any(method == "plugins.add" for method, _ in calls)
    dialog.local_plugins.setCurrentIndex(1)
    dialog.accept()
    qtbot.waitUntil(lambda: dialog._added is not None)
    assert dialog.server().plugin_id == "test.count"
    assert calls[-1] == ("plugins.add", {"id": "test.count", "address": "unix:/plugins/count.sock"})


def test_plugin_url_failure_keeps_the_dialog_and_entered_values(qtbot) -> None:
    from neurowatch.dialogs.plugin_dialogs import AddPluginDialog

    def rpc(method, params):
        if method == "plugins.discover":
            return {"plugins": []}
        raise RuntimeError("This plugin has already been added")

    dialog = AddPluginDialog([], rpc=rpc)
    qtbot.addWidget(dialog)
    dialog.source.set_value("url")
    dialog.name_edit.setText("remote")
    dialog.address_edit.setText("tls://analytics.example:443")
    dialog.accept()
    qtbot.waitUntil(lambda: "already been added" in dialog.problem.text())
    assert dialog._added is None
    assert dialog.address_edit.text() == "tls://analytics.example:443"


def test_a_drawing_leaves_the_tile_on_time_without_a_new_frame(qtbot) -> None:
    # A stalled stream (or a GPU tile, which repaints only on frames) must not
    # keep a motion box on the picture past its lifetime.
    from neurowatch.models.analytics import Drawing
    from neurowatch.models.camera import CameraConfig
    from neurowatch.widgets.cell_widget import CellWidget

    cell = CellWidget()
    qtbot.addWidget(cell)
    cell.set_camera(CameraConfig(id="c1", name="Door", url="rtsp://x/1"))
    box = {"type": "box", "rect": [0.1, 0.1, 0.2, 0.2], "style": "warning"}
    now = time.monotonic()
    short = Drawing("c1", "motion", "detections", (box,), now + 0.15)
    lasting = Drawing("c1", "motion", "debug", (box,), now + 30)
    cell.set_drawings([short, lasting])
    assert cell.drawings == [short, lasting]
    qtbot.waitUntil(lambda: cell.drawings == [lasting], timeout=2000)
    assert cell._drawings_expiry.isActive(), "the next one is still timed"
    cell.set_drawings([])
    assert not cell._drawings_expiry.isActive()


def test_a_plugins_detection_areas_are_drawn_on_the_desktop_with_its_other_shapes(
    qtbot,
) -> None:
    """Vehicle counting: its detection areas are an ``nw:zones`` field, drawn
    with its lines and crossing areas on one picture and captioned with the
    plugin's description."""
    from PySide6.QtWidgets import QLabel

    from neurowatch.dialogs.plugin_dialogs import BindingEditor, Camera
    from neurowatch.models.plugins import Manifest

    described = "Where vehicles are looked for (Watch) and never looked for (Ignore)."
    manifest = Manifest.from_dict(
        {
            **MANIFEST,
            "id": "nw.counter",
            "config_schema": {
                "type": "object",
                "properties": {
                    "lines": {"type": "array", "x-nw-widget": "nw:lines"},
                    "lanes": {"type": "array", "x-nw-widget": "nw:areas"},
                    "detection_areas": {
                        "type": "array",
                        "x-nw-widget": "nw:zones",
                        "title": "Detection areas",
                        "description": described,
                    },
                },
            },
        }
    )
    assert manifest is not None
    lane = {"id": "l1", "points": [[0, 0], [1, 0], [1, 1]]}
    ignored = {
        "id": "p",
        "name": "Parking",
        "kind": "exclude",
        "points": [[0, 0], [0.5, 0], [0.5, 0.5]],
    }
    existing = PluginBinding("counter", "c2", True, {"lanes": [lane], "detection_areas": [ignored]})
    editor = BindingEditor(manifest, [Camera("c2", "Road", {})], existing)
    qtbot.addWidget(editor)
    assert editor.shapes is not None
    editor.shapes.tabs.set_value("detection_areas")
    assert any(label.text() == described for label in editor.findChildren(QLabel))
    saved = editor.binding("counter").config
    assert saved["lanes"] == [lane]
    assert saved["detection_areas"][0]["kind"] == "exclude"
    assert saved["detection_areas"][0]["points"] == ignored["points"]


def test_figures_are_read_for_showing_beside_the_picture() -> None:
    from neurowatch.models.analytics import Figure, parse_cameras

    cameras = parse_cameras(
        {
            "cameras": {
                "c1": [
                    {
                        "plugin": "counter",
                        "figures": [
                            {"label": "Total today", "value": "15"},
                            {"value": "no label"},
                            "not a figure",
                        ],
                    },
                    {"plugin": "motion"},
                ]
            }
        }
    )
    assert cameras["c1"][0].figures == (Figure("Total today", "15"),)
    assert cameras["c1"][1].figures == ()


def test_figures_stay_hidden_until_asked_for(qtbot) -> None:
    from neurowatch.models.analytics import CameraPlugin, Figure

    view = _view(qtbot, FakeRecorder())
    assert view.figures_shown.isHidden(), "nothing to offer yet"
    view._selection = ("counter", "c1")
    figures = (
        Figure("Cars", "1200", "car", "series1"),
        Figure("Total today", "1205", "total"),
        Figure("Lane 1", "91"),
    )
    view.set_analytics({"c1": [CameraPlugin("counter", "Counting", "running", figures=figures)]})
    assert not view.figures_shown.isHidden(), "offered once the plugin reports some"
    assert not view.figures_shown.isChecked()
    assert view.live_figures.summary() == [], "no numbers until asked for"
    assert view.live_figures.other_text() == ""
    assert view.live_figures.key() == ["Cars"], "what the boxes' colour means, all the same"
    view.figures_shown.setChecked(True)
    assert not view.live_figures.isHidden()
    assert view.live_figures.summary() == [("Cars", "1,200"), ("Total today", "1,205")]
    assert view.live_figures.other_text() == "Lane 1: 91"
    assert view.live_figures.key() == [], "the cell shows the colour now"
    view.figures_shown.setChecked(False)
    assert view.live_figures.summary() == []
    view.set_analytics({})
    assert view.figures_shown.isHidden()
    assert view.live_figures.isHidden()


def test_the_legend_shows_icons_and_the_colour_of_each_types_boxes(qtbot) -> None:
    from neurowatch.models.analytics import Figure
    from neurowatch.widgets.figures_legend import _ICON_NAMES, _ICONS, FiguresLegend

    assert {path.stem for path in _ICONS.glob("*.png")} == set(_ICON_NAMES), "every icon ships"
    legend = FiguresLegend()
    qtbot.addWidget(legend)
    legend.resize(1000, 200)
    legend.show()
    legend.set_figures(
        (
            Figure("Cars", "5", "car", "series1"),
            Figure("Vans", "0", "van", "series6"),
            Figure("Odd", "1", "../secret"),
        ),
        "Door",
    )
    car, van, odd = legend._cells
    assert not car.icon.pixmap().isNull() and not van.icon.pixmap().isNull()
    assert odd.icon.pixmap().isNull(), "only the app's own icons, never a path from a plugin"
    assert "#3b82f6" in car.swatch.styleSheet()
    assert van.property("empty") is True
    legend.apply_theme("light")
    assert "#172033" in legend.styleSheet(), "follows the theme"
    legend.apply_theme("dark")
    # Narrow: the cells wrap, none is lost; none left behind when fewer come.
    legend.resize(300, 200)
    legend._reflow()
    assert legend._columns == 2
    legend.set_figures((Figure("Cars", "6", "car", "series1"),))
    assert legend.summary() == [("Cars", "6")]
    legend.set_figures(())
    assert legend.isHidden()


def _named_areas(camera: str = "c1", until: float = 100.0) -> Any:
    from neurowatch.models.analytics import Drawing

    points = [[0, 0], [1, 0], [1, 1]]
    return Drawing(
        camera,
        "counter",
        "lines",
        (
            {"type": "polygon", "points": points, "label": "Lane 1", "style": "success"},
            {"type": "polygon", "points": points, "label": "Lane 1", "style": "success"},
            {
                "type": "polygon",
                "points": points,
                "label": "Ignored: Parking",
                "style": "danger",
                "fill": True,
            },
            {"type": "polygon", "points": points, "style": "muted"},
            {"type": "box", "rect": [0, 0, 1, 1], "label": "Motion 73%", "style": "accent"},
        ),
        until,
    )


def test_the_key_names_each_area_once_and_no_object() -> None:
    from neurowatch.models.analytics import Marking, markings_drawn

    assert markings_drawn([_named_areas()]) == (
        Marking("Lane 1", "success"),
        Marking("Ignored: Parking", "danger", True),
    )


def test_the_key_gives_a_counted_types_colour_only_while_its_cell_does_not() -> None:
    from neurowatch.models.analytics import Figure, Marking, markings_reported

    figures = (
        Figure("Cars", "5", "car", "series1"),
        Figure("Total today", "5", "total"),
        Figure("Gate", "In 5", "", "warning"),
        Figure("Analysis rate", "9 fps"),
    )
    assert markings_reported(figures, counts_shown=False) == (
        Marking("Cars", "series1"),
        Marking("Gate", "warning"),
    )
    assert markings_reported(figures, counts_shown=True) == (Marking("Gate", "warning"),)


def test_the_key_outlasts_a_layer_lapsing_for_a_moment() -> None:
    from neurowatch.models.analytics import MarkingHold

    hold = MarkingHold(hold_s=6.0)
    hold.update({"c1": [_named_areas()]}, now=10.0)
    hold.update({"c2": [_named_areas("c2", until=5.0)]}, now=10.0)
    assert [m.label for m in hold.shown(["c1"], now=10.0)] == ["Lane 1", "Ignored: Parking"]
    assert hold.shown(["c2"], now=10.0) == (), "a drawing already lapsed is not drawn"
    assert hold.shown(["c9"], now=10.0) == (), "another camera's key is not this one's"
    hold.update({"c1": []}, now=12.0)
    assert len(hold.shown(["c1"], now=15.9)) == 2, "held through the gap, or the wall would jump"
    assert hold.shown(["c1"], now=16.1) == ()


def test_the_key_shows_colours_and_names_and_counts_what_does_not_fit(qtbot) -> None:
    from neurowatch.models.analytics import Figure, Marking
    from neurowatch.widgets.figures_legend import _KEY_LIMIT, FiguresLegend

    legend = FiguresLegend()
    qtbot.addWidget(legend)
    legend.resize(900, 200)
    legend.show()
    legend.set_markings((Marking("Lane 1", "success"), Marking("Ignored", "danger", True)))
    assert not legend.isHidden(), "a key alone is reason enough to show"
    legend.layout().activate()
    lane, ignored = legend._key_entries
    assert legend.key() == ["Lane 1", "Ignored"]
    assert "#3ccf6e" in lane.swatch.styleSheet() and "transparent" in lane.swatch.styleSheet()
    assert "background: #ff5a5a" in ignored.swatch.styleSheet(), "filled like its drawing"
    legend.set_figures((Figure("Cars", "5", "car", "series1"),))
    legend.set_markings(())
    assert not legend.isHidden(), "figures keep it open"
    legend.set_figures(())
    assert legend.isHidden()

    many = tuple(Marking(f"Lane {n}", "success") for n in range(_KEY_LIMIT + 3))
    legend.set_markings(many)
    assert legend.key()[-1] == "+3 more"
    assert len(legend.key()) == _KEY_LIMIT + 1
    assert legend._key_entries[-1].swatch.isHidden()
    # Fewer again: the note goes, and the last entry has its colour back.
    legend.set_markings(many[:_KEY_LIMIT])
    assert legend.key()[-1] == f"Lane {_KEY_LIMIT - 1}"
    assert not legend._key_entries[-1].swatch.isHidden()
    # Narrow: entries wrap; a long name never sets the window's minimum width.
    legend.set_markings((Marking("A very long name of an area " * 2, "success"),))
    legend.resize(120, 200)
    assert legend.minimumSizeHint().width() < 120


def test_the_figures_and_the_key_share_one_row_and_wrap_only_without_room(qtbot) -> None:
    from neurowatch.models.analytics import Figure, Marking
    from neurowatch.widgets.figures_legend import FiguresLegend

    legend = FiguresLegend()
    qtbot.addWidget(legend)
    legend.resize(1000, 200)
    legend.show()
    legend.set_figures((Figure("Following", "2"), Figure("Analysis rate", "9.8 fps")))
    legend.set_markings((Marking("Cars", "series1"), Marking("Ignored", "danger", True)))
    legend.layout().activate()
    rate = legend._text_entries[-1]
    first = legend._key_entries[0]
    assert rate.name.text() == "Analysis rate: 9.8 fps"
    assert first.y() == rate.y(), "the key follows the rate on its line"
    assert first.x() > rate.x() + rate.sizeHint().width() - 1

    legend.resize(120, 200)
    legend.layout().activate()
    assert first.y() > rate.y(), "no room: onto the next line, none lost"
    assert legend.layout().totalHeightForWidth(120) > legend.layout().totalHeightForWidth(1000)


def test_a_figure_and_the_key_about_the_same_thing_are_one_entry(qtbot) -> None:
    from neurowatch.models.analytics import Figure, Marking
    from neurowatch.widgets.figures_legend import FiguresLegend

    legend = FiguresLegend()
    qtbot.addWidget(legend)
    legend.set_figures(
        (Figure("Lane 1", "91"), Figure("Crossing", "In 12", "", "warning"), Figure("Rate", "9")),
    )
    legend.set_markings((Marking("Lane 1", "success"), Marking("Ignored", "danger", True)))
    assert legend.other_text() == "Lane 1: 91   ·   Crossing: In 12   ·   Rate: 9"
    assert legend.key() == ["Ignored"]
    lane, crossing, rate = legend._text_entries
    assert "#3ccf6e" in lane.swatch.styleSheet(), "the area's colour beside its number"
    assert "#ffb020" in crossing.swatch.styleSheet(), "the figure's own colour"
    assert rate.swatch.isHidden(), "not about anything on the picture"
    # A lane and its counting line under one name, reported twice: told once.
    legend.set_figures((Figure("Lane 1", "91"), Figure("Lane 1", "91"), Figure("Lane 2", "4")))
    assert legend.other_text() == "Lane 1: 91   ·   Lane 2: 4"


def test_only_the_analysed_tile_shows_the_panel_over_its_picture(qtbot) -> None:
    from PySide6.QtCore import Qt

    from neurowatch.models.analytics import Figure, Marking, TileInfo
    from neurowatch.models.camera import CameraConfig
    from neurowatch.widgets.analytics_paint import BOTTOM_RESERVED
    from neurowatch.widgets.figures_legend import OVERLAY_OPACITY
    from neurowatch.widgets.video_wall import VideoWallWidget

    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall.set_grid(1, 2)
    wall.resize(1600, 500)
    wall.set_cameras(
        [CameraConfig(id=i, name=i, url=f"rtsp://x/{i}") for i in ("road", "yard")], page=0
    )
    wall.show()
    qtbot.waitExposed(wall)
    road, yard = wall._cell_for_cam_id("road"), wall._cell_for_cam_id("yard")
    size_before = wall.size()

    wall.set_plugin_info({"road": TileInfo(markings=(Marking("Lane 1", "success"),))})
    panel = road._plugin_info
    assert panel is not None and not panel.isHidden()
    assert yard._plugin_info is None, "a camera no plugin analyses has none, not even hidden"
    assert wall.size() == size_before, "the wall keeps its size"
    assert panel.parentWidget() is road, "it belongs to that tile"
    assert road.rect().contains(panel.geometry()), "and stays on it"
    assert panel.geometry().bottom() == road.rect().bottom() - BOTTOM_RESERVED, (
        "above the tile's time and rate strip"
    )
    assert panel.testAttribute(Qt.WidgetAttribute.WA_TransparentForMouseEvents)
    style = panel.styleSheet()
    assert f"{round(OVERLAY_OPACITY * 255 / 100)})" in style, "the background is see-through"
    assert "rgba" not in style.split("figureNumber")[1], "the text is not"
    panel.apply_theme("light")
    assert panel.styleSheet() == style, "over video, whatever the app's theme"

    # Another page: the tile shows another camera, without this one's panel.
    road.set_camera(CameraConfig(id="gate", name="gate", url="rtsp://x/gate"))
    assert panel.isHidden()
    wall.set_plugin_info({})
    assert panel.isHidden()

    # Its own figures, not added to any other camera's.
    road.set_camera(CameraConfig(id="road", name="road", url="rtsp://x/road"))
    figures = (Figure("Cars", "5", "car", "series1"), Figure("Rate", "9"))
    wall.set_plugin_info({"road": TileInfo(figures)})
    assert panel.summary() == [("Cars", "5")]
    assert panel.other_text() == "Rate: 9"


def test_the_panel_fits_the_tile_it_is_on(qtbot) -> None:
    from PySide6.QtCore import QRect

    from neurowatch.models.analytics import Figure, Marking
    from neurowatch.widgets.analytics_paint import BOTTOM_RESERVED, TOP_RESERVED
    from neurowatch.widgets.figures_legend import FiguresLegend

    host = QWidget()
    qtbot.addWidget(host)
    host.resize(1920, 1080)
    host.show()
    panel = FiguresLegend(host, floating=True)
    counted = tuple(Figure(f"T{i}", "5", "car", "series1") for i in range(9))
    panel.set_figures((*counted, Figure("Lane 1", "9"), Figure("Rate", "9 fps")))
    panel.set_markings((Marking("Zone 1", "muted"),))

    # Full size: the same cells as the Plugins page, the bottom row beside the key.
    panel.place_in(QRect(0, 0, 1920, 1080))
    assert not panel.isHidden()
    assert not panel._cells[0]._compact and panel._cells[0].name.isVisibleTo(panel._cells[0])
    assert panel._text_entries[0].isVisibleTo(panel)

    full_height = panel.height()

    # A 2×2 tile: compact already, and about half the height it took before (123 px).
    panel.place_in(QRect(0, 0, 590, 332))
    assert panel._cells[0]._compact
    assert panel.height() <= 62
    assert panel.height() < full_height

    # A 3×3 tile: small cells, nothing outside the room between name and strip.
    panel.place_in(QRect(0, 0, 390, 220))
    assert not panel.isHidden()
    assert panel._cells[0]._compact and not panel._cells[0].name.isVisibleTo(panel._cells[0])
    assert panel.height() <= 220 - TOP_RESERVED - BOTTOM_RESERVED
    assert panel._text_entries[0].isVisibleTo(panel)

    assert panel.height() <= 62, "about half the 123 px it took before"

    # Smaller still: the counts only.
    panel.place_in(QRect(0, 0, 240, 135))
    assert not panel.isHidden()
    assert not panel._text_entries[0].isVisibleTo(panel)
    assert panel.height() <= 135 - TOP_RESERVED - BOTTOM_RESERVED

    # Nothing fits: nothing, rather than covering the whole picture.
    panel.place_in(QRect(0, 0, 160, 90))
    assert panel.isHidden()

    # Bigger again: back, and a key alone still shows on a small tile.
    panel.place_in(QRect(0, 0, 1920, 1080))
    assert not panel.isHidden()
    assert not panel._cells[0]._compact
    panel.set_figures(())
    panel.place_in(QRect(0, 0, 240, 135))
    assert not panel.isHidden()
    assert panel._key_entries[0].isVisibleTo(panel), "a key has no cells to give way to"


def test_a_compact_panel_leaves_out_zero_counts_and_tints_glyphs_in_their_colour(qtbot) -> None:
    from PySide6.QtCore import QRect

    from neurowatch.models.analytics import Figure
    from neurowatch.widgets.figures_legend import FiguresLegend

    host = QWidget()
    qtbot.addWidget(host)
    host.resize(1920, 1080)
    host.show()
    panel = FiguresLegend(host, floating=True)
    figures = (
        Figure("Cars", "5", "car", "series1"),
        Figure("Vans", "0", "van", "series6"),
        Figure("Total", "0", "total"),
    )
    panel.set_figures(figures)
    panel.place_in(QRect(0, 0, 590, 332))
    cars, vans, total = panel._cells
    assert cars.isVisibleTo(panel) and cars.icon_color == "#3b82f6"
    assert cars.swatch.isHidden(), "the glyph says it: one colour per type"
    assert not vans.isVisibleTo(panel), "nothing counted yet, no room for it"
    assert total.isVisibleTo(panel), "the total always shows"
    assert panel.summary()[1] == ("Vans", "0"), "still known"

    # A van comes along: its cell joins the row.
    panel.set_figures((figures[0], Figure("Vans", "1", "van", "series6"), figures[2]))
    assert vans.isVisibleTo(panel)

    # Full size: every type is labelled, with colour on its outline glyph.
    panel.set_figures(figures)
    panel.place_in(QRect(0, 0, 1920, 1080))
    assert vans.isVisibleTo(panel)
    assert cars.swatch.isHidden() and cars.icon_color == "#3b82f6"
    assert cars.styleSheet() == ""


def test_a_panel_stays_over_a_gpu_picture(qtbot) -> None:
    from neurowatch.models.analytics import Marking, TileInfo
    from neurowatch.models.camera import CameraConfig
    from neurowatch.widgets.cell_widget import CellWidget

    class _Surface(QWidget):
        failed = type("S", (), {"connect": staticmethod(lambda _slot: None)})()

        def __init__(self, parent, *_args) -> None:
            super().__init__(parent)

        def set_frame(self, *_args) -> None:
            pass

    import neurowatch.widgets.gpu_video_surface as gpu

    original = gpu.GpuVideoSurface
    gpu.GpuVideoSurface = _Surface
    try:
        cell = CellWidget()
        qtbot.addWidget(cell)
        cell.resize(800, 450)
        cell.set_camera(CameraConfig(id="c", name="c", url="rtsp://x/c"))
        cell.show()
        cell.set_plugin_info(TileInfo(markings=(Marking("Lane 1", "success"),)))
        cell.update_frame(bytes(1382400), 1280, 720, "nv12:1280")
        children = [c for c in cell.children() if isinstance(c, QWidget)]
        assert children[-1] is cell._plugin_info, "the panel is on top, not hidden by the surface"
        cell.update_frame(bytes(1382400), 1280, 720, "nv12:1280")
        children = [c for c in cell.children() if isinstance(c, QWidget)]
        assert children[-1] is cell._plugin_info, "also on later frames"
    finally:
        gpu.GpuVideoSurface = original


def test_the_key_is_plain_text_from_a_plugin(qtbot) -> None:
    from PySide6.QtCore import Qt

    from neurowatch.models.analytics import Marking
    from neurowatch.widgets.figures_legend import FiguresLegend

    legend = FiguresLegend()
    qtbot.addWidget(legend)
    legend.set_markings((Marking("<img src=x>", "success"),))
    assert legend._key_entries[0].name.textFormat() == Qt.TextFormat.PlainText


def test_the_plugins_page_shows_the_key_for_the_layers_it_draws(qtbot) -> None:
    from neurowatch.models.analytics import CameraPlugin, Layer

    view = _view(qtbot, FakeRecorder())
    view._selection = ("counter", "c1")
    layer = Layer("lines", "Lines")
    view.set_analytics({"c1": [CameraPlugin("counter", "Counting", "running", layers=(layer,))]})
    far = time.monotonic() + 60
    view.set_drawings({"c1": [_named_areas(until=far)]})
    assert view.live_figures.key() == ["Lane 1", "Ignored: Parking"]
    view._toggle_layer(("counter", "lines"), False)
    assert view.live_figures.key() == [], "switched off: out of the key at once"


def _analysed_wall(qtbot, monkeypatch):
    """A one-tile wall with a plugin running, recording what the tile draws."""
    from neurowatch.models.camera import CameraConfig
    from neurowatch.widgets import cell_widget
    from neurowatch.widgets.video_wall import VideoWallWidget

    painted: list[list[str]] = []
    monkeypatch.setattr(
        cell_widget,
        "paint_drawings",
        lambda _painter, _rect, drawings, **_kw: painted.append([d.layer for d in drawings]),
    )
    wall = VideoWallWidget()
    qtbot.addWidget(wall)
    wall.resize(640, 360)
    wall.set_grid(1, 1)
    wall.set_cameras([CameraConfig(id="c1", name="Door", url="rtsp://x/1")])
    wall.show()
    wall.set_analytics({"c1": ("Analysed", True)})
    return wall, wall._cells[0], painted


def _boxes(seq: int):
    from neurowatch.models.analytics import Drawing

    box = {"type": "box", "rect": [0.1, 0.2, 0.3, 0.4], "style": "series1"}
    return Drawing(
        "c1",
        "counter",
        "detections",
        (box,),
        time.monotonic() + 30,
        frame=seq,
        time_us=seq * 40_000,
        observed_us=seq * 40_000,
        ttl_ms=2500,
    )


def test_coming_back_to_a_wall_never_draws_older_results_on_new_pictures(
    qtbot, monkeypatch
) -> None:
    wall, cell, painted = _analysed_wall(qtbot, monkeypatch)
    frame = bytes(320 * 180 * 3)
    # Away from the wall its decoder stopped, while results went on arriving.
    wall.set_drawings({"c1": [_boxes(100)]})
    # Back: the decoder delivers newer pictures; result 100's picture was never held.
    for seq in (110, 111, 112):
        wall._on_identified_frame_ready(
            "c1", frame, 320, 180, "rgb", seq, seq * 40_000, seq * 40_000
        )
        wall._flush_pending_frames()
        painted.clear()
        cell.grab()
        assert not any(painted), "a box from an earlier picture lands where the vehicle is not"
    # A result about a picture now held: that picture, with its boxes.
    wall.set_drawings({"c1": [_boxes(112)]})
    painted.clear()
    cell.grab()
    assert any("detections" in layers for layers in painted)
    assert cell._frame_identity[0] == 112


def test_a_page_shown_again_keeps_its_own_pictures_identity(qtbot, monkeypatch) -> None:
    wall, cell, _painted = _analysed_wall(qtbot, monkeypatch)
    frame = bytes(320 * 180 * 3)
    wall.set_analytics({})
    wall._on_identified_frame_ready("c1", frame, 320, 180, "rgb", 7, 280_000, 280_000)
    wall._flush_pending_frames()
    cell.set_frame_identity(99, 99, 99)  # what the tile showed for another camera
    wall._assign_cameras()
    assert wall._cells[0]._frame_identity == (7, 280_000, 280_000)


def test_a_settings_choice_shows_its_label_and_saves_its_value(qtbot) -> None:
    from PySide6.QtWidgets import QComboBox

    from neurowatch.dialogs.plugin_dialogs import BindingEditor, Camera
    from neurowatch.models.plugins import Manifest, PluginBinding

    schema = {
        "properties": {
            "decoder": {
                "enum": ["auto", "vaapi", "software", "other"],
                "default": "auto",
                "x-nw-labels": {"auto": "Automatic", "software": "CPU (software)", "vaapi": 3},
                "title": "Video decoder",
            }
        }
    }
    assert form_fields(schema)[0].choice_labels == {
        "auto": "Automatic",
        "software": "CPU (software)",
    }, "a plugin's schema is not trusted: only text"
    manifest = Manifest.from_dict({"id": "nw.counter", "config_schema": schema})
    binding = PluginBinding("counter", "c1", True, {"decoder": "software"})
    editor = BindingEditor(manifest, [Camera("c1", "Road", {})], binding)
    qtbot.addWidget(editor)
    combo = editor._inputs["decoder"]
    assert isinstance(combo, QComboBox)
    assert combo.currentText() == "CPU (software)"
    assert [combo.itemText(i) for i in range(4)] == [
        "Automatic",
        "vaapi",
        "CPU (software)",
        "other",
    ], "a value with no label is shown as it is"
    assert editor.config()["decoder"] == "software", "the value is saved, not the label"
    combo.setCurrentIndex(0)
    assert editor.config()["decoder"] == "auto"
    combo.setCurrentIndex(combo.findData("vaapi"))
    assert combo.currentText() == "vaapi"
    assert editor.config()["decoder"] == "vaapi"


def test_a_plugin_without_events_has_no_output_and_says_nothing_about_events(qtbot) -> None:
    from neurowatch.models.plugins import Manifest, PluginServer

    counting = Manifest.from_dict({"id": "nw.counter", "permissions": ["video", "overlays"]})
    motion = Manifest.from_dict({"id": "nw.motion", "permissions": ["video", "events"]})
    older = Manifest.from_dict({"id": "nw.other"})
    assert counting is not None and not counting.reports_events
    assert motion is not None and motion.reports_events
    assert older is not None and older.reports_events, "no list: it may send them"

    view = _view(qtbot, FakeRecorder())
    plugin_id = "counter"
    view._server = lambda _plugin: PluginServer(plugin_id, "unix:/x.sock")
    for part in ("_fill_layer_boxes", "_fill_settings", "_refresh_status"):
        setattr(view, part, lambda: None)
    view._manifest = lambda _server: counting
    view.camera_tabs.setCurrentIndex(view._output_tab)
    loaded: list[bool] = []
    view._load_output = lambda: loaded.append(True)
    view._selection = (plugin_id, "c1")
    view._show_camera(plugin_id, "c1")
    assert not view.camera_tabs.isTabVisible(view._output_tab)
    assert view.camera_tabs.currentIndex() != view._output_tab, "moved off the hidden tab"
    assert loaded == [], "and no events are asked for"

    view._manifest = lambda _server: motion
    view._show_camera(plugin_id, "c1")
    assert view.camera_tabs.isTabVisible(view._output_tab), "a plugin with events keeps it"
    assert loaded == [True]


def test_plugins_are_read_often_only_while_one_is_on_its_way() -> None:
    from neurowatch.ipc.analytics_feed import (
        CAMERAS_EVERY_S,
        CAMERAS_STARTING_EVERY_S,
        STARTING_LIMIT_S,
        next_cameras_read,
    )
    from neurowatch.models.analytics import CameraPlugin

    def read(state: str, since: float = 1.0) -> float:
        return next_cameras_read({"c": [CameraPlugin("p", "P", state)]}, since)

    assert read("running") == CAMERAS_EVERY_S
    assert read("error") == CAMERAS_EVERY_S
    assert read("starting") == CAMERAS_STARTING_EVERY_S
    assert read("waiting", 3600) == CAMERAS_STARTING_EVERY_S
    assert read("offline") == CAMERAS_STARTING_EVERY_S, "just opened: it is coming"
    assert read("offline", STARTING_LIMIT_S + 1) == CAMERAS_EVERY_S, "later it is simply off"
    assert next_cameras_read({}, 1.0) == CAMERAS_EVERY_S


def test_a_count_changing_reads_the_cameras_plugins_at_once(monkeypatch):
    """Figures (counts) come with ``plugins.cameras``, read every few seconds;
    the overlay long poll tells when they change, so a count shows with the
    boxes that made it rather than seconds later."""
    from neurowatch.ipc.analytics_feed import AnalyticsFeedWorker

    calls: list[str] = []
    replies = iter(
        [
            {"seq": 1, "overlays": [], "figures": {"cam-1": [{"label": "Cars", "value": "1"}]}},
            {"seq": 2, "overlays": [], "figures": {"cam-1": [{"label": "Cars", "value": "1"}]}},
            {"seq": 3, "overlays": [], "figures": {"cam-1": [{"label": "Cars", "value": "2"}]}},
        ]
    )
    stopped = False

    class Client:
        def call(self, method, params=None, **_kwargs):
            nonlocal stopped
            calls.append(method)
            if method == "plugins.cameras":
                return {"cameras": {}}
            reply = next(replies, None)
            if reply is None:
                stopped = True
                return {"seq": 3, "overlays": []}
            return reply

    worker = AnalyticsFeedWorker("unix:/tmp/private.sock", client_factory=Client)
    monkeypatch.setattr(worker, "isInterruptionRequested", lambda: stopped)
    worker.run()
    assert calls == [
        "plugins.cameras",
        "plugins.overlays",  # the first figures: just read
        "plugins.overlays",  # drawings changed, counts did not
        "plugins.overlays",  # the count changed
        "plugins.cameras",
        "plugins.overlays",
    ]


def test_a_hosted_plugin_says_who_provides_it_and_offers_no_controls(qtbot) -> None:
    view = _view(qtbot, FakeRecorder())
    plugin_id = "hosted.nw.counter"
    managed = {"service": "nw.counter", "instance": "counter-1.example.test"}

    def provided(enabled: bool) -> QLabel:
        view._server = lambda _plugin: PluginServer(
            plugin_id, "quic:counter-1.example.test:443", enabled=enabled, managed=managed
        )
        view._refresh_status = lambda: None
        view._show_plugin(plugin_id)
        page = view.plugin_page.widget()
        buttons = [button.text() for button in page.findChildren(QPushButton)]
        assert "Remove…" not in buttons, "the portal decides"
        assert not [box for box in page.findChildren(QCheckBox) if box.text() == "On"]
        texts = [label.text() for label in page.findChildren(QLabel)]
        assert not [text for text in texts if "counter-1.example.test" in text], (
            "its address is not shown"
        )
        return page.findChild(QLabel, "pluginProvided")

    assert "under Subscription" in provided(True).text()
    assert "not available to this recorder" in provided(False).text()
    binds = [
        button
        for button in view.plugin_page.widget().findChildren(QPushButton)
        if button.text() == "Bind Camera…"
    ]
    assert binds and not any(button.isVisibleTo(view) for button in binds), "chosen in the portal"
    view._show_camera(plugin_id, "cam-1")
    assert not view.unbind_button.isVisibleTo(view), "chosen in the portal"


def test_a_camera_a_hosted_plugin_no_longer_covers_says_so() -> None:
    config = PluginsConfig.from_dict(
        {
            "bindings": [
                {"server": "hosted.x", "camera_id": "a", "enabled": False, "withdrawn": True}
            ]
        }
    )
    assert "Subscription" in binding_text(config.bindings[0], None, True)[0]


def test_a_hosted_plugin_round_trips_its_marker() -> None:
    managed = {"service": "nw.counter", "instance": "v1"}
    assert PluginServer("hosted.nw.counter", "quic:v1:443", managed=managed).managed == managed


def test_a_hosted_plugin_is_shown_by_its_catalog_name() -> None:
    hosted = PluginsConfig.from_dict(
        {
            "servers": [
                {
                    "id": "hosted.nw.counter",
                    "address": "quic:v1:443",
                    "managed": {
                        "service": "nw.counter",
                        "instance": "v1",
                        "label": "Vehicle counting",
                    },
                }
            ]
        }
    )
    server = hosted.servers[0]
    assert server.shown_name() == "Vehicle counting"
    assert server.shown_name("nw-counter") == "Vehicle counting"
    statuses = parse_status(
        {"plugins": [{"id": "hosted.nw.counter", "state": "unreachable", "detail": "timed out"}]}
    )
    [notice] = connection_notices(hosted, statuses)
    assert notice.text.startswith("Vehicle counting unavailable")
    plain = PluginServer("motion", "unix:/tmp/m.sock")
    assert plain.shown_name() == "motion"
    assert plain.shown_name("Motion detection") == "Motion detection"


def test_a_hosted_plugin_no_longer_provided_is_not_listed() -> None:
    config = PluginsConfig.from_dict(
        {
            "servers": [
                {
                    "id": "hosted.nw.counter",
                    "address": "quic:v1:443",
                    "enabled": False,
                    "managed": {"service": "nw.counter", "instance": "v1"},
                },
                {"id": "motion", "address": "unix:/tmp/m.sock", "enabled": False},
            ],
            "bindings": [
                {"server": "motion", "camera_id": "a", "enabled": False, "withdrawn": True},
                {"server": "motion", "camera_id": "b"},
            ],
        }
    )
    # A plugin switched off by hand stays listed; one NeuroWatch took away does not.
    assert [s.id for s in config.shown_servers()] == ["motion"]
    assert [b.camera_id for b in config.shown_bindings_of("motion")] == ["b"]


def test_a_hosted_plugin_goes_back_to_the_recorder_as_it_came() -> None:
    """Sent back without ``managed`` it read as a second plugin of that name,
    and every change on the Plugins page (Remove included) was refused."""
    raw = {
        "servers": [
            {
                "id": "hosted.nw.counter",
                "address": "quic:v1.example.test:443",
                "enabled": False,
                "managed": {"service": "nw.counter", "instance": "v1"},
            },
            {"id": "motion", "address": "unix:/tmp/m.sock"},
        ]
    }
    config = PluginsConfig.from_dict(raw)
    sent = config.without_server("motion").to_dict()["servers"]
    assert sent == [raw["servers"][0]], "the hidden hosted plugin goes back unchanged"


def test_plugin_supplied_text_is_shown_as_written(qtbot) -> None:
    # Security review 2026-11-20, finding 13: a plugin's name and maker come
    # from the plugin, before approval; Qt rendered HTML-looking text as rich
    # text (links, remote images).
    from PySide6.QtCore import Qt

    recorder = FakeRecorder()
    server = {"id": "motion", "address": "unix:/tmp/m.sock", "enabled": True}
    recorder.config["servers"].append(server)
    hostile = '<a href="https://example.test">Click</a><img src="https://example.test/x.png">'
    plugins_view_manifest = {**MANIFEST, "name": hostile, "vendor": hostile}
    original = MANIFEST.copy()
    MANIFEST.update(plugins_view_manifest)
    try:
        view = _view(qtbot, recorder)
        view._show_plugin("motion")
        page = view.plugin_page.widget()
        labels = page.findChildren(QLabel) + view.findChildren(QLabel)
        rich = [
            (label.objectName(), label.text()[:40])
            for label in labels
            if label.textFormat() != Qt.TextFormat.PlainText
        ]
        assert labels and not rich, rich
        assert any(hostile in label.text() for label in page.findChildren(QLabel))
    finally:
        MANIFEST.clear()
        MANIFEST.update(original)
