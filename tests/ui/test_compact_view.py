"""Hide panels: the main window shows only the wall under a slim strip."""

from __future__ import annotations

from collections.abc import Iterator
from typing import ClassVar

import pytest
from PySide6.QtCore import QEvent, Qt
from PySide6.QtGui import QKeyEvent
from PySide6.QtWidgets import QApplication

from neurowatch.main import MainWindow
from neurowatch.models.camera import CameraConfig


class _FakeClient:
    _endpoint = "unix:/nonexistent/test-compact.sock"
    calls: ClassVar[list[tuple[str, object]]] = []

    def __init__(self, endpoint: str | None = None) -> None:
        pass

    def is_online(self) -> bool:
        return True

    def call(self, method, params=None):
        _FakeClient.calls.append((method, params))
        if method == "config.get":
            return {
                "version": 3,
                "site_name": "Test Site",
                "wall": {"rows": 3, "cols": 3, "auto_fit": True, "page_interval_sec": 0},
                "ui": {"theme": "dark", "fullscreen": False},
            }
        if method == "camera.list":
            return []
        if method == "camera.status_all":
            return {}
        return {"ok": True}


@pytest.fixture
def win(monkeypatch, qtbot) -> Iterator[MainWindow]:
    _FakeClient.calls = []
    monkeypatch.setattr("neurowatch.main.SyncIpcClient", _FakeClient)
    monkeypatch.setattr("neurowatch.ipc.frame_reader.SHM_PREFIX", "/nonexistent/nw-test-")
    window = MainWindow()
    qtbot.addWidget(window)
    yield window
    window.close()
    window.deleteLater()
    QApplication.sendPostedEvents(None, QEvent.Type.DeferredDelete)


def _load(win: MainWindow, count: int) -> None:
    win._cameras = [
        CameraConfig(id=f"cam-{i}", name=f"Cam {i}", url="rtsp://x") for i in range(count)
    ]
    win._refresh_all()


def _panels(win: MainWindow) -> dict[str, bool]:
    """Which parts of the window are shown (the window itself is not)."""
    return {
        "title": win._title_bar.isVisibleTo(win),
        "toolbar": win._toolbar.isVisibleTo(win),
        "cameras": win._camera_panel.isVisibleTo(win),
        "dots": win._page_strip_holder.isVisibleTo(win),
        "status": win._status_bar.isVisibleTo(win),
        "strip": win._compact_bar.isVisibleTo(win),
    }


FULL = {"title": True, "toolbar": True, "cameras": True, "dots": True, "status": True}
# The status readings stay: they move into the strip.
COMPACT = {"title": False, "toolbar": False, "cameras": False, "dots": False, "status": True}


def _ctrl_shift_h(win: MainWindow) -> None:
    mods = Qt.KeyboardModifier.ControlModifier | Qt.KeyboardModifier.ShiftModifier
    win.keyPressEvent(QKeyEvent(QEvent.Type.KeyPress, Qt.Key.Key_H, mods))


def test_hide_panels_leaves_the_wall_under_a_slim_strip(win) -> None:
    _load(win, 18)
    assert _panels(win) == {**FULL, "strip": False}

    win._title_bar._hide_panels_btn.click()
    assert _panels(win) == {**COMPACT, "strip": True}
    bar = win._compact_bar
    assert bar._full_title == "NeuroWatch — Test Site · 18 cameras"
    assert bar.indicator.text() == "Page 1 / 2"
    assert bar.new_window_button.isEnabled()

    bar.show_panels_button.click()
    assert _panels(win) == {**FULL, "strip": False}


def test_the_status_readings_move_into_the_strip_and_back(win) -> None:
    status = win._status_bar
    win._set_compact(True)
    assert win._compact_bar.isAncestorOf(status)
    # The version stays at the bottom only.
    assert not status._version_lbl.isVisibleTo(status)
    assert status._net_lbl.isVisibleTo(status) and status._backend_lbl.isVisibleTo(status)

    win._set_compact(False)
    assert not win._compact_bar.isAncestorOf(status)
    layout = win._root_layout
    assert layout.indexOf(status) == layout.count() - 1  # at the bottom again
    assert status._version_lbl.isVisibleTo(status)


