"""The archive tier's settings and status (``recording.archive``,
``recording.archive_status``). No Qt."""

from __future__ import annotations

from dataclasses import dataclass

#: Recordings move after this many days unless configured otherwise.
DEFAULT_AFTER_DAYS = 7
MAX_AFTER_DAYS = 3650


def format_size(size: int) -> str:
    """``1.5 GB``-style size (powers of 1024, as the drive list shows them)."""
    value = float(max(0, size))
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if value < 1024 or unit == "TB":
            return f"{value:.0f} {unit}" if unit == "B" else f"{value:.1f} {unit}"
        value /= 1024
    return f"{value:.1f} TB"


def _count(value: object) -> int:
    if isinstance(value, bool) or not isinstance(value, (int, float, str)):
        return 0
    try:
        return max(0, int(value))
    except ValueError:
        return 0


def _text(value: object) -> str | None:
    return value if isinstance(value, str) and value else None


@dataclass(frozen=True)
class MissingLocation:
    """A recording location (or the archive) that is not there now."""

    path: str
    reason: str


@dataclass(frozen=True)
class ArchiveStatus:
    """What ``recording.archive_status`` reports."""

    state: str = "off"
    detail: str | None = None
    moved_files: int = 0
    moved_bytes: int = 0
    pending_files: int = 0
    pending_bytes: int = 0
    archived_files: int = 0
    archived_bytes: int = 0
    last_error: str | None = None
    missing_locations: tuple[MissingLocation, ...] = ()

    @classmethod
    def from_json(cls, data: object) -> ArchiveStatus:
        if not isinstance(data, dict):
            return cls()
        missing = tuple(
            MissingLocation(str(item.get("path", "")), str(item.get("reason", "")))
            for item in data.get("missing_locations") or []
            if isinstance(item, dict)
        )
        return cls(
            state=_text(data.get("state")) or "off",
            detail=_text(data.get("detail")),
            moved_files=_count(data.get("moved_files")),
            moved_bytes=_count(data.get("moved_bytes")),
            pending_files=_count(data.get("pending_files")),
            pending_bytes=_count(data.get("pending_bytes")),
            archived_files=_count(data.get("archived_files")),
            archived_bytes=_count(data.get("archived_bytes")),
            last_error=_text(data.get("last_error")),
            missing_locations=missing,
        )

    def summary(self) -> str:
        """One or two sentences for the Settings dialog."""
        first = {
            "off": "Archiving is off.",
            "waiting": f"Waiting: {self.detail}." if self.detail else "Waiting for its hours.",
            "missing": f"The archive folder is not available: {self.detail}"
            if self.detail
            else "The archive folder is not available.",
            "error": f"Archiving cannot run: {self.detail}."
            if self.detail
            else "Archiving cannot run.",
            "moving": "Moving recordings to the archive…",
            "idle": "Up to date.",
        }.get(self.state, self.state)
        parts = [first]
        counts = []
        if self.archived_files:
            counts.append(
                f"{self.archived_files} recordings ({format_size(self.archived_bytes)}) "
                "in the archive"
            )
        if self.state != "off" and self.pending_files:
            counts.append(
                f"{self.pending_files} ({format_size(self.pending_bytes)}) waiting to move"
            )
        if counts:
            parts.append(" · ".join(counts) + ".")
        if self.last_error and self.state != "off":
            parts.append(f"Last problem: {self.last_error}.")
        return " ".join(parts)

    def missing_text(self) -> str:
        """The locations that are missing now, one per line ("" when none)."""
        return "\n".join(f"{item.path}: {item.reason}" for item in self.missing_locations)


@dataclass(frozen=True)
class ArchiveSettings:
    """``recording.archive`` as the Settings dialog edits it."""

    enabled: bool = False
    path: str = ""
    after_days: int = DEFAULT_AFTER_DAYS
    quota_gb: int | None = None
    hours: tuple[str, str] | None = None
    max_mbps: int | None = None

    @classmethod
    def from_json(cls, data: object) -> ArchiveSettings:
        if not isinstance(data, dict):
            return cls()
        hours = data.get("hours")
        window = None
        if isinstance(hours, dict) and _text(hours.get("start")) and _text(hours.get("end")):
            window = (str(hours["start"]), str(hours["end"]))

        def optional(key: str) -> int | None:
            value = _count(data.get(key))
            return value or None

        days = _count(data.get("after_days")) or DEFAULT_AFTER_DAYS
        return cls(
            enabled=bool(data.get("enabled", False)),
            path=str(data.get("path") or ""),
            after_days=min(days, MAX_AFTER_DAYS),
            quota_gb=optional("quota_gb"),
            hours=window,
            max_mbps=optional("max_mbps"),
        )

    def to_json(self) -> dict[str, object]:
        return {
            "enabled": self.enabled,
            "path": self.path.strip(),
            "after_days": self.after_days,
            "quota_gb": self.quota_gb,
            "hours": {"start": self.hours[0], "end": self.hours[1]} if self.hours else None,
            "max_mbps": self.max_mbps,
        }
