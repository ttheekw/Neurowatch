"""Centralized Neurowatch light/dark themes and Qt style sheets.

Scope: dialogs, popups, menus, inputs and confirmations. Rules are applied per
dialog (``apply_dialog_theme``) or per menu (``themed_menu``); the application
style sheet only targets popup classes, so the video wall, toolbars and other
custom-painted surfaces keep their existing look.
"""

from __future__ import annotations

import logging
import sys
from dataclasses import dataclass
from functools import cache
from pathlib import Path

from PySide6.QtCore import Qt
from PySide6.QtGui import QColor, QFont, QFontDatabase, QIcon, QTextCharFormat
from PySide6.QtWidgets import (
    QAbstractButton,
    QApplication,
    QCalendarWidget,
    QDialogButtonBox,
    QLabel,
    QMenu,
    QWidget,
)

_ICONS = Path(__file__).resolve().parent / "resources" / "icons"

BUTTON_VARIANTS: tuple[str, ...] = ("primary", "secondary", "danger", "ghost")
STATUS_TONES: tuple[str, ...] = ("success", "warning", "error", "info", "muted")


@dataclass(frozen=True)
class ColorTokens:
    """Semantic colour roles. Text/background pairs are checked for WCAG AA in tests."""

    window: str = "#0b0f17"
    surface: str = "#121824"
    surface_raised: str = "#19212f"
    surface_sunken: str = "#0d121b"
    hover: str = "#222c3d"
    pressed: str = "#2a3549"
    border: str = "#273146"
    border_strong: str = "#3a4660"
    # Boundaries that identify controls (WCAG 1.4.11: >= 3:1 on surface and surface_raised).
    border_control: str = "#68758e"
    text: str = "#e6eaf2"
    text_muted: str = "#9aa5b8"
    text_disabled: str = "#5b6578"
    accent: str = "#2563eb"
    accent_hover: str = "#1d4ed8"
    accent_pressed: str = "#1e40af"
    accent_subtle: str = "#1b2b4b"
    on_accent: str = "#ffffff"
    focus_ring: str = "#60a5fa"
    danger: str = "#dc2626"
    danger_hover: str = "#b91c1c"
    danger_pressed: str = "#991b1b"
    on_danger: str = "#ffffff"
    success: str = "#4ade80"
    warning: str = "#fbbf24"
    error: str = "#f87171"
    info: str = "#60a5fa"


@dataclass(frozen=True)
class Metrics:
    radius: int = 6
    radius_small: int = 4
    control_height: int = 30
    font_size: int = 13
    font_size_small: int = 11
    font_size_title: int = 18


COLORS = ColorTokens()
LIGHT_COLORS = ColorTokens(
    window="#f3f6fb",
    surface="#ffffff",
    surface_raised="#f8fafc",
    surface_sunken="#eef2f7",
    hover="#e2e8f0",
    pressed="#cbd5e1",
    border="#cbd5e1",
    border_strong="#94a3b8",
    border_control="#64748b",
    text="#172033",
    text_muted="#475569",
    text_disabled="#94a3b8",
    accent="#1d4ed8",
    accent_hover="#1e40af",
    accent_pressed="#1e3a8a",
    accent_subtle="#dbeafe",
    on_accent="#ffffff",
    focus_ring="#2563eb",
    danger="#b91c1c",
    danger_hover="#991b1b",
    danger_pressed="#7f1d1d",
    on_danger="#ffffff",
    success="#166534",
    warning="#92400e",
    error="#b91c1c",
    info="#1d4ed8",
)
METRICS = Metrics()
_active_theme = "dark"


def normalize_theme(name: str | None) -> str:
    """Return a supported theme name, falling back safely to dark."""
    return "light" if str(name or "").lower() == "light" else "dark"


def active_colors(theme_name: str | None = None) -> ColorTokens:
    name = normalize_theme(theme_name if theme_name is not None else _active_theme)
    return LIGHT_COLORS if name == "light" else COLORS


# ---------------------------------------------------------------------------
# Accessibility helpers
# ---------------------------------------------------------------------------


def _relative_luminance(hex_color: str) -> float:
    color = QColor(hex_color)

    def channel(value: int) -> float:
        c = value / 255
        return c / 12.92 if c <= 0.03928 else ((c + 0.055) / 1.055) ** 2.4

    return (
        0.2126 * channel(color.red())
        + 0.7152 * channel(color.green())
        + 0.0722 * channel(color.blue())
    )


def contrast_ratio(fg: str, bg: str) -> float:
    """WCAG 2.x contrast ratio between two ``#rrggbb`` colours (1.0 – 21.0)."""
    a, b = _relative_luminance(fg), _relative_luminance(bg)
    lighter, darker = max(a, b), min(a, b)
    return (lighter + 0.05) / (darker + 0.05)


def _icon(name: str) -> str:
    return (_ICONS / name).as_posix()


def ui_font(point_size: float, weight: QFont.Weight = QFont.Weight.Normal) -> QFont:
    """The system's interface font (Segoe UI on Windows, the desktop's font on
    Linux) at ``point_size``. Never name a family such as "Sans": Windows has
    none by that name and substitutes a poorer one."""
    font = QFont(QApplication.font())
    font.setPointSizeF(point_size)
    font.setWeight(weight)
    return font


def mono_font(point_size: float, weight: QFont.Weight = QFont.Weight.Normal) -> QFont:
    """The system's fixed-width font (Consolas on Windows)."""
    font = QFontDatabase.systemFont(QFontDatabase.SystemFont.FixedFont)
    font.setPointSizeF(point_size)
    font.setWeight(weight)
    return font


#: Sizes of the logo rendered for window and taskbar icons.
_APP_ICON_SIZES = (16, 20, 24, 32, 40, 48, 64, 128, 256)


@cache
def app_icon() -> QIcon:
    """The NeuroWatch logo, for window title bars and the taskbar.

    Rendered from the SVG into pixmaps here (QtSvg) rather than ``QIcon(path)``,
    which needs Qt's SVG icon plugin: a bundle without it would show the
    desktop's generic icon on the taskbar."""
    from PySide6.QtGui import QPainter, QPixmap
    from PySide6.QtSvg import QSvgRenderer

    renderer = QSvgRenderer(_icon("neurowatch.svg"))
    if not renderer.isValid():
        logging.getLogger(__name__).warning(
            "App logo could not be read: %s", _icon("neurowatch.svg")
        )
    icon = QIcon()
    for size in _APP_ICON_SIZES:
        pixmap = QPixmap(size, size)
        pixmap.fill(Qt.GlobalColor.transparent)
        painter = QPainter(pixmap)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing)
        renderer.render(painter)
        painter.end()
        icon.addPixmap(pixmap)
    return icon


