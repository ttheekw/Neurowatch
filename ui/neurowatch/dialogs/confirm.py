"""Themed confirmation prompts with explicit, semantic action buttons."""

from __future__ import annotations

from PySide6.QtWidgets import QPushButton, QWidget

from neurowatch.dialogs.message import MessageDialog


def build_confirmation(
    parent: QWidget | None,
    title: str,
    text: str,
    *,
    confirm_text: str,
    cancel_text: str = "Cancel",
    destructive: bool = False,
    informative_text: str = "",
) -> tuple[MessageDialog, QPushButton]:
    """Build (but do not run) a confirmation and return it with its confirm button.

    Destructive prompts use a danger-styled confirm button and default to Cancel so
    pressing Enter never deletes anything by accident.
    """
    box = MessageDialog(
        parent,
        title,
        text,
        detail=informative_text,
        kind="warning" if destructive else "question",
    )
    cancel_btn = box.add_button(cancel_text, variant="secondary", accepts=False)
    confirm_btn = box.add_button(confirm_text, variant="danger" if destructive else "primary")
    box.set_escape_button(cancel_btn)
    box.set_default_button(cancel_btn if destructive else confirm_btn)
    return box, confirm_btn


def confirm_action(
    parent: QWidget | None,
    title: str,
    text: str,
    *,
    confirm_text: str,
    cancel_text: str = "Cancel",
    destructive: bool = False,
    informative_text: str = "",
) -> bool:
    """Show a modal confirmation and return ``True`` only if the user confirmed."""
    box, confirm_btn = build_confirmation(
        parent,
        title,
        text,
        confirm_text=confirm_text,
        cancel_text=cancel_text,
        destructive=destructive,
        informative_text=informative_text,
    )
    box.exec()
    confirmed = box.clicked_button() is confirm_btn
    box.deleteLater()
    return confirmed
