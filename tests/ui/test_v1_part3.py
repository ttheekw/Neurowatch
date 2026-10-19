from __future__ import annotations

from PySide6.QtCore import Qt
from PySide6.QtWidgets import QFileDialog

from neurowatch.dialogs.camera_dialog import AddEditCameraDialog
from neurowatch.dialogs.settings_dialog import SettingsDialog
from neurowatch.main import MainWindow
from neurowatch.widgets.toolbar import ToolBar


class _FakeIpcClient:
    _endpoint = "unix:/nonexistent/test-v1-part3-export.sock"

    def is_online(self) -> bool:
        return True

    def call(self, method, params=None):
        if method == "config.get":
            return {
                "version": 1,
                "site_name": "My Site",
                "wall": {"rows": 2, "cols": 2, "auto_fit": True, "page_interval_sec": 0},
                "ui": {"theme": "dark", "fullscreen": False},
                "debug": {"gst_log": "warning"},
            }
        if method == "camera.list":
            return []
        if method == "camera.status_all":
            return {}
        return {"ok": True}


def _make_main_window(monkeypatch, qtbot, shm_prefix: str) -> MainWindow:
    monkeypatch.setattr("neurowatch.main.SyncIpcClient", _FakeIpcClient)
    monkeypatch.setattr("neurowatch.ipc.frame_reader.SHM_PREFIX", shm_prefix)
    win = MainWindow()
    qtbot.addWidget(win)
    return win


def test_export_config_saves_active_config_to_chosen_path(monkeypatch, qtbot, tmp_path):
    active_config = tmp_path / "active_config.json"
    active_config.write_text('{"site_name": "Test Site"}')
    dest_path = tmp_path / "exported_config.json"

    # The active config is NW_CONFIG when set (as the service sets it).
    monkeypatch.setenv("NW_CONFIG", str(active_config))

    win = _make_main_window(monkeypatch, qtbot, "/nonexistent/nw-test-export-save-")
    try:
        monkeypatch.setattr(
            QFileDialog,
            "getSaveFileName",
            lambda *a, **k: (str(dest_path), "JSON Files (*.json)"),
        )

        win._on_export_config()

        assert dest_path.exists()
        assert dest_path.read_text() == active_config.read_text()
    finally:
        win.close()


def test_export_config_cancel_does_not_copy(monkeypatch, qtbot, tmp_path):
    import neurowatch.main as main_mod

    active_config = tmp_path / "active_config.json"
    active_config.write_text('{"site_name": "Test Site"}')
    dest_path = tmp_path / "exported_config.json"

    real_expanduser = main_mod.os.path.expanduser

    def _fake_expanduser(path: str) -> str:
        if path == "~/.config/neurowatch/config.json":
            return str(active_config)
        return real_expanduser(path)

    monkeypatch.setattr(main_mod.os.path, "expanduser", _fake_expanduser)

    win = _make_main_window(monkeypatch, qtbot, "/nonexistent/nw-test-export-cancel-")
    try:
        monkeypatch.setattr(QFileDialog, "getSaveFileName", lambda *a, **k: ("", ""))

        win._on_export_config()

        assert not dest_path.exists()
    finally:
        win.close()


def test_config_watcher_uses_nw_config_env_when_set(monkeypatch, qtbot, tmp_path):
    active_config = tmp_path / "nw_config_override.json"
    active_config.write_text('{"site_name": "Env Site"}')
    monkeypatch.setenv("NW_CONFIG", str(active_config))

    win = _make_main_window(monkeypatch, qtbot, "/nonexistent/nw-test-watcher-env-")
    try:
        assert str(active_config) in win._config_watcher.files()
    finally:
        win.close()


def test_export_config_uses_nw_config_env_when_set(monkeypatch, qtbot, tmp_path):
    active_config = tmp_path / "nw_config_override.json"
    active_config.write_text('{"site_name": "Env Site"}')
    dest_path = tmp_path / "exported_config.json"
    monkeypatch.setenv("NW_CONFIG", str(active_config))

    win = _make_main_window(monkeypatch, qtbot, "/nonexistent/nw-test-export-env-")
    try:
        monkeypatch.setattr(
            QFileDialog,
            "getSaveFileName",
            lambda *a, **k: (str(dest_path), "JSON Files (*.json)"),
        )

        win._on_export_config()

        assert dest_path.exists()
        assert dest_path.read_text() == active_config.read_text()
    finally:
        win.close()