def icon_path(name: str) -> str:
    """Path of a bundled theme icon, for ``url(...)`` in widget style sheets."""
    return _icon(name)


# ---------------------------------------------------------------------------
# Style sheets
# ---------------------------------------------------------------------------


def _menu_rules(c: ColorTokens = COLORS, m: Metrics = METRICS) -> str:
    return f"""
QMenu {{
    background-color: {c.surface_raised};
    color: {c.text};
    border: 1px solid {c.border_strong};
    padding: 6px;
    font-size: {m.font_size}px;
}}
QMenu::item {{
    background-color: transparent;
    padding: 7px 28px 7px 12px;
    margin: 1px 0;
    border-radius: {m.radius_small}px;
    min-width: 160px;
}}
QMenu::item:selected {{
    background-color: {c.accent};
    color: {c.on_accent};
}}
QMenu::item:pressed {{
    background-color: {c.accent};
    color: {c.on_accent};
}}
QMenu::item:disabled {{
    color: {c.text_disabled};
    background-color: transparent;
}}
QMenu::separator {{
    height: 1px;
    background-color: {c.border};
    margin: 5px 8px;
}}
QMenu::indicator {{
    width: 14px;
    height: 14px;
    left: 6px;
}}
QMenu::indicator:checked {{
    image: url("{_icon("check.svg")}");
}}
QMenu::right-arrow {{
    image: url("{_icon("chevron-right.svg")}");
    width: 12px;
    height: 12px;
}}
"""


# Combo boxes open a plain list under (or over) the field, sized to its items.
# Qt's default "popup" mode lines the current item up with the field and,
# from window positions Wayland does not report, shrinks the list to a row
# or two behind scroll arrows.
_COMBO_LIST_RULE = """
QComboBox {
    combobox-popup: 0;
}
QComboBox QAbstractItemView::item {
    min-height: 24px;
    padding: 0 6px;
}
"""


def arrow_rules() -> str:
    """Drop-down and spin-box arrows for widgets styled outside dialogs.

    Restyling ``::drop-down`` or the spin buttons in a widget's own style
    sheet removes the native arrow, so toolbars and panels append these.
    """
    return f"""{_COMBO_LIST_RULE}
QComboBox::drop-down {{
    subcontrol-origin: padding;
    subcontrol-position: center right;
    width: 20px;
    border: none;
}}
QComboBox::down-arrow {{
    image: url("{_icon("chevron-down.svg")}");
    width: 10px;
    height: 10px;
}}
QSpinBox::up-button, QDoubleSpinBox::up-button,
QSpinBox::down-button, QDoubleSpinBox::down-button {{
    subcontrol-origin: border;
    width: 16px;
    border: none;
    background: transparent;
}}
QSpinBox::up-button, QDoubleSpinBox::up-button {{ subcontrol-position: top right; }}
QSpinBox::down-button, QDoubleSpinBox::down-button {{ subcontrol-position: bottom right; }}
QSpinBox::up-button:hover, QDoubleSpinBox::up-button:hover,
QSpinBox::down-button:hover, QDoubleSpinBox::down-button:hover {{
    background: rgba(255, 255, 255, 0.08);
}}
QSpinBox::up-arrow, QDoubleSpinBox::up-arrow {{
    image: url("{_icon("chevron-up.svg")}");
    width: 8px;
    height: 8px;
}}
QSpinBox::down-arrow, QDoubleSpinBox::down-arrow {{
    image: url("{_icon("chevron-down.svg")}");
    width: 8px;
    height: 8px;
}}
"""


def _tooltip_rules(c: ColorTokens = COLORS, m: Metrics = METRICS) -> str:
    return f"""
QToolTip {{
    background-color: {c.surface_raised};
    color: {c.text};
    border: 1px solid {c.border_strong};
    padding: 5px 8px;
    font-size: {m.font_size_small}px;
}}
"""


def _calendar_rules(c: ColorTokens = COLORS, m: Metrics = METRICS) -> str:
    return f"""
QCalendarWidget QWidget {{
    border: none;
}}
QCalendarWidget QWidget#qt_calendar_navigationbar {{
    background-color: {c.surface_raised};
    border-bottom: 1px solid {c.border};
    min-height: 36px;
}}
QCalendarWidget QToolButton {{
    background-color: transparent;
    color: {c.text};
    font-size: {m.font_size}px;
    font-weight: 600;
    padding: 4px 10px;
    margin: 3px;
    border: 2px solid transparent;
    border-radius: {m.radius}px;
}}
QCalendarWidget QToolButton:hover {{
    background-color: {c.hover};
}}
QCalendarWidget QToolButton:focus {{
    border: 2px solid {c.focus_ring};
}}
QCalendarWidget QToolButton::menu-indicator {{
    image: none;
    width: 0;
}}
QCalendarWidget QToolButton#qt_calendar_prevmonth {{
    qproperty-icon: url("{_icon("chevron-left.svg")}");
}}
QCalendarWidget QToolButton#qt_calendar_nextmonth {{
    qproperty-icon: url("{_icon("chevron-right.svg")}");
}}
QCalendarWidget QSpinBox {{
    background-color: {c.surface_sunken};
    color: {c.text};
    border: 1px solid {c.border_control};
    border-radius: {m.radius_small}px;
    padding: 2px 6px;
    selection-background-color: {c.accent};
}}
QCalendarWidget QAbstractItemView {{
    background-color: {c.surface_sunken};
    color: {c.text};
    font-size: {m.font_size}px;
    selection-background-color: {c.accent};
    selection-color: {c.on_accent};
    outline: 0;
    border: 2px solid transparent;
}}
QCalendarWidget QAbstractItemView:focus {{
    border: 2px solid {c.focus_ring};
}}
QCalendarWidget QAbstractItemView:disabled {{
    color: {c.text_disabled};
}}
"""


