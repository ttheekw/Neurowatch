"""Right-side camera panel — search, status list, filter tabs, actions."""

from __future__ import annotations

from PySide6.QtCore import QMimeData, QSize, Qt, Signal
from PySide6.QtGui import QResizeEvent
from PySide6.QtWidgets import (
    QAbstractItemView,
    QApplication,
    QButtonGroup,
    QComboBox,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QListWidget,
    QListWidgetItem,
    QMenu,
    QPushButton,
    QSizePolicy,
    QVBoxLayout,
    QWidget,
)

from neurowatch.models.camera import CameraConfig, connection_status
from neurowatch.theme import arrow_rules, themed_menu

_BG = "#111120"
_HEADER_BG = "#0d0d1a"
_ITEM_HOVER = "#1a1a30"
_ITEM_SEL = "#1e2848"
_ACCENT = "#4e9af1"
_TEXT = "#c0c0d4"
_SUBTEXT = "#707088"
_DOT_GREEN = "#22c55e"
_DOT_RED = "#ef4444"
_DOT_AMBER = "#f59e0b"
_DOT_GRAY = "#525268"

_PANEL_W = 210
_ONLINE_STATUSES = {"streaming", "connecting", "reconnecting"}

_BTN_STYLE = """
QPushButton {
    background: #1a1a2e; color: #c0c0d4; border: 1px solid #252540;
    border-radius: 3px; padding: 4px 10px; font-size: 11px;
}
QPushButton:hover { background: #222240; }
"""
_FILTER_BTN = """
QPushButton {{
    background: {bg}; color: {fg}; border: 1px solid #252540;
    border-radius: 3px; padding: 2px 10px; font-size: 11px;
}}
QPushButton:hover {{ background: #20204a; }}
"""
_SEARCH_STYLE = """
QLineEdit {
    background: #18182c; color: #c0c0d4; border: 1px solid #252540;
    border-radius: 3px; padding: 4px 8px; font-size: 11px;
}
QLineEdit:focus { border-color: #4e9af1; }
QLineEdit::placeholder { color: #505068; }
"""


def _dot_label(color: str) -> QLabel:
    lbl = QLabel("●")
    lbl.setStyleSheet(f"color: {color}; font-size: 9px;")
    lbl.setFixedWidth(14)
    return lbl


class _CameraListWidget(QListWidget):
    """List widget that encodes camera id in drag MIME text."""

    def mimeData(self, items: list[QListWidgetItem]) -> QMimeData | None:  # type: ignore[override]
        mime = super().mimeData(items)
        if mime is None:
            return None
        if items:
            cam_id = items[0].data(Qt.ItemDataRole.UserRole)
            if cam_id:
                mime.setText(f"neurowatch/cam_id:{cam_id}")
        return mime


class _ElidedLabel(QLabel):
    """A name that ends in "…" when the panel is too narrow, never widening it."""

    def __init__(self, text: str) -> None:
        super().__init__(text)
        self._full = text
        self.setSizePolicy(QSizePolicy.Policy.Ignored, QSizePolicy.Policy.Preferred)
        self.setMinimumWidth(20)

    def resizeEvent(self, event: QResizeEvent) -> None:
        super().resizeEvent(event)
        elided = self.fontMetrics().elidedText(
            self._full, Qt.TextElideMode.ElideRight, max(0, self.width())
        )
        if elided != self.text():
            self.setText(elided)


