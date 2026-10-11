from __future__ import annotations

from PySide6.QtWidgets import (
    QDialog,
    QDialogButtonBox,
    QFormLayout,
    QLabel,
    QSpinBox,
    QVBoxLayout,
    QWidget,
)

from neurowatch.theme import apply_dialog_theme, set_label_role, style_button_box


class CustomGridDialog(QDialog):
    def __init__(self, rows: int, cols: int, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setWindowTitle("Custom Grid")
        self.setMinimumWidth(320)
        apply_dialog_theme(self)

        layout = QVBoxLayout(self)
        layout.setContentsMargins(20, 18, 20, 16)
        layout.setSpacing(10)

        title = QLabel("Custom grid layout")
        title.setObjectName("dialogTitle")
        layout.addWidget(title)
        subtitle = QLabel("Choose how many cameras each wall page shows.")
        subtitle.setObjectName("dialogSubtitle")
        subtitle.setWordWrap(True)
        layout.addWidget(subtitle)

        form = QFormLayout()
        form.setHorizontalSpacing(14)
        form.setVerticalSpacing(8)

        self._rows_spin = QSpinBox()
        self._rows_spin.setRange(1, 10)
        self._rows_spin.setValue(rows)
        self._rows_spin.setAccessibleName("Grid rows")
        form.addRow("Rows", self._rows_spin)

        self._cols_spin = QSpinBox()
        self._cols_spin.setRange(1, 10)
        self._cols_spin.setValue(cols)
        self._cols_spin.setAccessibleName("Grid columns")
        form.addRow("Columns", self._cols_spin)

        layout.addLayout(form)

        self._summary = QLabel()
        set_label_role(self._summary, "emphasis")
        layout.addWidget(self._summary)
        self._rows_spin.valueChanged.connect(self._update_summary)
        self._cols_spin.valueChanged.connect(self._update_summary)
        self._update_summary()

        btns = QDialogButtonBox(
            QDialogButtonBox.StandardButton.Ok | QDialogButtonBox.StandardButton.Cancel
        )
        btns.button(QDialogButtonBox.StandardButton.Ok).setText("Apply Grid")
        btns.accepted.connect(self.accept)
        btns.rejected.connect(self.reject)
        style_button_box(btns)
        layout.addWidget(btns)

    def _update_summary(self) -> None:
        rows, cols = self.rows, self.cols
        self._summary.setText(f"{rows} × {cols}  ·  {rows * cols} cells per page")

    @property
    def rows(self) -> int:
        return self._rows_spin.value()

    @property
    def cols(self) -> int:
        return self._cols_spin.value()