def _input_rules(c: ColorTokens = COLORS, m: Metrics = METRICS) -> str:
    fields = (
        "QLineEdit, QTextEdit, QPlainTextEdit, QComboBox, QSpinBox, QDoubleSpinBox, QDateTimeEdit"
    )
    return f"""{_COMBO_LIST_RULE}
QProgressBar {{
    min-height: 6px;
    max-height: 6px;
    background-color: {c.surface_sunken};
    border: 1px solid {c.border};
    border-radius: 3px;
}}
QProgressBar::chunk {{
    background-color: {c.accent};
    border-radius: 3px;
}}
QSlider:horizontal {{
    min-height: 20px;
}}
QSlider::groove:horizontal {{
    height: 4px;
    border-radius: 2px;
    background: {c.border};
}}
QSlider::sub-page:horizontal {{
    border-radius: 2px;
    background: {c.accent};
}}
QSlider::handle:horizontal {{
    width: 14px;
    height: 14px;
    margin: -5px 0;
    border-radius: 7px;
    background: {c.text};
    border: 1px solid {c.border_control};
}}
QSlider::handle:horizontal:hover {{
    background: {c.on_accent};
    border-color: {c.accent};
}}
QSlider::sub-page:horizontal:disabled {{
    background: {c.text_disabled};
}}
QSlider::handle:horizontal:disabled {{
    background: {c.text_disabled};
    border-color: {c.border};
}}
{fields} {{
    background-color: {c.surface_sunken};
    color: {c.text};
    border: 1px solid {c.border_control};
    border-radius: {m.radius}px;
    padding: 4px 8px;
    min-height: {m.control_height - 10}px;
    font-size: {m.font_size}px;
    selection-background-color: {c.accent};
    selection-color: {c.on_accent};
}}
QLineEdit:hover, QTextEdit:hover, QPlainTextEdit:hover, QComboBox:hover,
QSpinBox:hover, QDoubleSpinBox:hover, QDateTimeEdit:hover {{
    border-color: {c.text_muted};
}}
QLineEdit:focus, QTextEdit:focus, QPlainTextEdit:focus, QComboBox:focus,
QSpinBox:focus, QDoubleSpinBox:focus, QDateTimeEdit:focus {{
    border: 2px solid {c.focus_ring};
    padding: 3px 7px;
}}
QLineEdit:disabled, QTextEdit:disabled, QPlainTextEdit:disabled, QComboBox:disabled,
QSpinBox:disabled, QDoubleSpinBox:disabled, QDateTimeEdit:disabled {{
    color: {c.text_disabled};
    background-color: {c.surface};
    border-color: {c.border};
}}
QComboBox::drop-down, QDateTimeEdit::drop-down {{
    subcontrol-origin: padding;
    subcontrol-position: center right;
    width: 22px;
    border: none;
}}
QComboBox::down-arrow, QDateTimeEdit::down-arrow {{
    image: url("{_icon("chevron-down.svg")}");
    width: 12px;
    height: 12px;
}}
QComboBox QAbstractItemView {{
    background-color: {c.surface_raised};
    color: {c.text};
    border: 1px solid {c.border_strong};
    selection-background-color: {c.accent};
    selection-color: {c.on_accent};
    outline: 0;
    padding: 4px;
}}
QSpinBox::up-button, QDoubleSpinBox::up-button, QSpinBox::down-button, QDoubleSpinBox::down-button,
QDateTimeEdit::up-button, QDateTimeEdit::down-button {{
    subcontrol-origin: border;
    width: 18px;
    border: none;
    background: transparent;
}}
QSpinBox::up-button, QDoubleSpinBox::up-button, QDateTimeEdit::up-button {{
    subcontrol-position: top right;
}}
QSpinBox::down-button, QDoubleSpinBox::down-button, QDateTimeEdit::down-button {{
    subcontrol-position: bottom right;
}}
QSpinBox::up-button:hover, QDoubleSpinBox::up-button:hover,
QSpinBox::down-button:hover, QDoubleSpinBox::down-button:hover,
QDateTimeEdit::up-button:hover, QDateTimeEdit::down-button:hover {{
    background-color: {c.hover};
}}
QSpinBox::up-arrow, QDoubleSpinBox::up-arrow, QDateTimeEdit::up-arrow {{
    image: url("{_icon("chevron-up.svg")}");
    width: 10px;
    height: 10px;
}}
QSpinBox::down-arrow, QDoubleSpinBox::down-arrow, QDateTimeEdit::down-arrow {{
    image: url("{_icon("chevron-down.svg")}");
    width: 10px;
    height: 10px;
}}
"""


def _button_rules(c: ColorTokens = COLORS, m: Metrics = METRICS) -> str:
    return f"""
QPushButton {{
    background-color: {c.surface_raised};
    color: {c.text};
    border: 1px solid {c.border_strong};
    border-radius: {m.radius}px;
    padding: 5px 14px;
    min-height: {m.control_height - 12}px;
    font-size: {m.font_size}px;
    font-weight: 500;
}}
QPushButton:hover {{
    background-color: {c.hover};
}}
QPushButton:pressed {{
    background-color: {c.pressed};
}}
QPushButton:focus {{
    border: 2px solid {c.focus_ring};
    padding: 4px 13px;
}}
QPushButton:disabled {{
    color: {c.text_disabled};
    background-color: {c.surface};
    border-color: {c.border};
}}
QPushButton[variant="primary"] {{
    background-color: {c.accent};
    color: {c.on_accent};
    border: 1px solid {c.accent};
    font-weight: 600;
}}
QPushButton[variant="primary"]:hover {{
    background-color: {c.accent_hover};
    border-color: {c.accent_hover};
}}
QPushButton[variant="primary"]:pressed {{
    background-color: {c.accent_pressed};
}}
QPushButton[variant="primary"]:focus {{
    border: 2px solid {c.focus_ring};
}}
QPushButton[variant="secondary"] {{
    background-color: {c.surface_raised};
    color: {c.text};
    border: 1px solid {c.border_strong};
}}
QPushButton[variant="secondary"]:hover {{
    background-color: {c.hover};
}}
QPushButton[variant="secondary"]:focus {{
    border: 2px solid {c.focus_ring};
}}
QPushButton[variant="danger"] {{
    background-color: {c.danger};
    color: {c.on_danger};
    border: 1px solid {c.danger};
    font-weight: 600;
}}
QPushButton[variant="danger"]:hover {{
    background-color: {c.danger_hover};
    border-color: {c.danger_hover};
}}
QPushButton[variant="danger"]:pressed {{
    background-color: {c.danger_pressed};
}}
QPushButton[variant="danger"]:focus {{
    border: 2px solid {c.focus_ring};
}}
QPushButton[variant="ghost"] {{
    background-color: transparent;
    color: {c.text_muted};
    border: 1px solid transparent;
}}
QPushButton[variant="ghost"]:hover {{
    background-color: {c.hover};
    color: {c.text};
}}
QPushButton[variant="ghost"]:focus {{
    border: 2px solid {c.focus_ring};
}}
QPushButton[variant]:disabled {{
    color: {c.text_disabled};
    background-color: {c.surface};
    border: 1px solid {c.border};
}}
QPushButton[variant="ghost"]:disabled {{
    background-color: transparent;
    border-color: transparent;
}}
QPushButton:checked, QPushButton[variant="secondary"]:checked,
QPushButton[variant="ghost"]:checked {{
    background-color: {c.accent_subtle};
    color: {c.text};
    border: 1px solid {c.accent};
}}
QPushButton:checked:hover, QPushButton[variant="secondary"]:checked:hover {{
    background-color: {c.accent_subtle};
    border-color: {c.focus_ring};
}}
"""


