"""Labels and tooltips show their text as written.

Qt guesses the format of a label's or tooltip's text: anything that looks like
HTML is rendered as rich text. Names shown in the app come from outside it
(cameras found on the network, a linked portal, plugins, error text), and rich
text reads local files: a name of `<img src="/dev/zero">` used all the memory
and ended the app. `install` turns the guess off for the whole application; a
label that needs rich text asks for it with
`setTextFormat(Qt.TextFormat.RichText)`.
"""

from __future__ import annotations

from PySide6.QtCore import QEvent, QObject, Qt
from PySide6.QtWidgets import QApplication, QLabel

_INSTALLED = "nwPlainText"


class _PlainLabels(QObject):
    """Sets plain text on each label still on Qt's guess when it is first
    styled, which is before it measures or paints its text. Tooltips are
    labels too (Qt's own), so they are covered."""

    def eventFilter(self, watched: QObject, event: QEvent) -> bool:  # noqa: N802
        if (
            event.type() == QEvent.Type.Polish
            and isinstance(watched, QLabel)
            and watched.textFormat() == Qt.TextFormat.AutoText
        ):
            watched.setTextFormat(Qt.TextFormat.PlainText)
        return False


def install(app: QApplication) -> None:
    """Show every label's and tooltip's text as written, from now on."""
    if app.property(_INSTALLED):
        return
    app.setProperty(_INSTALLED, True)
    app.installEventFilter(_PlainLabels(app))
