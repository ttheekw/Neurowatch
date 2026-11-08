"""Always-on decoding for plugins: the camera setting and the binding switch."""

from neurowatch.models.camera import CameraConfig


def _camera(**extra):
    return CameraConfig.from_dict({"id": "cam", "name": "Gate", "url": "rtsp://example.invalid/1",
                                   "sub_url": "rtsp://example.invalid/2", **extra})


def test_the_camera_model_round_trips_the_streams_decoded_all_the_time():
    camera = _camera(always_decode=["sub", 7, "3"])
    assert camera.always_decode == ["sub", "3"], "only stream names"
    assert camera.to_dict()["always_decode"] == ["sub", "3"]
    assert _camera().always_decode == []


def test_the_camera_dialog_ticks_main_and_sub_and_keeps_other_streams(qtbot):
    import neurowatch.dialogs.camera_dialog as module

    dialog = module.AddEditCameraDialog(camera=_camera(always_decode=["3", "sub"]))
    qtbot.addWidget(dialog)
    assert not dialog._always_main.isChecked()
    assert dialog._always_sub.isChecked()
    dialog._always_main.setChecked(True)
    dialog._always_sub.setChecked(False)
    assert dialog.get_config().always_decode == ["3", "main"]
    # A new camera decodes nothing all the time.
    fresh = module.AddEditCameraDialog()
    qtbot.addWidget(fresh)
    assert not fresh._always_main.isChecked() and not fresh._always_sub.isChecked()


def test_decoding_has_a_tab_of_its_own_beside_the_cameras_detection(qtbot):
    import neurowatch.dialogs.camera_dialog as module

    dialog = module.AddEditCameraDialog(camera=_camera())
    qtbot.addWidget(dialog)
    tabs = [dialog._tabs.tabText(i) for i in range(dialog._tabs.count())]
    assert "Motion" not in tabs
    assert tabs[-2:] == ["Detection", "Decoding"]
    decoding = dialog._tabs.widget(tabs.index("Decoding"))
    assert decoding.isAncestorOf(dialog._always_main) and decoding.isAncestorOf(dialog._always_sub)
    detection = dialog._tabs.widget(tabs.index("Detection"))
    assert not detection.isAncestorOf(dialog._always_main), "decoding is not detection"
    dialog._tabs.setCurrentIndex(tabs.index("Decoding"))
    assert dialog._test_panel.isHidden(), "no connection test beside a settings page"


def test_a_binding_forces_the_plugins_own_decoder_only_when_ticked():
    from neurowatch.models.plugins import PluginBinding, PluginsConfig

    config = PluginsConfig.from_dict({"servers": [], "bindings": [
        {"server": "s", "camera_id": "a", "own_decoder": True},
        {"server": "s", "camera_id": "b", "own_decoder": "yes"},
        {"server": "s", "camera_id": "c"},
    ]})
    assert [b.own_decoder for b in config.bindings] == [True, False, False]
    assert config.bindings[0].to_dict()["own_decoder"] is True
    assert "own_decoder" not in PluginBinding("s", "c").to_dict(), "automatic is the default"


def test_the_binding_editor_offers_the_switch_only_to_plugins_that_can_share(qtbot):
    from neurowatch.dialogs.plugin_dialogs import BindingEditor, Camera
    from neurowatch.models.plugins import Manifest, PluginBinding

    def manifest(permissions):
        return Manifest.from_dict({"id": "p", "name": "P", "version": "1",
                                   "permissions": permissions})

    cameras = [Camera("a", "Gate", {})]
    sharing = BindingEditor(manifest(["video", "decoded_frames"]), cameras,
                            PluginBinding("s", "a", own_decoder=True))
    qtbot.addWidget(sharing)
    assert sharing.own_decoder.isChecked() and not sharing.own_decoder.isHidden()
    sharing.own_decoder.setChecked(False)
    assert sharing.binding("s").own_decoder is False
    own_only = BindingEditor(manifest(["video"]), cameras, PluginBinding("s", "a"))
    qtbot.addWidget(own_only)
    assert own_only.own_decoder.isHidden()
    assert own_only.binding("s").own_decoder is False
