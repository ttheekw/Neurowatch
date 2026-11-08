"""Cloud Backup at a glance, from ``system.status`` ``backup``: the status
bar's symbol (green backing up, red failing) and the tray's problems. No Qt."""

from __future__ import annotations

from dataclasses import dataclass, field

#: The symbol's colours: the tray's healthy green and its problem red.
GREEN = "#22c55e"
RED = "#ef4444"
GREY = "#94a3b8"


@dataclass(frozen=True)
class BackupSummary:
    """``off``: no camera has a plan (no symbol); ``ok``; ``starting`` (in
    trouble, not yet for long enough to fail); ``failing``."""

    state: str = "off"
    cameras: int = 0
    #: (camera name, why) of each camera whose backup is failing.
    failing: tuple[tuple[str, str], ...] = field(default_factory=tuple)


def backup_summary(system: object) -> BackupSummary:
    """Read ``system.status`` ``backup``; an older recorder has none (off)."""
    raw = system.get("backup") if isinstance(system, dict) else None
    if not isinstance(raw, dict):
        return BackupSummary()
    state = str(raw.get("state") or "off")
    if state not in ("off", "ok", "starting", "failing"):
        state = "off"
    failing: list[tuple[str, str]] = []
    listed = raw.get("failing")
    for camera in listed if isinstance(listed, list) else []:
        if isinstance(camera, dict):
            name = str(camera.get("name") or camera.get("id") or "A camera")
            failing.append((name, str(camera.get("why") or "Not backed up.")))
    cameras = raw.get("cameras")
    return BackupSummary(state, cameras if isinstance(cameras, int) else 0, tuple(failing))


@dataclass(frozen=True)
class BackupNotice:
    #: Empty: no symbol.
    text: str
    tooltip: str
    color: str


def backup_notice(summary: BackupSummary) -> BackupNotice:
    """The status bar's Cloud Backup symbol: its words, tooltip and colour."""
    count = summary.cameras
    cameras = f"{count} {'camera' if count == 1 else 'cameras'}"
    if summary.state == "failing":
        names = ", ".join(name for name, _ in summary.failing)
        reasons = "\n".join(f"{name}: {why}" for name, why in summary.failing)
        text = (
            "Backup failing"
            if len(summary.failing) <= 1
            else f"Backup failing ({len(summary.failing)})"
        )
        tooltip = (
            f"Cloud Backup is not backing up {names or 'a camera'}. The recorder keeps trying.\n"
            f"{reasons}"
        ).strip()
        return BackupNotice(text, tooltip, RED)
    if summary.state == "starting":
        return BackupNotice(
            "Backup",
            f"Cloud Backup is connecting for {cameras}.",
            GREY,
        )
    if summary.state == "ok":
        return BackupNotice("Backup", f"Cloud Backup: backing up {cameras} to the cloud.", GREEN)
    return BackupNotice("", "", GREY)


def backup_problems(summary: BackupSummary) -> list[tuple[str, str]]:
    """The tray's problems: one per camera whose backup is failing."""
    return [
        (f"Cloud Backup failing: {name}", f"Cloud Backup failing on {name}: {why}")
        for name, why in summary.failing
    ]


#: A tile's backup symbol: "ok" (green), "starting" (grey), "failing" (red).
TILE_TEXT = {
    "ok": "Cloud Backup: backing up to the cloud",
    "starting": "Cloud Backup: connecting",
    "failing": "Cloud Backup failing",
}


def backup_tile_marks(system: object) -> dict[str, tuple[str, str]]:
    """``system.status`` ``backup.states``: each camera with a plan, as
    ``{camera_id: (state, tooltip line)}`` for its wall tile's symbol."""
    raw = system.get("backup") if isinstance(system, dict) else None
    states = raw.get("states") if isinstance(raw, dict) else None
    marks: dict[str, tuple[str, str]] = {}
    for camera_id, entry in states.items() if isinstance(states, dict) else ():
        state = entry.get("state") if isinstance(entry, dict) else None
        if state not in TILE_TEXT:
            continue
        why = entry.get("why") if state == "failing" else None
        line = f"{TILE_TEXT[state]}: {why}" if why else TILE_TEXT[state]
        marks[str(camera_id)] = (str(state), line)
    return marks
