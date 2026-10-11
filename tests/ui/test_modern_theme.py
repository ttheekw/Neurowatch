"""Centralized theme: tokens, contrast, focus states, button variants, menus."""
from __future__ import annotations

import pytest
from PySide6.QtGui import QPalette
from PySide6.QtWidgets import QApplication, QMenu, QPushButton, QWidget

from neurowatch import theme


def test_tokens_meet_wcag_aa_text_contrast():
    c = theme.COLORS
    assert theme.contrast_ratio(c.text, c.surface) >= 7.0
    assert theme.contrast_ratio(c.text_muted, c.surface) >= 4.5
    assert theme.contrast_ratio(c.text_muted, c.surface_sunken) >= 4.5
    assert theme.contrast_ratio(c.on_accent, c.accent) >= 4.5
    assert theme.contrast_ratio(c.on_danger, c.danger) >= 4.5
    for tone in (c.success, c.warning, c.error, c.info):
        assert theme.contrast_ratio(tone, c.surface) >= 4.5


def test_contrast_ratio_extremes():
    assert theme.contrast_ratio("#000000", "#ffffff") == pytest.approx(21.0)
    assert theme.contrast_ratio("#777777", "#777777") == pytest.approx(1.0)


def test_dialog_stylesheet_has_explicit_focus_states_for_every_control():
    qss = theme.dialog_stylesheet()
    for selector in (
        "QLineEdit:focus",
        "QTextEdit:focus",
        "QComboBox:focus",
        "QSpinBox:focus",
        "QDateTimeEdit:focus",
        "QPushButton:focus",
        "QCheckBox:focus",
        "QRadioButton:focus",
        "QListWidget:focus",
        "QTabBar::tab:focus",
    ):
        assert selector in qss, selector
    assert theme.COLORS.focus_ring in qss


def test_dialog_stylesheet_defines_semantic_button_variants():
    qss = theme.dialog_stylesheet()
    for variant in theme.BUTTON_VARIANTS:
        assert f'QPushButton[variant="{variant}"]' in qss
    assert theme.BUTTON_VARIANTS == ("primary", "secondary", "danger", "ghost")


def test_set_button_variant_sets_property(qtbot):
    btn = QPushButton("Save")
    qtbot.addWidget(btn)
    theme.set_button_variant(btn, "danger")
    assert btn.property("variant") == "danger"
    theme.set_button_variant(btn, "primary")
    assert btn.property("variant") == "primary"


def test_set_button_variant_rejects_unknown(qtbot):
    btn = QPushButton("x")
    qtbot.addWidget(btn)
    with pytest.raises(ValueError):
        theme.set_button_variant(btn, "shiny")


def test_apply_dialog_theme_marks_and_styles_widget(qtbot):
    w = QWidget()
    qtbot.addWidget(w)
    theme.apply_dialog_theme(w)
    assert w.property("nwThemed") is True
    assert w.styleSheet() == theme.dialog_stylesheet()


def test_themed_menu_has_hover_disabled_and_separator_states(qtbot):
    parent = QWidget()
    qtbot.addWidget(parent)
    menu = theme.themed_menu(parent)
    assert isinstance(menu, QMenu)
    assert menu.parent() is parent
    qss = menu.styleSheet()
    for selector in ("QMenu::item:selected", "QMenu::item:disabled", "QMenu::separator"):
        assert selector in qss


def test_apply_app_theme_covers_popups_without_restyling_generic_widgets(qtbot):
    app = QApplication.instance()
    previous = app.styleSheet()
    try:
        theme.apply_app_theme(app)
        qss = app.styleSheet()
        assert "QMenu" in qss
        assert "QToolTip" in qss
        assert "QMessageBox" in qss
        # The video wall and toolbars must not be restyled by global rules.
        assert "\nQPushButton {" not in qss
        assert "\nQWidget {" not in qss
        # With a style sheet active Qt wraps the base style; inspect it unwrapped.
        app.setStyleSheet("")
        assert app.style().name().lower() == "fusion"
    finally:
        app.setStyleSheet(previous)


def test_light_theme_changes_palette_and_dialog_styles(qtbot):
    app = QApplication.instance()
    try:
        theme.apply_app_theme(app, "light")
        assert app.palette().color(QPalette.ColorRole.Window).name() == theme.LIGHT_COLORS.window
        assert theme.LIGHT_COLORS.surface in theme.dialog_stylesheet()
        assert theme.dialog_stylesheet("light") != theme.dialog_stylesheet("dark")
    finally:
        theme.apply_app_theme(app, "dark")


