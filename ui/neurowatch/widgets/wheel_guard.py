"""Keep the scroll wheel from changing fields the pointer only passes over."""

from __future__ import annotations

from PySide6.QtCore import QEvent, QObject, Qt
from PySide6.QtWidgets import QWidget


class FocusedWheelOnly(QObject):
    """A field takes the scroll wheel only once clicked into; otherwise the
    wheel scrolls the panel underneath instead of changing the value.

    A date field under a passing scroll changed the section under the
    pointer (one notch moved a year back), so a range's end fell before its
    start unnoticed."""

    def eventFilter(self, watched: QObject, event: QEvent) -> bool:  # noqa: N802
        if event.type() == QEvent.Type.Wheel and isinstance(watched, QWidget):
            if not watched.hasFocus():
                event.ignore()
                return True
        return False


def guard_wheel(owner: QObject, *fields: QWidget) -> FocusedWheelOnly:
    """Let ``fields`` take the wheel only while focused (a click focuses
    them). The guard lives as long as ``owner``."""
    guard = FocusedWheelOnly(owner)
    for field in fields:
        field.setFocusPolicy(Qt.FocusPolicy.StrongFocus)
        field.installEventFilter(guard)
    return guard
