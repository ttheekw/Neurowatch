"""Fit, fonts and icons that matter most on Windows."""

from __future__ import annotations

import re
from pathlib import Path

import pytest
from PySide6.QtCore import QSize
from PySide6.QtWidgets import QMainWindow

ROOT = Path(__file__).resolve().parents[2]


@pytest.fixture
def parent(qtbot) -> QMainWindow:
    window = QMainWindow()
    qtbot.addWidget(window)
    window.show()
    return window


@pytest.mark.parametrize(("screen_height", "columns"), [(2000, 1), (600, 2)])
def test_add_camera_goes_side_by_side_when_stacking_would_not_fit(
    qtbot, monkeypatch, parent, screen_height: int, columns: int
) -> None:
    from neurowatch.dialogs import camera_dialog
    from neurowatch.widgets.window_chrome import chrome_size_hint

    monkeypatch.setattr(
        camera_dialog.AddEditCameraDialog, "_available_height", lambda self: screen_height
    )
    dialog = camera_dialog.AddEditCameraDialog(parent=parent, ipc_endpoint=None)
    qtbot.addWidget(dialog)
    dialog.fit_to_screen()
    assert dialog.property("columns") == columns
    if columns == 2:
        wide = chrome_size_hint(dialog)
        dialog._set_direction(camera_dialog.QBoxLayout.Direction.TopToBottom)
        assert wide.height() < chrome_size_hint(dialog).height(), "side by side is shorter"


def test_no_font_family_that_windows_lacks() -> None:
    # "Sans", "Sans Serif" and "Mono" are Linux names: Windows substitutes a
    # poorer font (theme.ui_font / theme.mono_font use the system's).
    uses = [
        f"{path.relative_to(ROOT)}:{number}"
        for path in (ROOT / "ui" / "neurowatch").rglob("*.py")
        for number, line in enumerate(path.read_text().splitlines(), 1)
        if re.search(r'QFont\("|font-family:\s*(monospace|sans)', line)
    ]
    assert uses == []


def test_ui_and_mono_fonts_are_the_systems(qtbot) -> None:
    from PySide6.QtGui import QFont, QFontDatabase
    from PySide6.QtWidgets import QApplication

    from neurowatch.theme import mono_font, ui_font

    font = ui_font(9, QFont.Weight.Bold)
    assert font.family() == QApplication.font().family()
    assert font.pointSizeF() == 9 and font.weight() == QFont.Weight.Bold
    fixed = QFontDatabase.systemFont(QFontDatabase.SystemFont.FixedFont).family()
    assert mono_font(8).family() == fixed


def test_the_app_and_tray_icons_are_the_logo_without_plugins(qtbot) -> None:
    from neurowatch import tray
    from neurowatch.theme import app_icon

    icon = app_icon()
    assert not icon.isNull()
    assert QSize(16, 16) in icon.availableSizes() and QSize(256, 256) in icon.availableSizes()
    assert not tray.tray_icon(tray.RUNNING).isNull()


def test_native_corners_are_only_asked_of_windows(qtbot) -> None:
    from PySide6.QtWidgets import QDialog

    from neurowatch.widgets.window_chrome import round_native_corners

    dialog = QDialog()
    qtbot.addWidget(dialog)
    assert round_native_corners(dialog) is False  # not Windows: nothing to do
