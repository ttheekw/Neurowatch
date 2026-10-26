"""Settings pages (a sidebar), with Startup and Cloud."""

from __future__ import annotations

from PySide6.QtWidgets import QLabel, QWidget

from neurowatch.dialogs import settings_dialog
from neurowatch.dialogs.settings_dialog import TABS, SettingsDialog
from neurowatch.service.startup import StartupPrefs


def _dialog(qtbot, monkeypatch, config: dict | None = None, **kwargs) -> SettingsDialog:
    monkeypatch.setattr(SettingsDialog, "_scan_drives", lambda self: None)
    monkeypatch.setattr(SettingsDialog, "_read_archive_status", lambda self: None)
    dlg = SettingsDialog(config or {}, **kwargs)
    qtbot.addWidget(dlg)
    return dlg


def test_settings_are_split_into_pages(qtbot, monkeypatch) -> None:
    dlg = _dialog(qtbot, monkeypatch)
    titles = [dlg._nav.item(i).text() for i in range(dlg._nav.count())]
    assert titles == [
        "General",
        "Video Wall",
        "Recording",
        "Archive",
        "Startup",
        "Cloud",
        "Debug",
    ]
    assert list(TABS.values()) == titles
    # Each group sits on its own page.
    page = dlg._stack.widget
    assert page(0).isAncestorOf(dlg._site_name)
    assert page(1).isAncestorOf(dlg._fps_wall)
    assert page(2).isAncestorOf(dlg._rec_storage) and page(2).isAncestorOf(dlg._calc_cams)
    assert page(3).isAncestorOf(dlg._archive_path)
    assert page(4).isAncestorOf(dlg._start_fullscreen)
    assert page(6).isAncestorOf(dlg._gst_log)


def test_startup_choices_round_trip(qtbot, monkeypatch) -> None:
    dlg = _dialog(
        qtbot,
        monkeypatch,
        {"ui": {"fullscreen": False}},
        startup=StartupPrefs(at_login=True, open_window=True, at_boot=False),
    )
    assert dlg._at_login.isChecked() and dlg._open_window.isChecked()
    assert not dlg._start_fullscreen.isChecked()

    dlg._at_boot.setChecked(True)
    dlg._open_tray.setChecked(True)
    dlg._start_fullscreen.setChecked(True)
    assert dlg.startup_prefs() == StartupPrefs(at_login=True, open_window=False, at_boot=True)
    assert dlg.get_config()["ui"]["fullscreen"] is True

    dlg._at_login.setChecked(False)
    assert not dlg._at_boot.isEnabled(), "from boot needs starting at sign-in"
    assert dlg.startup_prefs() == StartupPrefs(at_login=False, open_window=False, at_boot=False)


def test_startup_that_cannot_be_changed_here_says_why(qtbot, monkeypatch) -> None:
    dlg = _dialog(qtbot, monkeypatch, startup_note="Running from source: …")
    assert not dlg._at_login.isEnabled()
    assert dlg.startup_prefs() is None, "nothing to apply"
    notes = [label.text() for label in dlg._stack.widget(4).findChildren(QLabel)]
    assert "Running from source: …" in notes


def test_the_cloud_tab_opens_directly_and_links_there(qtbot, monkeypatch) -> None:
    """The status bar's cloud light opens Settings at Cloud; the link panel is
    made only when that tab shows (it asks the recorder for cloud.status)."""
    made: list[str | None] = []

    class FakePanel(QWidget):
        def __init__(self, parent=None, *, endpoint=None, rpc=None) -> None:
            super().__init__(parent)
            made.append(endpoint)

    import neurowatch.cloud.link_panel as link_panel

    monkeypatch.setattr(link_panel, "CloudLinkPanel", FakePanel)
    dlg = _dialog(qtbot, monkeypatch, ipc_endpoint="unix:/tmp/nw.sock")
    assert made == [], "not made until the tab shows"
    dlg._nav.setCurrentRow(list(TABS).index("cloud"))
    assert made == ["unix:/tmp/nw.sock"]
    dlg._nav.setCurrentRow(0)
    dlg._nav.setCurrentRow(list(TABS).index("cloud"))
    assert made == ["unix:/tmp/nw.sock"], "made once"

    made.clear()
    direct = _dialog(qtbot, monkeypatch, ipc_endpoint="unix:/tmp/nw.sock", tab="cloud")
    assert direct.current_page() == "cloud" and made == ["unix:/tmp/nw.sock"]

    offline = _dialog(qtbot, monkeypatch, tab="cloud")
    texts = [label.text() for label in offline._stack.widget(5).findChildren(QLabel)]
    assert any("Connect to the recorder" in text for text in texts)
    assert settings_dialog.TABS["cloud"] == "Cloud"


def test_the_settings_window_opens_wide_and_resizes(qtbot, monkeypatch) -> None:
    from neurowatch.widgets import window_chrome
    dlg = _dialog(qtbot, monkeypatch)
    dlg.show()
    qtbot.waitExposed(dlg)
    screen = dlg.screen().availableGeometry()
    # 72% of the screen's width (under GNOME's 75% auto-maximize), at least 900 px
    # but never wider than a small screen.
    wanted = max(min(900, int(screen.width() * 0.95)), int(screen.width() * 0.72))
    assert dlg.width() >= wanted
    assert dlg.width() <= screen.width() * 0.75 or dlg.width() <= 900
    assert dlg.findChild(window_chrome.EdgeResizer) is not None, "drag its edges"


def test_extended_debug_logging_is_a_setting(qtbot, monkeypatch, tmp_path) -> None:
    log_path = tmp_path / "state" / "logs" / "backend-debug.log"
    dlg = _dialog(
        qtbot,
        monkeypatch,
        {"debug": {"gst_log": "warn", "extended": False, "log_path": str(log_path)}},
    )
    assert dlg._stack.widget(6).isAncestorOf(dlg._extended_debug)
    assert not dlg._extended_debug.isChecked()
    assert not dlg._gst_log.isEnabled(), "GStreamer's level matters only in the debug log"
    assert dlg._gst_log.currentText() == "warning"

    dlg._extended_debug.setChecked(True)
    assert dlg._gst_log.isEnabled()
    dlg._gst_log.setCurrentText("info")
    debug = dlg.get_config()["debug"]
    assert debug == {"gst_log": "info", "extended": True}, "the log's path is not a setting"

    # Before the first debug log, the folder offered is the nearest one there.
    assert dlg.log_folder() == tmp_path
    log_path.parent.mkdir(parents=True)
    assert dlg.log_folder() == log_path.parent
    opened: list[str] = []
    monkeypatch.setattr(
        settings_dialog.QDesktopServices, "openUrl", lambda url: opened.append(url.toLocalFile())
    )
    dlg._open_log_folder()
    assert opened == [str(log_path.parent)]


def test_an_older_recorder_without_a_debug_log_path_shows_no_folder(qtbot, monkeypatch) -> None:
    dlg = _dialog(qtbot, monkeypatch, {"debug": {"gst_log": "debug", "extended": True}})
    assert dlg._extended_debug.isChecked() and dlg._gst_log.isEnabled()
    assert dlg.log_folder() is None
    labels = [label.text() for label in dlg._stack.widget(6).findChildren(QLabel)]
    assert "Debug Log:" not in labels
