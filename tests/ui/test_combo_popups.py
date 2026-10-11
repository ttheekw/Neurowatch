"""Every drop-down opens a plain list that shows its items.

Qt's default "popup" combo mode lines the current item up with the field and
sizes the list from window positions, which Wayland does not report: the
list shrank to a single row behind scroll arrows (Add Camera → Search
Network's network list). The themes switch every combo to the list mode.
"""

from __future__ import annotations

import pytest
from PySide6.QtWidgets import QApplication, QComboBox, QStyle, QStyleOptionComboBox, QWidget

from neurowatch.theme import apply_app_theme


@pytest.fixture
def themed_app(qapp: QApplication):
    previous = qapp.styleSheet()
    apply_app_theme(qapp, "dark")
    yield qapp
    qapp.setStyleSheet(previous)


def _camera_dialog() -> QWidget:
    from neurowatch.dialogs.camera_dialog import AddEditCameraDialog

    dialog = AddEditCameraDialog(ipc_endpoint="unix:/nonexistent/combo-test.sock")
    # As after the interface lookup: one more network above "Another range…".
    dialog._net_combo.insertItem(1, "Ethernet (192.168.1.0/24)", {"scope": "current"})
    return dialog


def _settings_dialog() -> QWidget:
    from neurowatch.dialogs.settings_dialog import SettingsDialog

    return SettingsDialog({})


def _toolbar() -> QWidget:
    from neurowatch.widgets.toolbar import ToolBar

    return ToolBar()


def _camera_panel() -> QWidget:
    from neurowatch.widgets.camera_panel import CameraPanel

    return CameraPanel()


@pytest.mark.parametrize("build", [_camera_dialog, _settings_dialog, _toolbar, _camera_panel])
def test_drop_downs_open_a_list_showing_their_items(qtbot, themed_app, build) -> None:
    widget = build()
    qtbot.addWidget(widget)
    widget.show()
    QApplication.processEvents()
    combos = [c for c in widget.findChildren(QComboBox) if c.count() > 0]
    assert combos
    for combo in combos:
        option = QStyleOptionComboBox()
        combo.initStyleOption(option)
        name = combo.accessibleName() or combo.currentText()
        assert not combo.style().styleHint(QStyle.StyleHint.SH_ComboBox_Popup, option, combo), name

        combo.showPopup()
        QApplication.processEvents()
        view = combo.view()
        shown = min(combo.count(), combo.maxVisibleItems())
        rows = sum(view.sizeHintForRow(i) for i in range(shown))
        assert view.viewport().height() >= rows, f"{name}: list cut to fewer than {shown} rows"
        assert view.sizeHintForRow(0) >= 20, f"{name}: rows too cramped"
        combo.hidePopup()
