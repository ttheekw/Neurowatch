"""Modernized dialogs, confirmations and menus use the centralized theme."""

from __future__ import annotations

from types import SimpleNamespace

from PySide6.QtCore import QDate, QDateTime
from PySide6.QtWidgets import (
    QCalendarWidget,
    QDialogButtonBox,
    QLabel,
    QMessageBox,
    QPushButton,
    QVBoxLayout,
    QWidget,
)

from neurowatch import theme
from neurowatch.models.camera import CameraConfig


def _assert_themed(dlg: QWidget) -> None:
    assert dlg.property("nwThemed") is True
    assert dlg.styleSheet() == theme.dialog_stylesheet()


def _no_inline_colors(root: QWidget) -> list[str]:
    offenders = []
    for child in root.findChildren(QWidget):
        if "#" in child.styleSheet():
            offenders.append(f"{type(child).__name__}:{child.objectName()}")
    return offenders


def _box_button(dlg: QWidget, role: QDialogButtonBox.StandardButton) -> QPushButton:
    for box in dlg.findChildren(QDialogButtonBox):
        btn = box.button(role)
        if btn is not None:
            return btn
    raise AssertionError(f"no {role} button")


# ── About ────────────────────────────────────────────────────────────────


def test_about_dialog_is_themed_with_hierarchy(qtbot):
    from neurowatch.dialogs.about_dialog import AboutDialog

    dlg = AboutDialog()
    qtbot.addWidget(dlg)
    _assert_themed(dlg)
    assert _no_inline_colors(dlg) == []
    titles = [lbl for lbl in dlg.findChildren(QLabel) if lbl.objectName() == "dialogTitle"]
    assert len(titles) == 1
    close = _box_button(dlg, QDialogButtonBox.StandardButton.Close)
    assert close.property("variant") == "primary"


def test_about_dialog_shows_the_licence_and_no_warranty_notice(qtbot):
    from neurowatch.dialogs.about_dialog import NO_WARRANTY, AboutDialog

    dlg = AboutDialog()
    qtbot.addWidget(dlg)
    texts = [label.text() for label in dlg.findChildren(QLabel)]
    assert NO_WARRANTY in texts
    assert "WITHOUT ANY WARRANTY" in NO_WARRANTY and "AGPL" in NO_WARRANTY


def test_notice_carries_the_full_disclaimer():
    from pathlib import Path

    notice = (Path(__file__).resolve().parents[2] / "NOTICE").read_text()
    for heading in (
        "Disclaimer and limitation of liability",
        "Not a guaranteed security",
        "No liability",
        "Your installation, your responsibility",
    ):
        assert heading in notice


# ── Custom grid ──────────────────────────────────────────────────────────


def test_custom_grid_dialog_themed_and_previews_cell_count(qtbot):
    from neurowatch.dialogs.custom_grid_dialog import CustomGridDialog

    dlg = CustomGridDialog(3, 4)
    qtbot.addWidget(dlg)
    _assert_themed(dlg)
    assert _no_inline_colors(dlg) == []
    assert (dlg.rows, dlg.cols) == (3, 4)
    assert "12" in dlg._summary.text()
    dlg._rows_spin.setValue(5)
    assert "20" in dlg._summary.text()
    assert dlg.rows == 5
    assert _box_button(dlg, QDialogButtonBox.StandardButton.Ok).property("variant") == "primary"
    assert (
        _box_button(dlg, QDialogButtonBox.StandardButton.Cancel).property("variant") == "secondary"
    )
    assert dlg._rows_spin.accessibleName() == "Grid rows"
    assert dlg._cols_spin.accessibleName() == "Grid columns"


# ── Settings ─────────────────────────────────────────────────────────────


def test_settings_dialog_themed_preserves_config(qtbot):
    from neurowatch.dialogs.settings_dialog import SettingsDialog

    cfg = {
        "site_name": "Site A",
        "wall": {"rows": 4, "cols": 8, "page_interval_sec": 0},
        "recording": {"storage_path": "/data", "segment_minutes": 5},
        "ui": {"theme": "dark"},
        "debug": {"gst_log": "info"},
    }
    dlg = SettingsDialog(cfg)
    qtbot.addWidget(dlg)
    _assert_themed(dlg)
    assert _no_inline_colors(dlg) == []
    assert _box_button(dlg, QDialogButtonBox.StandardButton.Ok).property("variant") == "primary"
    assert dlg._browse_btn.accessibleName() == "Browse recording storage path"

    out = dlg.get_config()
    assert out["site_name"] == "Site A"
    assert out["wall"] == {"rows": 4, "cols": 8, "page_interval_sec": 0}
    assert out["recording"]["storage_path"] == "/data"
    assert out["recording"]["segment_minutes"] == 5
    assert out["debug"]["gst_log"] == "info"