def _choice_rules(c: ColorTokens = COLORS, m: Metrics = METRICS) -> str:
    return f"""
QCheckBox, QRadioButton {{
    color: {c.text};
    spacing: 8px;
    padding: 3px 4px;
    border: 2px solid transparent;
    border-radius: {m.radius_small}px;
    font-size: {m.font_size}px;
}}
QCheckBox:focus, QRadioButton:focus {{
    border: 2px solid {c.focus_ring};
}}
QCheckBox:disabled, QRadioButton:disabled {{
    color: {c.text_disabled};
}}
QCheckBox::indicator, QRadioButton::indicator {{
    width: 16px;
    height: 16px;
    background-color: {c.surface_sunken};
    border: 1px solid {c.border_control};
}}
QCheckBox::indicator {{
    border-radius: {m.radius_small}px;
}}
QRadioButton::indicator {{
    border-radius: 9px;
}}
QCheckBox::indicator:hover, QRadioButton::indicator:hover {{
    border-color: {c.focus_ring};
}}
QCheckBox::indicator:checked {{
    background-color: {c.accent};
    border-color: {c.accent};
    image: url("{_icon("check.svg")}");
}}
QRadioButton::indicator:checked {{
    background-color: qradialgradient(cx:0.5, cy:0.5, radius:0.5, fx:0.5, fy:0.5,
        stop:0 {c.on_accent}, stop:0.35 {c.on_accent}, stop:0.45 {c.accent}, stop:1 {c.accent});
    border-color: {c.accent};
}}
QCheckBox::indicator:disabled, QRadioButton::indicator:disabled {{
    background-color: {c.surface};
    border-color: {c.border};
}}
QAbstractItemView::indicator {{
    width: 16px;
    height: 16px;
    background-color: {c.surface_sunken};
    border: 1px solid {c.border_control};
    border-radius: {m.radius_small}px;
}}
QAbstractItemView::indicator:hover {{
    border-color: {c.focus_ring};
}}
QAbstractItemView::indicator:checked {{
    background-color: {c.accent};
    border-color: {c.accent};
    image: url("{_icon("check.svg")}");
}}
"""


def _table_rules(c: ColorTokens = COLORS, m: Metrics = METRICS) -> str:
    """Result lists in dialogs, and the panel shown when a list is empty."""
    return f"""
QTableWidget, QTableView {{
    background-color: {c.surface_sunken};
    alternate-background-color: {c.surface};
    color: {c.text};
    border: 1px solid {c.border};
    border-radius: {m.radius}px;
    gridline-color: transparent;
    selection-background-color: {c.accent_subtle};
    selection-color: {c.text};
    outline: 0;
}}
QTableWidget::item, QTableView::item {{
    padding: 4px 8px;
    border: none;
}}
QTableWidget::item:selected, QTableView::item:selected {{
    background-color: {c.accent_subtle};
    color: {c.text};
}}
QHeaderView {{
    background: transparent;
    border: none;
}}
QHeaderView::section {{
    background-color: {c.surface_raised};
    color: {c.text_muted};
    border: none;
    border-bottom: 1px solid {c.border};
    padding: 6px 8px;
    font-size: {m.font_size_small + 1}px;
    font-weight: 600;
}}
QHeaderView::section:first {{
    border-top-left-radius: {m.radius}px;
}}
QHeaderView::section:last {{
    border-top-right-radius: {m.radius}px;
}}
QPushButton#gridChoice {{
    background-color: {c.surface_raised};
    color: {c.text_muted};
    border: 1px solid {c.border};
    border-radius: {m.radius}px;
    padding: 5px 12px;
    min-height: 0;
    font-weight: 600;
}}
QPushButton#gridChoice:hover {{
    background-color: {c.hover};
    color: {c.text};
}}
QPushButton#gridChoice:checked {{
    background-color: {c.accent_subtle};
    color: {c.text};
    border-color: {c.accent};
}}
QPushButton#gridChoice:focus {{
    border-color: {c.focus_ring};
}}
QFrame#emptyState {{
    background-color: {c.surface_sunken};
    border: 1px dashed {c.border_strong};
    border-radius: {m.radius}px;
}}
QFrame#driveRow {{
    background-color: {c.surface_raised};
    border: 1px solid {c.border};
    border-radius: {m.radius}px;
}}
QFrame#driveRow[current="true"] {{
    border-color: {c.accent};
}}
QLabel#emptyTitle {{
    color: {c.text};
    font-size: {m.font_size + 1}px;
    font-weight: 600;
}}
QLabel#emptyDetail {{
    color: {c.text_muted};
    font-size: {m.font_size_small + 1}px;
}}
"""