def test_recording_disk_updates_from_settings_in_the_fullscreen_overlay(
    win, qtbot, tmp_path, monkeypatch
) -> None:
    from types import SimpleNamespace

    from neurowatch.widgets import status_bar_widget

    calls = []

    def usage(path):
        calls.append(path)
        return SimpleNamespace(free=321 * 1024**3)

    # The initial load uses the default location. Let its reader finish first.
    qtbot.waitUntil(lambda: not win._status_bar._disk_pending)
    assert win._status_bar._storage_path == "~/Videos/neurowatch"
    monkeypatch.setattr(status_bar_widget.shutil, "disk_usage", usage)
    monkeypatch.setattr(win, "isFullScreen", lambda: True)
    win._set_compact(True)
    win._hide_fullscreen_bars()
    win._on_settings_saved({"recording": {"storage_path": str(tmp_path / "new-recordings")}})
    qtbot.waitUntil(lambda: not win._status_bar._disk_pending)
    assert calls == [tmp_path]
    assert win._status_overlay.isAncestorOf(win._status_bar)
    assert win._status_bar._disk_lbl.text() == "Disk: 321 GB Free"
    assert win._status_bar._disk_lbl.toolTip().endswith(str(tmp_path / "new-recordings"))


def test_the_strip_pages_through_the_wall(win, qtbot) -> None:
    _load(win, 18)
    win._set_compact(True)
    win._compact_bar.next.click()
    qtbot.waitUntil(lambda: win._wall.page == 1, timeout=2000)
    qtbot.waitUntil(lambda: win._compact_bar.indicator.text() == "Page 2 / 2", timeout=2000)
    win._compact_bar.previous.click()
    qtbot.waitUntil(lambda: win._wall.page == 0, timeout=2000)


def test_one_page_shows_no_paging_and_no_new_window(win) -> None:
    _load(win, 4)
    win._set_compact(True)
    assert not win._compact_bar._paging.isVisibleTo(win._compact_bar)
    assert not win._compact_bar.new_window_button.isEnabled()
    # The page dots stay hidden while compact, whatever the page count does.
    _load(win, 18)
    assert not win._page_strip_holder.isVisibleTo(win)
    assert win._compact_bar._paging.isVisibleTo(win._compact_bar)


def test_ctrl_shift_h_toggles_and_compact_view_shows_live(win) -> None:
    _load(win, 4)
    win._title_bar.show_tab("playback")
    assert win._content_stack.currentIndex() == 1

    _ctrl_shift_h(win)
    assert win._compact
    assert win._content_stack.currentIndex() == 0  # the strip has no tabs
    _ctrl_shift_h(win)
    assert not win._compact
    assert _panels(win) == {**FULL, "strip": False}


def test_fullscreen_compact_view_reveals_the_strip_at_the_top_edge(win) -> None:
    _load(win, 4)
    win._set_compact(True)
    win._enter_fullscreen_mode()
    assert win._compact_bar.isVisibleTo(win)
    assert not win._title_bar.isVisibleTo(win) and not win._toolbar.isVisibleTo(win)

    win._hide_fullscreen_bars()
    if win.isFullScreen():  # some offscreen platforms ignore fullscreen
        assert not win._compact_bar.isVisibleTo(win)
    win._show_top_bars()
    assert win._compact_bar.isVisibleTo(win)
    assert not win._title_bar.isVisibleTo(win)

    win.showNormal()
    win._exit_fullscreen_mode()
    assert win._compact_bar.isVisibleTo(win)
    assert not win._title_bar.isVisibleTo(win)


