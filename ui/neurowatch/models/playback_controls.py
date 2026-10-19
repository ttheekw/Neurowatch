"""Bound pending scrub requests without changing transport/control ordering."""

from __future__ import annotations

from collections.abc import Iterable
from typing import Any

Control = tuple[str, dict[str, Any]]


def enqueue_controls(queue: list[Control], calls: Iterable[Control]) -> None:
    """Only the newest unsent seek per session matters within a seek burst.

    In-flight requests are not touched. Play/pause/speed/audio and other
    controls are barriers: a later seek must never move ahead of them.
    """
    for method, params in calls:
        if method == "playback.seek" and isinstance(params.get("session_id"), str):
            replacement = None
            for index in range(len(queue) - 1, -1, -1):
                earlier_method, earlier_params = queue[index]
                if earlier_method != "playback.seek":
                    break
                if earlier_params.get("session_id") == params["session_id"]:
                    replacement = index
                    break
            if replacement is None:
                queue.append((method, params))
            else:
                queue[replacement] = (method, params)
        else:
            queue.append((method, params))
