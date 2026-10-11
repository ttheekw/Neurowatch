"""Camera search results from the backend (``camera.discovery.*``).

The backend searches the network and talks ONVIF; these types only read its
answers for the Add Camera dialog. Unknown fields and states are tolerated so
an older UI keeps working with a newer backend.
"""

from __future__ import annotations

import ipaddress
from dataclasses import dataclass

from neurowatch.models.camera import CameraStream

MAX_SCAN_ADDRESSES = 1024

_STATUS = {
    "ready": "Ready",
    "credentials_needed": "Needs password",
    "unavailable": "Unavailable",
}
_ADDABLE = {"ready", "credentials_needed", "possible_match"}


def _text(value: object) -> str | None:
    return value if isinstance(value, str) and value else None


def _number(value: object) -> int | None:
    return value if isinstance(value, int) and not isinstance(value, bool) and value > 0 else None


@dataclass(frozen=True)
class Candidate:
    """A device that answered a search."""

    candidate_id: str
    address: str
    state: str
    manufacturer: str | None = None
    model: str | None = None
    name: str | None = None
    firmware: str | None = None
    matched_name: str | None = None
    endpoint_reference: str | None = None

    @classmethod
    def from_dict(cls, raw: dict) -> Candidate:
        matched = raw.get("matched_camera")
        return cls(
            candidate_id=str(raw.get("candidate_id", "")),
            address=str(raw.get("address", "")),
            state=str(raw.get("state", "")),
            manufacturer=_text(raw.get("manufacturer")),
            model=_text(raw.get("model")),
            name=_text(raw.get("name")),
            firmware=_text(raw.get("firmware")),
            matched_name=_text(matched.get("name")) if isinstance(matched, dict) else None,
            endpoint_reference=_text(raw.get("endpoint_reference")),
        )

    @property
    def title(self) -> str:
        brand = " ".join(part for part in (self.manufacturer, self.model) if part)
        return self.name or brand or "ONVIF device"

    @property
    def status_text(self) -> str:
        if self.state == "already_added":
            return f"Added as {self.matched_name}" if self.matched_name else "Already added"
        if self.state == "possible_match":
            return f"May be {self.matched_name}" if self.matched_name else "Possible match"
        return _STATUS.get(self.state, "Unknown")

    @property
    def can_add(self) -> bool:
        return self.state in _ADDABLE


@dataclass(frozen=True)
class StreamChoice:
    """One stream a device offers. ``url`` has no credentials."""

    stream_id: str
    name: str
    width: int | None = None
    height: int | None = None
    encoding: str | None = None
    source: str | None = None
    url: str | None = None

    @classmethod
    def from_dict(cls, raw: dict) -> StreamChoice:
        return cls(
            stream_id=str(raw.get("stream_id", "")),
            name=str(raw.get("name") or ""),
            width=_number(raw.get("width")),
            height=_number(raw.get("height")),
            encoding=_text(raw.get("encoding")),
            source=_text(raw.get("source")),
            url=_text(raw.get("url")),
        )

    @property
    def pixels(self) -> int:
        return (self.width or 0) * (self.height or 0)

    @property
    def label(self) -> str:
        size = f"{self.width}×{self.height}" if self.width and self.height else None
        return " · ".join(part for part in (self.name, size, self.encoding) if part)


def check_range(text: str) -> str | None:
    """Why ``text`` cannot be searched, or None. The backend checks again."""
    try:
        network = ipaddress.IPv4Network(text.strip(), strict=False)
    except ValueError:
        return "Enter a range such as 192.168.1.0/24, or one address."
    hosts = network.num_addresses if network.prefixlen >= 31 else network.num_addresses - 2
    if hosts > MAX_SCAN_ADDRESSES:
        return f"A search covers at most {MAX_SCAN_ADDRESSES} addresses (a /22 or smaller)."
    return None


def discovered_streams(choices: list[StreamChoice]) -> list[CameraStream]:
    """The camera's stream list: highest resolution first, as most NVRs number them."""
    with_url = [c for c in choices if c.url]
    ordered = sorted(with_url, key=lambda c: -c.pixels)
    return [
        CameraStream(
            id=str(index + 1), url=c.url or "", name=c.name, width=c.width, height=c.height
        )
        for index, c in enumerate(ordered)
    ]