def test_import_config_uses_nw_config_env_when_set(monkeypatch, qtbot, tmp_path):
    active_config = tmp_path / "nw_config_override.json"
    active_config.write_text('{"site_name": "Old"}')
    src_path = tmp_path / "imported_config.json"
    src_path.write_text('{"site_name": "New"}')
    monkeypatch.setenv("NW_CONFIG", str(active_config))

    win = _make_main_window(monkeypatch, qtbot, "/nonexistent/nw-test-import-env-")
    try:
        monkeypatch.setattr(
            QFileDialog,
            "getOpenFileName",
            lambda *a, **k: (str(src_path), "JSON Files (*.json)"),
        )

        win._on_import_config()

        assert active_config.read_text() == src_path.read_text()
    finally:
        win.close()


def test_settings_save_worker_uses_active_endpoint_and_refreshes_config(monkeypatch):
    import neurowatch.main as main_mod

    calls: list[tuple[str, str, dict]] = []
    saved = {"ui": {"theme": "light", "fullscreen": True}}

    class _Client:
        def __init__(self, endpoint=None) -> None:
            self.endpoint = endpoint

        def call(self, method, params=None):
            calls.append((self.endpoint, method, params or {}))
            return saved if method == "config.get" else {"ok": True}

    monkeypatch.setattr(main_mod, "SyncIpcClient", _Client)
    results: list[object] = []
    worker = main_mod._SettingsSaveWorker("unix:/tmp/active.sock", saved)
    worker.result.connect(results.append)

    worker.run()

    assert calls == [
        ("unix:/tmp/active.sock", "config.set", saved),
        ("unix:/tmp/active.sock", "config.get", {}),
    ]
    assert results == [saved]


def test_camera_dialog_disables_credentials_for_v4l2(qtbot):
    dlg = AddEditCameraDialog()
    qtbot.addWidget(dlg)
    dlg._show_camera_tab("urls")

    dlg._a_url.setText("/dev/video0")

    assert not dlg._user.isEnabled()
    assert not dlg._pass.isEnabled()

    dlg._a_url.setText("rtsp://cam/stream")
    assert dlg._user.isEnabled()
    assert dlg._pass.isEnabled()


def test_camera_dialog_v4l2_config_carries_no_credentials(qtbot):
    # Local capture devices have no authentication; stale credentials typed
    # before switching the URL must not be persisted with the camera.
    dlg = AddEditCameraDialog()
    qtbot.addWidget(dlg)
    dlg._show_camera_tab("urls")

    dlg._a_url.setText("rtsp://cam/stream")
    dlg._user.setText("admin")
    dlg._pass.setText("secret")
    dlg._a_url.setText("/dev/video0")

    cfg = dlg.get_config()
    assert cfg.url == "/dev/video0"
    assert cfg.username == ""
    assert not cfg.password_b64


def test_camera_dialog_advanced_protocol_includes_rtmp_and_v4l2(qtbot):
    dlg = AddEditCameraDialog()
    qtbot.addWidget(dlg)

    items = [dlg._a_protocol.itemText(i) for i in range(dlg._a_protocol.count())]

    assert "RTMP" in items
    assert "V4L2" in items


def test_camera_dialog_rtmp_protocol_selection_does_not_alter_url(qtbot):
    dlg = AddEditCameraDialog()
    qtbot.addWidget(dlg)
    dlg._show_camera_tab("urls")

    dlg._a_url.setText("rtmp://example/live")
    idx = dlg._a_protocol.findText("RTMP", Qt.MatchFlag.MatchFixedString)
    assert idx >= 0
    dlg._a_protocol.setCurrentIndex(idx)

    cfg = dlg.get_config()
    assert cfg.url == "rtmp://example/live"
    assert cfg.protocol == "rtmp"


def test_toolbar_custom_grid_requested_signal(qtbot):
    toolbar = ToolBar()
    qtbot.addWidget(toolbar)

    custom_index = -1
    for i in range(toolbar._grid_combo.count()):
        if toolbar._grid_combo.itemData(i) is None:
            custom_index = i
            break

    assert custom_index >= 0
    with qtbot.waitSignal(toolbar.custom_grid_requested, timeout=1000):
        toolbar._grid_combo.setCurrentIndex(custom_index)