def _container_rules(c: ColorTokens = COLORS, m: Metrics = METRICS) -> str:
    return f"""
QDialog, QMessageBox {{
    background-color: {c.surface};
    color: {c.text};
    font-size: {m.font_size}px;
}}
QLabel {{
    color: {c.text};
    background: transparent;
}}
QDialog[chromeEffects="true"] {{
    background: transparent;
}}
QFrame#chromeFrame {{
    background-color: {c.surface};
    border: 1px solid {c.border_strong};
}}
QFrame#chromeFrame[rounded="true"] {{
    border-color: {c.border};
    border-radius: 12px;
}}
QWidget#chromeTitleBar {{
    background: transparent;
    border: none;
}}
QWidget#chromeTitleBar[variant="window"] {{
    background-color: {c.surface_sunken};
    border-bottom: 1px solid {c.border};
}}
QLabel#chromeTitle {{
    color: {c.text};
    font-size: {m.font_size_title - 1}px;
    font-weight: 700;
}}
QWidget#chromeTitleBar[variant="window"] QLabel#chromeTitle {{
    color: {c.text_muted};
    font-size: {m.font_size_small + 1}px;
    font-weight: 600;
}}
QPushButton#chromeButton {{
    background: transparent;
    color: {c.text_muted};
    border: none;
    border-radius: 15px;
    padding: 0;
    min-width: 30px;
    max-width: 30px;
    min-height: 30px;
    max-height: 30px;
    font-size: 13px;
}}
QWidget#chromeTitleBar[variant="window"] QPushButton#chromeButton {{
    border-radius: 6px;
    min-width: 34px;
    max-width: 34px;
    min-height: 26px;
    max-height: 26px;
}}
QWidget#chromeTitleBar QToolButton#pageControl {{
    background: transparent;
    color: {c.text_muted};
    border: none;
    border-radius: 6px;
    padding: 3px 9px;
    min-height: 20px;
    font-size: {m.font_size_small + 1}px;
    font-weight: 600;
}}
QWidget#chromeTitleBar QToolButton#pageControl:hover,
QWidget#chromeTitleBar QToolButton#pageControl:focus {{
    background: {c.hover};
    color: {c.text};
}}
QWidget#chromeTitleBar QToolButton#pageControl:checked {{
    background: {c.accent_subtle};
    color: {c.text};
}}
QWidget#chromeTitleBar QToolButton#pageControl:disabled {{
    color: {c.text_disabled};
}}
QWidget#chromeTitleBar QToolButton#pageControl::menu-indicator {{
    image: none;
    width: 0;
}}
QWidget#chromeTitleBar QToolButton#pageControl[popupMode="1"] {{
    padding-right: 20px;
}}
QWidget#chromeTitleBar QToolButton#pageControl::menu-button {{
    border: none;
    border-left: 1px solid {c.border};
    background: transparent;
    width: 16px;
    border-top-right-radius: 6px;
    border-bottom-right-radius: 6px;
}}
QWidget#chromeTitleBar QToolButton#pageControl::menu-button:hover {{
    background: {c.pressed};
}}
QWidget#chromeTitleBar QToolButton#pageControl::menu-arrow {{
    image: url("{_icon("chevron-down.svg")}");
    width: 9px;
    height: 9px;
}}
QWidget#chromeTitleBar QLabel#pageIndicator {{
    color: {c.text};
    font-size: {m.font_size_small + 1}px;
    font-weight: 600;
    padding: 0 2px;
}}
QWidget#chromeTitleBar QFrame#chromeSeparator {{
    background: {c.border};
    border: none;
    min-width: 1px;
    max-width: 1px;
    min-height: 16px;
    max-height: 16px;
}}
QPushButton#chromeButton:hover, QPushButton#chromeButton:focus {{
    background: {c.hover};
    color: {c.text};
}}
QPushButton#chromeButton:pressed {{
    background: {c.pressed};
}}
QPushButton#chromeButton[role="close"]:hover, QPushButton#chromeButton[role="close"]:focus {{
    background: {c.danger};
    color: {c.on_danger};
}}
QPushButton#chromeButton[role="close"]:pressed {{
    background: {c.danger_pressed};
    color: {c.on_danger};
}}
QLabel#dialogTitle {{
    font-size: {m.font_size_title}px;
    font-weight: 700;
    color: {c.text};
}}
QLabel#aboutName {{
    font-size: 22px;
    font-weight: 700;
    color: {c.text};
}}
QFrame#aboutRule {{
    border: none;
    background: {c.border};
    max-height: 1px;
}}
QLabel#dialogSubtitle, QLabel[role="hint"] {{
    color: {c.text_muted};
    font-size: {m.font_size_small + 1}px;
}}
QLabel[role="sectionLabel"] {{
    color: {c.text_muted};
    font-size: {m.font_size_small}px;
    font-weight: 600;
}}
QLabel[role="emphasis"] {{
    color: {c.text};
    font-weight: 700;
}}
QLabel[tone="success"] {{ color: {c.success}; }}
QLabel[tone="warning"] {{ color: {c.warning}; }}
QLabel[tone="error"] {{ color: {c.error}; }}
QLabel[tone="info"] {{ color: {c.info}; }}
QLabel[tone="muted"] {{ color: {c.text_muted}; }}
QLabel#previewSurface {{
    background-color: {c.surface_sunken};
    color: {c.text_muted};
    border: 1px dashed {c.border_strong};
    border-radius: {m.radius}px;
}}
QGroupBox {{
    background-color: {c.surface_raised};
    border: 1px solid {c.border};
    border-radius: {m.radius + 2}px;
    margin-top: 18px;
    padding: 12px 10px 8px 10px;
    font-size: {m.font_size}px;
}}
QGroupBox::title {{
    subcontrol-origin: margin;
    subcontrol-position: top left;
    left: 10px;
    padding: 0 4px;
    color: {c.text_muted};
    font-weight: 600;
}}
QFrame#settingsCard {{
    background-color: {c.surface_raised};
    border: 1px solid {c.border};
    border-radius: 10px;
}}
QFrame#settingsCard QLabel {{ background: transparent; }}
QLabel#cardTitle {{
    color: {c.text}; font-size: 14px; font-weight: 600; background: transparent;
}}
QFrame#optionCard {{
    background-color: {c.surface};
    border: 1px solid {c.border_strong};
    border-radius: 10px;
}}
QFrame#optionCard:hover {{ border-color: {c.border_control}; }}
QFrame#optionCard[checked="true"] {{
    background-color: {c.accent_subtle};
    border: 1px solid {c.accent};
}}
QFrame#optionCard QCheckBox {{ font-weight: 600; background: transparent; }}
QFrame#optionCard QLabel {{ background: transparent; }}
QWidget#segmented {{ background: transparent; }}
QWidget#segmented QPushButton {{
    background-color: {c.surface};
    color: {c.text_muted};
    border: 1px solid {c.border_strong};
    border-radius: 0;
    padding: 5px 14px;
    margin: 0;
    font-weight: 500;
}}
QWidget#segmented QPushButton:hover {{ color: {c.text}; background-color: {c.hover}; }}
QWidget#segmented QPushButton:checked {{
    background-color: {c.accent_subtle};
    color: {c.text};
    border-color: {c.accent};
}}
QWidget#segmented QPushButton:focus {{ border: 1px solid {c.focus_ring}; padding: 5px 14px; }}
QWidget#segmented QPushButton[segment="first"] {{
    border-top-left-radius: {m.radius}px; border-bottom-left-radius: {m.radius}px;
}}
QWidget#segmented QPushButton[segment="last"] {{
    border-top-right-radius: {m.radius}px; border-bottom-right-radius: {m.radius}px;
}}
QWidget#segmented QPushButton[segment="only"] {{ border-radius: {m.radius}px; }}
QProgressBar#motionMeter {{
    background: {c.surface_sunken}; border: none; border-radius: 3px; max-height: 6px;
}}
QProgressBar#motionMeter::chunk {{ background: #38bdf8; border-radius: 3px; }}
QTabWidget::pane {{
    border: 1px solid {c.border};
    border-radius: {m.radius}px;
    background-color: {c.surface};
    top: -1px;
}}
QTabBar::tab {{
    background-color: transparent;
    color: {c.text_muted};
    padding: 8px 16px;
    border: 2px solid transparent;
    border-bottom: 2px solid transparent;
    font-size: {m.font_size}px;
}}
QTabBar::tab:hover {{
    color: {c.text};
}}
QTabBar::tab:selected {{
    color: {c.text};
    border-bottom: 2px solid {c.accent};
    font-weight: 600;
}}
QTabBar::tab:focus {{
    border: 2px solid {c.focus_ring};
    border-radius: {m.radius_small}px;
}}
QListWidget {{
    background-color: {c.surface_sunken};
    color: {c.text};
    border: 1px solid {c.border_control};
    border-radius: {m.radius}px;
    padding: 4px;
    outline: 0;
}}
QListWidget:focus {{
    border: 2px solid {c.focus_ring};
}}
QListWidget::item {{
    padding: 4px 6px;
    border-radius: {m.radius_small}px;
}}
QListWidget::item:hover {{
    background-color: {c.hover};
}}
QListWidget::item:selected {{
    background-color: {c.accent};
    color: {c.on_accent};
}}
QScrollArea {{
    background: transparent;
    border: none;
}}
QListWidget#settingsNav {{
    background: transparent;
    border: none;
    border-right: 1px solid {c.border};
    border-radius: 0;
    padding: 0 10px 0 0;
}}
QListWidget#settingsNav::item {{
    color: {c.text_muted};
    padding: 0 12px;
    border-radius: {m.radius}px;
}}
QListWidget#settingsNav::item:hover {{ background-color: {c.hover}; color: {c.text}; }}
QListWidget#settingsNav::item:selected {{
    background-color: {c.accent_subtle};
    color: {c.text};
}}
QWidget#settingsPage {{ background: transparent; }}
QLabel#settingsPageTitle {{
    color: {c.text}; font-size: 18px; font-weight: 600; background: transparent;
}}
QWidget#cameraRecordingPage {{
    background-color: {c.surface};
}}
""" + _scrollbar_rules(c)


