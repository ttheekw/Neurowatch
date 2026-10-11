"""Open windows are saved in the config and reopen when the app starts."""

from __future__ import annotations

from collections.abc import Iterator
from typing import ClassVar

import pytest
from PySide6.QtCore import QEvent
from PySide6.QtWidgets import QApplication

from neurowatch.main import MainWindow
from neurowatch.models.camera import CameraConfig
from neurowatch.models.wall_layout import Placement
from neurowatch.models.window_session import SavedWindow, saved_windows


class _FakeClient:
    _endpoint = "unix:/nonexistent/test-window-session.sock"
    endpoint = _endpoint
    calls: ClassVar[list[tuple[str, object]]] = []

    def __init__(self, endpoint: str | None = None) -> None:
        pass

    def is_online(self) -> bool:
        return True

    def call(self, method, params=None):
        _FakeClient.calls.append((method, params))
        if method == "config.get":
            return {
                "version": 3,
                "site_name": "Test Site",
                "wall": {"rows": 3, "cols": 3, "auto_fit": True, "page_interval_sec": 0},
                "ui": {"theme": "dark", "fullscreen": False},
            }
        if method == "camera.list":
            return []
        if method == "camera.status_all":
            return {}
        return {"ok": True}


@pytest.fixture
def win(monkeypatch, qtbot) -> Iterator[MainWindow]:
    _FakeClient.calls = []
    monkeypatch.setattr("neurowatch.main.SyncIpcClient", _FakeClient)
    monkeypatch.setattr("neurowatch.ipc.frame_reader.SHM_PREFIX", "/nonexistent/nw-test-")
    window = MainWindow()
    qtbot.addWidget(window)
    yield window
    window.close()
    window.deleteLater()
    QApplication.sendPostedEvents(None, QEvent.Type.DeferredDelete)


def _load(win: MainWindow, count: int) -> None:
    win._cameras = [
        CameraConfig(id=f"cam-{i}", name=f"Cam {i}", url="rtsp://x") for i in range(count)
    ]
    win._refresh_all()


def _ids(start: int, stop: int) -> list[str]:
    return [f"cam-{i}" for i in range(start, stop)]


def _saved_desktops() -> list[dict]:
    return [
        params["desktop"]
        for method, params in _FakeClient.calls
        if method == "config.set" and isinstance(params, dict) and "desktop" in params
    ]


def _window_dict(**changes: object) -> dict:
    window = {
        "number": 2,
        "rows": 2,
        "cols": 2,
        "page": 1,
        "cycling": False,
        "tiles": [
            {
                "camera_id": f"cam-{i}",
                "page": (i - 9) // 4,
                "row": ((i - 9) % 4) // 2,
                "col": (i - 9) % 2,
            }
            for i in range(9, 15)
        ],
        "geometry": None,
    }
    window.update(changes)
    return window


# ── The saved form ───────────────────────────────────────────────────────


def test_saved_windows_skip_what_they_do_not_understand() -> None:
    windows = saved_windows(
        {
            "desktop": {
                "windows": [
                    _window_dict(number=3),
                    _window_dict(number=3, rows=4),  # the same number again
                    {"number": 4, "rows": 0, "cols": 2, "tiles": []},  # no grid
                    "not a window",
                    _window_dict(
                        number=2,
                        tiles=[{"camera_id": "a", "page": 0, "row": 0, "col": 0}, {"page": 1}],
                        geometry={"screen": "DP-1", "x": 5, "y": 6, "width": 0, "height": 9},
                    ),
                ]
            }
        }
    )
    assert [w.number for w in windows] == [2, 3]
    assert [t.camera_id for t in windows[0].tiles] == ["a"]
    assert windows[0].geometry is None  # no size
    assert windows[1].rows == 2
    assert saved_windows({}) == [] and saved_windows({"desktop": {"windows": "x"}}) == []


def test_a_saved_window_reads_back_as_it_was_written() -> None:
    window = SavedWindow.from_dict(
        _window_dict(
            geometry={
                "screen": "DP-1",
                "x": -10,
                "y": 40,
                "width": 1600,
                "height": 900,
                "state": "maximized",
            }
        )
    )
    assert window is not None
    assert SavedWindow.from_dict(window.to_dict()) == window


# ── Saving ───────────────────────────────────────────────────────────────


def test_opening_a_window_saves_it_with_its_cameras_grid_and_place(win, qtbot) -> None:
    _load(win, 18)
    window = win._open_detached_window(1, grid=(2, 3))
    assert window is not None
    assert win._windows_save_timer.isActive()  # batched: one save shortly
    win._windows_save_timer.stop()
    win._save_windows()
    qtbot.waitUntil(lambda: bool(_saved_desktops()), timeout=2000)

    [saved] = _saved_desktops()[-1]["windows"]
    assert saved["number"] == 2 and (saved["rows"], saved["cols"]) == (2, 3)
    assert [t["camera_id"] for t in saved["tiles"]] == _ids(9, 18)
    assert saved["tiles"][6] == {
        "camera_id": "cam-15",
        "page": 1,
        "row": 0,
        "col": 0,
        "row_span": 1,
        "col_span": 1,
    }
    assert saved["cycling"] is True
    geometry = saved["geometry"]
    assert geometry["width"] > 0 and geometry["state"] == "normal"
    assert win._ignore_config_changes_until > 0  # not mistaken for an outside edit


