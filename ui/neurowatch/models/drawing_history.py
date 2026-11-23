"""Stored plugin drawings at a recording picture's UTC capture time."""

from __future__ import annotations

import math
from collections.abc import Callable
from dataclasses import dataclass, replace
from typing import Any, Protocol

from neurowatch.models.analytics import Drawing, parse_overlays


class HistoryClient(Protocol):
    def call(self, method: str, params: dict[str, Any]) -> Any: ...


@dataclass
class DrawingWindow:
    cameras: tuple[str, ...]
    start_ms: int
    end_ms: int
    records: dict[str, list[dict[str, Any]]]
    limited: bool = False

    def contains(self, cameras: list[str], at_ms: int) -> bool:
        return tuple(cameras) == self.cameras and self.start_ms <= at_ms <= self.end_ms

    def at(self, camera: str, at_ms: int, mode: str = "chosen") -> list[Drawing]:
        if mode == "off":
            return []
        latest: dict[tuple[str, str], dict[str, Any]] = {}
        time_us = at_ms * 1000
        for record in self.records.get(camera, []):
            if int(record.get("time_us", 0)) > time_us:
                continue
            key = str(record.get("plugin", "")), str(record.get("layer", ""))
            previous = latest.get(key)
            if previous is None or (int(record["time_us"]), int(record["id"])) > (
                int(previous["time_us"]),
                int(previous["id"]),
            ):
                latest[key] = record
        shown: list[Drawing] = []
        for record in latest.values():
            if mode == "chosen" and not record.get("on_wall"):
                continue
            if time_us >= int(record["time_us"]) + int(record.get("ttl_ms", 0)) * 1000:
                continue
            parsed = parse_overlays({"overlays": [record]}, 0.0).get(camera, [])
            # A recorded empty result is the layer cleared: nothing to show.
            shown.extend(replace(drawing, until=math.inf) for drawing in parsed if drawing.items)
        return shown


def read_window(
    client: HistoryClient, cameras: list[str], at_ms: int, cancelled: Callable[[], bool]
) -> DrawingWindow:
    start, end = max(0, at_ms - 10_000), at_ms + 30_000
    window = DrawingWindow(tuple(cameras), start, end, {})
    for camera in cameras:
        records: list[dict[str, Any]] = []
        cursor: dict[str, int] = {}
        for _ in range(160):
            if cancelled():
                return window
            answer = client.call(
                "plugins.history",
                {
                    "camera_id": camera,
                    "from_us": start * 1000,
                    "to_us": end * 1000,
                    "limit": 128,
                    **cursor,
                },
            )
            if not isinstance(answer, dict) or not isinstance(answer.get("drawings"), list):
                raise ValueError("The recorder returned unreadable drawing history.")
            records.extend(row for row in answer["drawings"] if isinstance(row, dict))
            window.limited |= bool(answer.get("trimmed"))
            following = answer.get("next")
            if not following:
                break
            if not isinstance(following, dict) or not all(
                isinstance(following.get(key), int) for key in ("after_us", "after_id")
            ):
                raise ValueError("Drawing history returned an invalid page cursor.")
            if cursor and (following["after_us"], following["after_id"]) <= (
                cursor["after_us"],
                cursor["after_id"],
            ):
                raise ValueError("Drawing history did not advance.")
            cursor = following
        else:
            window.limited = True
        window.records[camera] = records
    return window