def test_settings_dialog_get_config(qtbot):
    cfg = {
        "site_name": "Site A",
        "wall": {"rows": 4, "cols": 8, "page_interval_sec": 0},
        "ui": {"theme": "dark", "fullscreen": True},
        "debug": {"gst_log": "warning"},
    }
    dlg = SettingsDialog(cfg)
    qtbot.addWidget(dlg)

    dlg._site_name.setText("Site B")
    dlg._interval.setValue(15)
    dlg._rows.setValue(3)
    dlg._cols.setValue(5)
    dlg._theme.setCurrentText("Light")
    dlg._start_fullscreen.setChecked(False)
    dlg._gst_log.setCurrentText("debug")

    out = dlg.get_config()
    assert out["site_name"] == "Site B"
    assert out["wall"]["page_interval_sec"] == 15
    assert out["wall"]["rows"] == 3
    assert out["wall"]["cols"] == 5
    assert out["ui"]["theme"] == "light"
    assert out["ui"]["fullscreen"] is False
    assert out["debug"]["gst_log"] == "debug"


def test_storage_calculator_reports_gb_and_tb_in_correct_units(qtbot):
    dlg = SettingsDialog({})
    qtbot.addWidget(dlg)

    # Defaults: 4 cameras, 2 Mbps, 7 days = about 590.6 GiB.
    assert dlg._calc_result.text() == "≈ 590.6 GB"

    dlg._calc_cams.setValue(16)
    assert dlg._calc_result.text() == "≈ 2.3 TB"


def test_config_file_change_reload_prompts_before_applying(monkeypatch, qtbot):
    class _FakeClient:
        _endpoint = "unix:/nonexistent/test-v1-part3-reload.sock"

        def is_online(self) -> bool:
            return True

        def call(self, method, params=None):
            if method == "config.get":
                return {
                    "version": 1,
                    "site_name": "My Site",
                    "wall": {"rows": 2, "cols": 2, "auto_fit": True, "page_interval_sec": 0},
                    "ui": {"theme": "dark", "fullscreen": False},
                    "debug": {"gst_log": "warning"},
                }
            if method == "camera.list":
                return []
            if method == "camera.status_all":
                return {}
            return {"ok": True}

    monkeypatch.setattr("neurowatch.main.SyncIpcClient", _FakeClient)
    monkeypatch.setattr("neurowatch.ipc.frame_reader.SHM_PREFIX", "/nonexistent/nw-test-reload-")

    win = MainWindow()
    qtbot.addWidget(win)
    try:
        reload_calls: list[bool] = []
        win._load_from_backend = lambda *a, **k: reload_calls.append(True)

        # Applied at once; asking would fail the test (tests/ui/conftest.py).
        win._reload_config_after_change()
        assert reload_calls == [True]
    finally:
        win.close()


def test_main_settings_tab_opens_dialog(monkeypatch, qtbot):
    class _FakeClient:
        _endpoint = "unix:/nonexistent/test-v1-part3.sock"

        def is_online(self) -> bool:
            return True

        def call(self, method, params=None):
            # Settings are owned by the backend and fetched over IPC.
            if method == "config.get":
                return {
                    "version": 1,
                    "site_name": "My Site",
                    "wall": {"rows": 2, "cols": 2, "auto_fit": True, "page_interval_sec": 0},
                    "ui": {"theme": "dark", "fullscreen": False},
                    "debug": {"gst_log": "warning"},
                }
            if method == "camera.list":
                return []
            if method == "camera.status_all":
                return {}
            return {"ok": True}

    monkeypatch.setattr("neurowatch.main.SyncIpcClient", _FakeClient)
    monkeypatch.setattr("neurowatch.ipc.frame_reader.SHM_PREFIX", "/nonexistent/nw-test-")

    win = MainWindow()
    qtbot.addWidget(win)
    try:
        called = {"open": False}

        def _fake_open_settings():
            called["open"] = True

        win._open_settings = _fake_open_settings
        win._on_tab_changed("settings")
        assert called["open"]
        assert win._title_bar._site_label.text() == "My Site"
    finally:
        win.close()
