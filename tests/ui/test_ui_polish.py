"""Rendering regressions in the desktop chrome: menus, title bar and toolbar.

These grab real pixels offscreen, so they check what a user sees rather than
style sheet text.
"""

from __future__ import annotations

from PySide6.QtCore import QRect, Qt
from PySide6.QtGui import QColor, QImage
from PySide6.QtTest import QTest
from PySide6.QtWidgets import QApplication, QComboBox, QMenu, QPushButton, QSpinBox

from neurowatch.models.camera import CameraConfig
from neurowatch.widgets.cell_widget import CellWidget
from neurowatch.widgets.title_bar import TitleBar
from neurowatch.widgets.toolbar import ToolBar


def _image(widget) -> QImage:
    return widget.grab().toImage()


def _bright_columns(image: QImage, rect: QRect, threshold: int = 120) -> list[int]:
    """X positions inside ``rect`` holding a bright (text/glyph) pixel."""
    columns = []
    for x in range(max(rect.left(), 0), min(rect.right() + 1, image.width())):
        for y in range(max(rect.top(), 0), min(rect.bottom() + 1, image.height())):
            if QColor(image.pixel(x, y)).lightness() > threshold:
                columns.append(x)
                break
    return columns


def test_submenus_show_only_qts_own_arrow(qtbot):
    cell = CellWidget()
    qtbot.addWidget(cell)
    cell.set_camera(CameraConfig(id="c1", name="Door", url="rtsp://h/main", sub_url="rtsp://h/sub"))

    menu = cell._build_context_menu()
    titles = [m.title() for m in menu.findChildren(QMenu)]

    assert "Stream" in titles
    assert "Aspect Ratio" in titles
    assert not any(glyph in title for title in titles for glyph in "\u25b6\u25ba>\u203a")


def test_title_bar_focus_does_not_move_or_box_the_tab(qtbot):
    bar = TitleBar()
    qtbot.addWidget(bar)
    bar.resize(1200, 38)
    bar.show()
    qtbot.waitExposed(bar)
    tab = bar._tab_btns["diagnostics"]
    # The label's upper half: the keyboard-focus underline is drawn below it.
    area = QRect(0, 0, tab.width(), tab.height() // 2)
    before = _image(tab)

    # A mouse click leaves no focus box behind.
    QTest.mouseClick(bar._tab_btns["playback"], Qt.MouseButton.LeftButton)
    assert not bar._tab_btns["playback"].hasFocus()
    clicked = _image(bar._tab_btns["playback"])
    assert QColor(clicked.pixel(0, clicked.height() // 2)).lightness() < 60

    # Keyboard focus shows a ring, but the label does not move.
    tab.setFocus(Qt.FocusReason.TabFocusReason)
    QTest.qWait(20)
    unfocused = _bright_columns(before, area)
    focused = _bright_columns(_image(tab), area)
    # Anti-aliased edges may differ by a pixel on the tinted background; the
    # old focus rule added a 2px box and moved the label by that much.
    assert unfocused and focused
    assert abs(focused[0] - unfocused[0]) <= 1
    assert abs(focused[-1] - unfocused[-1]) <= 2


def test_window_button_hover_fills_the_whole_button(qtbot):
    bar = TitleBar()
    qtbot.addWidget(bar)
    bar.resize(1200, 38)
    bar.show()
    qtbot.waitExposed(bar)
    close = bar.findChild(QPushButton, "close")
    assert close is not None

    QTest.mouseMove(bar, bar.rect().center())
    QTest.mouseMove(close, close.rect().center())
    qtbot.waitUntil(close.underMouse, timeout=1000)
    QTest.qWait(20)
    image = _image(close)

    red = QColor("#c0392b").rgb()
    for x in (1, close.width() // 2, close.width() - 2):
        assert image.pixel(x, 2) == red, f"hover highlight missing at x={x}"


def test_toolbar_dropdowns_and_spinbox_draw_their_arrows(qtbot):
    toolbar = ToolBar()
    qtbot.addWidget(toolbar)
    toolbar.resize(1400, 36)
    toolbar.show()
    qtbot.waitExposed(toolbar)

    for widget in [*toolbar.findChildren(QComboBox), *toolbar.findChildren(QSpinBox)]:
        image = _image(widget)
        arrow_area = QRect(widget.width() - 18, 4, 16, widget.height() - 8)
        assert _bright_columns(image, arrow_area, threshold=100), (
            f"{type(widget).__name__} shows no arrow"
        )


def test_toolbar_toggles_look_different_when_checked(qtbot):
    toolbar = ToolBar()
    qtbot.addWidget(toolbar)
    toolbar.resize(1400, 36)
    toolbar.show()
    qtbot.waitExposed(toolbar)
    auto_fit = toolbar._autofit_btn

    auto_fit.setChecked(True)
    QTest.qWait(20)
    checked = QColor(_image(auto_fit).pixel(4, auto_fit.height() // 2))
    auto_fit.setChecked(False)
    QTest.qWait(20)
    unchecked = QColor(_image(auto_fit).pixel(4, auto_fit.height() // 2))

    assert checked != unchecked


def test_toolbar_labels_avoid_emoji_that_shift_the_baseline(qtbot):
    toolbar = ToolBar()
    qtbot.addWidget(toolbar)

    labels = [button.text() for button in toolbar.findChildren(QPushButton)]

    assert not any(ord(char) > 0xFFFF for label in labels for char in label), labels


def test_main_window_does_not_start_with_a_focused_title_bar_button(monkeypatch, qtbot):
    from neurowatch.main import MainWindow

    class _FakeClient:
        _endpoint = "unix:/nonexistent/test-ui-polish.sock"

        def is_online(self) -> bool:
            return True

        def call(self, method, params=None):
            if method == "config.get":
                return {
                    "version": 1,
                    "site_name": "Site",
                    "wall": {"rows": 2, "cols": 2, "auto_fit": True, "page_interval_sec": 0},
                    "ui": {"theme": "dark", "fullscreen": False},
                }
            if method == "camera.list":
                return []
            return {}

    monkeypatch.setattr("neurowatch.main.SyncIpcClient", _FakeClient)
    monkeypatch.setattr("neurowatch.ipc.frame_reader.SHM_PREFIX", "/nonexistent/nw-test-")
    win = MainWindow()
    qtbot.addWidget(win)
    try:
        win.show()
        qtbot.waitExposed(win)
        QTest.qWait(50)

        focused = QApplication.focusWidget()
        assert focused is None or not win._title_bar.isAncestorOf(focused), focused
    finally:
        # Join the window's worker threads before Python drops it.
        win.shutdown()