def test_status_tone_property(qtbot):
    from PySide6.QtWidgets import QLabel

    lbl = QLabel()
    qtbot.addWidget(lbl)
    theme.set_status_tone(lbl, "error")
    assert lbl.property("tone") == "error"
    with pytest.raises(ValueError):
        theme.set_status_tone(lbl, "loud")
    for tone in theme.STATUS_TONES:
        assert f'QLabel[tone="{tone}"]' in theme.dialog_stylesheet()


# ---------------------------------------------------------------------------
# WCAG 1.4.11 non-text contrast (>= 3:1 against the adjacent surface)
# ---------------------------------------------------------------------------

_NON_TEXT_MIN = 3.0


def _rule(qss: str, selector: str) -> dict[str, str]:
    """Merged declarations of every block whose selector list contains ``selector``."""
    props: dict[str, str] = {}
    for chunk in qss.split("}"):
        if "{" not in chunk:
            continue
        selectors, body = chunk.split("{", 1)
        if selector not in {s.strip() for s in selectors.split(",")}:
            continue
        for decl in body.split(";"):
            if ":" in decl:
                key, value = decl.split(":", 1)
                props[key.strip()] = value.strip()
    assert props, f"no rule for {selector!r}"
    return props


def _color(value: str) -> str:
    return next(tok for tok in value.split() if tok.startswith("#"))


def _assert_non_text(fg: str, bg: str, what: str) -> None:
    ratio = theme.contrast_ratio(fg, bg)
    assert ratio >= _NON_TEXT_MIN, f"{what}: {fg} on {bg} = {ratio:.2f}:1"


def test_selected_menu_list_and_combo_items_contrast_with_adjacent_surface():
    qss = theme.dialog_stylesheet()
    menu_bg = _color(_rule(qss, "QMenu")["background-color"])
    menu_sel = _color(_rule(qss, "QMenu::item:selected")["background-color"])
    _assert_non_text(menu_sel, menu_bg, "menu selected item")

    list_bg = _color(_rule(qss, "QListWidget")["background-color"])
    list_sel = _color(_rule(qss, "QListWidget::item:selected")["background-color"])
    _assert_non_text(list_sel, list_bg, "list selected item")

    popup = _rule(qss, "QComboBox QAbstractItemView")
    _assert_non_text(
        _color(popup["selection-background-color"]),
        _color(popup["background-color"]),
        "combo popup selected item",
    )


def test_selected_item_text_stays_readable_on_selection_background():
    qss = theme.dialog_stylesheet()
    for selector in ("QMenu::item:selected", "QListWidget::item:selected"):
        rule = _rule(qss, selector)
        assert theme.contrast_ratio(_color(rule["color"]), _color(rule["background-color"])) >= 4.5
    popup = _rule(qss, "QComboBox QAbstractItemView")
    assert (
        theme.contrast_ratio(
            _color(popup["selection-color"]), _color(popup["selection-background-color"])
        )
        >= 4.5
    )


def test_input_and_choice_indicator_borders_contrast_with_containing_surfaces():
    qss = theme.dialog_stylesheet()
    surfaces = {
        "dialog": _color(_rule(qss, "QDialog")["background-color"]),
        "group": _color(_rule(qss, "QGroupBox")["background-color"]),
    }
    borders = {
        "input": _color(_rule(qss, "QLineEdit")["border"]),
        "input:hover": _color(_rule(qss, "QLineEdit:hover")["border-color"]),
        "checkbox indicator": _color(_rule(qss, "QCheckBox::indicator")["border"]),
        "radio indicator": _color(_rule(qss, "QRadioButton::indicator")["border"]),
        "list": _color(_rule(qss, "QListWidget")["border"]),
    }
    for field in ("QComboBox", "QSpinBox", "QDateTimeEdit", "QTextEdit", "QPlainTextEdit"):
        assert _color(_rule(qss, field)["border"]) == borders["input"], field
    for name, border in borders.items():
        for surface_name, surface in surfaces.items():
            _assert_non_text(border, surface, f"{name} border on {surface_name}")


def test_stylesheet_icon_urls_are_quoted():
    import re

    for qss in (theme.dialog_stylesheet(), theme.app_stylesheet(), theme.datetime_input_stylesheet()):
        urls = re.findall(r"url\(([^)]*)\)", qss)
        assert urls
        for url in urls:
            assert url.startswith('"') and url.endswith('"'), url