# ── Camera ───────────────────────────────────────────────────────────────


def test_camera_dialog_themed_without_inline_colors(qtbot):
    from neurowatch.dialogs.camera_dialog import AddEditCameraDialog

    dlg = AddEditCameraDialog()
    qtbot.addWidget(dlg)
    _assert_themed(dlg)
    assert _no_inline_colors(dlg) == []
    assert _box_button(dlg, QDialogButtonBox.StandardButton.Ok).property("variant") == "primary"
    assert (
        _box_button(dlg, QDialogButtonBox.StandardButton.Cancel).property("variant") == "secondary"
    )
    assert dlg._test_btn.property("variant") == "secondary"
    assert dlg._discover_btn.property("variant") == "secondary"
    assert dlg._rec_stop_btn.property("variant") == "danger"
    assert dlg._preview.objectName() == "previewSurface"


def test_camera_dialog_status_uses_tones(qtbot):
    from neurowatch.dialogs.camera_dialog import AddEditCameraDialog

    dlg = AddEditCameraDialog()
    qtbot.addWidget(dlg)
    dlg._tabs.setCurrentWidget(dlg._advanced_tab)
    dlg._on_test()  # no URL
    assert dlg._status_label.property("tone") == "warning"
    dlg._on_discover()  # no host
    assert dlg._discover_status.property("tone") == "warning"
    dlg._on_test_result({"ok": False, "error": "boom"})
    assert dlg._status_label.property("tone") == "error"
    assert dlg._status_label.styleSheet() == ""


def test_camera_dialog_edit_mode_preserves_config(qtbot):
    from neurowatch.dialogs.camera_dialog import AddEditCameraDialog

    cam = CameraConfig.from_dict(
        {"id": "c1", "name": "Door", "url": "rtsp://10.0.0.5:554/s1", "transport": "udp"}
    )
    dlg = AddEditCameraDialog(camera=cam)
    qtbot.addWidget(dlg)
    out = dlg.get_config()
    assert out.id == "c1"
    assert out.name == "Door"
    assert out.url == "rtsp://10.0.0.5:554/s1"
    assert out.transport == "udp"


# ── Playback calendar ────────────────────────────────────────────────────


def test_playback_calendar_dialog_themed(qtbot):
    from neurowatch.widgets.playback_view import _CalendarDialog

    initial = QDateTime(QDate(2026, 3, 14), QDateTime.currentDateTime().time())
    dlg = _CalendarDialog(initial)
    qtbot.addWidget(dlg)
    _assert_themed(dlg)
    assert _no_inline_colors(dlg) == []
    cal = dlg.findChild(QCalendarWidget)
    assert cal.objectName() == "playbackCalendar"
    assert cal.isGridVisible() is False
    assert cal.verticalHeaderFormat() == QCalendarWidget.VerticalHeaderFormat.NoVerticalHeader
    assert dlg.selected_date().date() == QDate(2026, 3, 14)
    assert _box_button(dlg, QDialogButtonBox.StandardButton.Ok).property("variant") == "primary"


def test_playback_datetime_inputs_use_themed_calendar_popup():
    from neurowatch.widgets.playback_view import PlaybackView

    qss = PlaybackView._dt_style(None)  # type: ignore[arg-type]
    assert qss == theme.datetime_input_stylesheet()
    assert "QDateTimeEdit:focus" in qss
    assert "QCalendarWidget" in qss


# ── Confirmations ────────────────────────────────────────────────────────


def test_destructive_confirmation_defaults_to_cancel(qtbot):
    from neurowatch.dialogs.confirm import build_confirmation

    box, confirm_btn = build_confirmation(
        None, "Remove Camera", "Remove 'Door'?", confirm_text="Remove", destructive=True
    )
    qtbot.addWidget(box)
    _assert_themed(box)
    assert confirm_btn.text() == "Remove"
    assert confirm_btn.property("variant") == "danger"
    assert box.default_button() is not confirm_btn
    assert box.default_button().property("variant") == "secondary"
    assert box.kind == "warning"


def test_non_destructive_confirmation_defaults_to_primary(qtbot):
    from neurowatch.dialogs.confirm import build_confirmation

    box, confirm_btn = build_confirmation(None, "Apply", "Apply?", confirm_text="Apply")
    qtbot.addWidget(box)
    assert confirm_btn.property("variant") == "primary"
    assert box.default_button() is confirm_btn