def dialog_stylesheet(theme_name: str | None = None) -> str:
    """Complete style sheet for a themed dialog and all of its child controls."""
    return _dialog_stylesheet(normalize_theme(theme_name or _active_theme))


@cache
def _dialog_stylesheet(theme_name: str) -> str:
    c = active_colors(theme_name)
    return "".join(
        (
            _container_rules(c),
            _table_rules(c),
            _input_rules(c),
            _button_rules(c),
            _choice_rules(c),
            _calendar_rules(c),
            _menu_rules(c),
            _tooltip_rules(c),
        )
    )


def menu_stylesheet(theme_name: str | None = None) -> str:
    """Style sheet for popup/context menus (applies to submenus too)."""
    return _menu_rules(active_colors(theme_name))


def datetime_input_stylesheet(theme_name: str | None = None) -> str:
    """Style sheet for date/time inputs embedded outside dialogs, incl. calendar popup."""
    c = active_colors(theme_name)
    return _input_rules(c) + _calendar_rules(c) + _menu_rules(c)


def _event_review_rules(c: ColorTokens) -> str:
    """The event list and detail pane on the Events and Playback pages.
    Explicit surfaces prevent white Qt scroll viewports in dark pages."""
    return f"""
QWidget#eventPreview {{
    background: {c.surface}; color: {c.text};
    border: 1px solid {c.border}; border-radius: 12px;
}}
QWidget#playbackEvents {{
    background: {c.surface}; color: {c.text};
    border: 1px solid {c.border}; border-radius: 12px;
}}
QWidget#playbackEvents QWidget#eventPreview {{
    border: none; border-radius: 0; background: transparent;
}}
QWidget#eventPreview QLabel, QWidget#playbackEvents QLabel {{
    background: transparent; color: {c.text}; border: none;
}}
QWidget#eventPreview QLabel[role="muted"], QWidget#playbackEvents QLabel[role="muted"] {{
    color: {c.text_muted};
}}
QWidget#eventPreview QLabel#eventPreviewTitle {{
    font-size: 15px; font-weight: 600;
}}
QWidget#eventPreview QLabel#eventPreviewEmptyTitle {{
    color: {c.text}; font-size: 15px; font-weight: 600;
}}
QWidget#eventPreview QLabel#eventPreviewState {{
    border-radius: 10px; padding: 2px 9px; font-size: 11px; font-weight: 700;
}}
QWidget#eventPreview QLabel#eventPreviewState[tone="warning"] {{
    color: {c.warning}; background: {_tint(c.warning, c.surface, 0.16)};
}}
QWidget#eventPreview QLabel#eventPreviewState[tone="info"] {{
    color: {c.info}; background: {_tint(c.info, c.surface, 0.16)};
}}
QListView#eventList, QWidget#eventsHistory QListView#eventList,
QWidget#playbackEvents QListView#eventList {{
    background: transparent; border: none; outline: none; color: {c.text};
}}
QWidget#eventPreview QStackedWidget, QWidget#eventPreview QWidget#eventPreviewPage,
QWidget#eventsHistory QStackedWidget, QWidget#eventsHistory QWidget#eventsEmpty,
QWidget#playbackEvents QSplitter, QWidget#playbackEvents QWidget#eventListBox,
QWidget#playbackEvents QCheckBox {{
    background: transparent;
}}
QScrollArea#playbackEventsScroll, QWidget#playbackEventsViewport {{
    background: {c.window}; border: none;
}}
"""


def _tint(color: str, background: str, amount: float) -> str:
    """``amount`` of ``color`` over ``background``, as an opaque colour."""
    fg, bg = QColor(color), QColor(background)
    mixed = QColor(
        round(fg.red() * amount + bg.red() * (1 - amount)),
        round(fg.green() * amount + bg.green() * (1 - amount)),
        round(fg.blue() * amount + bg.blue() * (1 - amount)),
    )
    return str(mixed.name())


