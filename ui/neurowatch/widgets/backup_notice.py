"""The status bar's Cloud Backup symbol: a cloud and "Backup", green while
the recorder backs its cameras up, red when a camera's backup is failing
(the reasons in the tooltip), grey while it starts; hidden without a plan."""

from __future__ import annotations

from PySide6.QtWidgets import QWidget

from neurowatch.models.backup_status import BackupNotice, backup_notice, backup_summary
from neurowatch.widgets.status_bar_widget import StatusIndicator


class BackupIndicator(StatusIndicator):
    #: Wider than the other marks: a cloud reads as one only this large.
    _MARK_WIDTH = 15

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__("", "cloud", parent)
        self.setObjectName("backupIndicator")
        self.setAccessibleName("Cloud Backup")
        self._notice = BackupNotice("", "", "")
        self.hide()

    def notice(self) -> BackupNotice:
        return self._notice

    def set_status(self, system: object) -> None:
        """Each poll's ``system.status`` (none while the backend is unreachable:
        no symbol, as the backend dot says that)."""
        notice = backup_notice(backup_summary(system))
        if notice == self._notice:
            return
        self._notice = notice
        self.setText(notice.text)
        self.setToolTip(notice.tooltip)
        self.setAccessibleDescription(notice.tooltip)
        self.setStyleSheet(f"color: {notice.color}; font-weight: 600;")
        self.setVisible(bool(notice.text))