def test_remove_camera_uses_themed_confirmation(monkeypatch):
    import neurowatch.main as main_mod

    asked: list[tuple] = []

    def _confirm(parent, title, text, **kwargs):
        asked.append((title, text, kwargs))
        return False

    monkeypatch.setattr(main_mod, "confirm_action", _confirm)
    calls: list[str] = []
    fake = SimpleNamespace(
        _cameras=[CameraConfig.from_dict({"id": "c1", "name": "Door", "url": "rtsp://x"})],
        _ipc=SimpleNamespace(call=lambda m, p=None: calls.append(m)),
    )
    main_mod.MainWindow._on_remove_camera(fake, "c1")  # type: ignore[arg-type]
    assert asked and asked[0][0] == "Remove Camera"
    assert asked[0][2] == {"confirm_text": "Remove", "destructive": True}
    assert calls == []


# ── Menus ────────────────────────────────────────────────────────────────


def test_title_bar_file_menu_is_themed_and_accessible(qtbot):
    from neurowatch.widgets.title_bar import TitleBar

    bar = TitleBar()
    qtbot.addWidget(bar)
    picked: list[str] = []
    bar.add_menu_action("Example…", lambda: picked.append("example"))
    menu = bar._build_file_menu()
    assert menu.styleSheet() == theme.menu_stylesheet()
    labels = [a.text() for a in menu.actions() if not a.isSeparator()]
    assert labels == [
        "Export Config…",
        "Export Video…",
        "Import Config…",
        "Settings",
        "Diagnostics",
        "Example…",
        "About Neurowatch",
    ]
    [example] = [a for a in menu.actions() if a.text() == "Example…"]
    example.trigger()
    assert picked == ["example"], "an extension's entry runs its callback"
    assert bar._menu_btn.accessibleName() == "Application menu"
    with qtbot.waitSignal(bar.about_requested, timeout=500):
        menu.actions()[-1].trigger()


def test_title_bar_minimize_leaves_fullscreen_before_minimizing(qtbot):
    from neurowatch.widgets.title_bar import TitleBar

    window = QWidget()
    layout = QVBoxLayout(window)
    bar = TitleBar(window)
    layout.addWidget(bar)
    fullscreen_exits: list[bool] = []
    window._exit_fullscreen_mode = lambda: fullscreen_exits.append(True)  # type: ignore[attr-defined]
    qtbot.addWidget(window)

    window.showFullScreen()
    assert window.isFullScreen()
    bar._minimize()

    assert window.isMinimized()
    assert not window.isFullScreen()
    assert fullscreen_exits == [True]


def test_camera_panel_context_menu_is_themed(qtbot):
    from neurowatch.widgets.camera_panel import CameraPanel

    panel = CameraPanel()
    qtbot.addWidget(panel)
    cam = CameraConfig.from_dict({"id": "c1", "name": "Door", "url": "rtsp://x"})
    menu = panel._build_context_menu(cam)
    assert menu.styleSheet() == theme.menu_stylesheet()
    labels = [a.text() for a in menu.actions() if not a.isSeparator()]
    assert labels == ["Edit", "Remove", "Open Full-Screen", "Copy URL"]
    remove = next(a for a in menu.actions() if a.text() == "Remove")
    with qtbot.waitSignal(panel.remove_requested, timeout=500) as blocker:
        remove.trigger()
    assert blocker.args == ["c1"]


def test_cell_context_menu_is_themed_without_changing_actions(qtbot):
    from PySide6.QtCore import QPoint, QTimer
    from PySide6.QtWidgets import QApplication, QMenu

    from neurowatch.widgets.cell_widget import CellWidget

    cell = CellWidget()
    qtbot.addWidget(cell)
    cell._camera = CameraConfig.from_dict({"id": "c1", "name": "Door", "url": "rtsp://x"})
    shown: list[QMenu] = []

    def _capture_and_close() -> None:
        popup = QApplication.activePopupWidget()
        if isinstance(popup, QMenu):
            shown.append(popup)
            popup.close()

    QTimer.singleShot(50, _capture_and_close)
    event = SimpleNamespace(globalPos=lambda: QPoint(0, 0), accept=lambda: None)
    cell.contextMenuEvent(event)
    assert len(shown) == 1
    menu = shown[0]
    assert menu.styleSheet() == theme.menu_stylesheet()
    labels = [a.text() for a in menu.actions() if not a.isSeparator()]
    assert labels[:2] == ["Edit Camera", "Remove Camera"]
    assert labels[-1] == "Aspect Ratio"
