"""Secondary toolbar — grid selector, page navigation, camera action buttons."""

from __future__ import annotations

from PySide6.QtCore import Qt, Signal
from PySide6.QtWidgets import (
    QComboBox,
    QFrame,
    QHBoxLayout,
    QLabel,
    QPushButton,
    QSizePolicy,
    QSpinBox,
    QWidget,
)

from neurowatch.models.wall_layout import GRID_PRESETS
from neurowatch.theme import arrow_rules

_HEIGHT = 36
_BG = "#14141e"
_TEXT = "#c0c0d4"
_BTN = "#1e1e32"
_BTN_HOVER = "#28284a"
_BTN_PRIMARY = "#1d3a6e"

_COMBO_STYLE = f"""
QComboBox {{
    background: {_BTN}; color: {_TEXT}; border: 1px solid #2a2a44;
    border-radius: 3px; padding: 2px 22px 2px 8px; min-width: 96px; font-size: 12px;
}}
QComboBox:hover {{ border-color: #4a4a6a; }}
{arrow_rules()}
QComboBox QAbstractItemView {{
    background: #1a1a2e; color: {_TEXT}; selection-background-color: #2a3a6a;
    border: 1px solid #2a2a44;
}}
"""
_BTN_STYLE = f"""
QPushButton {{
    background: {_BTN}; color: {_TEXT}; border: 1px solid #252540;
    border-radius: 3px; padding: 3px 10px; font-size: 12px;
}}
QPushButton:hover {{ background: {_BTN_HOVER}; }}
QPushButton:checked {{ background: {_BTN_PRIMARY}; color: #ffffff; border-color: #3a5a9a; }}
QPushButton:checked:hover {{ background: #24467f; }}
QPushButton:disabled {{ color: #44445a; border-color: #1e1e30; }}
"""
_PAGE_BTN = f"""
QPushButton {{
    background: {_BTN}; color: {_TEXT}; border: 1px solid #252540;
    border-radius: 3px; padding: 3px 8px; font-size: 12px;
}}
QPushButton:hover {{ background: {_BTN_HOVER}; }}
QPushButton:disabled {{ color: #44445a; border-color: #1e1e30; }}
"""

# (label, rows, cols); labels are COLS×ROWS, the NVR convention.
_GRID_PRESETS = GRID_PRESETS


def _sep() -> QFrame:
    line = QFrame()
    line.setFrameShape(QFrame.Shape.VLine)
    line.setFixedWidth(1)
    line.setStyleSheet("color: #252540;")
    return line


