"""NeuroWatch Cloud in the tray: its state in the status line, and a problem
when a linked cloud cannot be reached. A cloud that was never linked is not a
problem (it is optional). No Qt: this runs on the tray's worker thread."""

from __future__ import annotations

from collections.abc import Callable, Mapping
from dataclasses import dataclass
from typing import Any

from neurowatch.cloud.cloud_link import cloud_indicator


@dataclass(frozen=True)
class CloudTrayStatus:
    """What the cloud adds to the tray: a detail for its status line and
    problems for its warning badge, as ``(label, explanation)`` pairs."""

    detail: str = ""
    problems: tuple[tuple[str, str], ...] = ()


def short_state(tooltip: str) -> str:
    """ "NeuroWatch Cloud: connected" -> "connected"."""
    return tooltip.split(":", 1)[1].strip() if ":" in tooltip else tooltip


def tray_status(request: Callable[..., Any], endpoint: str) -> CloudTrayStatus:
    cloud = request("cloud.status", endpoint=endpoint, timeout=1.5)
    detail = short_state(cloud_indicator(cloud if isinstance(cloud, dict) else None).tooltip)
    problems: tuple[tuple[str, str], ...] = ()
    linked = isinstance(cloud, Mapping) and cloud.get("phase") == "linked"
    if linked and cloud.get("cloud_reachable") is not True:
        problems = (("Cloud unreachable", "Linked, but can't reach NeuroWatch Cloud"),)
    return CloudTrayStatus(detail=f"Cloud: {detail}", problems=problems)
