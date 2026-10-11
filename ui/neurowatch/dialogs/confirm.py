"""Themed confirmation prompts with explicit, semantic action buttons."""

from __future__ import annotations

from PySide6.QtWidgets import QMessageBox, QPushButton, QWidget

from neurowatch.theme import apply_dialog_theme, set_button_variant


def build_confirmation(
    parent: QWidget | None,
    title: str,
    text: str,
    *,
    confirm_text: str,
    cancel_text: str = "Cancel",
    destructive: bool = False,
    informative_text: str = "",
) -> tuple[QMessageBox, QPushButton]:
    """Build (but do not run) a confirmation box and return it with its confirm button.

    Destructive prompts use a danger-styled confirm button and default to Cancel so
    pressing Enter never deletes anything by accident.
    """
    box = QMessageBox(parent)
    box.setWindowTitle(title)
    box.setText(text)
    if informative_text:
        box.setInformativeText(informative_text)
    box.setIcon(QMessageBox.Icon.Warning if destructive else QMessageBox.Icon.Question)
    apply_dialog_theme(box)

    confirm_btn = box.addButton(
        confirm_text,
        QMessageBox.ButtonRole.DestructiveRole
        if destructive
        else QMessageBox.ButtonRole.AcceptRole,
    )
    cancel_btn = box.addButton(cancel_text, QMessageBox.ButtonRole.RejectRole)
    set_button_variant(confirm_btn, "danger" if destructive else "primary")
    set_button_variant(cancel_btn, "secondary")
    box.setEscapeButton(cancel_btn)
    box.setDefaultButton(cancel_btn if destructive else confirm_btn)
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
    return box.clickedButton() is confirm_btn