def events_stylesheet(theme_name: str | None = None) -> str:
    """Events page: heading, filter row, event list and details."""
    c = active_colors(theme_name)
    return (
        scope_stylesheet(
            _input_rules(c) + _button_rules(c) + _container_rules(c) + _calendar_rules(c),
            "QWidget#eventsPage",
        )
        + f"""
QWidget#eventsPage {{ background: {c.window}; }}
QWidget#eventsHistory {{
    background: {c.surface}; border: 1px solid {c.border}; border-radius: 12px;
}}
QWidget#eventsFooter {{
    background: transparent; border: none; border-top: 1px solid {c.border};
    border-radius: 0;
}}
QWidget#eventsPage QLabel[role="pageTitle"] {{
    color: {c.text}; font-size: 23px; font-weight: 700;
}}
QWidget#eventsPage QLabel[role="sectionTitle"] {{
    color: {c.text}; font-size: 14px; font-weight: 600;
}}
QWidget#eventsPage QLabel[role="muted"] {{ color: {c.text_muted}; font-size: 12px; }}
QWidget#eventsPage QLabel#eventsCount {{
    color: {c.text_muted}; font-size: 12px; padding: 0 8px 6px 0;
}}
QWidget#eventsPage QLineEdit#eventsSearch {{ padding-left: 10px; }}
QWidget#eventsPage QLabel#eventsEmptyTitle {{
    color: {c.text}; font-size: 18px; font-weight: 600;
}}
QWidget#eventsPage QSplitter::handle {{ background: transparent; }}
"""
        + _event_review_rules(c)
    )


def playback_stylesheet(theme_name: str | None = None) -> str:
    """Playback page chrome; selectors leave video tiles and other pages alone."""
    c = active_colors(theme_name)
    base = scope_stylesheet(
        _input_rules(c) + _button_rules(c) + _choice_rules(c) + _container_rules(c),
        "QWidget#playbackPage",
    )
    return (
        base
        + _event_review_rules(c)
        + f"""
QWidget#playbackPage {{ background: {c.window}; }}
QWidget#playbackSidebar, QWidget#playbackTimelineCard {{
    background: {c.surface}; border: 1px solid {c.border}; border-radius: 10px;
}}
QWidget#playbackPage QLabel[role="pageTitle"] {{
    color: {c.text}; font-size: 23px; font-weight: 700;
}}
QWidget#playbackPage QLabel[role="sectionTitle"] {{
    color: {c.text}; font-size: 13px; font-weight: 600;
}}
QWidget#playbackPage QLabel[role="muted"] {{ color: {c.text_muted}; font-size: 12px; }}
QWidget#playbackPage QLabel#playbackSelectionCount {{
    color: {c.info}; background: {c.accent_subtle}; border-radius: 5px;
    padding: 3px 7px; font-size: 11px;
}}
QWidget#playbackPage QListWidget#playbackCameras {{
    background: {c.surface_sunken}; color: {c.text};
    border: 1px solid {c.border}; border-radius: 7px; outline: none;
}}
QWidget#playbackPage QListWidget#playbackCameras::item {{
    padding: 9px 8px; border-radius: 4px; font-size: 13px;
}}
QWidget#playbackPage QListWidget#playbackCameras::item:hover {{ background: {c.hover}; }}
QWidget#playbackPage QListWidget#playbackCameras::item:selected {{
    background: {c.accent_subtle}; color: {c.text};
}}
QWidget#playbackPage QListWidget#playbackCameras::indicator {{
    width: 16px; height: 16px; border: 1px solid {c.border_control};
    border-radius: 4px; background: {c.surface};
}}
QWidget#playbackPage QListWidget#playbackCameras::indicator:checked {{
    background: {c.accent}; border-color: {c.accent}; image: url("{icon_path("check.svg")}");
}}
QWidget#playbackPage QGroupBox#playbackRange {{
    border: none; margin-top: 18px; padding-top: 6px; color: {c.text}; font-weight: 600;
}}
QWidget#playbackPage QGroupBox#playbackRange::title {{
    subcontrol-origin: margin; subcontrol-position: top left; padding: 0;
}}
QWidget#playbackPage QLabel#playbackPlaceholder {{
    color: {COLORS.text_muted}; font-size: 14px; padding: 32px;
}}
QWidget#playbackPage QSplitter::handle {{ background: {c.window}; }}
QWidget#playbackPage QSplitter::handle:hover {{ background: {c.border_strong}; }}
"""
    )


def playback_transport_stylesheet(theme_name: str | None = None) -> str:
    """Compact playback controls, shared with the clip trim timeline."""
    c = active_colors(theme_name)
    return f"""
QWidget#playbackTransport {{ background: {c.surface}; border-top: 1px solid {c.border}; }}
QWidget#playbackTransport QPushButton {{
    background: {c.surface_raised}; color: {c.text_muted}; border: 1px solid {c.border};
    border-radius: 6px; padding: 0; font-size: 11px; font-weight: 500;
}}
QWidget#playbackTransport QPushButton:hover {{ background: {c.hover}; color: {c.text}; }}
QWidget#playbackTransport QPushButton:pressed {{ background: {c.pressed}; }}
QWidget#playbackTransport QPushButton:checked {{
    background: {c.accent_subtle}; color: {c.info}; border-color: {c.accent};
}}
QWidget#playbackTransport QPushButton:focus {{ border: 1px solid {c.focus_ring}; }}
QWidget#playbackTransport QPushButton#playbackPlay {{
    background: {c.accent}; color: {c.on_accent}; border-color: {c.accent}; font-weight: 600;
}}
QWidget#playbackTransport QPushButton#playbackPlay:hover {{ background: {c.accent_hover}; }}
QWidget#playbackTransport QPushButton:disabled,
QWidget#playbackTransport QPushButton#playbackPlay:disabled {{
    background: {c.surface}; color: {c.text_disabled}; border-color: {c.border};
}}
QWidget#playbackTransport QLabel {{ background: transparent; color: {c.text_muted}; }}
QWidget#playbackTransport QLabel#playbackMotionLegend {{ color: #f97316; font-size: 11px; }}
QWidget#playbackTransport QLabel#playbackClock {{
    color: {c.text}; background: {c.surface_sunken}; border-radius: 5px; padding: 5px 8px;
}}
"""


def _scrollbar_rules(c: ColorTokens = COLORS) -> str:
    """Slim rounded scroll bars without arrow buttons, in both directions."""
    return f"""
QScrollBar:vertical {{
    background: transparent; width: 10px; margin: 2px; border: none;
}}
QScrollBar:horizontal {{
    background: transparent; height: 10px; margin: 2px; border: none;
}}
QScrollBar::handle:vertical {{
    background: {c.border_strong}; border-radius: 3px; min-height: 24px;
}}
QScrollBar::handle:horizontal {{
    background: {c.border_strong}; border-radius: 3px; min-width: 24px;
}}
QScrollBar::handle:vertical:hover, QScrollBar::handle:horizontal:hover {{
    background: {c.text_disabled};
}}
QScrollBar::add-line, QScrollBar::sub-line {{ width: 0; height: 0; border: none; }}
QScrollBar::add-page, QScrollBar::sub-page {{ background: transparent; }}
"""


