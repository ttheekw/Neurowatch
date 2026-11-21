"""Text from outside the app is shown as written (`neurowatch.plain_text`).

Qt reads label and tooltip text that looks like HTML as rich text, and rich
text opens local files: a camera or device name of `<img src="/dev/zero">`
used all the memory and ended the app (vulnerability checklist 2026-11-20,
item 1). The name used here is harmless if it is ever rendered, so a
regression fails these tests instead of the machine running them.
"""

from __future__ import annotations

from PySide6.QtCore import QPoint, Qt
from PySide6.QtWidgets import QApplication, QLabel, QToolTip, QVBoxLayout, QWidget

from neurowatch import plain_text
from neurowatch.theme import apply_app_theme

HOSTILE = '<img src="no-such-picture.png"><b>Front door</b>'


def _shown(qtbot, *labels: QLabel) -> QWidget:
    window = QWidget()
    layout = QVBoxLayout(window)
    for label in labels:
        layout.addWidget(label)
    qtbot.addWidget(window)
    window.show()
    QApplication.processEvents()
    return window


def test_the_app_theme_turns_rich_text_guessing_off(qtbot) -> None:
    app = QApplication.instance()
    assert isinstance(app, QApplication)
    apply_app_theme(app)
    assert app.property("nwPlainText"), "each Qt process applies the theme before showing anything"


def test_a_name_that_looks_like_html_is_shown_as_written(qtbot) -> None:
    app = QApplication.instance()
    assert isinstance(app, QApplication)
    plain_text.install(app)
    label, wrapped = QLabel(HOSTILE), QLabel(HOSTILE)
    wrapped.setWordWrap(True)
    window = _shown(qtbot, label, wrapped)
    assert label.textFormat() == Qt.TextFormat.PlainText
    assert wrapped.textFormat() == Qt.TextFormat.PlainText
    window.grab()  # painting them reads no file


def test_text_set_after_the_label_is_shown_is_plain_too(qtbot) -> None:
    app = QApplication.instance()
    assert isinstance(app, QApplication)
    plain_text.install(app)
    label = QLabel("Front door")
    window = _shown(qtbot, label)
    label.setText(HOSTILE)
    window.grab()
    assert label.textFormat() == Qt.TextFormat.PlainText
    assert label.text() == HOSTILE


def test_a_label_that_asks_for_rich_text_keeps_it(qtbot) -> None:
    app = QApplication.instance()
    assert isinstance(app, QApplication)
    plain_text.install(app)
    label = QLabel("<b>Recording</b>")
    label.setTextFormat(Qt.TextFormat.RichText)
    window = _shown(qtbot, label)
    assert window.isVisible()
    assert label.textFormat() == Qt.TextFormat.RichText


def test_a_tooltip_is_shown_as_written(qtbot) -> None:
    app = QApplication.instance()
    assert isinstance(app, QApplication)
    plain_text.install(app)
    window = _shown(qtbot)
    QToolTip.showText(QPoint(10, 10), HOSTILE, window)
    QApplication.processEvents()
    tips = [widget for widget in app.allWidgets() if widget.inherits("QTipLabel")]
    try:
        assert tips, "Qt shows a tooltip in a label of its own"
        for tip in tips:
            assert isinstance(tip, QLabel)
            assert tip.textFormat() == Qt.TextFormat.PlainText
    finally:
        QToolTip.hideText()