class _CameraItem(QWidget):
    selected = Signal(str)

    def __init__(self, cam: CameraConfig, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.cam_id = cam.id
        self._status = "offline"
        self._styled = False  # first set_status() always applies its style
        self._recording = "idle"

        layout = QHBoxLayout(self)
        layout.setContentsMargins(8, 4, 8, 4)
        layout.setSpacing(6)

        self._dot = _dot_label(_DOT_GRAY)
        self._dot.setToolTip("Camera connection")
        layout.addWidget(self._dot)
        self._recording_dot = _dot_label(_DOT_RED)
        # Keep the dot's slot while hidden so every camera name lines up.
        policy = self._recording_dot.sizePolicy()
        policy.setRetainSizeWhenHidden(True)
        self._recording_dot.setSizePolicy(policy)
        self._recording_dot.hide()
        layout.addWidget(self._recording_dot)

        name_lbl = _ElidedLabel(cam.name or cam.url)
        name_lbl.setStyleSheet(f"color: {_TEXT}; font-size: 11px;")
        name_lbl.setToolTip(cam.name or cam.url)
        layout.addWidget(name_lbl, 1)

        self._badge = QLabel()
        self._badge.setFixedHeight(16)
        self._badge.setStyleSheet(
            "font-size: 9px; font-weight: bold; padding: 0 5px; border-radius: 3px;"
        )
        layout.addWidget(self._badge)
        self._set_badge("offline")

        self._motion = QLabel("Motion")
        self._motion.setFixedHeight(16)
        self._motion.setToolTip("The camera reports motion now")
        self._motion.setStyleSheet(
            "background: #ea580c; color: #fff7ed; font-size: 9px; font-weight: bold;"
            " padding: 0 5px; border-radius: 3px;"
        )
        self._motion.hide()
        layout.addWidget(self._motion)

    def set_motion(self, active: bool) -> None:
        self._motion.setVisible(active)

    def set_status(self, status: str) -> None:
        # Status is re-sent on every poll; restyling a widget is expensive.
        if status == self._status and self._styled:
            return
        self._status = status
        self._styled = True
        colors = {
            "streaming": _DOT_GREEN,
            "connecting": _DOT_AMBER,
            "reconnecting": _DOT_AMBER,
            "error": _DOT_RED,
            "unauthorized": _DOT_RED,
            "offline": _DOT_GRAY,
        }
        self._dot.setStyleSheet(f"color: {colors.get(status, _DOT_GRAY)}; font-size: 9px;")
        self._set_badge(status)

    def set_recording(self, state: str) -> None:
        """Recording must never recolor the independent connection indicator."""
        if state == self._recording:
            return
        self._recording = state
        if state == "recording":
            self._recording_dot.setStyleSheet(f"color: {_DOT_RED}; font-size: 9px;")
            self._recording_dot.setToolTip("Recording")
            self._recording_dot.show()
        elif state == "scheduled":
            self._recording_dot.setStyleSheet(f"color: {_DOT_AMBER}; font-size: 9px;")
            self._recording_dot.setToolTip("Recording scheduled")
            self._recording_dot.show()
        else:
            self._recording_dot.hide()

    def _set_badge(self, status: str) -> None:
        if status == "streaming":
            self._badge.setText("Live")
            self._badge.setStyleSheet(
                "background: #166534; color: #bbf7d0; font-size: 9px;"
                " font-weight: bold; padding: 0 5px; border-radius: 3px;"
            )
        elif status in {"connecting", "reconnecting"}:
            self._badge.setText(status.capitalize())
            self._badge.setStyleSheet(
                "background: #78350f; color: #fde68a; font-size: 9px;"
                " padding: 0 5px; border-radius: 3px;"
            )
        elif status in {"error", "unauthorized"}:
            self._badge.setText("Wrong password" if status == "unauthorized" else "Error")
            self._badge.setStyleSheet(
                "background: #3f1414; color: #fca5a5; font-size: 9px;"
                " padding: 0 5px; border-radius: 3px;"
            )
        else:
            # Grey like its dot: offline is a state, not a fault.
            self._badge.setText("Offline")
            self._badge.setStyleSheet(
                "background: #262636; color: #b4b4c8; font-size: 9px;"
                " padding: 0 5px; border-radius: 3px;"
            )


class CameraPanel(QWidget):
    """Right camera panel matching reference UI."""

    camera_selected = Signal(str)  # cam_id or "" for deselect
    add_clicked = Signal()
    refresh_clicked = Signal()
    edit_requested = Signal(str)
    remove_requested = Signal(str)
    fullscreen_requested = Signal(str)

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setFixedWidth(_PANEL_W)
        self.setStyleSheet(f"background-color: {_BG}; border-left: 1px solid #1a1a30;")

        self._all_cameras: list[CameraConfig] = []
        self._filter = "all"
        self._search = ""
        self._sort_key = "name_asc"
        self._item_widgets: dict[str, _CameraItem] = {}
        self._selected_id: str | None = None
        self._camera_statuses: dict[str, str] = {}
        self._recording_states: dict[str, str] = {}
        self._motion_ids: set[str] = set()

        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(0)

        layout.addWidget(self._make_header())
        layout.addWidget(self._make_search())
        layout.addWidget(self._make_list(), stretch=1)
        layout.addWidget(self._make_filter_row())
        layout.addWidget(self._make_action_row())

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def set_cameras(self, cameras: list[CameraConfig]) -> None:
        self._all_cameras = list(cameras)
        ids = {camera.id for camera in cameras}
        self._recording_states = {
            cid: state for cid, state in self._recording_states.items() if cid in ids
        }
        self._rebuild_list()

    def set_camera_status(self, cam_id: str, status: str) -> None:
        status = connection_status(status)
        old_status = self._camera_statuses.get(cam_id, "offline")
        self._camera_statuses[cam_id] = status
        if widget := self._item_widgets.get(cam_id):
            widget.set_status(status)
        # Only rebuild the list when the active filter could change which items are shown
        # (i.e. a camera crosses the online/offline boundary under an active filter).
        if self._filter != "all":
            old_online = old_status in _ONLINE_STATUSES
            new_online = status in _ONLINE_STATUSES
            if old_online != new_online:
                self._rebuild_list()

    def set_camera_recording(self, cam_id: str, state: str) -> None:
        """Route recording state update to the corresponding _CameraItem."""
        self._recording_states[cam_id] = state
        if widget := self._item_widgets.get(cam_id):
            widget.set_recording(state)

    def set_motion_cameras(self, camera_ids: set[str]) -> None:
        """Cameras with motion going on show a Motion badge in the list."""
        self._motion_ids = set(camera_ids)
        for cam_id, widget in self._item_widgets.items():
            widget.set_motion(cam_id in camera_ids)

    @property
    def selected_id(self) -> str | None:
        return self._selected_id

    # ------------------------------------------------------------------
    # Build sub-widgets
    # ------------------------------------------------------------------

    def _make_header(self) -> QWidget:
        header = QWidget()
        header.setFixedHeight(36)
        header.setStyleSheet(f"background: {_HEADER_BG}; border-bottom: 1px solid #1a1a30;")
        hl = QHBoxLayout(header)
        hl.setContentsMargins(10, 0, 6, 0)

        title = QLabel("Cameras")
        title.setStyleSheet(f"color: {_TEXT}; font-size: 12px; font-weight: bold;")
        hl.addWidget(title)
        hl.addStretch()
        return header

    def _make_search(self) -> QWidget:
        w = QWidget()
        w.setStyleSheet(f"background: {_BG};")
        vl = QVBoxLayout(w)
        vl.setContentsMargins(8, 6, 8, 4)
        vl.setSpacing(4)

        self._search_box = QLineEdit()
        self._search_box.setPlaceholderText("Search cameras…")
        self._search_box.setStyleSheet(_SEARCH_STYLE)
        self._search_box.textChanged.connect(self._on_search)
        vl.addWidget(self._search_box)

        # Sort row
        sort_row = QWidget()
        sort_row.setStyleSheet(f"background: {_BG};")
        sr = QHBoxLayout(sort_row)
        sr.setContentsMargins(0, 0, 0, 0)
        sr.setSpacing(4)

        sort_lbl = QLabel("Sort:")
        sort_lbl.setStyleSheet(f"color: {_SUBTEXT}; font-size: 10px;")
        sr.addWidget(sort_lbl)

        _combo_style = f"""
            QComboBox {{ background: #18182c; color: {_TEXT}; border: 1px solid #252540;
                border-radius: 3px; padding: 2px 20px 2px 6px; font-size: 10px; }}
            {arrow_rules()}
            QComboBox QAbstractItemView {{ background: #1a1a2e; color: {_TEXT};
                selection-background-color: #2a3a6a; border: 1px solid #252540; }}
        """
        self._sort_combo = QComboBox()
        self._sort_combo.setToolTip("Sort camera list")
        self._sort_combo.addItems(["Name ↑", "Name ↓", "Status", "Online First", "Offline First"])
        self._sort_combo.setStyleSheet(_combo_style)
        self._sort_combo.currentIndexChanged.connect(self._on_sort_changed)
        sr.addWidget(self._sort_combo, stretch=1)

        vl.addWidget(sort_row)
        return w

    def _make_list(self) -> QListWidget:
        self._list = _CameraListWidget()
        self._list.setStyleSheet(
            f"QListWidget {{ background: {_BG}; border: none; }}"
            f"QListWidget::item {{ border-bottom: 1px solid #181828; padding: 0; }}"
            f"QListWidget::item:selected {{ background: {_ITEM_SEL}; }}"
            f"QListWidget::item:hover {{ background: {_ITEM_HOVER}; }}"
        )
        self._list.setSpacing(0)
        self._list.setHorizontalScrollBarPolicy(Qt.ScrollBarPolicy.ScrollBarAlwaysOff)
        self._list.setDragEnabled(True)
        self._list.setDragDropMode(QAbstractItemView.DragDropMode.DragOnly)
        self._list.setContextMenuPolicy(Qt.ContextMenuPolicy.CustomContextMenu)
        self._list.itemClicked.connect(self._on_item_click)
        self._list.customContextMenuRequested.connect(self._on_context_menu)
        return self._list

    def _make_filter_row(self) -> QWidget:
        w = QWidget()
        w.setStyleSheet(f"background: {_BG}; border-top: 1px solid #1a1a30;")
        hl = QHBoxLayout(w)
        hl.setContentsMargins(6, 4, 6, 4)
        hl.setSpacing(4)

        self._filter_btns: dict[str, QPushButton] = {}
        grp = QButtonGroup(self)
        for key, label in [("all", "All"), ("online", "Online"), ("offline", "Offline")]:
            btn = QPushButton(label)
            btn.setCheckable(True)
            btn.setChecked(key == "all")
            btn.setStyleSheet(self._filter_btn_style(key == "all"))
            btn.clicked.connect(lambda _c, k=key: self._set_filter(k))
            grp.addButton(btn)
            hl.addWidget(btn)
            self._filter_btns[key] = btn
        self._filter_btns["all"].setToolTip("Show all cameras")
        self._filter_btns["online"].setToolTip("Show only connected cameras")
        self._filter_btns["offline"].setToolTip("Show only disconnected cameras")
        return w

    def _make_action_row(self) -> QWidget:
        w = QWidget()
        w.setStyleSheet(f"background: {_HEADER_BG}; border-top: 1px solid #1a1a30;")
        hl = QHBoxLayout(w)
        hl.setContentsMargins(6, 6, 6, 6)
        hl.setSpacing(4)

        add_btn = QPushButton("＋ Add Camera")
        add_btn.setStyleSheet(_BTN_STYLE)
        add_btn.setToolTip("Add a new camera")
        add_btn.clicked.connect(self.add_clicked)
        hl.addWidget(add_btn)

        refresh_btn = QPushButton("↺ Refresh")
        refresh_btn.setStyleSheet(_BTN_STYLE)
        refresh_btn.setToolTip("Refresh camera list from backend")
        refresh_btn.clicked.connect(self.refresh_clicked)
        hl.addWidget(refresh_btn)
        return w

    # ------------------------------------------------------------------

    def _on_sort_changed(self, index: int) -> None:
        keys = ["name_asc", "name_desc", "status", "online_first", "offline_first"]
        self._sort_key = keys[index] if index < len(keys) else "name_asc"
        self._rebuild_list()

    def _sorted_for_display(self, cameras: list[CameraConfig]) -> list[CameraConfig]:
        _status_order = {
            "streaming": 0,
            "connecting": 1,
            "reconnecting": 2,
            "offline": 3,
            "error": 4,
            "unauthorized": 4,
        }
        key = self._sort_key
        if key == "name_asc":
            return sorted(cameras, key=lambda c: c.name.lower())
        if key == "name_desc":
            return sorted(cameras, key=lambda c: c.name.lower(), reverse=True)
        if key == "status":
            return sorted(
                cameras,
                key=lambda c: _status_order.get(self._camera_statuses.get(c.id, "offline"), 5),
            )
        if key == "online_first":
            return sorted(
                cameras,
                key=lambda c: (
                    0 if self._camera_statuses.get(c.id, "offline") in _ONLINE_STATUSES else 1
                ),
            )
        if key == "offline_first":
            return sorted(
                cameras,
                key=lambda c: (
                    1 if self._camera_statuses.get(c.id, "offline") in _ONLINE_STATUSES else 0
                ),
            )
        return cameras

    def _rebuild_list(self) -> None:
        self._list.clear()
        self._item_widgets.clear()

        for cam in self._sorted_for_display(self._all_cameras):
            if self._search and self._search not in cam.name.lower():
                continue
            status = self._camera_statuses.get(cam.id, "offline")
            if self._filter == "online" and status not in _ONLINE_STATUSES:
                continue
            if self._filter == "offline" and status in _ONLINE_STATUSES:
                continue
            item_widget = _CameraItem(cam)
            item_widget.set_status(status)
            item_widget.set_recording(self._recording_states.get(cam.id, "idle"))
            item_widget.set_motion(cam.id in self._motion_ids)
            item = QListWidgetItem()
            # As wide as the panel, whatever the name's length.
            item.setSizeHint(QSize(1, item_widget.sizeHint().height()))
            item.setData(Qt.ItemDataRole.UserRole, cam.id)
            self._list.addItem(item)
            self._list.setItemWidget(item, item_widget)
            self._item_widgets[cam.id] = item_widget

    def _on_item_click(self, item: QListWidgetItem) -> None:
        cam_id = item.data(Qt.ItemDataRole.UserRole)
        if cam_id == self._selected_id:
            # Deselect
            self._list.clearSelection()
            self._selected_id = None
            self.camera_selected.emit("")
        else:
            self._selected_id = cam_id
            self.camera_selected.emit(cam_id)

    def _on_search(self, text: str) -> None:
        self._search = text.lower().strip()
        self._rebuild_list()

    def _on_context_menu(self, pos) -> None:
        item = self._list.itemAt(pos)
        if not item:
            return
        cam_id = item.data(Qt.ItemDataRole.UserRole)
        cam = next((c for c in self._all_cameras if c.id == cam_id), None)
        if not cam:
            return
        menu = self._build_context_menu(cam)
        menu.exec(self._list.mapToGlobal(pos))
        menu.deleteLater()

    def _build_context_menu(self, cam: CameraConfig) -> QMenu:
        cam_id = cam.id
        menu = themed_menu(self)
        menu.addAction("Edit", lambda: self.edit_requested.emit(cam_id))
        menu.addAction("Remove", lambda: self.remove_requested.emit(cam_id))
        menu.addSeparator()
        menu.addAction("Open Full-Screen", lambda: self.fullscreen_requested.emit(cam_id))
        menu.addAction("Copy URL", lambda: QApplication.clipboard().setText(cam.url))
        return menu

    def _set_filter(self, key: str) -> None:
        self._filter = key
        for k, btn in self._filter_btns.items():
            btn.setStyleSheet(self._filter_btn_style(k == key))
        self._rebuild_list()

    @staticmethod
    def _filter_btn_style(active: bool) -> str:
        bg = "#1d3a6e" if active else "#1a1a2e"
        fg = "#93c5fd" if active else "#707088"
        return _FILTER_BTN.format(bg=bg, fg=fg)