def app_stylesheet(theme_name: str | None = None) -> str:
    """Application-wide rules: popup classes (menus, tooltips, message
    boxes) and scroll bars."""
    c = active_colors(theme_name)
    return (
        _scrollbar_rules(c)
        + _tooltip_rules(c)
        + _menu_rules(c)
        + _message_box_container_rules(c)
        + scope_stylesheet(_button_rules(c) + _choice_rules(c), "QMessageBox")
    )


def scope_stylesheet(rules: str, ancestor: str) -> str:
    """Prefix every selector in ``rules`` with ``ancestor`` (descendant combinator)."""
    blocks: list[str] = []
    for chunk in rules.split("}"):
        if "{" not in chunk:
            blocks.append(chunk)
            continue
        selectors, body = chunk.split("{", 1)
        scoped = ",\n".join(
            f"{ancestor} {sel.strip()}" for sel in selectors.split(",") if sel.strip()
        )
        blocks.append(f"\n{scoped} {{{body}")
    return "}".join(blocks)


def _message_box_container_rules(c: ColorTokens = COLORS, m: Metrics = METRICS) -> str:
    return f"""
QMessageBox {{
    background-color: {c.surface};
    font-size: {m.font_size}px;
}}
QMessageBox QLabel {{
    color: {c.text};
    background: transparent;
}}
"""


# ---------------------------------------------------------------------------
# Application helpers
# ---------------------------------------------------------------------------


def set_button_variant(button: QAbstractButton, variant: str) -> None:
    """Assign a semantic variant (``primary``/``secondary``/``danger``/``ghost``)."""
    if variant not in BUTTON_VARIANTS:
        raise ValueError(f"unknown button variant: {variant!r}")
    button.setProperty("variant", variant)
    _repolish(button)


def set_status_tone(label: QLabel, tone: str) -> None:
    """Colour a status label semantically (``success``/``warning``/``error``/…)."""
    if tone not in STATUS_TONES:
        raise ValueError(f"unknown status tone: {tone!r}")
    label.setProperty("tone", tone)
    _repolish(label)


def set_label_role(label: QLabel, role: str) -> None:
    """Typographic role for a label: ``hint``, ``sectionLabel`` or ``emphasis``."""
    label.setProperty("role", role)
    _repolish(label)


def style_button_box(box: QDialogButtonBox) -> None:
    """Map standard dialog button roles onto semantic variants.

    A lone button is the dialog's primary action; otherwise accept-type roles are
    primary, destructive roles are danger and everything else is secondary.
    """
    buttons = box.buttons()
    for button in buttons:
        role = box.buttonRole(button)
        if len(buttons) == 1:
            variant = "primary"
        elif role == QDialogButtonBox.ButtonRole.DestructiveRole:
            variant = "danger"
        elif role in (
            QDialogButtonBox.ButtonRole.AcceptRole,
            QDialogButtonBox.ButtonRole.YesRole,
            QDialogButtonBox.ButtonRole.ApplyRole,
        ):
            variant = "primary"
        else:
            variant = "secondary"
        set_button_variant(button, variant)


def apply_dialog_theme(widget: QWidget, theme_name: str | None = None) -> None:
    """Apply the dialog style sheet to ``widget`` and everything it contains."""
    widget.setProperty("nwThemed", True)
    widget.setStyleSheet(dialog_stylesheet(theme_name))


def themed_menu(parent: QWidget | None = None, title: str = "") -> QMenu:
    """Create a ``QMenu`` carrying the shared menu style (inherited by submenus)."""
    menu = QMenu(title, parent)
    menu.setStyleSheet(menu_stylesheet())
    return menu


def style_calendar(calendar: QCalendarWidget) -> None:
    """Clean calendar grid: no week numbers or grid lines, muted day-of-week header."""
    calendar.setGridVisible(False)
    calendar.setVerticalHeaderFormat(QCalendarWidget.VerticalHeaderFormat.NoVerticalHeader)
    header = QTextCharFormat()
    colors = active_colors()
    header.setForeground(QColor(colors.text_muted))
    header.setBackground(QColor(colors.surface_sunken))
    calendar.setHeaderTextFormat(header)
    weekend = QTextCharFormat()
    weekend.setForeground(QColor(colors.warning))
    for day in (Qt.DayOfWeek.Saturday, Qt.DayOfWeek.Sunday):
        calendar.setWeekdayTextFormat(day, weekend)


def apply_app_theme(app: QApplication, theme_name: str = "dark") -> None:
    """Apply a real Fusion light/dark palette and shared popup rules."""
    global _active_theme
    _active_theme = normalize_theme(theme_name)
    colors = active_colors()
    sheet = app_stylesheet(_active_theme)
    current = app.styleSheet()
    previous = app.property("nwThemeStylesheet") or ""
    if previous == sheet and sheet in current and app.property("nwFusionStyle"):
        # Already applied. Setting the style, palette or style sheet again
        # restyles every widget of the app (1.8 s with a full wall): a settings
        # reload after each camera save froze the window.
        return
    app.setStyle("Fusion")
    app.setProperty("nwFusionStyle", True)
    if sys.platform == "win32":
        # Crisp text like native Windows apps: Qt's DirectWrite engine hints
        # only lightly by default, which looks soft at 125% / 150% scaling.
        font = app.font()
        font.setHintingPreference(QFont.HintingPreference.PreferFullHinting)
        app.setFont(font)
    palette = app.palette()
    from PySide6.QtGui import QPalette

    palette.setColor(QPalette.ColorRole.Window, QColor(colors.window))
    palette.setColor(QPalette.ColorRole.WindowText, QColor(colors.text))
    palette.setColor(QPalette.ColorRole.Base, QColor(colors.surface_sunken))
    palette.setColor(QPalette.ColorRole.AlternateBase, QColor(colors.surface_raised))
    palette.setColor(QPalette.ColorRole.Text, QColor(colors.text))
    palette.setColor(QPalette.ColorRole.Button, QColor(colors.surface_raised))
    palette.setColor(QPalette.ColorRole.ButtonText, QColor(colors.text))
    palette.setColor(QPalette.ColorRole.Highlight, QColor(colors.accent))
    palette.setColor(QPalette.ColorRole.HighlightedText, QColor(colors.on_accent))
    palette.setColor(QPalette.ColorRole.ToolTipBase, QColor(colors.surface_raised))
    palette.setColor(QPalette.ColorRole.ToolTipText, QColor(colors.text))
    app.setPalette(palette)

    if previous and previous in current:
        current = current.replace(previous, "")
    app.setStyleSheet(current + sheet)
    app.setProperty("nwThemeStylesheet", sheet)


def _repolish(widget: QWidget) -> None:
    style = widget.style()
    style.unpolish(widget)
    style.polish(widget)
    widget.update()
