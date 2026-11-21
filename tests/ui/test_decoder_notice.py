"""A graphics card that can decode video but lacks its driver: the status-bar
notice, and the message shown once when the app starts."""

from __future__ import annotations

from collections.abc import Iterator
from typing import Any, ClassVar

import pytest
from PySide6.QtCore import QEvent, QPointF, Qt
from PySide6.QtGui import QMouseEvent
from PySide6.QtWidgets import QApplication

from neurowatch.main import MainWindow
from neurowatch.widgets.decoder_notice import DecoderDriverNotice

_REASON = (
    "This PC has Intel graphics that can decode video, but its VA-API driver (iHD)"
    " isn't installed, so video is decoded on the CPU."
)
_MISSING: dict[str, Any] = {
    "kind": "software",
    "label": "Software (avdec)",
    "hardware": False,
    "available": True,
    "reason": _REASON,
    "reason_code": "VAAPI_DRIVER_MISSING",
    "gpu": {"vendor": "Intel", "kernel_driver": "i915", "render_node": "/dev/dri/renderD128"},
    "suggested_packages": {
        "deb": ["intel-media-va-driver"],
        "rpm": ["libva-intel-media-driver"],
        "arch": ["intel-media-driver"],
    },
}


def _click(widget: DecoderDriverNotice) -> None:
    point = QPointF(2, 2)
    widget.mousePressEvent(
        QMouseEvent(
            QEvent.Type.MouseButtonPress,
            point,
            point,
            Qt.MouseButton.LeftButton,
            Qt.MouseButton.LeftButton,
            Qt.KeyboardModifier.NoModifier,
        )
    )


def test_the_notice_shows_only_while_a_driver_is_missing(qtbot) -> None:
    notice = DecoderDriverNotice()
    qtbot.addWidget(notice)
    assert notice.isHidden()
    notice.set_acceleration(_MISSING)
    assert not notice.isHidden()
    assert notice.text() == "Intel GPU driver missing"
    assert notice.toolTip().startswith(_REASON)
    assert notice.mark() == "alert"
    with qtbot.waitSignal(notice.clicked, timeout=1000):
        _click(notice)
    notice.set_acceleration({"kind": "vaapi", "hardware": True})
    assert notice.isHidden() and notice.notice is None
    clicks: list[bool] = []
    notice.clicked.connect(lambda: clicks.append(True))
    _click(notice)
    assert clicks == [], "hidden: nothing to open"


class _FakeClient:
    _endpoint = "unix:/nonexistent/test-decoder-notice.sock"
    acceleration: ClassVar[dict[str, Any]] = {}

    def __init__(self, endpoint: str | None = None) -> None:
        pass

    def is_online(self) -> bool:
        return True

    def call(self, method: str, params: object = None) -> Any:
        if method == "system.status":
            return {"status": "running", "video_acceleration": _FakeClient.acceleration}
        if method == "config.get":
            return {
                "version": 3,
                "wall": {"rows": 2, "cols": 2, "auto_fit": True, "page_interval_sec": 0},
                "ui": {"theme": "dark", "fullscreen": False},
            }
        if method in ("camera.list",):
            return []
        if method == "camera.status_all":
            return {}
        return {"ok": True}


@pytest.fixture
def make_window(monkeypatch, qtbot) -> Iterator[Any]:
    monkeypatch.setattr("neurowatch.main.SyncIpcClient", _FakeClient)
    monkeypatch.setattr("neurowatch.ipc.frame_reader.SHM_PREFIX", "/nonexistent/nw-test-")
    shown: list[MainWindow] = []

    def make(acceleration: dict[str, Any]) -> MainWindow:
        _FakeClient.acceleration = acceleration
        window = MainWindow()
        qtbot.addWidget(window)
        shown.append(window)
        return window

    yield make
    for window in shown:
        window.close()
        window.deleteLater()
    QApplication.sendPostedEvents(None, QEvent.Type.DeferredDelete)


def test_startup_says_once_that_the_driver_is_missing(make_window, monkeypatch, qtbot) -> None:
    messages: list[tuple[str, str, str, str]] = []
    monkeypatch.setattr(
        "neurowatch.main.show_message",
        lambda _parent, title, text, kind="info", detail="": messages.append(
            (title, text, kind, detail)
        ),
    )
    window = make_window(_MISSING)
    assert not window._decoder_notice.isHidden()
    assert window._decoder_notice.text() == "Intel GPU driver missing"

    window.report_startup_error()
    qtbot.waitUntil(lambda: bool(messages), timeout=2000)
    title, text, kind, detail = messages[0]
    assert (title, text, kind) == ("Hardware video decoding unavailable", _REASON, "warning")
    assert "Debian/Ubuntu: intel-media-va-driver" in detail
    assert "Fedora: libva-intel-media-driver" in detail
    assert "Arch: intel-media-driver" in detail

    window.report_startup_error()  # a reload later in the same run: not again
    window._report_decoder_notice()
    assert len(messages) == 1

    # The poll's status keeps the notice current (a backend restarted with a driver).
    window._on_system_status({"video_acceleration": {"kind": "vaapi", "hardware": True}})
    assert window._decoder_notice.isHidden()

    # A click on the notice opens Diagnostics.
    window._on_system_status({"video_acceleration": _MISSING})
    tabs: list[str] = []
    window._title_bar.tab_changed.connect(tabs.append)
    window._decoder_notice.clicked.emit()
    assert tabs == ["diagnostics"]


def test_no_message_when_the_decoder_works(make_window, monkeypatch, qtbot) -> None:
    messages: list[str] = []
    monkeypatch.setattr(
        "neurowatch.main.show_message", lambda _p, title, *_a, **_k: messages.append(title)
    )
    window = make_window({"kind": "vaapi", "label": "VA-API", "hardware": True})
    window.report_startup_error()
    window._report_decoder_notice()
    assert messages == []
    assert window._decoder_notice.isHidden()
