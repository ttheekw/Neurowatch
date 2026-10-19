"""Clip export, as the desktop UI sees it: request parameters, job status and
the words shown for them. No Qt.

The backend does the work (``export.start`` / ``export.status`` /
``export.cancel``, ``crates/nw-backend/src/export``); see ``ExportJob`` for
what a job reports.
"""

from __future__ import annotations

import datetime
import os
import re
from dataclasses import dataclass
from typing import Any

#: Longest clip the backend exports.
MAX_CLIP_MS = 24 * 60 * 60 * 1000
#: How often a running export is asked for its progress.
POLL_INTERVAL_MS = 500

RUNNING = "running"
DONE = "done"
FAILED = "failed"
CANCELLED = "cancelled"


@dataclass(frozen=True)
class ExportJob:
    """One ``export.status`` answer."""

    job_id: str
    state: str
    progress: float
    path: str
    start_ms: int
    end_ms: int
    #: Done: recording time of the first picture (the keyframe at or before
    #: the chosen start) and the clip's length, gaps left out.
    first_ms: int | None = None
    duration_ms: int | None = None
    size_bytes: int | None = None
    audio: bool | None = None
    gaps: int = 0
    skipped_files: int = 0
    truncated: bool = False
    error: str = ""

    @classmethod
    def from_json(cls, value: dict[str, Any]) -> ExportJob:
        def optional_int(key: str) -> int | None:
            raw = value.get(key)
            return int(raw) if isinstance(raw, int | float) else None

        audio = value.get("audio")
        return cls(
            job_id=str(value.get("job_id", "")),
            state=str(value.get("state", RUNNING)),
            progress=max(0.0, min(1.0, float(value.get("progress", 0.0) or 0.0))),
            path=str(value.get("path", "")),
            start_ms=int(value.get("start_ms", 0) or 0),
            end_ms=int(value.get("end_ms", 0) or 0),
            first_ms=optional_int("first_ms"),
            duration_ms=optional_int("duration_ms"),
            size_bytes=optional_int("size_bytes"),
            audio=audio if isinstance(audio, bool) else None,
            gaps=int(value.get("gaps", 0) or 0),
            skipped_files=int(value.get("skipped_files", 0) or 0),
            truncated=bool(value.get("truncated", False)),
            error=str(value.get("error", "") or ""),
        )

    @property
    def finished(self) -> bool:
        return self.state != RUNNING


def start_params(
    camera_id: str, start_ms: int, end_ms: int, path: str, *, overwrite: bool
) -> dict[str, Any]:
    """``export.start`` parameters."""
    return {
        "camera_id": camera_id,
        "start_ms": int(start_ms),
        "end_ms": int(end_ms),
        "path": path,
        "overwrite": overwrite,
    }


def check_clip(start_ms: int, end_ms: int) -> str | None:
    """Why a clip range cannot be exported, or ``None``."""
    if end_ms <= start_ms:
        return "The clip's end must be later than its start."
    if end_ms - start_ms > MAX_CLIP_MS:
        return "A clip can be at most 24 hours long."
    return None


MAX_TIMELAPSE_MS = 31 * MAX_CLIP_MS
MAX_TIMELAPSE_FRAMES = 500_000


def check_timelapse(start_ms: int, end_ms: int, interval_sec: int, fps: int) -> str | None:
    if not 1 <= interval_sec <= 3600 or not 1 <= fps <= 60:
        return "Use a sampling interval of 1–3600 seconds and a frame rate of 1–60 fps."
    if start_ms < 0 or end_ms <= start_ms or end_ms - start_ms > MAX_TIMELAPSE_MS:
        return "Choose a forward timelapse range of at most 31 days."
    if (end_ms - start_ms + interval_sec * 1000 - 1) // (
        interval_sec * 1000
    ) > MAX_TIMELAPSE_FRAMES:
        return "Use a longer sampling interval for this time range."
    return None


def default_clip_name(camera_name: str, start_ms: int) -> str:
    """``<camera>_<YYYY-MM-DD_HH-MM-SS>.mp4`` (local time), as the backend names it."""
    name = re.sub(r"[^\w-]", "_", camera_name).strip("_") or "camera"
    at = datetime.datetime.fromtimestamp(start_ms / 1000)
    return f"{name}_{at:%Y-%m-%d_%H-%M-%S}.mp4"


def with_mp4_suffix(path: str) -> str:
    """``path`` ending in ``.mp4`` (any case kept)."""
    return path if os.path.splitext(path)[1].lower() == ".mp4" else f"{path}.mp4"


def format_length(ms: int) -> str:
    """``H:MM:SS`` for an hour or more, else ``M:SS``."""
    seconds = max(0, round(ms / 1000))
    hours, rest = divmod(seconds, 3600)
    minutes, secs = divmod(rest, 60)
    return f"{hours}:{minutes:02d}:{secs:02d}" if hours else f"{minutes}:{secs:02d}"


def format_size(count: int) -> str:
    units = ("B", "KB", "MB", "GB", "TB")
    value = float(count)
    unit = 0
    while value >= 1000 and unit < len(units) - 1:
        value /= 1000
        unit += 1
    shown = f"{value:.0f}" if value >= 10 or unit == 0 else f"{value:.1f}"
    return f"{shown} {units[unit]}"


def progress_text(job: ExportJob) -> str:
    return f"Exporting… {round(job.progress * 100)}%"


def result_text(job: ExportJob) -> str:
    """What a finished job says."""
    if job.state == CANCELLED:
        return "Export cancelled. No file was saved."
    if job.state == FAILED:
        return f"Export failed: {job.error or 'unknown error'}"
    parts = [f"Saved {os.path.basename(job.path)}"]
    details = []
    if job.duration_ms is not None:
        details.append(format_length(job.duration_ms))
    if job.size_bytes is not None:
        details.append(format_size(job.size_bytes))
    if details:
        parts[0] += f" ({', '.join(details)})"
    return ". ".join([parts[0], *result_notes(job)]) + "."


def result_notes(job: ExportJob) -> list[str]:
    """What a done clip holds that the user might not expect."""
    notes = []
    if job.first_ms is not None and job.start_ms - job.first_ms >= 500:
        early = (job.start_ms - job.first_ms) / 1000
        notes.append(f"It starts {early:.1f} s early, at the previous keyframe")
    if job.gaps:
        notes.append(
            f"{job.gaps} gap{'s' if job.gaps != 1 else ''} in the recording "
            f"{'were' if job.gaps != 1 else 'was'} left out"
        )
    if job.truncated:
        notes.append("It ends early, where the recording's format changes")
    if job.skipped_files:
        notes.append(
            f"{job.skipped_files} unreadable recording file"
            f"{'s were' if job.skipped_files != 1 else ' was'} skipped"
        )
    return notes
