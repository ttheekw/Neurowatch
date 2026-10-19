"""Motion settings and camera-event status (``motion.configure``, ``motion.status``).

No Qt: the camera dialog and the headless commands both use it.
"""

from __future__ import annotations

import math
from typing import Any

from neurowatch.models.events import kinds_text

#: ``motion.source`` values, as the backend names them.
SOURCES = ("off", "builtin", "onvif", "both")


def valid_motion_zones(zones: object) -> bool:
    if not isinstance(zones, list) or len(zones) > 32:
        return False
    ids: set[str] = set()
    for zone in zones:
        if not isinstance(zone, dict):
            return False
        ident, points = zone.get("id"), zone.get("points")
        if not isinstance(ident, str) or not ident or ident in ids:
            return False
        ids.add(ident)
        if zone.get("kind", "include") not in ("include", "exclude"):
            return False
        if not isinstance(points, list) or not 3 <= len(points) <= 64:
            return False
        if any(
            not isinstance(point, (list, tuple))
            or len(point) != 2
            or any(
                not isinstance(value, (int, float))
                or isinstance(value, bool)
                or not math.isfinite(value)
                or not 0 <= value <= 1
                for value in point
            )
            for point in points
        ):
            return False
    return True


_STATE_TEXT = {
    "connecting": "Connecting to the camera's event service…",
    "subscribed": "Connected to the camera's events.",
    "unsupported": "This camera does not send ONVIF events.",
    "unauthorized": "The camera refused the username or password for its events.",
    "disabled": "ONVIF is turned off on this camera; turn it on in the camera's web settings.",
    "unreachable": "The camera's event service does not answer.",
    "error": "The camera's events could not be read.",
}

_STATE_TONE = {
    "subscribed": "success",
    "connecting": "info",
    "unsupported": "warning",
    "unauthorized": "error",
    "disabled": "warning",
    "unreachable": "error",
    "error": "error",
}


def source_of(motion: dict[str, Any]) -> str:
    """The detection in use, like the backend's ``effective_source``."""
    source = motion.get("source")
    if source in SOURCES and source != "off":
        return str(source)
    return "builtin" if motion.get("enabled") else "off"


def uses_camera_events(motion: dict[str, Any]) -> bool:
    return source_of(motion) in ("onvif", "both")


def with_camera_events(motion: dict[str, Any], enabled: bool) -> dict[str, Any]:
    """``motion`` with the camera's own events turned on or off; the built-in
    detector and every other setting stay as they are."""
    builtin = source_of(motion) in ("builtin", "both")
    if enabled:
        source = "both" if builtin else "onvif"
    else:
        source = "builtin" if builtin else "off"
    updated = dict(motion)
    updated["source"] = source
    # ``source`` decides from now on (the backend does the same).
    updated["enabled"] = False
    return updated


def camera_event_topics(status: Any, camera_id: str) -> list[str]:
    """The camera's own names of the events it sent (ONVIF topics)."""
    cameras = status.get("cameras") if isinstance(status, dict) else None
    camera = cameras.get(camera_id) if isinstance(cameras, dict) else None
    onvif = camera.get("onvif") if isinstance(camera, dict) else None
    topics = onvif.get("topics") if isinstance(onvif, dict) else None
    return [t for t in topics or [] if isinstance(t, str)]


def camera_events_status(status: Any, camera_id: str) -> tuple[str, str]:
    """(text, tone) describing a camera's event subscription in ``motion.status``."""
    cameras = status.get("cameras") if isinstance(status, dict) else None
    camera = cameras.get(camera_id) if isinstance(cameras, dict) else None
    if not isinstance(camera, dict) or not uses_camera_events({"source": camera.get("source")}):
        return ("Camera events are off.", "info")
    onvif = camera.get("onvif")
    if not isinstance(onvif, dict):
        return (_STATE_TEXT["connecting"], "info")
    state = str(onvif.get("state") or "connecting")
    text = _STATE_TEXT.get(state, _STATE_TEXT["error"])
    kinds = [k for k in camera.get("kinds") or [] if isinstance(k, str)]
    if state == "subscribed":
        delivery = onvif.get("delivery")
        if delivery == "push":
            text += " The camera sends them to NeuroWatch as they happen."
        elif delivery == "pull":
            text += " NeuroWatch asks the camera for them."
            note = onvif.get("note")
            if isinstance(note, str) and note:
                text += f" Sending them directly did not work: {note}."
        if kinds:
            text += f" It reports {kinds_text(kinds)}."
        else:
            text += " Nothing reported yet: it shows here once the camera sends an event."
    detail = onvif.get("detail")
    if state not in ("subscribed", "connecting") and isinstance(detail, str) and detail:
        text += f" ({detail})"
    return (text, _STATE_TONE.get(state, "error"))