class ToolBar(QWidget):
    """Action toolbar directly below the title bar."""

    grid_changed = Signal(int, int)  # rows, cols
    add_camera = Signal()
    edit_camera = Signal()
    remove_camera = Signal()
    prev_page = Signal()
    next_page = Signal()
    fullscreen_toggled = Signal()
    auto_fit_toggled = Signal(bool)
    custom_grid_requested = Signal()
    auto_scroll_changed = Signal(int)  # interval_sec; 0 = disabled
    new_window_requested = Signal()

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setFixedHeight(_HEIGHT)
        self.setAttribute(Qt.WidgetAttribute.WA_StyledBackground)
        self.setStyleSheet(f"background-color: {_BG};")

        layout = QHBoxLayout(self)
        layout.setContentsMargins(10, 2, 10, 2)
        layout.setSpacing(6)

        # Grid preset combo
        self._grid_combo = QComboBox()
        self._grid_combo.setStyleSheet(_COMBO_STYLE)
        self._grid_combo.setToolTip("Select grid layout")
        for label, r, c in _GRID_PRESETS:
            self._grid_combo.addItem(f"{label} Grid", (r, c))
        self._grid_combo.addItem("Custom…", None)
        # Default: 8×4 (8 cols × 4 rows)
        self._set_combo_to(4, 8)
        self._grid_combo.currentIndexChanged.connect(self._on_grid_combo)
        layout.addWidget(self._grid_combo)

        # Auto-Fit toggle
        self._autofit_btn = QPushButton("Auto-Fit")
        self._autofit_btn.setCheckable(True)
        self._autofit_btn.setChecked(True)
        self._autofit_btn.setStyleSheet(_BTN_STYLE)
        self._autofit_btn.setToolTip("Toggle auto-fit cell stretching")
        self._autofit_btn.clicked.connect(lambda checked: self.auto_fit_toggled.emit(checked))
        layout.addWidget(self._autofit_btn)

        layout.addWidget(_sep())

        # Page navigation
        self._prev_btn = QPushButton("< Prev")
        self._prev_btn.setStyleSheet(_PAGE_BTN)
        self._prev_btn.setToolTip("Previous page (PgUp)")
        self._prev_btn.clicked.connect(self.prev_page)
        layout.addWidget(self._prev_btn)

        self._page_label = QLabel("Page 1 / 1")
        self._page_label.setAlignment(Qt.AlignmentFlag.AlignCenter)
        self._page_label.setStyleSheet(f"color: {_TEXT}; font-size: 12px; min-width: 88px;")
        layout.addWidget(self._page_label)

        self._next_btn = QPushButton("Next >")
        self._next_btn.setStyleSheet(_PAGE_BTN)
        self._next_btn.setToolTip("Next page (PgDn)")
        self._next_btn.clicked.connect(self.next_page)
        layout.addWidget(self._next_btn)

        self._new_window_btn = QPushButton("⧉ New Window")
        self._new_window_btn.setStyleSheet(_BTN_STYLE)
        self._new_window_btn.clicked.connect(self.new_window_requested)
        layout.addWidget(self._new_window_btn)
        self.set_new_window_enabled(False)

        layout.addWidget(_sep())

        # Camera actions
        self._add_btn = QPushButton("＋ Add Camera")
        self._add_btn.setStyleSheet(_BTN_STYLE)
        self._add_btn.setToolTip("Add a new camera (Ctrl+N)")
        self._add_btn.clicked.connect(self.add_camera)
        layout.addWidget(self._add_btn)

        self._edit_btn = QPushButton("✏ Edit Camera")
        self._edit_btn.setStyleSheet(_BTN_STYLE)
        self._edit_btn.setToolTip("Edit selected camera")
        self._edit_btn.setEnabled(False)
        self._edit_btn.clicked.connect(self.edit_camera)
        layout.addWidget(self._edit_btn)

        self._remove_btn = QPushButton("✕ Remove")
        self._remove_btn.setStyleSheet(_BTN_STYLE)
        self._remove_btn.setToolTip("Remove selected camera")
        self._remove_btn.setEnabled(False)
        self._remove_btn.clicked.connect(self.remove_camera)
        layout.addWidget(self._remove_btn)

        layout.addWidget(_sep())

        # Full-screen toggle
        fs_btn = QPushButton("⛶")
        fs_btn.setToolTip("Toggle Full Screen (F11)")
        fs_btn.setFixedSize(30, 28)
        fs_btn.setStyleSheet(_BTN_STYLE)
        fs_btn.clicked.connect(self.fullscreen_toggled)
        layout.addWidget(fs_btn)

        layout.addWidget(_sep())

        # Auto-scroll controls
        self._autoscroll_spin = QSpinBox()
        self._autoscroll_spin.setRange(0, 300)
        self._autoscroll_spin.setValue(0)
        self._autoscroll_spin.setSuffix(" s")
        self._autoscroll_spin.setFixedWidth(64)
        self._autoscroll_spin.setToolTip("Auto-scroll interval in seconds (0 = disabled)")
        self._autoscroll_spin.setStyleSheet(
            f"QSpinBox {{ background: #1e1e32; color: {_TEXT}; border: 1px solid #252540;"
            f" border-radius: 3px; padding: 2px 18px 2px 4px; font-size: 12px; }}" + arrow_rules()
        )
        self._autoscroll_spin.valueChanged.connect(self._on_autoscroll_value_changed)
        layout.addWidget(self._autoscroll_spin)

        self._autoscroll_btn = QPushButton("⟳ Auto")
        self._autoscroll_btn.setCheckable(True)
        self._autoscroll_btn.setToolTip("Enable auto-scroll page cycling")
        self._autoscroll_btn.setStyleSheet(_BTN_STYLE)
        self._autoscroll_btn.clicked.connect(self._on_autoscroll_toggle)
        layout.addWidget(self._autoscroll_btn)

        # Flexible spacer
        sp = QWidget()
        sp.setSizePolicy(QSizePolicy.Policy.Expanding, QSizePolicy.Policy.Preferred)
        layout.addWidget(sp)

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def set_page_info(self, page: int, total: int) -> None:
        """Update the page label and enable/disable nav buttons."""
        self._page_label.setText(f"Page {page} / {total}")
        self._prev_btn.setEnabled(page > 1)
        self._next_btn.setEnabled(page < total)

    def set_new_window_enabled(self, enabled: bool) -> None:
        """New Window needs a page to spare: this window keeps at least one."""
        self._new_window_btn.setEnabled(enabled)
        self._new_window_btn.setToolTip(
            "Move a page of cameras to a new window (Ctrl+Shift+N)"
            if enabled
            else "New Window needs at least two pages: this window keeps one"
        )

    def set_camera_selected(self, selected: bool) -> None:
        """Enable/disable Edit and Remove buttons based on selection."""
        self._edit_btn.setEnabled(selected)
        self._remove_btn.setEnabled(selected)

    def set_grid(self, rows: int, cols: int) -> None:
        """Update combo to reflect the active grid without emitting signal."""
        self._grid_combo.blockSignals(True)
        self._set_combo_to(rows, cols)
        self._grid_combo.blockSignals(False)

    def set_auto_fit(self, enabled: bool) -> None:
        """Update Auto-Fit button state without emitting user intent."""
        self._autofit_btn.setChecked(enabled)

    def set_auto_scroll(self, interval_sec: int) -> None:
        """Update auto-scroll controls without emitting signal (for config load)."""
        self._autoscroll_spin.blockSignals(True)
        self._autoscroll_spin.setValue(max(0, interval_sec))
        self._autoscroll_spin.blockSignals(False)
        self._autoscroll_btn.setChecked(interval_sec > 0)

    def set_auto_scroll_interval(self, interval_sec: int) -> None:
        """Show ``interval_sec`` in the interval box without turning auto-scroll
        on or off (the page interval is shared with the extra windows)."""
        self._autoscroll_spin.blockSignals(True)
        self._autoscroll_spin.setValue(max(0, interval_sec))
        self._autoscroll_spin.blockSignals(False)

    # ------------------------------------------------------------------

    def _set_combo_to(self, rows: int, cols: int) -> None:
        for i in range(self._grid_combo.count()):
            if self._grid_combo.itemData(i) == (rows, cols):
                self._grid_combo.setCurrentIndex(i)
                return
        # If not found, select "Custom…"
        for i in range(self._grid_combo.count()):
            if self._grid_combo.itemData(i) is None:
                self._grid_combo.setCurrentIndex(i)
                return

    def _on_grid_combo(self, _idx: int) -> None:
        data = self._grid_combo.currentData()
        if data is None:
            self.custom_grid_requested.emit()
        elif data:
            r, c = data
            self.grid_changed.emit(r, c)

    def _on_autoscroll_toggle(self, checked: bool) -> None:
        interval = self._autoscroll_spin.value() if checked else 0
        self.auto_scroll_changed.emit(interval)

    def _on_autoscroll_value_changed(self, value: int) -> None:
        if self._autoscroll_btn.isChecked():
            self.auto_scroll_changed.emit(value)
