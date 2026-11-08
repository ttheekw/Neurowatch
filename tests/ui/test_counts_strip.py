"""Synthetic figures verify the compact overlay, with no cameras or plugin processes."""
from PySide6.QtCore import QRect, Qt
from PySide6.QtWidgets import QWidget

from neurowatch.models.analytics import Figure
from neurowatch.widgets.figures_legend import FiguresLegend


def _figures():
    labels = [("People", "person", "12"), ("Cars", "car", "373"),
              ("Motorbikes", "motorbike", "129"), ("Buses", "bus", "44"),
              ("Three-wheelers", "three_wheeler", "191"), ("Bicycles", "bicycle", "2"),
              ("Vans", "van", "14"), ("Lorries", "lorry", "24")]
    figures = [Figure(label, count, icon, f"series{i+1}")
               for i, (label, icon, count) in enumerate(labels)]
    return (*figures, Figure("Total today", "789", "total"))


def test_floating_counts_and_total_fit_one_slim_row_with_labels(qtbot):
    host = QWidget()
    host.setStyleSheet("QWidget { background-color:#12121c; }")
    qtbot.addWidget(host)
    host.resize(1160, 600)
    host.show()
    panel = FiguresLegend(host, floating=True)
    panel.set_figures(_figures())
    panel.place_in(host.rect())
    panel.layout().activate()
    assert not panel.isHidden()
    assert len({cell.y() for cell in panel._cells}) == 1, "total stays beside the categories"
    assert panel.height() <= 46, "one slim strip leaves the video visible"
    assert all(cell.name.isVisibleTo(cell) for cell in panel._cells)
    assert all(cell.geometry().right() < panel.width() for cell in panel._cells)
    assert all(cell.icon.width() <= 20 for cell in panel._cells)
    assert panel._cells[-1].number.text() == "789"
    assert panel._cells[-1].property("total") is True
    assert panel.testAttribute(Qt.WidgetAttribute.WA_TransparentForMouseEvents)


def test_counts_preserve_large_values_and_return_to_one_row_after_resizing(qtbot):
    host = QWidget()
    qtbot.addWidget(host)
    host.resize(1600, 900)
    host.show()
    panel = FiguresLegend(host, floating=True)
    figures = tuple(Figure(f.label, "1234567", f.icon, f.style) for f in _figures())
    panel.set_figures(figures)
    for width in (1160, 590, 390, 1160):
        panel.place_in(QRect(0, 0, width, 600))
        panel.layout().activate()
        assert not panel.isHidden()
        assert all(cell.number.text() == "1,234,567" for cell in panel._cells)
        assert all(cell.width() >= cell.minimumSizeHint().width() for cell in panel._cells)
        assert all(cell.geometry().right() < width for cell in panel._cells)
        if width == 1160:
            assert len({cell.y() for cell in panel._cells}) == 1


def test_outline_icons_are_crisp_at_high_dpi_and_reject_unknown_names(qtbot):
    from neurowatch.widgets.count_icons import count_icon
    from neurowatch.widgets.figures_legend import _ICON_NAMES

    for name in _ICON_NAMES:
        pixmap = count_icon(name, "#67e8f9", 18, 2.0)
        assert not pixmap.isNull()
        assert pixmap.width() == 36
        assert pixmap.devicePixelRatio() == 2.0
    assert count_icon("../secret", "#67e8f9", 18, 1.0).isNull()


def test_small_tile_keeps_the_total_on_the_same_row_for_normal_counts(qtbot):
    host = QWidget()
    qtbot.addWidget(host)
    host.resize(390, 220)
    host.show()
    panel = FiguresLegend(host, floating=True)
    panel.set_figures(_figures())
    panel.place_in(host.rect())
    # Density changes trigger Qt's queued layout pass before painting.
    qtbot.waitUntil(lambda: len({cell.y() for cell in panel._cells}) == 1)
    assert panel.height() <= 34
    assert all(cell.number.isVisibleTo(panel) for cell in panel._cells)
