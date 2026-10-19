"""Free and unlimited builds, as the recorder reports them (``system.status``).

A free build runs up to 16 cameras; the others stay in the configuration but
do not run until there is room or NeuroWatch Unlimited is installed. No Qt here.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

#: JSON-RPC error code of ``camera.add`` refused by the camera limit.
CAMERA_LIMIT_REACHED = -32010
DOWNLOADS_URL = "https://neurowatch.app/downloads"
UNLIMITED_OFFER = (
    "NeuroWatch Unlimited costs USD 50 once and has no software camera limit. "
    "Capacity depends on your hardware. Includes a support response within 24 hours. "
    "Contact hello@neurowatch.app for support."
)


@dataclass(frozen=True)
class Edition:
    """The recorder's edition and the cameras its limit keeps from running."""

    edition: str = "free"
    camera_limit: int | None = None
    over_limit: tuple[str, ...] = field(default_factory=tuple)  # camera names
    downloads_url: str = DOWNLOADS_URL

    @classmethod
    def from_status(cls, status: object) -> Edition:
        if not isinstance(status, dict):
            return cls()
        limit = status.get("camera_limit")
        held = status.get("cameras_over_limit")
        names = tuple(
            str(c.get("name") or c.get("id") or "")
            for c in (held if isinstance(held, list) else [])
            if isinstance(c, dict)
        )
        url = status.get("downloads_url")
        if not (isinstance(url, str) and url.startswith("https://")):
            url = DOWNLOADS_URL
        return cls(
            edition=str(status.get("edition") or "free"),
            camera_limit=limit if isinstance(limit, int) else None,
            over_limit=names,
            downloads_url=url,
        )

    @property
    def unlimited_url(self) -> str:
        """The downloads page, opened at NeuroWatch Unlimited."""
        return f"{self.downloads_url}?edition=unlimited"

    @property
    def notice(self) -> str:
        """Status-bar text while cameras are held back; empty otherwise."""
        count = len(self.over_limit)
        if not count:
            return ""
        noun = "camera" if count == 1 else "cameras"
        return f"{count} {noun} not running (free version: {self.camera_limit} cameras)"

    @property
    def notice_detail(self) -> str:
        if not self.over_limit:
            return ""
        return (
            f"This free version of NeuroWatch runs up to {self.camera_limit} cameras. "
            f"Not running: {', '.join(self.over_limit)}. They are kept as they are and run "
            "again when you remove other cameras or install NeuroWatch Unlimited.\n"
            f"{UNLIMITED_OFFER}\nClick to get NeuroWatch Unlimited."
        )


def limit_message(data: Any) -> str:
    """What to tell someone whose new camera was refused by the limit."""
    limit = data.get("camera_limit") if isinstance(data, dict) else None
    limit_text = f"{limit} cameras" if isinstance(limit, int) else "its camera limit"
    return (
        f"This free version of NeuroWatch runs up to {limit_text}. To add more, "
        f"remove a camera or install NeuroWatch Unlimited.\n{UNLIMITED_OFFER}"
    )
