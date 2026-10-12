"""Adding several searched cameras at once with one login (no Qt).

The Add Camera dialog's Search tab ticks the cameras to add and takes one
username and password for all of them. For each camera the backend signs in
over ONVIF and picks the main and sub streams (``camera.discovery.profiles``),
then adds it (``camera.discovery.add``). These types track each camera's
progress and word the results; ``AddEditCameraDialog`` runs the calls.
"""

from __future__ import annotations

from dataclasses import dataclass, field

#: Phases a ticked camera goes through.
IDLE = "idle"
CHECKING = "checking"
SIGNED_IN = "signed_in"
LOGIN_FAILED = "login_failed"
NO_STREAMS = "no_streams"
ADDING = "adding"
ADDED = "added"
FAILED = "failed"

#: Phases still in progress (the dialog's buttons wait for them).
BUSY = {CHECKING, ADDING}


@dataclass
class RowProgress:
    """Where one camera is in a check or an add."""

    phase: str = IDLE
    detail: str = ""

    @property
    def text(self) -> str:
        """The row's status column."""
        if self.phase == CHECKING:
            return "Signing in…"
        if self.phase == ADDING:
            return "Adding…"
        if self.phase == SIGNED_IN:
            return f"Login works · {self.detail}" if self.detail else "Login works"
        if self.phase == ADDED:
            return "Added ✓"
        if self.phase == LOGIN_FAILED:
            return "Wrong username or password"
        if self.phase == NO_STREAMS:
            return "No video stream found"
        if self.phase == FAILED:
            return self.detail or "Could not add"
        return ""

    @property
    def tone(self) -> str:
        if self.phase in (SIGNED_IN, ADDED):
            return "success"
        if self.phase in (LOGIN_FAILED, NO_STREAMS, FAILED):
            return "error"
        return "info"


def phase_for_error(message: str) -> str:
    """The phase a backend error puts a camera in."""
    lowered = message.lower()
    if "username and password" in lowered or "unauthorized" in lowered:
        return LOGIN_FAILED
    return FAILED


def stream_detail(result: dict) -> str:
    """ "1920×1080" for the main stream ``camera.discovery.profiles`` picked."""
    main = result.get("main_stream_id")
    for stream in result.get("streams") or []:
        if isinstance(stream, dict) and stream.get("stream_id") == main:
            width, height = stream.get("width"), stream.get("height")
            if isinstance(width, int) and isinstance(height, int) and width and height:
                return f"{width}×{height}"
    return ""


def camera_names(rows: list[tuple[str, str, str]], taken: set[str]) -> dict[str, str]:
    """A name for each ``(candidate_id, title, address)``: the device's title,
    with its address added when the title alone would be ambiguous (another
    camera in the batch or already configured has it, or it is generic)."""
    counts: dict[str, int] = {}
    for _cid, title, _address in rows:
        counts[title] = counts.get(title, 0) + 1
    lowered = {name.lower() for name in taken}
    names: dict[str, str] = {}
    for cid, title, address in rows:
        ambiguous = counts[title] > 1 or title.lower() in lowered or title in ("", "ONVIF device")
        names[cid] = f"{title or 'Camera'} ({address})" if ambiguous else title
    return names


@dataclass
class BatchSummary:
    """What a check or an add came to."""

    added: int = 0
    signed_in: int = 0
    login_failed: int = 0
    other_failed: int = 0
    failures: list[str] = field(default_factory=list)

    @classmethod
    def of(cls, progress: dict[str, RowProgress], ids: list[str]) -> BatchSummary:
        summary = cls()
        for cid in ids:
            row = progress.get(cid, RowProgress())
            if row.phase == ADDED:
                summary.added += 1
            elif row.phase == SIGNED_IN:
                summary.signed_in += 1
            elif row.phase == LOGIN_FAILED:
                summary.login_failed += 1
            elif row.phase in (NO_STREAMS, FAILED):
                summary.other_failed += 1
                summary.failures.append(row.text)
        return summary

    def message(self, total: int, adding: bool) -> tuple[str, str]:
        """(status line, tone) for the batch just finished."""
        if adding and self.added == total:
            return (f"Added {self.added} camera{'s' if self.added != 1 else ''}.", "success")
        if not adding and self.signed_in == total:
            return ("The login works on every ticked camera. Click Add to add them.", "success")
        parts = []
        if adding and self.added:
            parts.append(f"Added {self.added} of {total}.")
        elif not adding and self.signed_in:
            parts.append(f"The login works on {self.signed_in} of {total}.")
        if self.login_failed:
            parts.append(
                f"{self.login_failed} refused it: untick the others, enter that camera's "
                "login and try again."
            )
        if self.other_failed:
            parts.append(f"{self.other_failed} could not be added (see Status).")
        return (" ".join(parts), "warning" if (self.added or self.signed_in) else "error")