def test_fullscreen_status_shows_over_the_wall_while_the_strip_hides(win, monkeypatch) -> None:
    from neurowatch.widgets.status_overlay import overlay_style

    _load(win, 4)
    monkeypatch.setattr(win, "isFullScreen", lambda: True)
    status, overlay, strip = win._status_bar, win._status_overlay, win._compact_bar
    win._set_compact(True)
    win._show_top_bars()
    assert strip.isAncestorOf(status) and not overlay.isVisibleTo(win)

    win._hide_fullscreen_bars()
    assert not strip.isVisibleTo(win)
    assert overlay.isAncestorOf(status) and status.isVisibleTo(win)
    assert overlay.isVisibleTo(win)
    assert overlay.testAttribute(Qt.WidgetAttribute.WA_TransparentForMouseEvents)
    # Laid out as at the bottom of the window (version, readings left and
    # right), on the overlay's background at 50% rather than its own.
    assert status._version_lbl.isVisibleTo(status) and status._spacer.isVisibleTo(status)
    assert not status.testAttribute(Qt.WidgetAttribute.WA_StyledBackground)
    assert overlay.opacity == 50
    assert overlay.styleSheet() == overlay_style(50)
    assert "rgba(10, 10, 20, 128)" in overlay_style(50), "the bar's colour, half opaque"
    wall = win._wall.geometry()
    assert overlay.geometry().topLeft() == wall.topLeft()
    assert overlay.width() == wall.width()

    win._show_top_bars()  # the mouse at the top edge: the strip takes them back
    assert strip.isAncestorOf(status) and not overlay.isVisibleTo(win)
    assert not status._version_lbl.isVisibleTo(status), "the strip's form again"

    win._hide_fullscreen_bars()
    win._set_compact(False)  # Show panels: back to the bottom, overlay gone
    assert not overlay.isVisibleTo(win) and not overlay.isAncestorOf(status)
    assert win._root_layout.indexOf(status) == win._root_layout.count() - 1


def test_fullscreen_status_can_be_turned_off(win, monkeypatch) -> None:
    from neurowatch.dialogs.settings_dialog import SettingsDialog

    _load(win, 4)
    monkeypatch.setattr(win, "isFullScreen", lambda: True)
    win._config.setdefault("ui", {})["fullscreen_status"] = False
    win._set_compact(True)
    win._hide_fullscreen_bars()
    assert not win._status_overlay.isVisibleTo(win)
    assert not win._status_bar.isVisibleTo(win), "hidden with the strip, as before"

    win._config["ui"]["fullscreen_status"] = True
    win._apply_settings()
    assert win._status_overlay.isAncestorOf(win._status_bar)

    dialog = SettingsDialog({"ui": {"fullscreen_status": False}})
    assert not dialog._fullscreen_status.isChecked()
    dialog._fullscreen_status.setChecked(True)
    assert dialog.get_config()["ui"]["fullscreen_status"] is True
    assert SettingsDialog({})._fullscreen_status.isChecked(), "on by default"


def test_fullscreen_status_opacity_is_chosen_with_a_slider(win, monkeypatch) -> None:
    from neurowatch.dialogs.settings_dialog import SettingsDialog
    from neurowatch.widgets.status_overlay import overlay_style

    dialog = SettingsDialog({})
    slider = dialog._status_opacity
    assert (slider.minimum(), slider.maximum(), slider.value()) == (0, 100, 50)
    assert dialog._status_opacity_value.text() == "50%"
    slider.setValue(30)
    assert dialog._status_opacity_value.text() == "30%"
    assert dialog.get_config()["ui"]["fullscreen_status_opacity"] == 30
    dialog._fullscreen_status.setChecked(False)
    assert not slider.isEnabled(), "no opacity to choose while the overlay is off"
    assert SettingsDialog({"ui": {"fullscreen_status_opacity": 80}})._status_opacity.value() == 80

    _load(win, 4)
    monkeypatch.setattr(win, "isFullScreen", lambda: True)
    win._config.setdefault("ui", {})["fullscreen_status_opacity"] = 30
    win._set_compact(True)
    win._hide_fullscreen_bars()
    assert win._status_overlay.opacity == 30
    assert win._status_overlay.styleSheet() == overlay_style(30)
    assert "rgba(10, 10, 20, 76)" in overlay_style(30)
    win._config["ui"]["fullscreen_status_opacity"] = 100
    win._apply_settings()  # Settings saved: the overlay follows at once
    assert win._status_overlay.opacity == 100
    assert "rgba(10, 10, 20, 255)" in win._status_overlay.styleSheet()


def test_changing_the_grid_saves_off_the_ui_thread_as_our_own_change(win, qtbot) -> None:
    """The config watcher used to ask "Configuration Changed — reload?" after a
    grid change, because that save was not marked as this window's own."""
    import time

    _load(win, 4)
    _FakeClient.calls = []
    win._on_grid_changed(2, 2)
    assert win._ignore_config_changes_until > time.monotonic()
    qtbot.waitUntil(lambda: ("config.set", {"wall": win._config["wall"]}) in _FakeClient.calls)
    assert win._config["wall"]["rows"] == 2 and win._config["wall"]["cols"] == 2
