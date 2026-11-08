from __future__ import annotations

import pytest

from neurowatch.models.drawing_history import DrawingWindow, read_window


def record(ident: int, at: int, *, items: bool = True) -> dict:
    return {
        "id": ident,
        "camera_id": "c",
        "plugin": "m",
        "layer": "boxes",
        "time_us": at * 1000,
        "frame": ident,
        "ttl_ms": 1000,
        "on_wall": True,
        "items": [{"type": "box", "rect": [0, 0, 0.5, 0.5]}] if items else [],
    }


def test_playback_drawings_follow_picture_time_clears_and_backward_seeks() -> None:
    window = DrawingWindow(
        ("c",), 0, 5000, {"c": [record(1, 1000), record(2, 1500, items=False), record(3, 2000)]}
    )
    assert not window.at("c", 900)
    assert len(window.at("c", 1100)) == 1
    assert not window.at("c", 1600)
    assert len(window.at("c", 2100)) == 1
    assert not window.at("c", 3100)
    assert len(window.at("c", 1100)) == 1
    assert not window.at("c", 1100, "off")
    assert window.at("c", 1100)[0].until == float("inf")


def test_history_pages_are_sealed_by_ipc_transport_and_stuck_cursors_fail() -> None:
    class Client:
        def __init__(self) -> None:
            self.calls: list[dict] = []

        def call(self, method: str, params: dict) -> dict:
            assert method == "plugins.history"
            self.calls.append(params)
            return {"drawings": [record(1, 1000)], "next": {"after_us": 1000000, "after_id": 1}}

    client = Client()
    with pytest.raises(ValueError, match="did not advance"):
        read_window(client, ["c"], 1000, lambda: False)
    assert client.calls[1]["after_id"] == 1
    assert client.calls[1]["camera_id"] == "c"