def test_changes_inside_a_window_and_closing_it_are_saved(win, qtbot) -> None:
    _load(win, 18)
    window = win._open_detached_window(1)
    assert window is not None
    win._windows_save_timer.stop()

    window.set_grid(2, 2)
    assert win._windows_save_timer.isActive()
    win._windows_save_timer.stop()
    window.set_cycling(False)
    assert win._windows_save_timer.isActive()

    window.close()
    qtbot.waitUntil(lambda: not win._detached, timeout=2000)
    assert win._windows_save_timer.isActive()
    win._windows_save_timer.stop()
    win._save_windows()
    qtbot.waitUntil(lambda: bool(_saved_desktops()), timeout=2000)
    assert _saved_desktops()[-1] == {"windows": []}  # closed by hand: not reopened


def test_shutting_down_saves_a_pending_change_before_the_windows_close(win) -> None:
    _load(win, 18)
    window = win._open_detached_window(1)
    assert window is not None and win._windows_save_timer.isActive()
    _FakeClient.calls = []
    win.shutdown()
    [desktop] = _saved_desktops()
    assert [w["number"] for w in desktop["windows"]] == [2]  # still listed: it reopens


# ── Reopening ────────────────────────────────────────────────────────────


def test_saved_windows_reopen_with_their_layout_page_and_cycling(win) -> None:
    _load(win, 18)
    win._config["desktop"] = {"windows": [_window_dict()]}
    win.restore_windows()

    [window] = win._detached
    assert window.number == 2 and window.grid == (2, 2)
    assert window.camera_ids == _ids(9, 15)
    assert window.page_count == 2 and window.page == 1
    assert not window._cycling
    assert window.isVisible()
    # The main wall keeps the rest, in their saved order.
    assert [p.cam_id for p in win._wall.placements()] == _ids(0, 9) + _ids(15, 18)
    assert win._windows_save_timer.isActive()  # saved again once, as reopened


def test_cameras_that_are_gone_are_left_out_and_an_empty_window_stays_closed(win) -> None:
    _load(win, 12)  # cam-12.. no longer exist
    win._config["desktop"] = {
        "windows": [
            _window_dict(number=2),
            _window_dict(number=3, tiles=[{"camera_id": "cam-40", "page": 0, "row": 0, "col": 0}]),
            # A camera can only be in one window: the first keeps it.
            _window_dict(number=4, tiles=[{"camera_id": "cam-9", "page": 0, "row": 0, "col": 0}]),
        ]
    }
    win.restore_windows()
    assert [w.number for w in win._detached] == [2]
    assert win._detached[0].camera_ids == _ids(9, 12)


def test_reopening_happens_once(win) -> None:
    _load(win, 18)
    win._config["desktop"] = {"windows": [_window_dict()]}
    win.restore_windows()
    win.restore_windows()
    assert len(win._detached) == 1


def test_a_window_goes_back_to_its_screen_and_size(win, qtbot) -> None:
    _load(win, 18)
    screen = QApplication.primaryScreen()
    area = screen.availableGeometry()
    geometry = {
        "screen": screen.name(),
        "x": area.x() + 20,
        "y": area.y() + 30,
        "width": 700,
        "height": 420,
        "state": "normal",
    }
    win._config["desktop"] = {"windows": [_window_dict(geometry=geometry)]}
    win.restore_windows()
    [window] = win._detached
    assert (window.width(), window.height()) == (700, 420)
    assert window.session().geometry is not None
    assert window.session().geometry.screen == screen.name()


def test_a_window_whose_screen_is_gone_opens_beside_the_main_window(win) -> None:
    _load(win, 18)
    geometry = {
        "screen": "HDMI-9",
        "x": 99999,
        "y": 0,
        "width": 700,
        "height": 420,
        "state": "fullscreen",
    }
    win._config["desktop"] = {"windows": [_window_dict(geometry=geometry)]}
    win.restore_windows()
    [window] = win._detached
    assert not window.isFullScreen()
    assert window.screen() is not None


def test_placements_restore_exactly_as_saved(win) -> None:
    _load(win, 18)
    tiles = [
        {"camera_id": "cam-9", "page": 0, "row": 0, "col": 0, "row_span": 2, "col_span": 2},
        {"camera_id": "cam-10", "page": 1, "row": 0, "col": 1},
        {"camera_id": "cam-11", "page": 1, "row": 0, "col": 0},
    ]
    win._config["desktop"] = {"windows": [_window_dict(tiles=tiles, page=0)]}
    win.restore_windows()
    [window] = win._detached
    assert window._placements == [
        Placement("cam-9", 0, 0, 0, 2, 2),
        Placement("cam-11", 1, 0, 0),
        Placement("cam-10", 1, 0, 1),
    ]
